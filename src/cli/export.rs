//! `clewdr export-config` — dump config + selected db tables to a portable bundle.
//!
//! Default output is a plaintext JSON bundle (encryption arrives in a later
//! commit). Files are written with mode 0600 and an atomic rename so a
//! crash mid-write doesn't leave a half-written secret on disk.
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
    config::{CONFIG_PATH, DB_PATH},
    error::ClewdrError,
};

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

    // Encryption is implemented in a follow-up commit. Until that lands,
    // every export goes out plaintext — but with stern warnings and
    // 0600 file mode. The flag still parses so the CLI surface doesn't
    // change between this commit and the encryption one.
    let encryption_implemented = false;
    if !encryption_implemented && !args.no_encrypt {
        eprintln!(
            "{}",
            "note: encryption is not yet implemented; this bundle will be written in plaintext."
                .yellow()
        );
        eprintln!(
            "      Pass --no-encrypt to silence this hint, or wait for the encrypted-export commit."
        );
    }

    let db_path = DB_PATH.to_owned();
    let cfg_path = CONFIG_PATH.to_owned();
    let out_path = args.path.clone();

    let bundle = build_bundle(&db_path, &cfg_path, &args).await?;
    let json = serde_json::to_vec_pretty(&bundle)?;

    write_secret_file(&out_path, &json)?;

    let n_tables = bundle.tables.len();
    let n_rows: usize = bundle.tables.values().map(Vec::len).sum();
    eprintln!(
        "{} wrote {} ({} bytes, {} table(s), {} row(s))",
        "✓".green().bold(),
        out_path.display(),
        json.len(),
        n_tables,
        n_rows
    );

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
        eprintln!("  Use --no-secrets to strip them, or wait for the encrypted-export commit.");
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
    use std::collections::BTreeMap;

    let config_toml = if cfg_path.exists() {
        std::fs::read_to_string(cfg_path).unwrap_or_default()
    } else {
        String::new()
    };

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
        let schema = read_table_schema(pool, table).await?;
        let mut rows = read_table_rows(pool, table, &schema).await?;

        // Settings: always strip the never-exported keys.
        if table == "settings" {
            drop_blocked_settings_rows(&mut rows);
        }

        // --no-secrets: null cookie_blob / oauth_*_token / password / etc.
        if args.no_secrets && !secret_columns_for(table).is_empty() {
            redact_secrets_in_place(table, &mut rows);
        }

        schema_map.insert(table.to_string(), schema);
        tables_map.insert(table.to_string(), rows);
    }

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
