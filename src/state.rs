use std::sync::Arc;

use axum::extract::FromRef;
use sqlx::SqlitePool;
use tokio::sync::broadcast;

use crate::providers::claude::ClaudeCodeProvider;
use crate::services::cookie_actor::CookieActorHandle;
use crate::services::user_limiter::UserLimiterMap;
use crate::stealth::SharedStealthProfile;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub cookie_actor: CookieActorHandle,
    pub code_provider: Arc<ClaudeCodeProvider>,
    pub auth: AuthState,
    pub user_limiter: UserLimiterMap,
    pub stealth_profile: SharedStealthProfile,
    pub event_tx: broadcast::Sender<()>,
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

impl FromRef<AppState> for broadcast::Sender<()> {
    fn from_ref(state: &AppState) -> Self {
        state.event_tx.clone()
    }
}
