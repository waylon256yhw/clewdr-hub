use axum::{Extension, Json, extract::State};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::db::models::AuthenticatedUser;
use crate::db::hash_password_public;
use crate::error::ClewdrError;

#[derive(Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

pub async fn change_password(
    State(db): State<SqlitePool>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<serde_json::Value>, ClewdrError> {
    if req.new_password.trim().is_empty() {
        return Err(ClewdrError::BadRequest { msg: "new password cannot be empty" });
    }

    let row: Option<(String,)> = sqlx::query_as(
        "SELECT password_hash FROM users WHERE id = ?1 AND role = 'admin'"
    )
    .bind(user.user_id)
    .fetch_optional(&db)
    .await?;

    let Some((current_hash,)) = row else {
        return Err(ClewdrError::NotFound { msg: "admin user not found" });
    };

    // Verify current password
    let verify_result = {
        let hash = current_hash.clone();
        let pw = req.current_password.clone();
        tokio::task::spawn_blocking(move || {
            use argon2::password_hash::PasswordVerifier;
            let parsed = argon2::password_hash::PasswordHash::new(&hash)
                .map_err(|_| ClewdrError::InvalidAuth)?;
            argon2::Argon2::default()
                .verify_password(pw.as_bytes(), &parsed)
                .map_err(|_| ClewdrError::InvalidAuth)
        })
        .await
        .map_err(|_| ClewdrError::InvalidAuth)?
    };
    verify_result?;

    // Hash new password
    let new_hash = {
        let pw = req.new_password.clone();
        let result: Result<String, ClewdrError> = tokio::task::spawn_blocking(move || hash_password_public(&pw))
            .await
            .map_err(|e| ClewdrError::UnexpectedNone {
                msg: Box::leak(format!("argon2 task panicked: {e}").into_boxed_str()),
            })?;
        result?
    };

    sqlx::query("UPDATE users SET password_hash = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
        .bind(&new_hash)
        .bind(user.user_id)
        .execute(&db)
        .await?;

    Ok(Json(serde_json::json!({ "message": "password updated" })))
}
