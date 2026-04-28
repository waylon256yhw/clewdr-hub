//! `clewdr import-config` — restore from a bundle in one transaction.
//!
//! Two modes:
//! - `--mode merge` (default): UPSERT each row by primary / unique keys.
//!   Rows already present that don't appear in the bundle are preserved;
//!   rows that do conflict are replaced. The local admin row is preserved
//!   unless `--overwrite-admin` is set.
//! - `--mode restore --yes`: truncate every config-like table, then
//!   re-insert from the bundle verbatim (including ids). The `--yes`
//!   guard is mandatory because this destroys local data.
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
use semver::Version;
use serde::Deserialize;
use sqlx::{SqliteConnection, SqlitePool};

use crate::{
    cli::bundle::{
        BUNDLE_VERSION, Bundle, CellValue, NEVER_EXPORTED, RUNTIME_TABLES,
        SETTINGS_KEYS_NEVER_EXPORTED, TableSchema, read_table_schema as read_db_schema,
    },
    config::{CONFIG_PATH, DB_PATH},
    error::ClewdrError,
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
    "api_keys",
    "api_key_account_bindings",
    "settings",
    "models",
    "model_pricing",
];

/// Tables wiped at the start of `--mode restore`. Reverse FK order so each
/// child's rows are gone before its parent's, even though deferred FKs
/// would also accept any delete order.
const TABLE_DELETE_ORDER_RESTORE: &[&str] = &[
    "model_pricing",
    "models",
    "settings",
    "api_key_account_bindings",
    "api_keys",
    "accounts",
    "users",
    "proxies",
    "policies",
];

/// Junction tables that get wiped before insertion regardless of mode —
/// per the plan, the bindings table is always restored from the bundle so
/// `api_keys` UPSERT churn doesn't leave orphan associations behind.
const TABLES_WIPED_EVEN_IN_MERGE: &[&str] = &["api_key_account_bindings"];

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
    let bundle = parse_bundle(&raw)?;
    check_version_compatibility(&bundle, args.force)?;

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

const ENCRYPTED_BUNDLE_MAGIC: &[u8] = b"CLWDR1\0";

fn parse_bundle(raw: &[u8]) -> Result<Bundle, ClewdrError> {
    if raw.starts_with(ENCRYPTED_BUNDLE_MAGIC) {
        return Err(ClewdrError::BadRequest {
            msg: "encrypted bundles are supported in a follow-up commit",
        });
    }
    let bundle: Bundle = serde_json::from_slice(raw)?;
    if bundle.version != BUNDLE_VERSION {
        return Err(ClewdrError::BadRequest {
            msg: "unsupported bundle version",
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

fn parse_clewdr_version(s: &str) -> Result<Version, ClewdrError> {
    Version::parse(s.trim_start_matches('v')).map_err(|_| ClewdrError::BadRequest {
        msg: "version string is not valid semver",
    })
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

    // Reject runtime / never-exported tables in a bundle outright. They
    // shouldn't be there, but if they are we don't want to silently drop
    // them — operator should know their bundle has unexpected content.
    for table in bundle.tables.keys() {
        if NEVER_EXPORTED.contains(&table.as_str()) {
            summary
                .schema_warnings
                .push(format!("ignored never-exported table in bundle: {table}"));
        } else if RUNTIME_TABLES.contains(&table.as_str())
            && !TABLE_INSERT_ORDER.contains(&table.as_str())
        {
            summary
                .schema_warnings
                .push(format!("ignored runtime table in bundle: {table}"));
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

    // Always end the transaction. Dry-run rolls back unconditionally;
    // production import commits on success and rolls back on error.
    let final_sql = if args.dry_run || result.is_err() {
        "ROLLBACK"
    } else {
        "COMMIT"
    };
    let _ = sqlx::query(final_sql).execute(&mut *conn).await;

    result?;
    Ok(summary)
}

async fn apply_tables(
    conn: &mut SqliteConnection,
    bundle: &Bundle,
    args: &Args,
    plans: &BTreeMap<String, ColumnPlan>,
    summary: &mut ImportSummary,
) -> Result<(), ClewdrError> {
    // Restore mode wipes config tables in reverse FK order before inserts.
    if args.mode == Mode::Restore {
        for table in TABLE_DELETE_ORDER_RESTORE {
            if !plans.contains_key(*table) {
                continue;
            }
            let res = wipe_table(conn, table).await?;
            summary
                .per_table
                .entry((*table).to_string())
                .or_default()
                .deleted_first = res;
        }
    }

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
            let res = wipe_table(conn, table).await?;
            summary
                .per_table
                .entry((*table).to_string())
                .or_default()
                .deleted_first = res;
        }

        let counts = summary.per_table.entry((*table).to_string()).or_default();
        for row in rows {
            // Admin protection: only applies in merge mode. In restore
            // mode, we just wiped the entire users table — skipping the
            // bundle's admin would leave a userless DB and any api_keys
            // row in the bundle would FK-violate at COMMIT. Restore is
            // the "I want this exact state" mode, so the admin row from
            // the bundle is part of that state.
            if *table == "users"
                && args.mode == Mode::Merge
                && !args.overwrite_admin
                && matches!(row.get("username"), Some(CellValue::Text(s)) if s == "admin")
            {
                counts.skipped_admin += 1;
                continue;
            }
            if *table == "settings"
                && let Some(CellValue::Text(k)) = row.get("key")
                && SETTINGS_KEYS_NEVER_EXPORTED.contains(&k.as_str())
            {
                counts.skipped_blocked_settings += 1;
                continue;
            }

            execute_row_insert(conn, table, plan, row).await?;
            counts.inserted += 1;
        }
    }
    Ok(())
}

async fn execute_row_insert(
    conn: &mut SqliteConnection,
    table: &str,
    plan: &ColumnPlan,
    row: &BTreeMap<String, CellValue>,
) -> Result<(), ClewdrError> {
    if plan.writable.is_empty() {
        return Ok(());
    }

    // INSERT OR REPLACE handles both id collisions (PK) and natural-key
    // collisions (UNIQUE). The conflict resolution is: bundle wins, the
    // existing local row is deleted (CASCADE-deleting any dependents)
    // before the bundle row lands.
    let cols = plan.writable.join(", ");
    let placeholders = vec!["?"; plan.writable.len()].join(", ");
    let sql = format!("INSERT OR REPLACE INTO {table} ({cols}) VALUES ({placeholders})");

    let mut q = sqlx::query(&sql);
    for col in &plan.writable {
        let cell = row.get(col).cloned().unwrap_or(CellValue::Null);
        q = bind_cell(q, cell);
    }
    q.execute(&mut *conn).await?;
    Ok(())
}

/// Delete every row from `table`, except for `settings` rows whose `key`
/// is in [`SETTINGS_KEYS_NEVER_EXPORTED`]. The session_secret row in
/// particular must survive a restore — it's intentionally excluded from
/// the bundle so each host mints its own, and wiping it would break every
/// active admin session and every signed cookie.
async fn wipe_table(conn: &mut SqliteConnection, table: &str) -> Result<usize, ClewdrError> {
    if table == "settings" {
        let placeholders = vec!["?"; SETTINGS_KEYS_NEVER_EXPORTED.len()].join(", ");
        let sql = format!("DELETE FROM settings WHERE key NOT IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for k in SETTINGS_KEYS_NEVER_EXPORTED {
            q = q.bind(*k);
        }
        let res = q.execute(&mut *conn).await?;
        return Ok(res.rows_affected() as usize);
    }
    let res = sqlx::query(&format!("DELETE FROM {table}"))
        .execute(&mut *conn)
        .await?;
    Ok(res.rows_affected() as usize)
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
            let parts = vec![
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
    fn parse_bundle_rejects_encrypted_magic() {
        let mut buf = ENCRYPTED_BUNDLE_MAGIC.to_vec();
        buf.extend_from_slice(&[0u8; 64]);
        assert!(parse_bundle(&buf).is_err());
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
