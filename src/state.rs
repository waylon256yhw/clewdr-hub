use std::sync::Arc;

use axum::extract::FromRef;
use sqlx::SqlitePool;

use crate::providers::claude::ClaudeCodeProvider;
use crate::services::cookie_actor::CookieActorHandle;
use crate::services::user_limiter::UserLimiterMap;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub cookie_actor: CookieActorHandle,
    pub code_provider: Arc<ClaudeCodeProvider>,
    pub auth: AuthState,
    pub user_limiter: UserLimiterMap,
}

#[derive(Clone)]
pub struct AuthState {
    pub db: SqlitePool,
    pub legacy_user_password: Option<String>,
    pub legacy_admin_password: Option<String>,
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
