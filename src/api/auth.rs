use axum::{Extension, Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::db::models::AuthenticatedUser;
use crate::db::queries;
use crate::error::ClewdrError;

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub api_key: String,
    pub user_id: i64,
    pub username: String,
    pub role: String,
    pub must_change_password: bool,
}

pub async fn login(
    State(db): State<SqlitePool>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, ClewdrError> {
    let row: Option<(i64, String, Option<String>, String, i32)> = sqlx::query_as(
        "SELECT id, username, password_hash, role, must_change_password FROM users WHERE username = ?1 AND disabled_at IS NULL",
    )
    .bind(&req.username)
    .fetch_optional(&db)
    .await?;

    let Some((user_id, username, password_hash, role, must_change)) = row else {
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

    let plaintext_key = queries::create_api_key(&db, user_id, Some("web-session")).await?;

    // Set 24h expiry on web-session keys
    sqlx::query(
        "UPDATE api_keys SET expires_at = datetime('now', '+24 hours') WHERE user_id = ?1 AND label = 'web-session' AND expires_at IS NULL"
    )
    .bind(user_id)
    .execute(&db)
    .await?;

    Ok(Json(LoginResponse {
        api_key: plaintext_key,
        user_id,
        username,
        role,
        must_change_password: must_change != 0,
    }))
}

pub async fn logout(
    State(db): State<SqlitePool>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<StatusCode, ClewdrError> {
    sqlx::query("DELETE FROM api_keys WHERE id = ?1")
        .bind(user.api_key_id)
        .execute(&db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
