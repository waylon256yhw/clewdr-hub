use std::collections::HashMap;
use std::sync::LazyLock;

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tracing::info;

use crate::error::ClewdrError;
use crate::stealth;

#[derive(Deserialize)]
pub struct UpdateSettingsRequest {
    pub settings: HashMap<String, String>,
}

pub async fn get_all(
    State(db): State<SqlitePool>,
) -> Result<Json<HashMap<String, String>>, ClewdrError> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT key, value FROM settings ORDER BY key")
        .fetch_all(&db)
        .await?;

    let map: HashMap<String, String> = rows.into_iter().collect();
    Ok(Json(map))
}

const STEALTH_KEYS: &[&str] = &[
    "cc_cli_version", "cc_sdk_version", "cc_node_version",
    "cc_stainless_os", "cc_stainless_arch", "cc_beta_flags", "cc_billing_salt",
    "proxy",
];

pub async fn update(
    State(db): State<SqlitePool>,
    Json(req): Json<UpdateSettingsRequest>,
) -> Result<Json<HashMap<String, String>>, ClewdrError> {
    let mut needs_stealth_reload = false;

    let mut tx = db.begin().await?;
    for (key, value) in &req.settings {
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, CURRENT_TIMESTAMP)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP"
        )
        .bind(key)
        .bind(value)
        .execute(&mut *tx)
        .await?;

        if STEALTH_KEYS.contains(&key.as_str()) {
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

// --- CLI version fetching from npm ---

#[derive(Serialize, Clone)]
pub struct CliVersionsResponse {
    pub versions: Vec<String>,
    pub cached: bool,
    pub fetched_at: Option<String>,
}

struct VersionCache {
    versions: Vec<String>,
    fetched_at: std::time::Instant,
    fetched_at_utc: chrono::DateTime<chrono::Utc>,
}

static VERSION_CACHE: LazyLock<Mutex<Option<VersionCache>>> = LazyLock::new(|| Mutex::new(None));

const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

pub async fn cli_versions(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<CliVersionsResponse>, ClewdrError> {
    let force = params.get("force").is_some();

    if !force {
        let cache = VERSION_CACHE.lock().await;
        if let Some(ref c) = *cache {
            if c.fetched_at.elapsed() < CACHE_TTL {
                return Ok(Json(CliVersionsResponse {
                    versions: c.versions.clone(),
                    cached: true,
                    fetched_at: Some(c.fetched_at_utc.to_rfc3339()),
                }));
            }
        }
    }

    let now = chrono::Utc::now();
    match fetch_npm_versions().await {
        Ok(versions) if !versions.is_empty() => {
            let mut cache = VERSION_CACHE.lock().await;
            *cache = Some(VersionCache {
                versions: versions.clone(),
                fetched_at: std::time::Instant::now(),
                fetched_at_utc: now,
            });
            Ok(Json(CliVersionsResponse { versions, cached: false, fetched_at: Some(now.to_rfc3339()) }))
        }
        _ => {
            // Fetch failed — return stale cache if available
            let cache = VERSION_CACHE.lock().await;
            if let Some(ref c) = *cache {
                Ok(Json(CliVersionsResponse {
                    versions: c.versions.clone(),
                    cached: true,
                    fetched_at: Some(c.fetched_at_utc.to_rfc3339()),
                }))
            } else {
                Ok(Json(CliVersionsResponse { versions: vec![], cached: false, fetched_at: None }))
            }
        }
    }
}

async fn fetch_npm_versions() -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let client = wreq::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let resp = client
        .get("https://registry.npmjs.org/@anthropic-ai/claude-code")
        .header("Accept", "application/vnd.npm.install-v1+json")
        .send()
        .await?;

    let body: serde_json::Value = resp.json().await?;

    let mut versions: Vec<String> = body["versions"]
        .as_object()
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default();

    // Sort by semver descending, take latest 10
    versions.sort_by(|a, b| {
        version_tuple(b).cmp(&version_tuple(a))
    });
    versions.truncate(5);

    Ok(versions)
}

fn version_tuple(v: &str) -> (u32, u32, u32) {
    let parts: Vec<u32> = v.split('.')
        .filter_map(|s| s.parse().ok())
        .collect();
    (
        parts.first().copied().unwrap_or(0),
        parts.get(1).copied().unwrap_or(0),
        parts.get(2).copied().unwrap_or(0),
    )
}
