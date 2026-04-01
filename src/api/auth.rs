use axum::{Extension, Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};

use crate::db::models::AuthenticatedUser;
use crate::error::ClewdrError;
use crate::session;
use crate::state::AuthState;

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
}

pub async fn login(
    State(auth): State<AuthState>,
    Json(req): Json<LoginRequest>,
) -> Result<impl IntoResponse, ClewdrError> {
    let row: Option<(i64, String, Option<String>, String, i32, i32)> = sqlx::query_as(
        "SELECT id, username, password_hash, role, must_change_password, session_version FROM users WHERE username = ?1 AND disabled_at IS NULL",
    )
    .bind(&req.username)
    .fetch_optional(&auth.db)
    .await?;

    let Some((user_id, username, password_hash, role, must_change, session_version)) = row else {
        return Err(ClewdrError::InvalidAuth);
    };

    if role != "admin" {
        return Err(ClewdrError::InvalidAuth);
    }

    let Some(hash) = password_hash else {
        return Err(ClewdrError::InvalidAuth);
    };

    let pw = req.password.clone();
    tokio::task::spawn_blocking(move || {
        use argon2::password_hash::PasswordVerifier;
        let parsed = argon2::password_hash::PasswordHash::new(&hash)
            .map_err(|_| ClewdrError::InvalidAuth)?;
        argon2::Argon2::default()
            .verify_password(pw.as_bytes(), &parsed)
            .map_err(|_| ClewdrError::InvalidAuth)
    })
    .await
    .map_err(|_| ClewdrError::InvalidAuth)??;

    let cookie_value =
        session::create_session_cookie(&auth.session_secret, user_id, session_version, None);
    let set_cookie = session::set_cookie_header(&cookie_value, 86400);

    let body = LoginResponse {
        user_id,
        username,
        role,
        must_change_password: must_change != 0,
    };

    Ok((
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, set_cookie)],
        Json(body),
    ))
}

pub async fn logout(
    Extension(_user): Extension<AuthenticatedUser>,
) -> impl IntoResponse {
    let clear = session::clear_cookie_header();
    (
        StatusCode::NO_CONTENT,
        [(axum::http::header::SET_COOKIE, clear)],
    )
}
