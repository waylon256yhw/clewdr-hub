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

const DEFAULT_PASSWORD_HASH: &str = "$argon2id$v=19$m=65536,t=3,p=1$Li5+S+9BeUmy3TFviGbZ9Q$tI+ZLpzW3LhrR5OA8izKSR+mw4APjT6m4rQTicuXNsE";

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
                    "{}\n  {} {}",
                    "Admin panel initial password:".green().bold(),
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

    Ok(())
}

const DEFAULT_MODELS: &[(&str, &str, i32)] = &[
    ("claude-opus-4-7", "Claude Opus 4.7", 5),
    ("claude-opus-4-6", "Claude Opus 4.6", 10),
    ("claude-opus-4-5", "Claude Opus 4.5", 20),
    ("claude-opus-4-1", "Claude Opus 4.1", 30),
    ("claude-sonnet-4-6", "Claude Sonnet 4.6", 50),
    ("claude-sonnet-4-5", "Claude Sonnet 4.5", 60),
    ("claude-haiku-4-5", "Claude Haiku 4.5", 80),
    ("claude-haiku-3-5", "Claude Haiku 3.5", 90),
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
