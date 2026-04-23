use axum::extract::FromRef;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use tracing::warn;

use crate::db::api_key::parse_api_key;
use crate::db::queries::authenticate_api_key;
use crate::error::ClewdrError;
use crate::session;
use crate::state::AuthState;

fn extract_key_from_headers(parts: &Parts) -> Option<String> {
    if let Some(key) = parts.headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(key.to_string());
    }
    if let Some(auth) = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        && let Some(token) = auth.strip_prefix("Bearer ")
    {
        return Some(token.to_string());
    }
    None
}

fn extract_client_ip(parts: &Parts) -> Option<String> {
    for header in &["x-forwarded-for", "x-real-ip"] {
        if let Some(val) = parts.headers.get(*header).and_then(|v| v.to_str().ok()) {
            let ip = val.split(',').next().unwrap_or(val).trim();
            if !ip.is_empty() {
                return Some(ip.to_string());
            }
        }
    }
    None
}

pub struct RequireFlexibleAuth;

impl<S> FromRequestParts<S> for RequireFlexibleAuth
where
    AuthState: FromRef<S>,
    S: Sync + Send,
{
    type Rejection = ClewdrError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth_state = AuthState::from_ref(state);
        let key = extract_key_from_headers(parts).ok_or(ClewdrError::InvalidAuth)?;

        if let Some((lookup, hash)) = parse_api_key(&key) {
            match authenticate_api_key(&auth_state.db, &lookup, &hash).await {
                Ok(Some(authed_user)) => {
                    let ip = extract_client_ip(parts);
                    let db = auth_state.db.clone();
                    let ak_id = authed_user.api_key_id;
                    let uid = authed_user.user_id;
                    tokio::spawn(async move {
                        if let Some(ak_id) = ak_id {
                            let _ =
                                crate::db::queries::touch_api_key(&db, ak_id, ip.as_deref()).await;
                        }
                        let _ = crate::db::queries::touch_user(&db, uid).await;
                    });
                    parts.extensions.insert(authed_user);
                    return Ok(Self);
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("DB error during API key auth: {e}");
                }
            }
        }

        Err(ClewdrError::InvalidAuth)
    }
}

pub struct RequireAdminAuth;

impl<S> FromRequestParts<S> for RequireAdminAuth
where
    AuthState: FromRef<S>,
    S: Sync + Send,
{
    type Rejection = ClewdrError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth_state = AuthState::from_ref(state);

        let cookie_header = parts
            .headers
            .get("cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let cookie_value =
            session::extract_session_cookie(cookie_header).ok_or(ClewdrError::InvalidAuth)?;

        let claims = session::validate_session_cookie(&auth_state.session_secret, cookie_value)
            .ok_or(ClewdrError::InvalidAuth)?;

        let row: Option<(i64, String, String, i32, Option<String>, i64)> = sqlx::query_as(
            "SELECT u.id, u.username, u.role, u.session_version, u.disabled_at, u.policy_id
             FROM users u WHERE u.id = ?1",
        )
        .bind(claims.user_id)
        .fetch_optional(&auth_state.db)
        .await
        .map_err(|e| {
            warn!("DB error during cookie auth: {e}");
            ClewdrError::InvalidAuth
        })?;

        let Some((user_id, username, role, session_version, disabled_at, policy_id)) = row else {
            return Err(ClewdrError::InvalidAuth);
        };

        if disabled_at.is_some() || role != "admin" || session_version != claims.session_version {
            return Err(ClewdrError::InvalidAuth);
        }

        let Some((max_concurrent, rpm_limit, weekly_budget_nanousd, monthly_budget_nanousd)) = sqlx::query_as::<_, (i32, i32, i64, i64)>(
            "SELECT max_concurrent, rpm_limit, weekly_budget_nanousd, monthly_budget_nanousd FROM policies WHERE id = ?1",
        )
        .bind(policy_id)
        .fetch_optional(&auth_state.db)
        .await
        .map_err(|e| {
            warn!("DB error loading policy: {e}");
            ClewdrError::InvalidAuth
        })? else {
            return Err(ClewdrError::InvalidAuth);
        };

        parts
            .extensions
            .insert(crate::db::models::AuthenticatedUser {
                user_id,
                username,
                role,
                api_key_id: None,
                policy_id,
                max_concurrent,
                rpm_limit,
                weekly_budget_nanousd,
                monthly_budget_nanousd,
                bound_account_ids: Vec::new(),
            });

        Ok(Self)
    }
}
