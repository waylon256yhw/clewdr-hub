use axum::extract::FromRef;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use tracing::warn;

use crate::config::CLEWDR_CONFIG;
use crate::db::api_key::parse_api_key;
use crate::db::queries::authenticate_api_key;
use crate::error::ClewdrError;
use crate::state::AuthState;

/// Extract the API key/token from request headers.
/// Tries `x-api-key` first, falls back to `Authorization: Bearer`.
fn extract_key_from_headers(parts: &Parts) -> Option<String> {
    if let Some(key) = parts.headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(key.to_string());
    }
    if let Some(auth) = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(token) = auth.strip_prefix("Bearer ") {
            return Some(token.to_string());
        }
    }
    None
}

/// Middleware guard for `/v1/**` routes.
/// Authenticates via DB-backed API keys (sk-prefixed), with legacy password fallback.
pub struct RequireFlexibleAuth;

impl<S> FromRequestParts<S> for RequireFlexibleAuth
where
    AuthState: FromRef<S>,
    S: Sync + Send,
{
    type Rejection = ClewdrError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let auth_state = AuthState::from_ref(state);
        let key = extract_key_from_headers(parts).ok_or(ClewdrError::InvalidAuth)?;

        // 1. Try DB-backed API key auth
        if let Some((lookup, hash)) = parse_api_key(&key) {
            match authenticate_api_key(&auth_state.db, &lookup, &hash).await {
                Ok(Some(authed_user)) => {
                    parts.extensions.insert(authed_user);
                    return Ok(Self);
                }
                Ok(None) => {} // key not found or hash mismatch, fall through
                Err(e) => {
                    warn!("DB error during API key auth: {e}");
                }
            }
        }

        // 2. Legacy fallback: compare against config password
        if let Some(ref legacy_pw) = auth_state.legacy_user_password {
            if key == *legacy_pw {
                return Ok(Self);
            }
        }

        // 3. Also check live config (for hot-reloaded passwords)
        if CLEWDR_CONFIG.load().user_auth(&key) {
            return Ok(Self);
        }

        warn!("No valid authentication found (tried x-api-key and Bearer)");
        Err(ClewdrError::InvalidAuth)
    }
}

/// Middleware guard for `/api/**` admin routes.
/// Authenticates admin users via DB API keys or legacy admin password.
pub struct RequireAdminAuth;

impl<S> FromRequestParts<S> for RequireAdminAuth
where
    AuthState: FromRef<S>,
    S: Sync + Send,
{
    type Rejection = ClewdrError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let auth_state = AuthState::from_ref(state);
        let key = extract_key_from_headers(parts).ok_or(ClewdrError::InvalidAuth)?;

        // 1. Try DB-backed API key auth for admin users
        if let Some((lookup, hash)) = parse_api_key(&key) {
            match authenticate_api_key(&auth_state.db, &lookup, &hash).await {
                Ok(Some(authed_user)) if authed_user.role == "admin" => {
                    parts.extensions.insert(authed_user);
                    return Ok(Self);
                }
                Ok(_) => {} // not found, hash mismatch, or not admin — fall through
                Err(e) => {
                    warn!("DB error during admin API key auth: {e}");
                }
            }
        }

        // 2. Legacy fallback: compare against config admin_password
        if let Some(ref legacy_pw) = auth_state.legacy_admin_password {
            if key == *legacy_pw {
                return Ok(Self);
            }
        }

        // 3. Also check live config (for hot-reloaded passwords)
        if CLEWDR_CONFIG.load().admin_auth(&key) {
            return Ok(Self);
        }

        warn!("Invalid admin key");
        Err(ClewdrError::InvalidAuth)
    }
}
