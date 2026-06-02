//! `clewdr import-config` — restore from a bundle in one transaction.
//!
//! Two modes:
//! - `--mode merge` (default): UPSERT each row by primary / unique keys.
//!   Rows already present that don't appear in the bundle are preserved;
//!   rows that do conflict are replaced. The local admin row is preserved
//!   unless `--overwrite-admin` is set.
//! - `--mode restore --yes`: truncate every config-like table, then
//!   re-insert from the bundle. Bundle ids are kept verbatim unless the
//!   local admin row is being preserved, in which case ids are translated
//!   so foreign keys point at the preserved admin. The `--yes` guard is
//!   mandatory because this destroys local data.
//!
//! Schema diff is permissive but explicit: extra bundle columns are
//! warned and dropped; missing bundle columns let the DB default fire,
//! unless the column is NOT NULL with no default (then the import refuses
//! before any writes happen).
//!
//! All work runs inside a single SQLite transaction with
//! `PRAGMA defer_foreign_keys = ON`, so a failure mid-import — version
//! mismatch, schema drift, FK violation at commit — leaves the database
//! exactly as it was. `--dry-run` rolls back the same transaction
//! unconditionally and only prints the per-table counts.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use chrono::Utc;
use colored::Colorize;
use serde::Deserialize;
use sqlx::{SqliteConnection, SqlitePool};

use crate::{
    cli::bundle::{
        BUNDLE_VERSION, Bundle, CellValue, NEVER_EXPORTED, SETTINGS_KEYS_NEVER_EXPORTED,
        TableSchema, read_table_schema as read_db_schema,
    },
    cli::crypto,
    config::{CONFIG_PATH, DB_PATH},
    error::ClewdrError,
    services::version::parse_clewdr_version,
};

/// Order rows are written into the DB. Parents-before-children — even
/// though we run with `defer_foreign_keys=ON` and could insert in any
/// order, parent-first keeps statement-time errors comprehensible if a
/// non-FK constraint fires.
const TABLE_INSERT_ORDER: &[&str] = &[
    "policies",
    "proxies",
    "users",
    "accounts",
    "account_runtime_state",
    "api_keys",
    "api_key_account_bindings",
    "settings",
    "models",
    "model_pricing",
    "usage_rollups",
    "usage_lifetime_totals",
    "usage_daily_rollups",
    "usage_daily_rollup_state",
];

/// Tables wiped at the start of `--mode restore`. Reverse FK order so each
/// child's rows are gone before its parent's, even though deferred FKs
/// would also accept any delete order.
const TABLE_DELETE_ORDER_RESTORE: &[&str] = &[
    "usage_daily_rollup_state",
    "usage_daily_rollups",
    "usage_lifetime_totals",
    "usage_rollups",
    "model_pricing",
    "models",
    "settings",
    "api_key_account_bindings",
    "api_keys",
    "account_runtime_state",
    "accounts",
    "users",
    "proxies",
    "policies",
];

/// Junction tables that get wiped before insertion regardless of mode —
/// per the plan, the bindings table is always restored from the bundle so
/// `api_keys` UPSERT churn doesn't leave orphan associations behind.
const TABLES_WIPED_EVEN_IN_MERGE: &[&str] = &["api_key_account_bindings"];

/// Per-table spec used by merge mode to translate cross-table FKs and
/// avoid binding the bundle's synthetic primary key (which would collide
/// with any local row that happens to share the same numeric id — the
/// common case when two independently-seeded DBs both start allocating
/// ids from 1).
#[derive(Debug, Clone, Copy)]
struct TableMergeSpec {
    /// Synthetic PK column to drop from INSERT in merge mode. `None` for
    /// tables without a synthetic id (the natural key is the PK).
    id_column: Option<&'static str>,
    /// Natural key columns that form the UPSERT conflict target.
    natural_key: &'static [&'static str],
    /// Foreign keys: (column name, parent table). Each gets translated
    /// from the bundle's id space to the local id space via the running
    /// id_maps before the row is inserted.
    fks: &'static [(&'static str, &'static str)],
}

fn merge_spec(table: &str) -> TableMergeSpec {
    match table {
        "policies" => TableMergeSpec {
            id_column: Some("id"),
            natural_key: &["name"],
            fks: &[],
        },
        "users" => TableMergeSpec {
            id_column: Some("id"),
            natural_key: &["username"],
            fks: &[("policy_id", "policies")],
        },
        "proxies" => TableMergeSpec {
            id_column: Some("id"),
            natural_key: &["name"],
            fks: &[],
        },
        "accounts" => TableMergeSpec {
            id_column: Some("id"),
            natural_key: &["name"],
            fks: &[("proxy_id", "proxies")],
        },
        "api_keys" => TableMergeSpec {
            id_column: Some("id"),
            natural_key: &["lookup_key"],
            fks: &[("user_id", "users")],
        },
        "api_key_account_bindings" => TableMergeSpec {
            id_column: None,
            natural_key: &["api_key_id", "account_id"],
            fks: &[("api_key_id", "api_keys"), ("account_id", "accounts")],
        },
        "account_runtime_state" => TableMergeSpec {
            id_column: None,
            natural_key: &["account_id"],
            fks: &[("account_id", "accounts")],
        },
        "settings" => TableMergeSpec {
            id_column: None,
            natural_key: &["key"],
            fks: &[],
        },
        "models" => TableMergeSpec {
            id_column: None,
            natural_key: &["model_id"],
            fks: &[],
        },
        "model_pricing" => TableMergeSpec {
            id_column: Some("id"),
            natural_key: &["pricing_key"],
            fks: &[],
        },
        "usage_rollups" => TableMergeSpec {
            id_column: Some("id"),
            natural_key: &["user_id", "period_type", "period_start"],
            fks: &[("user_id", "users")],
        },
        "usage_lifetime_totals" => TableMergeSpec {
            id_column: None,
            natural_key: &["user_id"],
            fks: &[("user_id", "users")],
        },
        "usage_daily_rollups" => TableMergeSpec {
            id_column: Some("id"),
            natural_key: &["user_id", "model_key", "bucket_date_local"],
            fks: &[("user_id", "users")],
        },
        // Single-row state table: id is always 1 (CHECK constraint), so we
        // merge by id directly without translating it from the bundle's id
        // space. No FKs.
        "usage_daily_rollup_state" => TableMergeSpec {
            id_column: None,
            natural_key: &["id"],
            fks: &[],
        },
        _ => TableMergeSpec {
            id_column: None,
            natural_key: &[],
            fks: &[],
        },
    }
}

/// `parent_table → (bundle_id → local_id)` populated as merge mode imports
/// rows in FK-parent order. Children translate their FK columns through it.
type IdMaps = std::collections::HashMap<&'static str, std::collections::HashMap<i64, i64>>;

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// UPSERT each row by its UNIQUE key. Preserves rows not in the bundle.
    Merge,
    /// Truncate each config-like table, then re-insert from bundle.
    /// Requires `--yes` to confirm; admin user is preserved unless `--overwrite-admin`.
    Restore,
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Bundle path produced by `export-config`.
    pub path: PathBuf,

    /// Conflict strategy. Defaults to `merge`.
    #[arg(long, value_enum, default_value_t = Mode::Merge)]
    pub mode: Mode,

    /// Required when `--mode restore`. Acknowledges that target tables will be truncated.
    #[arg(long)]
    pub yes: bool,

    /// Overwrite the existing admin row even if its username matches.
    #[arg(long)]
    pub overwrite_admin: bool,

    /// Skip the actual writes; report what would change.
    #[arg(long)]
    pub dry_run: bool,

    /// Allow opening (creating) a fresh database. By default, import refuses on a missing DB.
    #[arg(long)]
    pub init: bool,

    /// Bypass minor version mismatch refusal. Major mismatch is always refused.
    #[arg(long)]
    pub force: bool,

    /// Read decryption passphrase from stdin instead of `/dev/tty`.
    #[arg(long)]
    pub passphrase_stdin: bool,
}

#[derive(Debug, Default, Clone)]
pub struct TableCounts {
    pub inserted: usize,
    pub skipped_admin: usize,
    pub skipped_blocked_settings: usize,
    pub deleted_first: usize,
}

#[derive(Debug, Default, Clone)]
pub struct ImportSummary {
    pub per_table: BTreeMap<String, TableCounts>,
    pub schema_warnings: Vec<String>,
}

pub async fn run(args: Args) -> Result<(), ClewdrError> {
    let cfg = read_minimal_config();
    if cfg.no_fs() {
        return Err(ClewdrError::BadRequest {
            msg: "import-config is not supported in no_fs mode (in-memory state is ephemeral)",
        });
    }

    if args.mode == Mode::Restore && !args.yes {
        return Err(ClewdrError::BadRequest {
            msg: "`--mode restore` truncates target tables; pass --yes to confirm",
        });
    }

    let raw = fs::read(&args.path)?;
    let bundle = parse_bundle(&raw, args.passphrase_stdin)?;
    check_version_compatibility(&bundle, args.force)?;
    detect_redacted_bundle(&bundle)?;

    let db_path = DB_PATH.to_owned();
    let pool = if args.init {
        crate::db::init_pool(&db_path).await?
    } else {
        crate::db::open_existing_pool(&db_path).await?
    };

    let summary = apply_bundle(&pool, &bundle, &args).await;
    pool.close().await;
    let summary = summary?;

    if !args.dry_run && !bundle.config_toml.trim().is_empty() {
        // Restore config_toml AFTER the DB import committed. If we wrote
        // the file first and the DB import then rolled back, the on-disk
        // config would point at restored state the DB doesn't actually
        // have.
        restore_config_toml(CONFIG_PATH.as_path(), &bundle.config_toml)?;
    }

    print_summary(&summary, &bundle, args.dry_run);
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// Bundle parsing + version compat
// ──────────────────────────────────────────────────────────────────────────

/// Read a bundle (encrypted or plaintext) into the in-memory [`Bundle`].
///
/// The encrypted path matches on [`crypto::ENCRYPTED_BUNDLE_MAGIC`] and
/// asks [`crypto::read_import_passphrase`] for the passphrase before
/// handing the ciphertext to [`crypto::decrypt_bundle`]. The plaintext
/// path runs `serde_json::from_slice` directly. Either way we end up
/// validating `version` against [`BUNDLE_VERSION`] before returning.
fn parse_bundle(raw: &[u8], passphrase_stdin: bool) -> Result<Bundle, ClewdrError> {
    if raw.starts_with(crypto::ENCRYPTED_BUNDLE_MAGIC) {
        let pwd = crypto::read_import_passphrase(passphrase_stdin)?;
        let plaintext = crypto::decrypt_bundle(raw, &pwd)?;
        return parse_plaintext_bundle(&plaintext);
    }
    parse_plaintext_bundle(raw)
}

fn parse_plaintext_bundle(raw: &[u8]) -> Result<Bundle, ClewdrError> {
    let bundle: Bundle = serde_json::from_slice(raw)?;
    if bundle.version != BUNDLE_VERSION {
        return Err(ClewdrError::BadRequestMessage {
            msg: if bundle.version > BUNDLE_VERSION {
                format!(
                    "unsupported bundle version {} (this clewdr reads version {}); upgrade clewdr to import this bundle",
                    bundle.version, BUNDLE_VERSION
                )
            } else {
                format!(
                    "unsupported bundle version {} (this clewdr reads version {})",
                    bundle.version, BUNDLE_VERSION
                )
            },
        });
    }
    Ok(bundle)
}

fn check_version_compatibility(bundle: &Bundle, force: bool) -> Result<(), ClewdrError> {
    let bundle_v = parse_clewdr_version(&bundle.clewdr_version)?;
    let current_v = parse_clewdr_version(env!("CARGO_PKG_VERSION"))?;
    if bundle_v.major > current_v.major {
        return Err(ClewdrError::BadRequest {
            msg: "bundle was produced by a newer major version; refusing to import",
        });
    }
    if bundle_v.major == current_v.major && bundle_v.minor > current_v.minor && !force {
        return Err(ClewdrError::BadRequest {
            msg: "bundle is from a newer minor version; pass --force to attempt anyway",
        });
    }
    Ok(())
}

/// Refuse to import a `--no-secrets`-redacted bundle.
///
/// Bundles produced by `export-config --no-secrets` null out secret
/// columns (cookie_blob, oauth_*_token, plaintext_key, key_hash,
/// password_hash, proxies.password). Those columns participate in
/// NOT NULL / CHECK constraints in the live schema — `accounts.cookie_blob
/// NOT NULL` for cookie auth, `api_keys.key_hash NOT NULL` always, the
/// users table CHECK that requires admin to have a password_hash. So a
/// redacted bundle can't actually be restored: the INSERTs roll back at
/// the constraint check, leaving the operator with a generic SQLite
/// error instead of a usable backup.
///
/// We catch this up front, before BEGIN, with an explicit message that
/// points at the right fix (re-export without `--no-secrets`).
fn detect_redacted_bundle(bundle: &Bundle) -> Result<(), ClewdrError> {
    if let Some(rows) = bundle.tables.get("accounts") {
        for row in rows {
            let auth = match row.get("auth_source") {
                Some(CellValue::Text(s)) => s.as_str(),
                _ => continue,
            };
            let null_or_missing =
                |col: &str| -> bool { matches!(row.get(col), None | Some(CellValue::Null)) };
            match auth {
                "cookie" if null_or_missing("cookie_blob") => {
                    return Err(ClewdrError::BadRequest {
                        msg: "bundle has accounts row with cookie auth but cookie_blob is NULL — \
                              this bundle was produced with --no-secrets and cannot be imported. \
                              Re-export without --no-secrets, or rotate credentials manually after a partial restore.",
                    });
                }
                "oauth"
                    if null_or_missing("oauth_access_token")
                        || null_or_missing("oauth_refresh_token") =>
                {
                    return Err(ClewdrError::BadRequest {
                        msg: "bundle has accounts row with oauth auth but oauth tokens are NULL — \
                              this bundle was produced with --no-secrets and cannot be imported. \
                              Re-export without --no-secrets.",
                    });
                }
                // Step 5 / C11: ApiKey rows need both base_url AND
                // secret. `api_key_base_url` is admin-supplied and
                // NOT redacted (it's metadata, not a credential), so
                // a `--no-secrets` bundle will have base_url present
                // but secret NULL. The mutex CHECK on the destination
                // schema would reject the partial row at INSERT time
                // anyway; catching it here gives the operator a clear
                // message pointing at the right fix instead of a raw
                // SQLite constraint violation.
                "api_key"
                    if null_or_missing("api_key_secret") || null_or_missing("api_key_base_url") =>
                {
                    return Err(ClewdrError::BadRequest {
                        msg: "bundle has accounts row with api_key auth but api_key_secret or \
                              api_key_base_url is NULL — this bundle was produced with --no-secrets \
                              and cannot be imported. Re-export without --no-secrets, or recreate \
                              the api_key account after a partial restore.",
                    });
                }
                _ => {}
            }
        }
    }
    if let Some(rows) = bundle.tables.get("api_keys") {
        for row in rows {
            if matches!(row.get("key_hash"), None | Some(CellValue::Null)) {
                return Err(ClewdrError::BadRequest {
                    msg: "bundle has api_keys row with NULL key_hash — \
                          this bundle was produced with --no-secrets and cannot be imported. \
                          Re-export without --no-secrets, or recreate keys after partial restore.",
                });
            }
        }
    }
    if let Some(rows) = bundle.tables.get("users") {
        for row in rows {
            let role = matches!(row.get("role"), Some(CellValue::Text(s)) if s == "admin");
            if role && matches!(row.get("password_hash"), None | Some(CellValue::Null)) {
                return Err(ClewdrError::BadRequest {
                    msg: "bundle has admin user with NULL password_hash — \
                          this bundle was produced with --no-secrets and cannot be imported. \
                          Re-export without --no-secrets, then run `clewdr reset-admin-password` if you want to rotate.",
                });
            }
        }
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// Apply bundle (single transaction)
// ──────────────────────────────────────────────────────────────────────────

async fn apply_bundle(
    pool: &SqlitePool,
    bundle: &Bundle,
    args: &Args,
) -> Result<ImportSummary, ClewdrError> {
    let mut summary = ImportSummary::default();

    // Pre-flight: every table we'd write needs a column plan compatible
    // with the live schema. Doing this *before* BEGIN means a hopeless
    // import bails without ever taking write locks on the DB.
    let mut conn = pool.acquire().await?;
    let mut plans: BTreeMap<String, ColumnPlan> = BTreeMap::new();
    for table in TABLE_INSERT_ORDER {
        if !bundle.tables.contains_key(*table) {
            continue;
        }
        let bundle_schema = bundle.schema.get(*table).ok_or(ClewdrError::BadRequest {
            msg: "bundle has rows but no schema for a table",
        })?;
        let db_schema = read_db_schema(&mut conn, table).await?;
        let plan = ColumnPlan::build(bundle_schema, &db_schema, table)?;
        for warning in &plan.warnings {
            summary.schema_warnings.push(warning.clone());
        }
        plans.insert((*table).to_string(), plan);
    }

    // Reject never-exported tables in a bundle outright. Runtime tables
    // are imported when present because `export-config --include-runtime`
    // advertises a full round trip for account state and usage counters.
    for table in bundle.tables.keys() {
        if NEVER_EXPORTED.contains(&table.as_str()) {
            summary
                .schema_warnings
                .push(format!("ignored never-exported table in bundle: {table}"));
        } else if !TABLE_INSERT_ORDER.contains(&table.as_str()) {
            summary
                .schema_warnings
                .push(format!("ignored unknown table in bundle: {table}"));
        }
    }

    // Begin transaction with deferred FKs so we can insert children before
    // parents within a single batch (FK violations only fail at COMMIT).
    sqlx::query("BEGIN").execute(&mut *conn).await?;
    sqlx::query("PRAGMA defer_foreign_keys = ON")
        .execute(&mut *conn)
        .await?;

    let result = apply_tables(&mut conn, bundle, args, &plans, &mut summary).await;

    // End the transaction.
    //
    // - dry-run: roll back unconditionally; we never want writes to land.
    // - apply_tables errored: roll back; bubble the original error up.
    // - apply_tables OK + COMMIT OK: success.
    // - apply_tables OK + COMMIT errored: deferred-FK violations only
    //   surface at COMMIT time, so swallowing this would let us report
    //   "import committed" while SQLite has actually rolled the txn back.
    //   Treat the COMMIT error as the import error.
    if args.dry_run || result.is_err() {
        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
        result?;
    } else {
        sqlx::query("COMMIT").execute(&mut *conn).await?;
    }
    Ok(summary)
}

async fn apply_tables(
    conn: &mut SqliteConnection,
    bundle: &Bundle,
    args: &Args,
    plans: &BTreeMap<String, ColumnPlan>,
    summary: &mut ImportSummary,
) -> Result<(), ClewdrError> {
    // Cache local-admin presence once per import. Admin protection applies
    // in both merge and restore modes unless `--overwrite-admin` is set:
    // restore is destructive, but it still must not replace the operator's
    // local admin password/session state by default.
    let preserve_local_admin = if !args.overwrite_admin {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT 1 FROM users WHERE username = 'admin' LIMIT 1")
                .fetch_optional(&mut *conn)
                .await?;
        row.is_some()
    } else {
        false
    };

    // Restore mode: wipe every table in TABLE_DELETE_ORDER_RESTORE,
    // *regardless* of whether the bundle contains rows for it. Restore
    // means "the live DB matches the bundle exactly"; a table missing
    // from an older bundle should be left empty, not preserved with
    // local rows. The only exception is the local admin row (and its
    // referenced policy) when admin preservation is active.
    if args.mode == Mode::Restore {
        for table in TABLE_DELETE_ORDER_RESTORE {
            let res = wipe_table(conn, table, preserve_local_admin).await?;
            summary
                .per_table
                .entry((*table).to_string())
                .or_default()
                .deleted_first = res;
        }
    }

    // Per-table id maps for FK translation. Merge mode always translates
    // synthetic ids. Restore mode normally writes bundle ids verbatim, but
    // admin preservation keeps the local admin id/policy row alive; in
    // that mode we also translate ids so the bundle's remaining rows can
    // land without colliding with the preserved local rows.
    let mut id_maps: IdMaps = std::collections::HashMap::new();
    let translate_preserved_admin = args.mode == Mode::Restore && preserve_local_admin;
    let translate_all_ids = args.mode == Mode::Merge || translate_preserved_admin;

    for table in TABLE_INSERT_ORDER {
        let Some(plan) = plans.get(*table) else {
            continue;
        };
        let Some(rows) = bundle.tables.get(*table) else {
            continue;
        };

        // Junction tables we always wipe — even in merge mode — so api_keys
        // UPSERT churn doesn't leave orphan rows behind.
        if args.mode == Mode::Merge && TABLES_WIPED_EVEN_IN_MERGE.contains(table) {
            let res = wipe_table(conn, table, false).await?;
            summary
                .per_table
                .entry((*table).to_string())
                .or_default()
                .deleted_first = res;
        }

        let spec = merge_spec(table);

        // Per-mode column list:
        //   plain restore       → keep `id` so bundle ids land verbatim
        //                         into freshly wiped tables.
        //   merge/admin-restore → drop `id` so natural-key UPSERTs can
        //                         preserve local ids where needed. The
        //                         mapping is captured below so child
        //                         tables can translate.
        let writable: Vec<String> = if translate_all_ids && let Some(id_col) = spec.id_column {
            plan.writable
                .iter()
                .filter(|c| c.as_str() != id_col)
                .cloned()
                .collect()
        } else {
            plan.writable.clone()
        };

        let counts = summary.per_table.entry((*table).to_string()).or_default();
        for bundle_row in rows {
            // Admin protection: skip the bundle's admin row only when
            // there's a *local* admin worth preserving. On `--init` or
            // any DB without an admin, we must let the bundle's admin
            // through — otherwise the resulting DB has no login and any
            // bundled api_keys.user_id pointing at the admin will FK-
            // violate at COMMIT.
            if *table == "users"
                && preserve_local_admin
                && matches!(bundle_row.get("username"), Some(CellValue::Text(s)) if s == "admin")
            {
                counts.skipped_admin += 1;
                // Even though we didn't write this row, we still need
                // an id_map entry so any bundled api_keys row with
                // user_id pointing at the bundle's admin can translate
                // to the local admin id. Without this, FK translation
                // would fail with "no matching row in parent table".
                if let Some(id_col) = spec.id_column
                    && let Some(CellValue::Integer(bundle_id)) = bundle_row.get(id_col)
                {
                    let local_id =
                        capture_local_id(conn, table, spec.natural_key, bundle_row).await?;
                    if let Some(lid) = local_id {
                        id_maps.entry(*table).or_default().insert(*bundle_id, lid);
                    }
                }
                continue;
            }
            if *table == "settings"
                && let Some(CellValue::Text(k)) = bundle_row.get("key")
                && SETTINGS_KEYS_NEVER_EXPORTED.contains(&k.as_str())
            {
                counts.skipped_blocked_settings += 1;
                continue;
            }

            // Merge mode FK translation: each FK column in this row gets
            // its bundle id rewritten to the matching local id captured
            // when we processed the parent table earlier.
            let row_to_insert: BTreeMap<String, CellValue> =
                if translate_all_ids && !spec.fks.is_empty() {
                    let mut translated = bundle_row.clone();
                    for (fk_col, parent_table) in spec.fks {
                        translate_fk(&mut translated, fk_col, parent_table, &id_maps)?;
                    }
                    translated
                } else {
                    bundle_row.clone()
                };

            execute_row_insert(
                conn,
                table,
                &writable,
                spec.natural_key,
                &row_to_insert,
                if translate_all_ids {
                    Mode::Merge
                } else {
                    args.mode
                },
            )
            .await?;
            counts.inserted += 1;

            // Capture the local id post-UPSERT so descendant tables can
            // translate their FKs through this entry.
            if translate_all_ids
                && let Some(id_col) = spec.id_column
                && let Some(CellValue::Integer(bundle_id)) = bundle_row.get(id_col)
            {
                let local_id =
                    capture_local_id(conn, table, spec.natural_key, &row_to_insert).await?;
                if let Some(lid) = local_id {
                    id_maps.entry(*table).or_default().insert(*bundle_id, lid);
                }
            }
        }
    }
    Ok(())
}

/// Rewrite a foreign-key column on `row` from a bundle id to the local id
/// captured when the parent table was imported. Nullable FKs (NULL or
/// missing column) are left alone; an FK that has no matching parent
/// entry is a fatal import error — we'd rather roll back the whole
/// transaction than smuggle an orphaned row that COMMIT would reject
/// anyway.
fn translate_fk(
    row: &mut BTreeMap<String, CellValue>,
    fk_col: &str,
    parent_table: &str,
    id_maps: &IdMaps,
) -> Result<(), ClewdrError> {
    let bundle_id = match row.get(fk_col) {
        Some(CellValue::Integer(i)) => *i,
        // Nullable FKs and missing columns stay as-is.
        None | Some(CellValue::Null) => return Ok(()),
        _ => {
            return Err(ClewdrError::ConflictMessage {
                msg: format!(
                    "FK column {fk_col} has non-integer value; cannot translate during merge"
                ),
            });
        }
    };
    let local_id = id_maps
        .get(parent_table)
        .and_then(|m| m.get(&bundle_id))
        .copied();
    match local_id {
        Some(lid) => {
            row.insert(fk_col.to_string(), CellValue::Integer(lid));
            Ok(())
        }
        None => Err(ClewdrError::ConflictMessage {
            msg: format!(
                "FK {fk_col} = {bundle_id} (bundle id) has no matching row in parent table {parent_table}; \
                 bundle is internally inconsistent"
            ),
        }),
    }
}

/// Look up the local row's `id` after a merge UPSERT, so child rows can
/// have their FK column translated. Tables without an `id` column return
/// `None`; they are not FK targets in the import graph, so no downstream
/// translation is needed.
async fn capture_local_id(
    conn: &mut SqliteConnection,
    table: &str,
    natural_key: &[&str],
    row: &BTreeMap<String, CellValue>,
) -> Result<Option<i64>, ClewdrError> {
    if natural_key.is_empty() {
        return Ok(None);
    }
    use sqlx::Row as _;
    let where_clause = natural_key
        .iter()
        .map(|c| format!("{c} = ?"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!("SELECT id FROM {table} WHERE {where_clause}");
    let mut q = sqlx::query(&sql);
    for col in natural_key {
        let val = row.get(*col).cloned().unwrap_or(CellValue::Null);
        q = bind_cell(q, val);
    }
    let row_opt = q.fetch_optional(&mut *conn).await?;
    Ok(row_opt.and_then(|r| r.try_get::<i64, _>("id").ok()))
}

async fn execute_row_insert(
    conn: &mut SqliteConnection,
    table: &str,
    writable: &[String],
    natural_key: &[&str],
    row: &BTreeMap<String, CellValue>,
    mode: Mode,
) -> Result<(), ClewdrError> {
    if writable.is_empty() {
        return Ok(());
    }

    let cols = writable.join(", ");
    let placeholders = vec!["?"; writable.len()].join(", ");
    let sql = match mode {
        Mode::Restore => {
            // Tables were wiped first — no conflict possible. Plain INSERT
            // is the simplest form here and gives clean errors if the
            // bundle is internally inconsistent (duplicate natural keys).
            format!("INSERT INTO {table} ({cols}) VALUES ({placeholders})")
        }
        Mode::Merge => build_merge_upsert_sql(table, writable, natural_key),
    };

    let mut q = sqlx::query(&sql);
    for col in writable {
        let cell = row.get(col).cloned().unwrap_or(CellValue::Null);
        q = bind_cell(q, cell);
    }
    q.execute(&mut *conn).await?;
    Ok(())
}

/// Build the `INSERT … ON CONFLICT(natural_key) DO UPDATE SET col=excluded.col …`
/// statement used by merge mode.
///
/// Critically, this does NOT use `INSERT OR REPLACE`. Replace would
/// implement conflict resolution by *deleting* the existing row before
/// inserting the bundle row — and ON DELETE CASCADE on child tables would
/// then drop local-only rows that don't conflict with the bundle (e.g. a
/// local user's local-only api_keys when merging the user). True UPSERT
/// keeps the same row id, so cascades never fire.
fn build_merge_upsert_sql(table: &str, cols: &[String], natural_key: &[&str]) -> String {
    let cols_list = cols.join(", ");
    let placeholders = vec!["?"; cols.len()].join(", ");
    if natural_key.is_empty() {
        // No declared conflict target — fall back to a permissive insert.
        return format!("INSERT OR IGNORE INTO {table} ({cols_list}) VALUES ({placeholders})");
    }
    let target = natural_key.join(", ");
    let updates: Vec<String> = cols
        .iter()
        .filter(|c| !natural_key.contains(&c.as_str()))
        .filter(|c| c.as_str() != "id") // never reassign row id on update
        .map(|c| format!("{c} = excluded.{c}"))
        .collect();
    if updates.is_empty() {
        format!(
            "INSERT INTO {table} ({cols_list}) VALUES ({placeholders}) ON CONFLICT({target}) DO NOTHING"
        )
    } else {
        format!(
            "INSERT INTO {table} ({cols_list}) VALUES ({placeholders}) ON CONFLICT({target}) DO UPDATE SET {set}",
            set = updates.join(", ")
        )
    }
}

fn bind_cell<'q>(
    q: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    cell: CellValue,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    match cell {
        // Any T works for a NULL bind; pick a small one.
        CellValue::Null => q.bind(Option::<i64>::None),
        CellValue::Integer(i) => q.bind(i),
        CellValue::Real(f) => q.bind(f),
        CellValue::Text(s) => q.bind(s),
        CellValue::Blob(b) => q.bind(b),
    }
}

/// Delete every row from `table`, except for `settings` rows whose `key`
/// is in [`SETTINGS_KEYS_NEVER_EXPORTED`]. The session_secret row in
/// particular must survive a restore — it's intentionally excluded from
/// the bundle so each host mints its own, and wiping it would break every
/// active admin session and every signed cookie.
async fn wipe_table(
    conn: &mut SqliteConnection,
    table: &str,
    preserve_local_admin: bool,
) -> Result<usize, ClewdrError> {
    if table == "settings" {
        debug_assert!(
            !SETTINGS_KEYS_NEVER_EXPORTED.is_empty(),
            "settings restore relies on at least one preserved per-host key"
        );
        let placeholders = vec!["?"; SETTINGS_KEYS_NEVER_EXPORTED.len()].join(", ");
        let sql = format!("DELETE FROM settings WHERE key NOT IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for k in SETTINGS_KEYS_NEVER_EXPORTED {
            q = q.bind(*k);
        }
        let res = q.execute(&mut *conn).await?;
        return Ok(res.rows_affected() as usize);
    }
    if preserve_local_admin && table == "users" {
        let res = sqlx::query("DELETE FROM users WHERE username != 'admin'")
            .execute(&mut *conn)
            .await?;
        return Ok(res.rows_affected() as usize);
    }
    if preserve_local_admin && table == "policies" {
        let res = sqlx::query(
            "DELETE FROM policies WHERE id NOT IN (
                 SELECT policy_id FROM users WHERE username = 'admin'
             )",
        )
        .execute(&mut *conn)
        .await?;
        return Ok(res.rows_affected() as usize);
    }
    let res = sqlx::query(&format!("DELETE FROM {table}"))
        .execute(&mut *conn)
        .await?;
    Ok(res.rows_affected() as usize)
}

// ──────────────────────────────────────────────────────────────────────────
// Schema diff
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ColumnPlan {
    /// Columns we'll write — present in both bundle.schema and live db.schema.
    pub writable: Vec<String>,
    /// Bundle columns the live DB doesn't have. Dropped (silently for INSERT,
    /// noisily as a warning).
    pub extra_in_bundle: Vec<String>,
    /// DB columns the bundle doesn't carry. We let the DB default fire by
    /// omitting them from INSERT; if the column is NOT NULL with no default
    /// the import is rejected before any writes happen.
    pub missing_in_bundle: Vec<String>,
    /// Human-readable warnings emitted up to the operator.
    pub warnings: Vec<String>,
}

impl ColumnPlan {
    fn build(bundle: &TableSchema, db: &TableSchema, table: &str) -> Result<Self, ClewdrError> {
        let bundle_cols: BTreeSet<&str> = bundle.columns.iter().map(|c| c.name.as_str()).collect();
        let db_cols: BTreeMap<&str, &crate::cli::bundle::ColumnInfo> =
            db.columns.iter().map(|c| (c.name.as_str(), c)).collect();

        let mut writable = Vec::new();
        let mut extra_in_bundle = Vec::new();
        let mut missing_in_bundle = Vec::new();
        let mut warnings = Vec::new();

        for col in &bundle.columns {
            if db_cols.contains_key(col.name.as_str()) {
                writable.push(col.name.clone());
            } else {
                extra_in_bundle.push(col.name.clone());
                warnings.push(format!(
                    "table {table}: bundle column {} not present in live DB; dropped",
                    col.name
                ));
            }
        }
        for (name, info) in &db_cols {
            if !bundle_cols.contains(name) {
                if info.notnull && info.default_value.is_none() {
                    return Err(ClewdrError::BadRequest {
                        msg: "live DB has a required column that the bundle is missing",
                    });
                }
                missing_in_bundle.push((*name).to_string());
                warnings.push(format!(
                    "table {table}: live DB column {name} not in bundle; will use default/NULL"
                ));
            }
        }

        Ok(Self {
            writable,
            extra_in_bundle,
            missing_in_bundle,
            warnings,
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Config TOML restore
// ──────────────────────────────────────────────────────────────────────────

fn restore_config_toml(path: &Path, contents: &str) -> Result<(), ClewdrError> {
    // This runs after the DB transaction commits. If the rename/write
    // fails, the DB has already been restored; the timestamped .bak is the
    // operator's recovery point for the previous config file.
    if path.exists() {
        let suffix = format!("toml.bak.{}", Utc::now().format("%Y%m%dT%H%M%SZ"));
        let backup = path.with_extension(suffix);
        fs::rename(path, &backup)?;
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)?;
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// Output
// ──────────────────────────────────────────────────────────────────────────

fn print_summary(summary: &ImportSummary, bundle: &Bundle, dry_run: bool) {
    let prefix = if dry_run {
        format!("{} ", "[dry-run]".cyan().bold())
    } else {
        String::new()
    };
    eprintln!(
        "{prefix}imported bundle from clewdr v{} produced {}",
        bundle.clewdr_version, bundle.produced_at
    );

    for warning in &summary.schema_warnings {
        eprintln!("  {} {warning}", "warn:".yellow().bold());
    }

    for table in TABLE_INSERT_ORDER {
        if let Some(c) = summary.per_table.get(*table) {
            let parts = [
                format!("inserted={}", c.inserted),
                format!("skipped_admin={}", c.skipped_admin),
                format!("skipped_blocked_settings={}", c.skipped_blocked_settings),
                format!("wiped_first={}", c.deleted_first),
            ];
            eprintln!("  {prefix}{:24} {}", format!("{table}:"), parts.join(", "));
        }
    }

    if dry_run {
        eprintln!(
            "{} no changes written ({})",
            "✓".cyan().bold(),
            "rolled back as part of --dry-run".cyan()
        );
    } else {
        eprintln!("{} import committed", "✓".green().bold());
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Config plumbing
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct ImportConfig {
    pub no_fs: Option<bool>,
}

impl ImportConfig {
    fn no_fs(&self) -> bool {
        self.no_fs.unwrap_or(false)
    }
}

fn read_minimal_config() -> ImportConfig {
    use figment::{
        Figment,
        providers::{Env, Format, Toml},
    };
    Figment::from(Toml::file(CONFIG_PATH.as_path()))
        .admerge(Env::prefixed("CLEWDR_").split("__"))
        .extract()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{
        bundle::CellValue,
        export::{Args as ExportArgs, build_bundle},
    };

    /// Build a freshly-seeded DB with one cookie account + one api key,
    /// then return the path so tests can run import workflows against it.
    async fn seeded_db(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("seed.db");
        let pool = crate::db::init_pool(&path).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO accounts (name, rr_order, max_slots, status, auth_source, cookie_blob, organization_uuid)
             VALUES ('alpha', 1, 5, 'active', 'cookie', X'AAAA', 'org-a')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO api_keys (user_id, label, lookup_key, key_hash, plaintext_key)
             VALUES (1, 'k1', 'sk-prefix1', X'1111', 'sk-1')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
        path
    }

    fn export_args(out: PathBuf) -> ExportArgs {
        ExportArgs {
            path: out,
            no_encrypt: true,
            no_secrets: false,
            include_runtime: false,
            passphrase_stdin: false,
        }
    }

    fn export_args_with_runtime(out: PathBuf) -> ExportArgs {
        ExportArgs {
            include_runtime: true,
            ..export_args(out)
        }
    }

    fn import_args(path: PathBuf, mode: Mode) -> Args {
        Args {
            path,
            mode,
            yes: matches!(mode, Mode::Restore),
            overwrite_admin: false,
            dry_run: false,
            init: false,
            force: false,
            passphrase_stdin: false,
        }
    }

    #[tokio::test]
    async fn round_trip_export_import_restore_preserves_rows() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");

        // Source DB
        let src_db = seeded_db(&dir).await;
        let bundle_path = dir.path().join("bundle.json");
        let bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();
        std::fs::write(&bundle_path, serde_json::to_vec(&bundle).unwrap()).unwrap();

        // Target DB — start with a different account and a different api key
        // so we can verify restore truly wipes them.
        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO accounts (name, rr_order, max_slots, status, auth_source, cookie_blob, organization_uuid)
             VALUES ('local-only', 1, 5, 'active', 'cookie', X'BBBB', 'org-local')",
        )
        .execute(&pool).await.unwrap();
        pool.close().await;

        // Apply restore
        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        let summary = apply_bundle(
            &pool,
            &bundle,
            &Args {
                path: bundle_path,
                mode: Mode::Restore,
                yes: true,
                overwrite_admin: false,
                dry_run: false,
                init: false,
                force: false,
                passphrase_stdin: false,
            },
        )
        .await
        .unwrap();

        // Restore wiped local-only and brought in alpha.
        let names: Vec<(String,)> = sqlx::query_as("SELECT name FROM accounts ORDER BY name")
            .fetch_all(&pool)
            .await
            .unwrap();
        let names: Vec<String> = names.into_iter().map(|(n,)| n).collect();
        assert_eq!(names, vec!["alpha"]);
        assert!(summary.per_table.contains_key("accounts"));
        // Wipe + insert means deleted_first is non-zero on accounts.
        assert!(summary.per_table["accounts"].deleted_first >= 1);
        pool.close().await;
    }

    #[tokio::test]
    async fn merge_preserves_local_only_rows() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");

        let src_db = seeded_db(&dir).await;
        let bundle_path = dir.path().join("bundle.json");
        let bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();

        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        // Pin local-only to non-conflicting id / rr_order so the merge
        // doesn't tickle PK or UNIQUE collision against the bundle's
        // alpha row (id=1, rr_order=1). Merge wins on conflict, so
        // overlapping-ids would clobber local-only — this test asserts
        // the *non-conflicting* coexistence semantic.
        sqlx::query(
            "INSERT INTO accounts (id, name, rr_order, max_slots, status, auth_source, cookie_blob, organization_uuid)
             VALUES (99, 'local-only', 99, 5, 'active', 'cookie', X'BBBB', 'org-local')",
        )
        .execute(&pool).await.unwrap();
        pool.close().await;

        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        apply_bundle(&pool, &bundle, &import_args(bundle_path, Mode::Merge))
            .await
            .unwrap();

        // Both 'alpha' (from bundle) and 'local-only' (already there) survive.
        let names: Vec<(String,)> = sqlx::query_as("SELECT name FROM accounts ORDER BY name")
            .fetch_all(&pool)
            .await
            .unwrap();
        let names: Vec<String> = names.into_iter().map(|(n,)| n).collect();
        assert_eq!(names, vec!["alpha", "local-only"]);
        pool.close().await;
    }

    #[tokio::test]
    async fn admin_row_is_preserved_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");
        let src_db = seeded_db(&dir).await;
        let bundle_path = dir.path().join("bundle.json");
        let bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();

        // Target DB: change admin's password_hash to something distinguishable.
        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        sqlx::query("UPDATE users SET password_hash = ?1 WHERE username = 'admin'")
            .bind("$argon2id$LOCAL_DISTINCT_HASH")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        let summary = apply_bundle(&pool, &bundle, &import_args(bundle_path, Mode::Merge))
            .await
            .unwrap();

        let (hash,): (String,) =
            sqlx::query_as("SELECT password_hash FROM users WHERE username = 'admin'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            hash, "$argon2id$LOCAL_DISTINCT_HASH",
            "admin row was clobbered by import without --overwrite-admin"
        );
        assert!(summary.per_table["users"].skipped_admin >= 1);
        pool.close().await;
    }

    #[tokio::test]
    async fn restore_preserves_admin_row_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");
        let src_db = seeded_db(&dir).await;
        let bundle_path = dir.path().join("bundle.json");
        let bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();

        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        sqlx::query(
            "UPDATE users SET password_hash = ?1, session_version = 42 WHERE username = 'admin'",
        )
        .bind("$argon2id$LOCAL_RESTORE_HASH")
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        let summary = apply_bundle(&pool, &bundle, &import_args(bundle_path, Mode::Restore))
            .await
            .unwrap();

        let (hash, session_version): (String, i64) = sqlx::query_as(
            "SELECT password_hash, session_version FROM users WHERE username = 'admin'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(hash, "$argon2id$LOCAL_RESTORE_HASH");
        assert_eq!(session_version, 42);
        assert!(summary.per_table["users"].skipped_admin >= 1);
        pool.close().await;
    }

    #[tokio::test]
    async fn overwrite_admin_replaces_admin_row() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");
        let src_db = seeded_db(&dir).await;
        let bundle_path = dir.path().join("bundle.json");
        let bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();
        let bundle_admin_hash: String = bundle.tables["users"]
            .iter()
            .find(|r| matches!(r.get("username"), Some(CellValue::Text(s)) if s == "admin"))
            .and_then(|r| match r.get("password_hash") {
                Some(CellValue::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap();

        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        sqlx::query("UPDATE users SET password_hash = ?1 WHERE username = 'admin'")
            .bind("$argon2id$LOCAL_DISTINCT_HASH")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        let mut args = import_args(bundle_path, Mode::Merge);
        args.overwrite_admin = true;
        apply_bundle(&pool, &bundle, &args).await.unwrap();

        let (hash,): (String,) =
            sqlx::query_as("SELECT password_hash FROM users WHERE username = 'admin'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            hash, bundle_admin_hash,
            "admin should have been overwritten with bundle's hash"
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn session_secret_is_never_imported() {
        // Even if a malicious bundle slipped session_secret into settings,
        // import must refuse to write it.
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");
        let src_db = seeded_db(&dir).await;
        let bundle_path = dir.path().join("bundle.json");
        let mut bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();
        // Inject an attacker-controlled session_secret row into the bundle.
        let mut row = std::collections::BTreeMap::new();
        row.insert(
            "key".to_string(),
            CellValue::Text("session_secret".to_string()),
        );
        row.insert(
            "value".to_string(),
            CellValue::Text("ATTACKER_SECRET".to_string()),
        );
        bundle.tables.get_mut("settings").unwrap().push(row);

        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        let (local_secret_before,): (String,) =
            sqlx::query_as("SELECT value FROM settings WHERE key = 'session_secret'")
                .fetch_one(&pool)
                .await
                .unwrap();
        pool.close().await;

        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        apply_bundle(&pool, &bundle, &import_args(bundle_path, Mode::Merge))
            .await
            .unwrap();

        let (local_secret_after,): (String,) =
            sqlx::query_as("SELECT value FROM settings WHERE key = 'session_secret'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(local_secret_before, local_secret_after);
        assert_ne!(local_secret_after, "ATTACKER_SECRET");
        pool.close().await;
    }

    #[tokio::test]
    async fn restore_preserves_session_secret() {
        // Restore mode wipes settings, but session_secret must survive
        // because the bundle deliberately doesn't carry it. Otherwise a
        // restored DB would have an empty session_secret and every active
        // admin cookie would silently break.
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");
        let src_db = seeded_db(&dir).await;
        let bundle_path = dir.path().join("bundle.json");
        let bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();

        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        let (secret_before,): (String,) =
            sqlx::query_as("SELECT value FROM settings WHERE key = 'session_secret'")
                .fetch_one(&pool)
                .await
                .unwrap();
        pool.close().await;

        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        apply_bundle(
            &pool,
            &bundle,
            &Args {
                path: bundle_path,
                mode: Mode::Restore,
                yes: true,
                overwrite_admin: false,
                dry_run: false,
                init: false,
                force: false,
                passphrase_stdin: false,
            },
        )
        .await
        .unwrap();

        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM settings WHERE key = 'session_secret'")
                .fetch_optional(&pool)
                .await
                .unwrap();
        let (secret_after,) = row.expect("session_secret was wiped by --mode restore");
        assert_eq!(
            secret_after, secret_before,
            "session_secret value changed across restore (must be preserved per-host)"
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn restore_imports_runtime_tables_from_include_runtime_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");

        let src_db = seeded_db(&dir).await;
        let pool = crate::db::open_existing_pool(&src_db).await.unwrap();
        sqlx::query(
            "INSERT INTO account_runtime_state (account_id, reset_time) VALUES (1, 123456)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO usage_rollups (
                 user_id, period_type, period_start, period_end, request_count, cost_nanousd
             ) VALUES (1, 'week', '2026-04-27T00:00:00Z', '2026-05-04T00:00:00Z', 7, 9001)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO usage_lifetime_totals (user_id, request_count, cost_nanousd)
             VALUES (1, 11, 12001)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Daily rollup fixture for one user/day/model bucket. The migration
        // also seeded usage_daily_rollup_state, so both new runtime tables
        // need to round-trip cleanly.
        sqlx::query(
            "INSERT INTO usage_daily_rollups (
                 user_id, model_key, bucket_date_local,
                 request_count, input_tokens, output_tokens, cost_nanousd
             ) VALUES (1, 'claude-opus-4-7', '2026-05-01', 3, 1500, 700, 8888)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let bundle_path = dir.path().join("runtime-bundle.json");
        let bundle = build_bundle(
            &src_db,
            &cfg_path,
            &export_args_with_runtime(bundle_path.clone()),
        )
        .await
        .unwrap();
        assert!(bundle.tables.contains_key("account_runtime_state"));
        assert!(bundle.tables.contains_key("usage_rollups"));
        assert!(bundle.tables.contains_key("usage_lifetime_totals"));
        assert!(bundle.tables.contains_key("usage_daily_rollups"));
        assert!(bundle.tables.contains_key("usage_daily_rollup_state"));

        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        pool.close().await;

        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        let mut args = import_args(bundle_path, Mode::Restore);
        args.overwrite_admin = true;
        let summary = apply_bundle(&pool, &bundle, &args).await.unwrap();

        let (reset_time,): (i64,) =
            sqlx::query_as("SELECT reset_time FROM account_runtime_state WHERE account_id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(reset_time, 123456);

        let (rollup_requests, rollup_cost): (i64, i64) = sqlx::query_as(
            "SELECT request_count, cost_nanousd FROM usage_rollups
             WHERE user_id = 1 AND period_type = 'week'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((rollup_requests, rollup_cost), (7, 9001));

        let (lifetime_requests, lifetime_cost): (i64, i64) = sqlx::query_as(
            "SELECT request_count, cost_nanousd FROM usage_lifetime_totals WHERE user_id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((lifetime_requests, lifetime_cost), (11, 12001));

        // Daily rollup round-trips with the (user, model, bucket) row intact.
        let (daily_requests, daily_input, daily_cost): (i64, i64, i64) = sqlx::query_as(
            "SELECT request_count, input_tokens, cost_nanousd FROM usage_daily_rollups
             WHERE user_id = 1 AND model_key = 'claude-opus-4-7'
               AND bucket_date_local = '2026-05-01'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((daily_requests, daily_input, daily_cost), (3, 1500, 8888));

        // The single-row state table is preserved (the bundle's row replaces
        // the local row that init_pool just seeded).
        let (state_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM usage_daily_rollup_state")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state_count, 1, "state table must still hold one row");

        assert_eq!(summary.per_table["account_runtime_state"].inserted, 1);
        assert_eq!(summary.per_table["usage_rollups"].inserted, 1);
        assert_eq!(summary.per_table["usage_lifetime_totals"].inserted, 1);
        assert_eq!(summary.per_table["usage_daily_rollups"].inserted, 1);
        assert_eq!(summary.per_table["usage_daily_rollup_state"].inserted, 1);
        pool.close().await;
    }

    #[tokio::test]
    async fn dry_run_does_not_persist_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");
        let src_db = seeded_db(&dir).await;
        let bundle_path = dir.path().join("bundle.json");
        let bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();

        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        let count_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
            .fetch_one(&pool)
            .await
            .unwrap();
        pool.close().await;

        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        let mut args = import_args(bundle_path, Mode::Merge);
        args.dry_run = true;
        let summary = apply_bundle(&pool, &bundle, &args).await.unwrap();
        // Counts still report what *would have* been inserted.
        assert!(summary.per_table["accounts"].inserted >= 1);

        let count_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count_before, count_after,
            "--dry-run leaked writes into the DB"
        );
        pool.close().await;
    }

    #[test]
    fn version_check_rejects_newer_major() {
        let mut bundle: Bundle = sample_bundle();
        bundle.clewdr_version = "999.0.0".to_string();
        assert!(check_version_compatibility(&bundle, false).is_err());
        // --force does NOT bypass major.
        assert!(check_version_compatibility(&bundle, true).is_err());
    }

    #[test]
    fn version_check_minor_mismatch_needs_force() {
        let mut bundle: Bundle = sample_bundle();
        // Force a higher minor relative to env's CARGO_PKG_VERSION.
        let cur = parse_clewdr_version(env!("CARGO_PKG_VERSION")).unwrap();
        bundle.clewdr_version = format!("{}.{}.0", cur.major, cur.minor + 1);
        assert!(check_version_compatibility(&bundle, false).is_err());
        assert!(check_version_compatibility(&bundle, true).is_ok());
    }

    #[test]
    fn version_check_same_or_older_is_silent() {
        let mut bundle: Bundle = sample_bundle();
        let cur = parse_clewdr_version(env!("CARGO_PKG_VERSION")).unwrap();
        bundle.clewdr_version = format!("{}.{}.0", cur.major, cur.minor);
        assert!(check_version_compatibility(&bundle, false).is_ok());
    }

    #[test]
    fn parse_plaintext_bundle_rejects_encrypted_magic() {
        // Feeding raw encrypted bytes to the plaintext parser must fail at
        // serde_json — the magic byte sequence is not valid JSON.
        let mut buf = crypto::ENCRYPTED_BUNDLE_MAGIC.to_vec();
        buf.extend_from_slice(&[0u8; 64]);
        assert!(parse_plaintext_bundle(&buf).is_err());
    }

    #[cfg(feature = "encrypt")]
    #[tokio::test]
    async fn encrypted_bundle_round_trips_through_crypto_helpers() {
        // End-to-end: build a real bundle, encrypt it with crypto::encrypt_bundle,
        // then feed the ciphertext through crypto::decrypt_bundle +
        // parse_plaintext_bundle and verify we get the same Bundle back.
        // (parse_bundle's TTY/stdin path can't be exercised in `cargo test`
        // — there's no stdin attached — so we drive the helpers directly.)
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");
        let src_db = seeded_db(&dir).await;
        let bundle_path = dir.path().join("bundle.json");
        let bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();

        let json = serde_json::to_vec(&bundle).unwrap();
        let ciphertext = crypto::encrypt_bundle(&json, "round-trip-passphrase").unwrap();
        assert!(ciphertext.starts_with(crypto::ENCRYPTED_BUNDLE_MAGIC));

        let decrypted = crypto::decrypt_bundle(&ciphertext, "round-trip-passphrase").unwrap();
        let restored = parse_plaintext_bundle(&decrypted).unwrap();
        assert_eq!(restored.version, bundle.version);
        assert_eq!(restored.clewdr_version, bundle.clewdr_version);
        assert_eq!(restored.tables.len(), bundle.tables.len());

        // Wrong passphrase: AEAD authentication failure surfaces as
        // BadRequest, not as a JSON parse error.
        assert!(crypto::decrypt_bundle(&ciphertext, "different-passphrase").is_err());
    }

    // ──────────────────────────────────────────────────────────────────
    // Regression tests for review #7 fixes
    // ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn commit_failure_propagates_when_deferred_fk_violates() {
        // Build a bundle whose api_keys row references a user that the
        // bundle doesn't carry. With deferred FKs the INSERT succeeds
        // and the failure only surfaces at COMMIT — apply_bundle must
        // propagate that error rather than reporting "import committed".
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");
        let src_db = seeded_db(&dir).await;
        let bundle_path = dir.path().join("bundle.json");
        let mut bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();
        // Sabotage: remove every users row from the bundle so the api_keys
        // entry we leave behind has no parent.
        bundle.tables.get_mut("users").unwrap().clear();

        // Restore against a fresh DB so the wipe + insert path runs.
        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        pool.close().await;

        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        let res = apply_bundle(
            &pool,
            &bundle,
            &Args {
                path: bundle_path,
                mode: Mode::Restore,
                yes: true,
                overwrite_admin: true,
                dry_run: false,
                init: false,
                force: false,
                passphrase_stdin: false,
            },
        )
        .await;
        // Must be an Err — this is the case where the previous
        // implementation silently succeeded.
        assert!(
            res.is_err(),
            "apply_bundle reported success despite COMMIT failure"
        );

        // And the on-disk DB must still have its admin row (the rollback
        // restored everything).
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE username = 'admin'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1, "rollback didn't restore the admin row");
        pool.close().await;
    }

    #[tokio::test]
    async fn merge_does_not_cascade_delete_local_only_children() {
        // Local has admin + a local-only api_key for admin. Bundle has the
        // same admin (different password_hash) but no api_keys. Under the
        // old INSERT OR REPLACE strategy, REPLACE would DELETE the local
        // admin row, CASCADE-deleting the local-only api_key. The new
        // ON CONFLICT(username) DO UPDATE keeps the same row id so no
        // cascade fires.
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");

        // Source DB has its own admin and no api_keys (we'll wipe rows
        // before exporting so the bundle has just admin + system tables).
        let src_db = dir.path().join("src.db");
        let pool = crate::db::init_pool(&src_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        pool.close().await;
        let bundle_path = dir.path().join("bundle.json");
        let bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();

        // Target DB: admin (id=1) + a local-only api_key for admin.
        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO api_keys (user_id, label, lookup_key, key_hash, plaintext_key)
             VALUES (1, 'local-only-key', 'sk-local-keep', X'CCCC', 'sk-local')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        // Force the bundle's admin row to *differ* from the local admin
        // (different password_hash) so the UPSERT actually has work to do.
        let mut bundle = bundle;
        for row in bundle.tables.get_mut("users").unwrap() {
            if matches!(row.get("username"), Some(CellValue::Text(s)) if s == "admin") {
                row.insert(
                    "password_hash".to_string(),
                    CellValue::Text("$argon2id$BUNDLE_HASH".to_string()),
                );
            }
        }

        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        let mut args = import_args(bundle_path, Mode::Merge);
        args.overwrite_admin = true; // we want the upsert path, not skip
        apply_bundle(&pool, &bundle, &args).await.unwrap();

        // Local-only api_key must still be there. Under the buggy
        // implementation it would have been CASCADE-deleted when REPLACE
        // dropped the local admin row.
        let (n,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM api_keys WHERE label = 'local-only-key'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            n, 1,
            "local-only api_key was cascade-deleted by merge UPSERT"
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn merge_imports_admin_when_no_local_admin_exists() {
        // Fresh DB with no admin (mimics `--init` against a new file).
        // The merge mode admin-skip must NOT fire — otherwise the resulting
        // DB has no login and bundled api_keys.user_id=1 would FK-violate.
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");
        let src_db = seeded_db(&dir).await;
        let bundle_path = dir.path().join("bundle.json");
        let bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();

        // Target: schema only, NO seed_admin. users table is empty.
        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        let (n_before,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n_before, 0, "fixture should start empty");
        pool.close().await;

        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        let summary = apply_bundle(&pool, &bundle, &import_args(bundle_path, Mode::Merge))
            .await
            .unwrap();

        let (n_admin,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM users WHERE username = 'admin'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            n_admin, 1,
            "admin from bundle was wrongly skipped against an empty DB"
        );
        // skipped_admin should be 0 in this scenario.
        assert_eq!(summary.per_table["users"].skipped_admin, 0);
        pool.close().await;
    }

    #[tokio::test]
    async fn redacted_no_secrets_bundle_is_refused_at_preflight() {
        // Build a normal bundle, then redact like --no-secrets would.
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");
        let src_db = seeded_db(&dir).await;
        let bundle_path = dir.path().join("bundle.json");
        let mut bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();
        // Null cookie_blob on every cookie account, mirroring --no-secrets.
        for row in bundle.tables.get_mut("accounts").unwrap() {
            if matches!(row.get("auth_source"), Some(CellValue::Text(s)) if s == "cookie") {
                row.insert("cookie_blob".to_string(), CellValue::Null);
            }
        }

        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        pool.close().await;

        // The reading path goes through fs::read so we round-trip through
        // disk to exercise the public entry point as much as feasible.
        std::fs::write(
            dir.path().join("redacted.json"),
            serde_json::to_vec(&bundle).unwrap(),
        )
        .unwrap();

        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        let res = apply_bundle(
            &pool,
            &bundle,
            &import_args(bundle_path.clone(), Mode::Merge),
        )
        .await;
        // apply_bundle itself is OK — preflight runs in `run`. So we
        // emulate the preflight directly.
        // (We exercise apply_bundle here for completeness; the redaction
        // detection lives upstream of it.)
        let _ = res;

        // The actual contract:
        assert!(detect_redacted_bundle(&bundle).is_err());
        pool.close().await;
    }

    /// Step 5 / C11: a `--no-secrets` bundle that contains an api_key
    /// account must be refused with an explicit error pointing at the
    /// right fix, not silently imported as a half-credential row that
    /// the schema mutex CHECK would then reject at INSERT time.
    #[test]
    fn detect_redacted_bundle_rejects_api_key_row_with_null_secret() {
        use crate::cli::bundle::CellValue;

        let mut row: std::collections::BTreeMap<String, CellValue> =
            std::collections::BTreeMap::new();
        row.insert("id".to_string(), CellValue::Integer(1));
        row.insert("name".to_string(), CellValue::Text("acct".to_string()));
        row.insert(
            "auth_source".to_string(),
            CellValue::Text("api_key".to_string()),
        );
        row.insert(
            "api_key_base_url".to_string(),
            CellValue::Text("https://api.anthropic.com/".to_string()),
        );
        // Secret nulled (simulating --no-secrets).
        row.insert("api_key_secret".to_string(), CellValue::Null);

        let bundle = Bundle {
            version: 1,
            produced_at: "2026-05-25T00:00:00Z".to_string(),
            clewdr_version: env!("CARGO_PKG_VERSION").to_string(),
            schema: std::collections::BTreeMap::new(),
            config_toml: String::new(),
            tables: std::collections::BTreeMap::from([("accounts".to_string(), vec![row])]),
            skipped: Vec::new(),
        };

        let err = detect_redacted_bundle(&bundle).unwrap_err();
        assert!(matches!(err, ClewdrError::BadRequest { .. }));
        let msg = err.to_string();
        assert!(
            msg.contains("api_key"),
            "error message should mention api_key, got: {msg}"
        );
    }

    /// Companion: a complete api_key row (both base_url AND secret
    /// present) must pass the redaction check so legitimate full
    /// bundles continue to import.
    #[test]
    fn detect_redacted_bundle_accepts_complete_api_key_row() {
        use crate::cli::bundle::CellValue;

        let mut row: std::collections::BTreeMap<String, CellValue> =
            std::collections::BTreeMap::new();
        row.insert("id".to_string(), CellValue::Integer(1));
        row.insert("name".to_string(), CellValue::Text("acct".to_string()));
        row.insert(
            "auth_source".to_string(),
            CellValue::Text("api_key".to_string()),
        );
        row.insert(
            "api_key_base_url".to_string(),
            CellValue::Text("https://api.anthropic.com/".to_string()),
        );
        row.insert(
            "api_key_secret".to_string(),
            CellValue::Text("sk-ant-test".to_string()),
        );

        let bundle = Bundle {
            version: 1,
            produced_at: "2026-05-25T00:00:00Z".to_string(),
            clewdr_version: env!("CARGO_PKG_VERSION").to_string(),
            schema: std::collections::BTreeMap::new(),
            config_toml: String::new(),
            tables: std::collections::BTreeMap::from([("accounts".to_string(), vec![row])]),
            skipped: Vec::new(),
        };

        assert!(detect_redacted_bundle(&bundle).is_ok());
    }

    #[tokio::test]
    async fn restore_wipes_tables_missing_from_bundle() {
        // Local has an extra proxy and an extra account. Bundle's
        // accounts table is empty; bundle is missing `proxies` entirely
        // (simulating an older / partial export). Restore must still
        // wipe both: restored state is "the bundle, exactly", and the
        // bundle says "no proxies / no accounts".
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");

        // Source DB exports a bundle with no accounts and no proxies.
        let src_db = dir.path().join("src.db");
        let pool = crate::db::init_pool(&src_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        pool.close().await;
        let bundle_path = dir.path().join("bundle.json");
        let mut bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();
        // Remove proxies entirely from the bundle to mimic an older
        // export that predates the proxies table.
        bundle.tables.remove("proxies");
        bundle.schema.remove("proxies");

        // Target DB has a local-only proxy + a local-only account.
        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO proxies (name, protocol, host, port) VALUES ('local-proxy', 'http', 'h.example', 1080)",
        )
        .execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO accounts (name, rr_order, max_slots, status, auth_source, cookie_blob, organization_uuid)
             VALUES ('local-acct', 1, 5, 'active', 'cookie', X'AA', 'org')",
        )
        .execute(&pool).await.unwrap();
        pool.close().await;

        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        apply_bundle(
            &pool,
            &bundle,
            &Args {
                path: bundle_path,
                mode: Mode::Restore,
                yes: true,
                overwrite_admin: false,
                dry_run: false,
                init: false,
                force: false,
                passphrase_stdin: false,
            },
        )
        .await
        .unwrap();

        let (n_proxies,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM proxies")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            n_proxies, 0,
            "restore left local proxies behind because bundle had no proxies table"
        );
        let (n_accounts,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            n_accounts, 0,
            "restore left local accounts behind because bundle's accounts was empty"
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn merge_translates_overlapping_ids_across_independent_dbs() {
        // The most common merge collision: two independently-seeded DBs
        // whose admin/policies/etc. all start at id=1. Without ID
        // translation, the bundle's row binds id=1 for the import, the
        // local row already holds id=1 with a different natural key,
        // and SQLite raises `UNIQUE constraint failed: <table>.id`
        // before the natural-key UPSERT can do anything.
        //
        // After the fix, merge mode drops the synthetic id from INSERT
        // and translates FK columns through `id_maps`, so child rows
        // pointing at the bundle's id space land on the local row that
        // matched on natural key.
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");

        // Source DB: admin (id=1, default policy), plus a custom policy
        // 'src-policy' (id=2) and a non-admin user 'bob' (id=2,
        // policy_id=2 → 'src-policy'). bob also has an api_key whose
        // user_id=2.
        let src_db = dir.path().join("src.db");
        let pool = crate::db::init_pool(&src_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO policies (name, max_concurrent, rpm_limit, weekly_budget_nanousd, monthly_budget_nanousd)
             VALUES ('src-policy', 4, 20, 1000000, 5000000)",
        )
        .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO users (username, role, policy_id) VALUES ('bob', 'member', 2)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO api_keys (user_id, label, lookup_key, key_hash, plaintext_key)
             VALUES (2, 'bob-key', 'sk-bob', X'BBBB', 'sk-bob-secret')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let bundle_path = dir.path().join("bundle.json");
        let bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();

        // Target DB: same id space, *different* content. admin id=1,
        // 'tgt-policy' (id=2), 'carol' user (id=2, policy_id=2 →
        // 'tgt-policy'). The deliberately overlapping ids are exactly
        // the case that broke the previous implementation.
        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO policies (name, max_concurrent, rpm_limit, weekly_budget_nanousd, monthly_budget_nanousd)
             VALUES ('tgt-policy', 6, 40, 2000000, 8000000)",
        )
        .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO users (username, role, policy_id) VALUES ('carol', 'member', 2)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        // Merge bundle into target.
        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        apply_bundle(&pool, &bundle, &import_args(bundle_path, Mode::Merge))
            .await
            .unwrap();

        // Both policies survive — no UNIQUE id collision blew up the import.
        let policy_names: Vec<String> = sqlx::query_as("SELECT name FROM policies ORDER BY name")
            .fetch_all(&pool)
            .await
            .unwrap()
            .into_iter()
            .map(|(n,): (String,)| n)
            .collect();
        assert!(
            policy_names.contains(&"src-policy".to_string())
                && policy_names.contains(&"tgt-policy".to_string()),
            "expected both policies to survive merge, got {policy_names:?}"
        );

        // Both users (bob and carol) coexist.
        let user_names: Vec<String> =
            sqlx::query_as("SELECT username FROM users ORDER BY username")
                .fetch_all(&pool)
                .await
                .unwrap()
                .into_iter()
                .map(|(n,): (String,)| n)
                .collect();
        assert_eq!(user_names, vec!["admin", "bob", "carol"]);

        // bob's policy_id was translated through id_maps[policies] so it
        // now points at the *local* id of `src-policy`, not the bundle's
        // original id=2 (which clashed with `tgt-policy`).
        let (bob_policy,): (String,) = sqlx::query_as(
            "SELECT p.name FROM users u JOIN policies p ON p.id = u.policy_id WHERE u.username = 'bob'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(bob_policy, "src-policy");

        // carol still references her local tgt-policy.
        let (carol_policy,): (String,) = sqlx::query_as(
            "SELECT p.name FROM users u JOIN policies p ON p.id = u.policy_id WHERE u.username = 'carol'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(carol_policy, "tgt-policy");

        // bob's api_key.user_id was translated too: it lands on bob's
        // local id, not on whichever id=2 row happened to be there.
        let (key_owner,): (String,) = sqlx::query_as(
            "SELECT u.username FROM api_keys k JOIN users u ON u.id = k.user_id WHERE k.label = 'bob-key'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(key_owner, "bob");
        pool.close().await;
    }

    #[tokio::test]
    async fn merge_translates_overlapping_ids_for_model_pricing() {
        // Same id-collision pattern as the policies/users test above, but
        // exercises the `model_pricing` spec specifically. Without
        // id_column = Some("id") on the merge spec, the bundle's id binds
        // verbatim into the INSERT, and a local row with the same numeric
        // id but a different pricing_key triggers
        // `UNIQUE constraint failed: model_pricing.id` before the
        // ON CONFLICT(pricing_key) UPSERT can run.
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("nope.toml");

        // Source DB: seeded pricing rows + one custom 'src-pricing' row.
        // Locking down the id (the next free id past whatever the seed
        // populated) keeps the collision deterministic across migration
        // changes.
        let src_db = dir.path().join("src.db");
        let pool = crate::db::init_pool(&src_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        let (next_pricing_id,): (i64,) =
            sqlx::query_as("SELECT COALESCE(MAX(id), 0) + 1 FROM model_pricing")
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO model_pricing (id, pricing_key, display_name,
                                        input_nanousd_per_token, output_nanousd_per_token)
             VALUES (?1, 'src-pricing', 'Source Pricing', 100, 200)",
        )
        .bind(next_pricing_id)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let bundle_path = dir.path().join("bundle.json");
        let bundle = build_bundle(&src_db, &cfg_path, &export_args(bundle_path.clone()))
            .await
            .unwrap();

        // Target DB: same seeded pricing rows + a *different* custom row
        // pinned to the same id `next_pricing_id`. With the spec fix this
        // merges cleanly; without it SQLite raises `UNIQUE constraint failed:
        // model_pricing.id` before any UPSERT logic runs.
        let tgt_db = dir.path().join("tgt.db");
        let pool = crate::db::init_pool(&tgt_db).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO model_pricing (id, pricing_key, display_name,
                                        input_nanousd_per_token, output_nanousd_per_token)
             VALUES (?1, 'tgt-pricing', 'Target Pricing', 300, 400)",
        )
        .bind(next_pricing_id)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let pool = crate::db::open_existing_pool(&tgt_db).await.unwrap();
        apply_bundle(&pool, &bundle, &import_args(bundle_path, Mode::Merge))
            .await
            .expect("merge must not collide on model_pricing.id");

        // Both rows survive the merge — the bundle's 'src-pricing' was
        // inserted with a fresh local id, and 'tgt-pricing' was preserved
        // at its original id.
        let pricing_keys: Vec<String> =
            sqlx::query_as("SELECT pricing_key FROM model_pricing ORDER BY pricing_key")
                .fetch_all(&pool)
                .await
                .unwrap()
                .into_iter()
                .map(|(k,): (String,)| k)
                .collect();
        assert!(
            pricing_keys.contains(&"src-pricing".to_string())
                && pricing_keys.contains(&"tgt-pricing".to_string()),
            "expected both pricing rows to survive merge, got {pricing_keys:?}"
        );
        pool.close().await;
    }

    fn sample_bundle() -> Bundle {
        Bundle {
            version: BUNDLE_VERSION,
            produced_at: "2026-04-28T00:00:00Z".to_string(),
            clewdr_version: env!("CARGO_PKG_VERSION").to_string(),
            schema: BTreeMap::new(),
            config_toml: String::new(),
            tables: BTreeMap::new(),
            skipped: vec![],
        }
    }
}
