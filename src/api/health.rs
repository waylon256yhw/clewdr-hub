use axum::{Json, extract::State};
use serde::Serialize;

use crate::services::cookie_actor::CookieActorHandle;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub accounts: AccountCounts,
    pub ready: bool,
}

#[derive(Serialize)]
pub struct AccountCounts {
    pub valid: usize,
    pub exhausted: usize,
}

pub async fn health(
    State(cookie_handle): State<CookieActorHandle>,
) -> Json<HealthResponse> {
    let (valid, exhausted) = match cookie_handle.get_status().await {
        Ok(s) => (s.valid.len(), s.exhausted.len()),
        Err(_) => (0, 0),
    };
    Json(HealthResponse {
        status: if valid > 0 { "ok" } else { "degraded" },
        accounts: AccountCounts { valid, exhausted },
        ready: valid > 0,
    })
}
