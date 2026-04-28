//! `clewdr export-config` — dump config + selected db tables to a portable bundle.
//!
//! Default output is an Argon2id-derived AES-256-GCM-encrypted bundle (see
//! [`crate::cli::crypto`] for the wire format). `--no-encrypt` keeps the
//! plaintext JSON form — written with mode 0600, atomic rename, and a
//! stern stderr warning so the operator knows the file carries unwrapped
//! cookies and OAuth tokens.
//!
//! What goes in the bundle is decided by [`crate::cli::bundle`] — that
//! module owns the table list, the secret-column allowlist, and the
//! per-cell encoding for BLOBs.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use chrono::Utc;
use colored::Colorize;
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::{
    cli::bundle::{
        self, Bundle, DEFAULT_TABLES, NEVER_EXPORTED, RUNTIME_TABLES, TableSchema,
        drop_blocked_settings_rows, read_table_rows, read_table_schema, redact_secrets_in_place,
        secret_columns_for,
    },
    cli::crypto,
    config::{CONFIG_PATH, DB_PATH},
    error::ClewdrError,
};

/// Top-level TOML keys whose values may carry session-bearing credentials
/// (proxy URL with `user:pass@host`, etc.). When the operator passes
/// `--no-secrets`, we drop these from the embedded `clewdr.toml` before
/// writing the bundle. Centralised here so `secret_columns_for` and this
/// list stay reviewable side-by-side.
const TOML_KEYS_REDACTED_FOR_NO_SECRETS: &[&str] = &["proxy"];

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Output bundle path.
    pub path: PathBuf,

    /// Write plaintext JSON instead of encrypting. Stamps the file 0600 and emits a stderr warning.
    #[arg(long)]
    pub no_encrypt: bool,

    /// Strip session cookies, OAuth refresh tokens, proxy passwords, and password hashes.
    #[arg(long)]
    pub no_secrets: bool,

    /// Include runtime tables (`account_runtime_state`, `usage_*`, `request_logs`).
    /// Off by default — these are large and not portable.
    #[arg(long)]
    pub include_runtime: bool,

    /// Read encryption passphrase from stdin instead of `/dev/tty`. Useful for scripts.
    #[arg(long, conflicts_with = "no_encrypt")]
    pub passphrase_stdin: bool,
}

pub async fn run(args: Args) -> Result<(), ClewdrError> {
    // No-fs mode (HF Space) has no on-disk DB or toml to export.
    let cfg = read_minimal_config();
    if cfg.no_fs() {
        return Err(ClewdrError::BadRequest {
            msg: "export-config is not supported in no_fs mode (in-memory state is ephemeral)",
        });
    }

    let db_path = DB_PATH.to_owned();
    let cfg_path = CONFIG_PATH.to_owned();
    let out_path = args.path.clone();

    // Resolve the passphrase up front when encrypting. Doing this *before*
    // building the bundle means a missing TTY / mismatched confirmation
    // bails without doing the (read-locked) DB pass.
    let passphrase = if args.no_encrypt {
        None
    } else {
        Some(crypto::read_export_passphrase(args.passphrase_stdin)?)
    };

    let bundle = build_bundle(&db_path, &cfg_path, &args).await?;
    let json = serde_json::to_vec_pretty(&bundle)?;

    let payload = match passphrase.as_deref() {
        Some(pwd) => crypto::encrypt_bundle(&json, pwd)?,
        None => json,
    };
    let payload_len = payload.len();

    write_secret_file(&out_path, &payload)?;

    let n_tables = bundle.tables.len();
    let n_rows: usize = bundle.tables.values().map(Vec::len).sum();
    eprintln!(
        "{} wrote {} ({} bytes, {} table(s), {} row(s), {})",
        "✓".green().bold(),
        out_path.display(),
        payload_len,
        n_tables,
        n_rows,
        if passphrase.is_some() {
            "encrypted".green().to_string()
        } else {
            "plaintext".yellow().bold().to_string()
        },
    );

    if args.no_encrypt {
        if !args.no_secrets {
            eprintln!(
                "{}",
                "WARNING: bundle contains session cookies, OAuth refresh tokens, and proxy credentials in plaintext."
                    .yellow()
                    .bold()
            );
            eprintln!(
                "  File mode is 0600. Treat this file with the same care as a password vault export."
            );
            eprintln!(
                "  Use --no-secrets to strip them, or drop --no-encrypt for AES-GCM encryption."
            );
        } else {
            eprintln!(
                "  Wrote plaintext JSON with secrets stripped (--no-secrets). File mode is 0600."
            );
        }
    } else {
        eprintln!(
            "  Encrypted with Argon2id + AES-256-GCM. Keep the passphrase safe — without it the bundle is unrecoverable."
        );
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// Bundle assembly
// ──────────────────────────────────────────────────────────────────────────

pub(crate) async fn build_bundle(
    db_path: &Path,
    cfg_path: &Path,
    args: &Args,
) -> Result<Bundle, ClewdrError> {
    let pool = crate::db::open_readonly_pool(db_path).await?;
    let bundle = build_bundle_from_pool(&pool, cfg_path, args).await;
    pool.close().await;
    bundle
}

async fn build_bundle_from_pool(
    pool: &SqlitePool,
    cfg_path: &Path,
    args: &Args,
) -> Result<Bundle, ClewdrError> {
    // Pin a single read snapshot for the entire export. Without this, two
    // back-to-back `SELECT *` calls can observe different committed states
    // if the running server commits writes between them — leaving the
    // bundle with, say, an api_keys row whose user_id refers to a user the
    // earlier `users` SELECT missed. SQLite WAL mode hands the snapshot to
    // a transaction's first read; rolling back at the end releases the
    // read locks without writing anything.
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN").execute(&mut *conn).await?;
    // Touch the snapshot immediately so it's pinned before any other
    // future on the runtime can interleave a write that beats us to the
    // first SELECT.
    let _: (i64,) = sqlx::query_as("SELECT 1").fetch_one(&mut *conn).await?;

    let inner = build_bundle_inner(&mut conn, args).await;

    // We never wrote anything — release the snapshot regardless of inner result.
    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;

    let (schema_map, tables_map, skipped) = inner?;

    let raw_toml = if cfg_path.exists() {
        std::fs::read_to_string(cfg_path).unwrap_or_default()
    } else {
        String::new()
    };
    let config_toml = if args.no_secrets {
        // Same redaction discipline as the database half: when the
        // operator says "strip secrets", strip them everywhere — DB rows
        // *and* config file. Otherwise a `proxy = "http://user:pass@host"`
        // line would walk straight into the bundle.
        sanitize_toml_for_no_secrets(&raw_toml)?
    } else {
        raw_toml
    };

    Ok(Bundle {
        version: bundle::BUNDLE_VERSION,
        produced_at: Utc::now().to_rfc3339(),
        clewdr_version: env!("CARGO_PKG_VERSION").to_string(),
        schema: schema_map,
        config_toml,
        tables: tables_map,
        skipped,
    })
}

/// Read every selected table inside the caller's pinned snapshot, returning
/// the schema map, the row map, and the list of intentionally-skipped tables.
async fn build_bundle_inner(
    conn: &mut sqlx::SqliteConnection,
    args: &Args,
) -> Result<
    (
        std::collections::BTreeMap<String, TableSchema>,
        std::collections::BTreeMap<String, Vec<bundle::Row>>,
        Vec<String>,
    ),
    ClewdrError,
> {
    use std::collections::BTreeMap;

    let mut tables_to_export: Vec<&str> = DEFAULT_TABLES.to_vec();
    if args.include_runtime {
        tables_to_export.extend(RUNTIME_TABLES);
    }
    let mut skipped: Vec<String> = NEVER_EXPORTED.iter().map(|s| s.to_string()).collect();
    if !args.include_runtime {
        for t in RUNTIME_TABLES {
            skipped.push((*t).to_string());
        }
    }

    let mut schema_map: BTreeMap<String, TableSchema> = BTreeMap::new();
    let mut tables_map: BTreeMap<String, Vec<bundle::Row>> = BTreeMap::new();

    for table in tables_to_export {
        let schema = read_table_schema(&mut *conn, table).await?;
        let mut rows = read_table_rows(&mut *conn, table, &schema).await?;

        // Settings: always strip the never-exported keys.
        if table == "settings" {
            drop_blocked_settings_rows(&mut rows);
        }

        // --no-secrets: null cookie_blob / oauth_*_token / api_key plaintext / etc.
        if args.no_secrets && !secret_columns_for(table).is_empty() {
            redact_secrets_in_place(table, &mut rows);
        }

        schema_map.insert(table.to_string(), schema);
        tables_map.insert(table.to_string(), rows);
    }

    Ok((schema_map, tables_map, skipped))
}

/// Strip TOML keys that may carry secrets (currently `proxy`, whose URL
/// form supports embedded credentials). Returns an empty string for empty
/// input. Errors propagate as TOML parse / serialize errors.
pub(crate) fn sanitize_toml_for_no_secrets(raw: &str) -> Result<String, ClewdrError> {
    if raw.trim().is_empty() {
        return Ok(String::new());
    }
    let mut value: toml::Value = toml::from_str(raw)?;
    if let toml::Value::Table(t) = &mut value {
        for key in TOML_KEYS_REDACTED_FOR_NO_SECRETS {
            t.remove(*key);
        }
    }
    Ok(toml::to_string_pretty(&value)?)
}

// ──────────────────────────────────────────────────────────────────────────
// Atomic 0600 writer
// ──────────────────────────────────────────────────────────────────────────

/// Write `data` to `path` atomically, with mode 0600 from the moment the
/// file exists on disk. We never want a half-written bundle visible to
/// other processes, and we never want a window where the file exists with
/// looser permissions.
fn write_secret_file(path: &Path, data: &[u8]) -> Result<(), ClewdrError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.exists() {
        return Err(ClewdrError::BadRequest {
            msg: "parent directory does not exist",
        });
    }

    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.as_file_mut().write_all(data)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// Config plumbing (Figment, no CLEWDR_CONFIG side effects)
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct ExportConfig {
    pub no_fs: Option<bool>,
}

impl ExportConfig {
    fn no_fs(&self) -> bool {
        self.no_fs.unwrap_or(false)
    }
}

fn read_minimal_config() -> ExportConfig {
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
    use crate::cli::bundle::CellValue;

    /// Build a freshly-seeded DB so the bundle has admin + models content
    /// to export. Returns the temp directory so the caller can keep it
    /// alive until the test finishes.
    async fn fixture_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clewdr.db");
        let pool = crate::db::init_pool(&path).await.unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        // Insert two synthetic accounts so we can verify --no-secrets
        // redacts both kinds of credentials. The accounts table CHECK
        // constraint enforces that cookie auth has only cookie_blob set
        // and oauth auth has only the oauth_* trio set, so we need one
        // row per auth_source to cover both secret-column families.
        sqlx::query(
            "INSERT INTO accounts (name, rr_order, max_slots, status, auth_source, cookie_blob, organization_uuid)
             VALUES ('cookie-acct', 1, 5, 'active', 'cookie', X'DEADBEEF', 'org-cookie')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO accounts (name, rr_order, max_slots, status, auth_source,
                                   oauth_access_token, oauth_refresh_token, oauth_expires_at,
                                   organization_uuid)
             VALUES ('oauth-acct', 2, 5, 'active', 'oauth',
                     X'AAAA', X'CAFEBABE', '2099-01-01T00:00:00Z', 'org-oauth')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Issue an API key for the seeded admin (user_id=1) so we can
        // verify the --no-secrets path strips both `plaintext_key` and
        // `key_hash`.
        sqlx::query(
            "INSERT INTO api_keys (user_id, label, lookup_key, key_hash, plaintext_key)
             VALUES (1, 'test-key', 'sk-prefix', X'AABB', 'sk-clewdr-test-secret')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
        (dir, path)
    }

    fn args_for(out: PathBuf, no_secrets: bool) -> Args {
        Args {
            path: out,
            no_encrypt: true,
            no_secrets,
            include_runtime: false,
            passphrase_stdin: false,
        }
    }

    #[tokio::test]
    async fn build_bundle_includes_default_tables_and_excludes_never_exported() {
        let (dir, db) = fixture_db().await;
        let cfg = dir.path().join("nope.toml"); // intentionally missing
        let args = args_for(dir.path().join("out.json"), false);
        let bundle = build_bundle(&db, &cfg, &args).await.unwrap();

        for t in bundle::DEFAULT_TABLES {
            assert!(bundle.tables.contains_key(*t), "default table missing: {t}");
            assert!(
                bundle.schema.contains_key(*t),
                "default schema missing: {t}"
            );
        }
        for t in bundle::NEVER_EXPORTED {
            assert!(!bundle.tables.contains_key(*t));
            assert!(bundle.skipped.iter().any(|s| s == *t));
        }
        // Runtime tables excluded by default
        for t in bundle::RUNTIME_TABLES {
            assert!(!bundle.tables.contains_key(*t));
            assert!(bundle.skipped.iter().any(|s| s == *t));
        }
    }

    #[tokio::test]
    async fn build_bundle_with_runtime_includes_runtime_tables() {
        let (dir, db) = fixture_db().await;
        let cfg = dir.path().join("nope.toml");
        let args = Args {
            path: dir.path().join("out.json"),
            no_encrypt: true,
            no_secrets: false,
            include_runtime: true,
            passphrase_stdin: false,
        };
        let bundle = build_bundle(&db, &cfg, &args).await.unwrap();
        for t in bundle::RUNTIME_TABLES {
            assert!(bundle.tables.contains_key(*t), "runtime table missing: {t}");
        }
    }

    #[tokio::test]
    async fn no_secrets_redacts_account_credentials() {
        let (dir, db) = fixture_db().await;
        let cfg = dir.path().join("nope.toml");

        // First with secrets — verify the BLOBs survive verbatim on each
        // auth_source variant.
        let bundle = build_bundle(&db, &cfg, &args_for(dir.path().join("a"), false))
            .await
            .unwrap();
        let cookie_acct = bundle.tables["accounts"]
            .iter()
            .find(|r| matches!(r.get("name"), Some(CellValue::Text(s)) if s == "cookie-acct"))
            .expect("cookie-acct row");
        match cookie_acct.get("cookie_blob") {
            Some(CellValue::Blob(b)) => assert_eq!(b, &vec![0xDE, 0xAD, 0xBE, 0xEF]),
            other => panic!("cookie_blob not preserved: {other:?}"),
        }
        let oauth_acct = bundle.tables["accounts"]
            .iter()
            .find(|r| matches!(r.get("name"), Some(CellValue::Text(s)) if s == "oauth-acct"))
            .expect("oauth-acct row");
        match oauth_acct.get("oauth_refresh_token") {
            Some(CellValue::Blob(b)) => assert_eq!(b, &vec![0xCA, 0xFE, 0xBA, 0xBE]),
            other => panic!("oauth_refresh_token not preserved: {other:?}"),
        }

        // Then with --no-secrets — both rows have their secret columns nulled.
        let bundle = build_bundle(&db, &cfg, &args_for(dir.path().join("b"), true))
            .await
            .unwrap();
        let cookie_acct = bundle.tables["accounts"]
            .iter()
            .find(|r| matches!(r.get("name"), Some(CellValue::Text(s)) if s == "cookie-acct"))
            .unwrap();
        assert_eq!(cookie_acct.get("cookie_blob"), Some(&CellValue::Null));
        let oauth_acct = bundle.tables["accounts"]
            .iter()
            .find(|r| matches!(r.get("name"), Some(CellValue::Text(s)) if s == "oauth-acct"))
            .unwrap();
        assert_eq!(oauth_acct.get("oauth_access_token"), Some(&CellValue::Null));
        assert_eq!(
            oauth_acct.get("oauth_refresh_token"),
            Some(&CellValue::Null)
        );
        // Non-secret columns are still there.
        assert!(matches!(cookie_acct.get("name"), Some(CellValue::Text(_))));
        assert!(matches!(
            cookie_acct.get("max_slots"),
            Some(CellValue::Integer(_))
        ));
    }

    #[tokio::test]
    async fn settings_session_secret_is_never_exported() {
        let (dir, db) = fixture_db().await;
        let cfg = dir.path().join("nope.toml");
        let bundle = build_bundle(&db, &cfg, &args_for(dir.path().join("c"), false))
            .await
            .unwrap();
        let settings = &bundle.tables["settings"];
        for row in settings {
            if let Some(CellValue::Text(k)) = row.get("key") {
                assert_ne!(k, "session_secret", "session_secret leaked into the bundle");
            }
        }
    }

    #[tokio::test]
    async fn no_secrets_redacts_api_key_plaintext_and_hash() {
        // P1 from review #6: --no-secrets must strip api_keys.plaintext_key
        // and api_keys.key_hash so a "secret-free" bundle can't be used to
        // authenticate against the proxy.
        let (dir, db) = fixture_db().await;
        let cfg = dir.path().join("nope.toml");

        // Sanity: with secrets, both columns survive verbatim.
        let bundle = build_bundle(&db, &cfg, &args_for(dir.path().join("a"), false))
            .await
            .unwrap();
        let key = bundle.tables["api_keys"]
            .iter()
            .find(|r| matches!(r.get("label"), Some(CellValue::Text(s)) if s == "test-key"))
            .expect("test-key row");
        assert_eq!(
            key.get("plaintext_key"),
            Some(&CellValue::Text("sk-clewdr-test-secret".to_string()))
        );
        match key.get("key_hash") {
            Some(CellValue::Blob(b)) => assert_eq!(b, &vec![0xAA, 0xBB]),
            other => panic!("key_hash not preserved: {other:?}"),
        }

        // With --no-secrets, both are nulled while metadata stays.
        let bundle = build_bundle(&db, &cfg, &args_for(dir.path().join("b"), true))
            .await
            .unwrap();
        let key = bundle.tables["api_keys"]
            .iter()
            .find(|r| matches!(r.get("label"), Some(CellValue::Text(s)) if s == "test-key"))
            .unwrap();
        assert_eq!(key.get("plaintext_key"), Some(&CellValue::Null));
        assert_eq!(key.get("key_hash"), Some(&CellValue::Null));
        assert_eq!(
            key.get("label"),
            Some(&CellValue::Text("test-key".to_string()))
        );
    }

    #[tokio::test]
    async fn no_secrets_redacts_proxy_url_in_toml() {
        // P2 from review #6: --no-secrets must also sanitize clewdr.toml
        // because the `proxy` URL form supports embedded credentials.
        let (dir, db) = fixture_db().await;
        let cfg = dir.path().join("clewdr.toml");
        std::fs::write(
            &cfg,
            "ip = \"127.0.0.1\"\nport = 8484\nproxy = \"http://user:pass@upstream.example:3128\"\n",
        )
        .unwrap();

        // Without --no-secrets the proxy URL must round-trip verbatim.
        let bundle = build_bundle(&db, &cfg, &args_for(dir.path().join("a"), false))
            .await
            .unwrap();
        assert!(
            bundle.config_toml.contains("user:pass@upstream.example"),
            "expected proxy creds preserved without --no-secrets, got:\n{}",
            bundle.config_toml
        );

        // With --no-secrets the entire `proxy` key must be gone.
        let bundle = build_bundle(&db, &cfg, &args_for(dir.path().join("b"), true))
            .await
            .unwrap();
        assert!(
            !bundle.config_toml.contains("proxy"),
            "expected `proxy` removed under --no-secrets, got:\n{}",
            bundle.config_toml
        );
        assert!(
            !bundle.config_toml.contains("user:pass"),
            "proxy credentials leaked under --no-secrets:\n{}",
            bundle.config_toml
        );
        // Other keys untouched.
        assert!(bundle.config_toml.contains("port"));
    }

    #[test]
    fn sanitize_toml_handles_empty_input() {
        assert_eq!(sanitize_toml_for_no_secrets("").unwrap(), "");
        assert_eq!(sanitize_toml_for_no_secrets("   \n").unwrap(), "");
    }

    #[test]
    fn sanitize_toml_drops_only_redacted_keys() {
        let input = "ip = \"0.0.0.0\"\nport = 8484\nproxy = \"http://x@y\"\nno_fs = false\n";
        let out = sanitize_toml_for_no_secrets(input).unwrap();
        assert!(out.contains("ip"));
        assert!(out.contains("port"));
        assert!(out.contains("no_fs"));
        assert!(!out.contains("proxy"));
        assert!(!out.contains("\"http://x@y\""));
    }

    #[test]
    fn sanitize_toml_passes_through_when_no_proxy() {
        let input = "ip = \"127.0.0.1\"\nport = 8484\n";
        let out = sanitize_toml_for_no_secrets(input).unwrap();
        assert!(out.contains("ip"));
        assert!(out.contains("port"));
    }

    #[tokio::test]
    async fn export_run_writes_file_with_secret_perms_and_round_trips_json() {
        let (dir, db) = fixture_db().await;
        let cfg = dir.path().join("nope.toml");
        let out = dir.path().join("out.bundle.json");

        // We can't easily call run() directly because it reads global
        // DB_PATH/CONFIG_PATH. Exercise build_bundle + write_secret_file
        // — the same path run() uses minus the std::env globals.
        let args = args_for(out.clone(), false);
        let bundle = build_bundle(&db, &cfg, &args).await.unwrap();
        let json = serde_json::to_vec_pretty(&bundle).unwrap();
        write_secret_file(&out, &json).unwrap();

        // Permissions check — must be 0600 on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
        }

        // Round-trip the JSON.
        let raw = std::fs::read(&out).unwrap();
        let restored: Bundle = serde_json::from_slice(&raw).unwrap();
        assert_eq!(restored.version, bundle::BUNDLE_VERSION);
        assert_eq!(restored.clewdr_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(restored.tables.len(), bundle.tables.len());
        // BLOB cells survive the JSON round-trip byte-identically.
        let cookie_acct = restored.tables["accounts"]
            .iter()
            .find(|r| matches!(r.get("name"), Some(CellValue::Text(s)) if s == "cookie-acct"))
            .unwrap();
        match cookie_acct.get("cookie_blob") {
            Some(CellValue::Blob(b)) => assert_eq!(b, &vec![0xDE, 0xAD, 0xBE, 0xEF]),
            other => panic!("cookie_blob lost on round-trip: {other:?}"),
        }
    }
}
