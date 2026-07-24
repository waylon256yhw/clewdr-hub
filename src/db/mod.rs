pub mod accounts;
pub mod api_key;
pub mod billing;
pub mod models;
pub mod proxies;
pub mod queries;

use std::path::Path;

use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHasher, SaltString, rand_core::OsRng},
};
use colored::Colorize;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use tracing::info;

use crate::error::ClewdrError;

const ADMIN_PASSWORD_ENV: &str = "ADMIN_PASSWORD";
const MAX_CONNECTIONS: u32 = 5;

/// Opens an existing SQLite database in **read-only** mode without running
/// any migrations.
///
/// Use this for verbs advertised as side-effect-free (`diagnose`, `status`,
/// future `export-config` probes). It differs from
/// [`open_existing_pool`] in three load-bearing ways:
///
/// 1. SQLite is opened with `mode=ro`, so any attempted write fails fast
///    instead of silently advancing the WAL.
/// 2. `sqlx::migrate!()` is **not** invoked. An older on-disk DB stays at
///    its original schema; we'd rather have a stale schema for diagnostics
///    than have a "read-only" verb mutate a backup file.
/// 3. The pool is sized to one connection — diagnostics are sequential and
///    we want the smallest possible footprint while a server may be
///    attached to the same DB on a separate connection.
///
/// `:memory:` is supported for tests.
pub async fn open_readonly_pool(db_path: &Path) -> Result<SqlitePool, ClewdrError> {
    let is_memory = db_path.to_str().is_some_and(|s| s.contains(":memory:"));
    if !is_memory && !db_path.exists() {
        return Err(ClewdrError::DbNotFound {
            path: db_path.to_path_buf(),
        });
    }

    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .read_only(true)
        .create_if_missing(false)
        .pragma("busy_timeout", "5000");

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    Ok(pool)
}

pub async fn init_pool(db_path: &Path) -> Result<SqlitePool, ClewdrError> {
    let is_memory = db_path.to_str().is_some_and(|s| s.contains(":memory:"));

    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .pragma("journal_mode", "WAL")
        .pragma("foreign_keys", "ON")
        .pragma("busy_timeout", "5000");

    let max_conn = if is_memory { 1 } else { MAX_CONNECTIONS };
    let pool = SqlitePoolOptions::new()
        .max_connections(max_conn)
        .connect_with(options)
        .await?;

    sqlx::migrate!().run(&pool).await?;
    info!("Database initialized and migrations applied");

    Ok(pool)
}

/// Opens an existing SQLite database without auto-creating one.
///
/// Subcommands like `reset-admin-password`, `export-config`, and `diagnose`
/// must NOT silently create a fresh empty DB when the user typos a `--db`
/// path or runs the verb before any server has ever been started. They use
/// this helper, which returns [`ClewdrError::DbNotFound`] in that case.
///
/// `:memory:` paths bypass the existence check (used by tests).
///
/// Migrations still run after a successful connect, so an old on-disk DB
/// gets upgraded transparently — only auto-creation is suppressed.
pub async fn open_existing_pool(db_path: &Path) -> Result<SqlitePool, ClewdrError> {
    let is_memory = db_path.to_str().is_some_and(|s| s.contains(":memory:"));

    if !is_memory && !db_path.exists() {
        return Err(ClewdrError::DbNotFound {
            path: db_path.to_path_buf(),
        });
    }

    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(false)
        .pragma("journal_mode", "WAL")
        .pragma("foreign_keys", "ON")
        .pragma("busy_timeout", "5000");

    let max_conn = if is_memory { 1 } else { MAX_CONNECTIONS };
    let pool = SqlitePoolOptions::new()
        .max_connections(max_conn)
        .connect_with(options)
        .await?;

    sqlx::migrate!().run(&pool).await?;
    Ok(pool)
}

const DEFAULT_PASSWORD_HASH: &str = "$argon2id$v=19$m=65536,t=3,p=1$Li5+S+9BeUmy3TFviGbZ9Q$tI+ZLpzW3LhrR5OA8izKSR+mw4APjT6m4rQTicuXNsE";

fn generated_initial_admin_password() -> String {
    use rand::RngExt;

    let mut bytes = [0u8; 12];
    rand::rng().fill(&mut bytes);
    bytes.iter().fold(String::with_capacity(24), |mut out, b| {
        use std::fmt::Write;
        write!(out, "{b:02x}").expect("writing to String cannot fail");
        out
    })
}

pub async fn seed_admin(pool: &SqlitePool) -> Result<(), ClewdrError> {
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE role = 'admin'")
        .fetch_one(pool)
        .await?;

    if count.0 == 0 {
        let (password_hash, must_change) = match std::env::var(ADMIN_PASSWORD_ENV) {
            Ok(p) if !p.trim().is_empty() => {
                info!("Using admin password from {ADMIN_PASSWORD_ENV} environment variable");
                let pw = p;
                let hash = tokio::task::spawn_blocking(move || hash_password(&pw))
                    .await
                    .map_err(|e| ClewdrError::UnexpectedNone {
                        msg: Box::leak(format!("argon2 task panicked: {e}").into_boxed_str()),
                    })??;
                (hash, 0i32)
            }
            _ => {
                // Keep the familiar password in debug builds for contributor
                // workflows and existing integration harnesses. Release
                // deployments get a unique bootstrap password instead of a
                // project-wide public default.
                let password = if cfg!(debug_assertions) {
                    "password".to_string()
                } else {
                    generated_initial_admin_password()
                };
                println!(
                    "{}\n  {} {}",
                    "Admin panel initial password:".green().bold(),
                    "Password:".bold(),
                    password.as_str().yellow().bold(),
                );
                let password_hash = if cfg!(debug_assertions) {
                    DEFAULT_PASSWORD_HASH.to_string()
                } else {
                    let pw = password;
                    tokio::task::spawn_blocking(move || hash_password(&pw))
                        .await
                        .map_err(|e| ClewdrError::UnexpectedNone {
                            msg: Box::leak(format!("argon2 task panicked: {e}").into_boxed_str()),
                        })??
                };
                (password_hash, 1i32)
            }
        };

        sqlx::query(
            "INSERT OR IGNORE INTO users (username, display_name, password_hash, role, policy_id, must_change_password) VALUES (?1, ?2, ?3, 'admin', 1, ?4)",
        )
        .bind("admin")
        .bind("Administrator")
        .bind(&password_hash)
        .bind(must_change)
        .execute(pool)
        .await?;

        info!("Admin user created");
    } else {
        info!("Admin user already exists, skipping seed");
    }

    // Ensure session secret exists (for HMAC cookie signing)
    let existing: Option<(String,)> =
        sqlx::query_as("SELECT value FROM settings WHERE key = 'session_secret'")
            .fetch_optional(pool)
            .await?;
    if existing.is_none() {
        use base64::Engine;
        use rand::RngExt;
        let mut bytes = [0u8; 32];
        rand::rng().fill(&mut bytes);
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES ('session_secret', ?1, CURRENT_TIMESTAMP)",
        )
        .bind(&encoded)
        .execute(pool)
        .await?;
        info!("Generated session secret");
    }

    seed_models(pool).await?;

    // Plan §9: idempotent state-row seed. The migration seeds this row
    // on first deploy; a subsequent "default restore" wipes runtime
    // tables (including this one) but leaves request_logs in place. On
    // the next startup the row is re-created with a fresh
    // writes_started_at so Ops accumulates daily rollups from that
    // moment forward without an import-time special case.
    crate::db::billing::ensure_daily_rollup_state(pool).await?;

    Ok(())
}

/// Replace the admin user's password hash and clear the
/// `must_change_password` flag.
///
/// Bumps `session_version` to invalidate any active session cookies — without
/// this, a forgotten / leaked password would still hand the attacker live
/// sessions even after the operator runs `reset-admin-password`.
///
/// Returns [`ClewdrError::NotFound`] when no admin row matches `username`.
/// We deliberately do not auto-seed in that case: a missing admin row means
/// the database is corrupted or pointed at a stale path, and silently
/// reinstating an admin would mask that.
pub async fn reset_admin_password(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
) -> Result<(), ClewdrError> {
    let result = sqlx::query(
        "UPDATE users
         SET password_hash = ?1,
             must_change_password = 0,
             session_version = session_version + 1,
             updated_at = CURRENT_TIMESTAMP
         WHERE username = ?2 AND role = 'admin'",
    )
    .bind(password_hash)
    .bind(username)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ClewdrError::NotFound {
            msg: "no admin user with that username",
        });
    }
    Ok(())
}

const DEFAULT_MODELS: &[(&str, &str, i32)] = &[
    ("claude-fable-5", "Claude Fable 5", 1),
    ("claude-opus-4-8", "Claude Opus 4.8", 3),
    ("claude-opus-4-7", "Claude Opus 4.7", 5),
    ("claude-opus-4-6", "Claude Opus 4.6", 10),
    ("claude-opus-4-5", "Claude Opus 4.5", 20),
    ("claude-opus-4-1", "Claude Opus 4.1", 30),
    ("claude-sonnet-5", "Claude Sonnet 5", 40),
    ("claude-sonnet-4-6", "Claude Sonnet 4.6", 50),
    ("claude-sonnet-4-5", "Claude Sonnet 4.5", 60),
    ("claude-haiku-4-5-20251001", "Claude Haiku 4.5", 80),
];

pub async fn seed_models(pool: &SqlitePool) -> Result<(), ClewdrError> {
    for &(model_id, display_name, sort_order) in DEFAULT_MODELS {
        sqlx::query(
            "INSERT OR IGNORE INTO models (model_id, display_name, source, sort_order) VALUES (?1, ?2, 'builtin', ?3)"
        )
        .bind(model_id)
        .bind(display_name)
        .bind(sort_order)
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub async fn reset_default_models(pool: &SqlitePool) -> Result<(), ClewdrError> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM models").execute(&mut *tx).await?;
    for &(model_id, display_name, sort_order) in DEFAULT_MODELS {
        sqlx::query(
            "INSERT OR IGNORE INTO models (model_id, display_name, source, sort_order) VALUES (?1, ?2, 'builtin', ?3)"
        )
        .bind(model_id)
        .bind(display_name)
        .bind(sort_order)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn load_session_secret(pool: &SqlitePool) -> Result<[u8; 32], ClewdrError> {
    use base64::Engine;
    let row: (String,) = sqlx::query_as("SELECT value FROM settings WHERE key = 'session_secret'")
        .fetch_one(pool)
        .await?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&row.0)
        .map_err(|e| ClewdrError::UnexpectedNone {
            msg: Box::leak(format!("invalid session_secret base64: {e}").into_boxed_str()),
        })?;
    let secret: [u8; 32] = decoded
        .try_into()
        .map_err(|_| ClewdrError::UnexpectedNone {
            msg: "session_secret must be 32 bytes",
        })?;
    Ok(secret)
}

fn hash_password(password: &str) -> Result<String, ClewdrError> {
    let salt = SaltString::generate(&mut OsRng);
    let params = Params::new(65536, 3, 1, None).map_err(|e| ClewdrError::UnexpectedNone {
        msg: Box::leak(format!("argon2 params error: {e}").into_boxed_str()),
    })?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| ClewdrError::UnexpectedNone {
            msg: Box::leak(format!("argon2 hash error: {e}").into_boxed_str()),
        })?;
    Ok(hash.to_string())
}

/// Public wrapper for admin API user creation/update.
pub fn hash_password_public(password: &str) -> Result<String, ClewdrError> {
    hash_password(password)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fresh in-memory pool with the full migration set applied.
    async fn fresh_memory_pool() -> SqlitePool {
        init_pool(Path::new(":memory:"))
            .await
            .expect("init_pool :memory:")
    }

    #[tokio::test]
    async fn open_existing_pool_rejects_missing_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.db");
        let res = open_existing_pool(&missing).await;
        match &res {
            Err(ClewdrError::DbNotFound { path }) => assert_eq!(path, &missing),
            other => panic!("expected DbNotFound, got {other:?}"),
        }
        // The bad path must not have been created as a side effect.
        assert!(!missing.exists(), "open_existing_pool created {missing:?}");
        // The user-facing message must not point at flags that don't apply
        // to every CLI verb (e.g. --init only exists on import-config).
        let rendered = res.unwrap_err().to_string();
        assert!(
            !rendered.contains("--init"),
            "DbNotFound message should not suggest --init: {rendered}"
        );
        assert!(
            rendered.contains("clewdr serve") || rendered.contains("--db"),
            "DbNotFound message should suggest a generic recovery: {rendered}"
        );
    }

    #[tokio::test]
    async fn open_existing_pool_opens_existing_db_and_runs_migrations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ok.db");
        // Seed via init_pool (which creates + migrates), then drop the pool
        // so the file is closed and we exercise open_existing_pool against
        // a real on-disk file the way a CLI verb would.
        {
            let pool = init_pool(&path).await.expect("init_pool");
            pool.close().await;
        }
        let pool = open_existing_pool(&path).await.expect("open_existing_pool");
        // Migrations table is the unambiguous proof that schema was applied.
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .expect("query _sqlx_migrations");
        assert!(n >= 1, "expected applied migrations");
    }

    #[tokio::test]
    async fn open_existing_pool_accepts_memory_path() {
        // Memory paths must bypass the on-disk existence check so unit
        // tests and ephemeral CLI runs (no_fs) can still open them.
        let pool = open_existing_pool(Path::new(":memory:"))
            .await
            .expect("memory pool");
        let _: (i64,) = sqlx::query_as("SELECT 1")
            .fetch_one(&pool)
            .await
            .expect("query");
    }

    #[tokio::test]
    async fn open_readonly_pool_rejects_writes_and_skips_migrations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ro.db");
        // Seed via init_pool so the file has a valid schema.
        {
            let pool = init_pool(&path).await.expect("init_pool");
            pool.close().await;
        }

        // Capture the migration count before opening read-only — open should
        // not insert new migration rows.
        let pre_count: i64 = {
            let pool = open_existing_pool(&path).await.expect("open existing");
            let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations")
                .fetch_one(&pool)
                .await
                .expect("count migrations");
            pool.close().await;
            n
        };

        let pool = open_readonly_pool(&path).await.expect("open readonly");

        // Reads work.
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .expect("read migrations");
        assert_eq!(n, pre_count);

        // PRAGMA integrity_check is read-only — must work.
        let (s,): (String,) = sqlx::query_as("PRAGMA integrity_check")
            .fetch_one(&pool)
            .await
            .expect("integrity_check");
        assert_eq!(s, "ok");

        // Writes must be rejected by SQLite — proves the connection is RO.
        let write_res = sqlx::query("CREATE TABLE diag_should_fail (x INTEGER)")
            .execute(&pool)
            .await;
        assert!(
            write_res.is_err(),
            "open_readonly_pool let a write through: {write_res:?}"
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn open_readonly_pool_rejects_missing_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.db");
        match open_readonly_pool(&missing).await {
            Err(ClewdrError::DbNotFound { path }) => assert_eq!(path, missing),
            other => panic!("expected DbNotFound, got {other:?}"),
        }
        assert!(!missing.exists());
    }

    #[tokio::test]
    async fn reset_admin_password_errors_when_admin_missing() {
        let pool = fresh_memory_pool().await;
        // Don't call seed_admin; users table is empty.
        let res = reset_admin_password(&pool, "admin", "$argon2id$dummy").await;
        match res {
            Err(ClewdrError::NotFound { msg }) => {
                assert!(msg.contains("admin"));
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reset_admin_password_updates_hash_and_bumps_session_version() {
        // Use init_pool then seed_admin so the row carries the default
        // password + must_change=1 starting state.
        let pool = fresh_memory_pool().await;
        seed_admin(&pool).await.expect("seed_admin");

        let (old_hash, must_change_before, session_v_before): (String, i32, i64) = sqlx::query_as(
            "SELECT password_hash, must_change_password, session_version
             FROM users WHERE username = 'admin'",
        )
        .fetch_one(&pool)
        .await
        .expect("read seeded admin");
        assert_eq!(must_change_before, 1, "fresh seed should require change");

        let new_hash = hash_password("hunter2").expect("hash");
        reset_admin_password(&pool, "admin", &new_hash)
            .await
            .expect("reset");

        let (after_hash, must_change_after, session_v_after): (String, i32, i64) = sqlx::query_as(
            "SELECT password_hash, must_change_password, session_version
             FROM users WHERE username = 'admin'",
        )
        .fetch_one(&pool)
        .await
        .expect("read updated admin");

        assert_ne!(after_hash, old_hash, "password hash must change");
        assert_eq!(after_hash, new_hash);
        assert_eq!(must_change_after, 0, "must_change should be cleared");
        assert_eq!(
            session_v_after,
            session_v_before + 1,
            "session_version must be bumped to invalidate live sessions"
        );
    }

    #[tokio::test]
    async fn reset_admin_password_rejects_non_admin_user() {
        let pool = fresh_memory_pool().await;
        seed_admin(&pool).await.expect("seed_admin");

        // Targeting a username that doesn't exist (or isn't admin) must NOT
        // touch the existing admin row.
        let (admin_hash_before,): (String,) =
            sqlx::query_as("SELECT password_hash FROM users WHERE username = 'admin'")
                .fetch_one(&pool)
                .await
                .expect("read admin");

        let new_hash = hash_password("anything").expect("hash");
        let res = reset_admin_password(&pool, "not-admin", &new_hash).await;
        assert!(matches!(res, Err(ClewdrError::NotFound { .. })));

        let (admin_hash_after,): (String,) =
            sqlx::query_as("SELECT password_hash FROM users WHERE username = 'admin'")
                .fetch_one(&pool)
                .await
                .expect("read admin");
        assert_eq!(admin_hash_before, admin_hash_after);
    }
}
