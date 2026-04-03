use axum::{Extension, Json, extract::State, response::IntoResponse};
use serde::Deserialize;

use crate::db::hash_password_public;
use crate::db::models::AuthenticatedUser;
use crate::error::ClewdrError;
use crate::session;
use crate::state::AuthState;

#[derive(Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

pub async fn change_password(
    State(auth): State<AuthState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<impl IntoResponse, ClewdrError> {
    let new_password = req.new_password.trim();

    if new_password.len() < 6 {
        return Err(ClewdrError::BadRequest {
            msg: "new password must be at least 6 characters",
        });
    }

    if new_password == req.current_password.trim() {
        return Err(ClewdrError::BadRequest {
            msg: "new password must differ from current password",
        });
    }

    let row: Option<(String,)> =
        sqlx::query_as("SELECT password_hash FROM users WHERE id = ?1 AND role = 'admin'")
            .bind(user.user_id)
            .fetch_optional(&auth.db)
            .await?;

    let Some((current_hash,)) = row else {
        return Err(ClewdrError::NotFound {
            msg: "admin user not found",
        });
    };

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

    let new_hash = {
        let pw = new_password.to_owned();
        let result: Result<String, ClewdrError> =
            tokio::task::spawn_blocking(move || hash_password_public(&pw))
                .await
                .map_err(|e| ClewdrError::UnexpectedNone {
                    msg: Box::leak(format!("argon2 task panicked: {e}").into_boxed_str()),
                })?;
        result?
    };

    let new_version: (i32,) = sqlx::query_as(
        "UPDATE users SET password_hash = ?1, must_change_password = 0, session_version = session_version + 1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2 AND password_hash = ?3 RETURNING session_version"
    )
    .bind(&new_hash)
    .bind(user.user_id)
    .bind(&current_hash)
    .fetch_one(&auth.db)
    .await?;

    let cookie_value =
        session::create_session_cookie(&auth.session_secret, user.user_id, new_version.0, None);
    let set_cookie = session::set_cookie_header(&cookie_value, 86400);

    Ok((
        [(axum::http::header::SET_COOKIE, set_cookie)],
        Json(serde_json::json!({ "message": "password updated" })),
    ))
}
