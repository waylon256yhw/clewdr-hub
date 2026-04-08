use std::sync::Arc;

use axum::extract::FromRef;
use serde::Serialize;
use sqlx::SqlitePool;
use tokio::sync::broadcast;

use crate::providers::claude::ClaudeCodeProvider;
use crate::services::cookie_actor::CookieActorHandle;
use crate::services::user_limiter::UserLimiterMap;
use crate::stealth::SharedStealthProfile;

#[derive(Clone, Debug, Serialize)]
pub struct AdminEvent {
    pub topic: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl AdminEvent {
    pub fn request_log(request_type: &str, status: &str) -> Self {
        Self {
            topic: "request_logs".to_string(),
            request_type: Some(request_type.to_string()),
            status: Some(status.to_string()),
        }
    }

    pub fn request_logs_refresh() -> Self {
        Self {
            topic: "request_logs".to_string(),
            request_type: None,
            status: None,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub cookie_actor: CookieActorHandle,
    pub code_provider: Arc<ClaudeCodeProvider>,
    pub auth: AuthState,
    pub user_limiter: UserLimiterMap,
    pub stealth_profile: SharedStealthProfile,
    pub event_tx: broadcast::Sender<AdminEvent>,
}

#[derive(Clone)]
pub struct AuthState {
    pub db: SqlitePool,
    pub session_secret: [u8; 32],
}

impl FromRef<AppState> for Arc<ClaudeCodeProvider> {
    fn from_ref(state: &AppState) -> Self {
        state.code_provider.clone()
    }
}

impl FromRef<AppState> for CookieActorHandle {
    fn from_ref(state: &AppState) -> Self {
        state.cookie_actor.clone()
    }
}

impl FromRef<AppState> for AuthState {
    fn from_ref(state: &AppState) -> Self {
        state.auth.clone()
    }
}

impl FromRef<AppState> for SqlitePool {
    fn from_ref(state: &AppState) -> Self {
        state.db.clone()
    }
}

impl FromRef<AppState> for UserLimiterMap {
    fn from_ref(state: &AppState) -> Self {
        state.user_limiter.clone()
    }
}

impl FromRef<AppState> for SharedStealthProfile {
    fn from_ref(state: &AppState) -> Self {
        state.stealth_profile.clone()
    }
}

impl FromRef<AppState> for broadcast::Sender<AdminEvent> {
    fn from_ref(state: &AppState) -> Self {
        state.event_tx.clone()
    }
}
