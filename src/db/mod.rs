pub mod api_key;
pub mod models;
pub mod queries;

use std::path::Path;

use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHasher, SaltString, rand_core::OsRng},
};
use colored::Colorize;
use rand::RngExt;
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

pub async fn seed_admin(pool: &SqlitePool) -> Result<(), ClewdrError> {
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE role = 'admin'")
        .fetch_one(pool)
        .await?;

    if count.0 == 0 {
        let password = match std::env::var(ADMIN_PASSWORD_ENV) {
            Ok(p) if !p.trim().is_empty() => {
                info!("Using admin password from {ADMIN_PASSWORD_ENV} environment variable");
                p
            }
            _ => {
                let generated = generate_password(16);
                println!(
                    "{} {}",
                    "Generated admin password:".green().bold(),
                    generated.yellow().bold()
                );
                generated
            }
        };

        let password_hash = tokio::task::spawn_blocking(move || hash_password(&password))
            .await
            .map_err(|e| ClewdrError::UnexpectedNone {
                msg: Box::leak(format!("argon2 task panicked: {e}").into_boxed_str()),
            })??;

        sqlx::query(
            "INSERT OR IGNORE INTO users (username, display_name, password_hash, role, policy_id) VALUES (?1, ?2, ?3, 'admin', 1)",
        )
        .bind("admin")
        .bind("Administrator")
        .bind(&password_hash)
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

fn generate_password(len: usize) -> String {
    const CHARSET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| {
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}
