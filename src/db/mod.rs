pub mod accounts;
pub mod api_key;
pub mod billing;
pub mod models;
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

const DEFAULT_PASSWORD_HASH: &str =
    "$argon2id$v=19$m=65536,t=3,p=1$Li5+S+9BeUmy3TFviGbZ9Q$tI+ZLpzW3LhrR5OA8izKSR+mw4APjT6m4rQTicuXNsE";

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
                println!(
                    "{}\n  {} {}\n  {} {}",
                    "Default admin credentials:".green().bold(),
                    "Username:".bold(),
                    "admin".yellow().bold(),
                    "Password:".bold(),
                    "password".yellow().bold(),
                );
                (DEFAULT_PASSWORD_HASH.to_string(), 1i32)
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

    // Ensure admin has at least one API key (also covers upgrades from pre-Phase-2)
    let admin_id: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM users WHERE username = 'admin' AND role = 'admin'")
            .fetch_optional(pool)
            .await?;

    if let Some((admin_id,)) = admin_id {
        let key_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM api_keys WHERE user_id = ?1")
                .bind(admin_id)
                .fetch_one(pool)
                .await?;

        if key_count.0 == 0 {
            let plaintext_key =
                queries::create_api_key(pool, admin_id, Some("bootstrap")).await?;
            println!(
                "{} {}",
                "Generated admin API key:".green().bold(),
                plaintext_key.yellow().bold()
            );
            println!(
                "{}",
                "Save this key — it cannot be recovered!".red().bold()
            );
        }
    }

    // Migrate proxy from TOML config to DB settings (one-time, guarded by flag)
    let migrated: Option<(String,)> = sqlx::query_as(
        "SELECT value FROM settings WHERE key = '_proxy_migrated'"
    )
    .fetch_optional(pool)
    .await?;
    if migrated.is_none() {
        if let Some(ref proxy) = crate::config::CLEWDR_CONFIG.load().proxy {
            if !proxy.is_empty() {
                sqlx::query(
                    "INSERT INTO settings (key, value, updated_at) VALUES ('proxy', ?1, CURRENT_TIMESTAMP)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP"
                )
                .bind(proxy)
                .execute(pool)
                .await?;
                info!("Migrated proxy setting from config to DB: {proxy}");
            }
        }
        sqlx::query(
            "INSERT OR IGNORE INTO settings (key, value, updated_at) VALUES ('_proxy_migrated', '1', CURRENT_TIMESTAMP)"
        )
        .execute(pool)
        .await?;
    }

    Ok(())
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
