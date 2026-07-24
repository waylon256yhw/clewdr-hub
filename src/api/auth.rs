use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use axum::{
    Extension, Json,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore, broadcast};
use tracing::{info, warn};

use crate::config::CLEWDR_CONFIG;
use crate::db::models::AuthenticatedUser;
use crate::error::ClewdrError;
use crate::middleware::{AdminSessionInfo, resolve_client_ip_from};
use crate::session;
use crate::state::{AdminEvent, AuthState};

const LOGIN_WINDOW: Duration = Duration::from_secs(10 * 60);
const MAX_LOGIN_FAILURES: u32 = 10;
const MAX_TRACKED_LOGIN_IPS: usize = 4_096;

#[derive(Clone, Copy)]
struct LoginFailures {
    count: u32,
    window_started: Instant,
}

static LOGIN_FAILURES: LazyLock<Mutex<HashMap<String, LoginFailures>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static LOGIN_VERIFY_SEMAPHORE: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(2)));

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub user_id: i64,
    pub username: String,
    pub role: String,
    pub must_change_password: bool,
    pub expires_at: u64,
}

#[derive(Serialize)]
pub struct SessionResponse {
    pub user_id: i64,
    pub username: String,
    pub role: String,
    pub must_change_password: bool,
    pub expires_at: u64,
}

async fn login_retry_after(client_ip: &str) -> Option<u64> {
    let mut failures = LOGIN_FAILURES.lock().await;
    let Some(entry) = failures.get(client_ip).copied() else {
        return None;
    };
    let elapsed = entry.window_started.elapsed();
    if elapsed >= LOGIN_WINDOW {
        failures.remove(client_ip);
        return None;
    }
    if entry.count < MAX_LOGIN_FAILURES {
        return None;
    }
    Some((LOGIN_WINDOW - elapsed).as_secs().max(1))
}

async fn record_login_failure(client_ip: &str) {
    let mut failures = LOGIN_FAILURES.lock().await;
    let now = Instant::now();
    failures.retain(|_, entry| entry.window_started.elapsed() < LOGIN_WINDOW);
    if !failures.contains_key(client_ip) && failures.len() >= MAX_TRACKED_LOGIN_IPS {
        // Keep the limiter memory-bounded under a wide-source scan. The
        // global Argon2 semaphore still protects the expensive verifier.
        return;
    }
    let entry = failures
        .entry(client_ip.to_string())
        .or_insert(LoginFailures {
            count: 0,
            window_started: now,
        });
    if entry.window_started.elapsed() >= LOGIN_WINDOW {
        *entry = LoginFailures {
            count: 0,
            window_started: now,
        };
    }
    entry.count = entry.count.saturating_add(1);
}

pub async fn login(
    State(auth): State<AuthState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Result<impl IntoResponse, ClewdrError> {
    let cfg = CLEWDR_CONFIG.load();
    let client_ip =
        resolve_client_ip_from(&headers, peer.ip().to_string(), &cfg.trusted_proxies).client_ip;
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .chars()
        .take(200)
        .collect::<String>();
    drop(cfg);

    if let Some(retry_after_secs) = login_retry_after(&client_ip).await {
        warn!(client_ip, "admin login rate limited");
        return Err(ClewdrError::LoginRateLimited { retry_after_secs });
    }

    let row: Option<(i64, String, Option<String>, String, i32, i32)> = sqlx::query_as(
        "SELECT id, username, password_hash, role, must_change_password, session_version FROM users WHERE username = ?1 AND disabled_at IS NULL",
    )
    .bind(&req.username)
    .fetch_optional(&auth.db)
    .await?;

    let Some((user_id, username, password_hash, role, must_change, session_version)) = row else {
        record_login_failure(&client_ip).await;
        warn!(client_ip, user_agent, "admin login failed");
        return Err(ClewdrError::InvalidAuth);
    };

    if role != "admin" {
        record_login_failure(&client_ip).await;
        warn!(client_ip, user_agent, "admin login failed");
        return Err(ClewdrError::InvalidAuth);
    }

    let Some(hash) = password_hash else {
        record_login_failure(&client_ip).await;
        warn!(client_ip, user_agent, "admin login failed");
        return Err(ClewdrError::InvalidAuth);
    };

    let _permit = LOGIN_VERIFY_SEMAPHORE
        .clone()
        .try_acquire_owned()
        .map_err(|_| ClewdrError::LoginRateLimited {
            retry_after_secs: 1,
        })?;
    let pw = req.password.clone();
    let verified = tokio::task::spawn_blocking(move || {
        use argon2::password_hash::PasswordVerifier;
        let parsed = argon2::password_hash::PasswordHash::new(&hash).map_err(|_| ())?;
        argon2::Argon2::default()
            .verify_password(pw.as_bytes(), &parsed)
            .map_err(|_| ())
    })
    .await
    .map_err(|_| ClewdrError::InvalidAuth)
    .and_then(|result| result.map_err(|_| ClewdrError::InvalidAuth));
    if verified.is_err() {
        record_login_failure(&client_ip).await;
        warn!(client_ip, user_agent, "admin login failed");
        return Err(ClewdrError::InvalidAuth);
    }

    LOGIN_FAILURES.lock().await.remove(&client_ip);
    let cfg = CLEWDR_CONFIG.load();
    let ttl_secs = cfg.admin_session_ttl_hours.saturating_mul(3600);
    let cookie_secure = cfg.admin_cookie_secure;
    drop(cfg);
    let cookie_value =
        session::create_session_cookie(&auth.session_secret, user_id, session_version, ttl_secs);
    let claims = session::validate_session_cookie(&auth.session_secret, &cookie_value)
        .ok_or(ClewdrError::InvalidAuth)?;
    let set_cookie = session::set_cookie_header(&cookie_value, ttl_secs, cookie_secure);

    let db = auth.db.clone();
    tokio::spawn(async move {
        let _ = crate::db::queries::touch_user(&db, user_id).await;
    });

    let body = LoginResponse {
        user_id,
        username,
        role,
        must_change_password: must_change != 0,
        expires_at: claims.expires_at,
    };
    info!(client_ip, user_agent, "admin login succeeded");

    Ok((
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, set_cookie)],
        Json(body),
    ))
}

pub async fn current_session(
    Extension(user): Extension<AuthenticatedUser>,
    Extension(claims): Extension<session::SessionClaims>,
    Extension(info): Extension<AdminSessionInfo>,
) -> Json<SessionResponse> {
    Json(SessionResponse {
        user_id: user.user_id,
        username: user.username,
        role: user.role,
        must_change_password: info.must_change_password,
        expires_at: claims.expires_at,
    })
}

pub async fn logout(Extension(_user): Extension<AuthenticatedUser>) -> impl IntoResponse {
    let clear = session::clear_cookie_header(CLEWDR_CONFIG.load().admin_cookie_secure);
    (
        StatusCode::NO_CONTENT,
        [(axum::http::header::SET_COOKIE, clear)],
    )
}

pub async fn logout_all(
    State(auth): State<AuthState>,
    State(event_tx): State<broadcast::Sender<AdminEvent>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<impl IntoResponse, ClewdrError> {
    sqlx::query(
        "UPDATE users SET session_version = session_version + 1, updated_at = CURRENT_TIMESTAMP
         WHERE id = ?1 AND role = 'admin'",
    )
    .bind(user.user_id)
    .execute(&auth.db)
    .await?;

    let _ = event_tx.send(AdminEvent::auth_revoked("logout_all"));
    info!(user_id = user.user_id, "all admin sessions revoked");
    let clear = session::clear_cookie_header(CLEWDR_CONFIG.load().admin_cookie_secure);
    Ok((
        StatusCode::NO_CONTENT,
        [(axum::http::header::SET_COOKIE, clear)],
    ))
}
