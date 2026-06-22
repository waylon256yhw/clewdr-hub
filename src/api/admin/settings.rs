use std::collections::HashMap;

use axum::{Json, extract::State};
use serde::Deserialize;
use sqlx::SqlitePool;
use tracing::info;

use crate::error::ClewdrError;
use crate::stealth;

#[derive(Deserialize)]
pub struct UpdateSettingsRequest {
    pub settings: HashMap<String, String>,
}

const HIDDEN_KEYS: &[&str] = &["session_secret", "_proxy_migrated"];
const DEPRECATED_KEYS: &[&str] = &[
    "cc_cli_version",
    "cc_sdk_version",
    "cc_node_version",
    "cc_stainless_os",
    "cc_stainless_arch",
    "cc_beta_flags",
    "proxy",
];

pub async fn get_all(
    State(db): State<SqlitePool>,
) -> Result<Json<HashMap<String, String>>, ClewdrError> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM settings ORDER BY key")
            .fetch_all(&db)
            .await?;

    let map: HashMap<String, String> = rows
        .into_iter()
        .filter(|(k, _)| {
            !HIDDEN_KEYS.contains(&k.as_str()) && !DEPRECATED_KEYS.contains(&k.as_str())
        })
        .collect();
    Ok(Json(map))
}

const RELOADABLE_SETTINGS_KEYS: &[&str] = &[
    "cc_billing_salt",
    "output_effort_override_enabled",
    "output_effort_override_level",
];

pub async fn update(
    State(db): State<SqlitePool>,
    Json(req): Json<UpdateSettingsRequest>,
) -> Result<Json<HashMap<String, String>>, ClewdrError> {
    let mut needs_stealth_reload = false;

    let mut tx = db.begin().await?;
    for (key, value) in &req.settings {
        if HIDDEN_KEYS.contains(&key.as_str()) || DEPRECATED_KEYS.contains(&key.as_str()) {
            continue;
        }
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, CURRENT_TIMESTAMP)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(key)
        .bind(value)
        .execute(&mut *tx)
        .await?;

        if RELOADABLE_SETTINGS_KEYS.contains(&key.as_str()) {
            needs_stealth_reload = true;
        }
    }
    tx.commit().await?;

    if needs_stealth_reload {
        stealth::reload_stealth_profile(&db).await;
        info!("Stealth profile reloaded after settings update");
    }

    get_all(State(db)).await
}
