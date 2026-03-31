use axum::{Json, extract::{Path, Query, State}, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::common::Paginated;
use crate::error::ClewdrError;

#[derive(Serialize, sqlx::FromRow)]
pub struct KeyResponse {
    pub id: i64,
    pub user_id: i64,
    pub username: String,
    pub label: Option<String>,
    pub lookup_key: String,
    pub disabled_at: Option<String>,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub last_used_ip: Option<String>,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct KeyCreatedResponse {
    pub id: i64,
    pub user_id: i64,
    pub label: Option<String>,
    pub lookup_key: String,
    pub plaintext_key: String,
    pub created_at: String,
}

#[derive(Deserialize)]
pub struct CreateKeyRequest {
    pub user_id: i64,
    pub label: Option<String>,
}

#[derive(Deserialize)]
pub struct KeyListParams {
    pub user_id: Option<i64>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}

pub async fn list(
    State(db): State<SqlitePool>,
    Query(params): Query<KeyListParams>,
) -> Result<Json<Paginated<KeyResponse>>, ClewdrError> {
    let offset = params.offset.unwrap_or(0).max(0);
    let limit = params.limit.unwrap_or(50).clamp(1, 100);

    let (total, items) = if let Some(uid) = params.user_id {
        let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM api_keys WHERE user_id = ?1")
            .bind(uid).fetch_one(&db).await?;
        let items: Vec<KeyResponse> = sqlx::query_as(
            r#"SELECT ak.id, ak.user_id, u.username, ak.label, ak.lookup_key,
                      ak.disabled_at, ak.expires_at, ak.last_used_at, ak.last_used_ip, ak.created_at
               FROM api_keys ak JOIN users u ON ak.user_id = u.id
               WHERE ak.user_id = ?1 ORDER BY ak.id LIMIT ?2 OFFSET ?3"#,
        ).bind(uid).bind(limit).bind(offset).fetch_all(&db).await?;
        (total.0, items)
    } else {
        let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM api_keys")
            .fetch_one(&db).await?;
        let items: Vec<KeyResponse> = sqlx::query_as(
            r#"SELECT ak.id, ak.user_id, u.username, ak.label, ak.lookup_key,
                      ak.disabled_at, ak.expires_at, ak.last_used_at, ak.last_used_ip, ak.created_at
               FROM api_keys ak JOIN users u ON ak.user_id = u.id
               ORDER BY ak.id LIMIT ?1 OFFSET ?2"#,
        ).bind(limit).bind(offset).fetch_all(&db).await?;
        (total.0, items)
    };

    Ok(Json(Paginated { items, total, offset, limit }))
}

pub async fn create(
    State(db): State<SqlitePool>,
    Json(req): Json<CreateKeyRequest>,
) -> Result<(StatusCode, Json<KeyCreatedResponse>), ClewdrError> {
    let user_exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM users WHERE id = ?1")
        .bind(req.user_id).fetch_optional(&db).await?;
    if user_exists.is_none() {
        return Err(ClewdrError::NotFound { msg: "user not found" });
    }

    let plaintext = crate::db::queries::create_api_key(&db, req.user_id, req.label.as_deref()).await?;
    let lookup_key = &plaintext[3..11]; // sk- prefix + 8 char lookup

    let row: (i64, String) = sqlx::query_as(
        "SELECT id, created_at FROM api_keys WHERE lookup_key = ?1"
    )
    .bind(lookup_key)
    .fetch_one(&db)
    .await?;

    Ok((StatusCode::CREATED, Json(KeyCreatedResponse {
        id: row.0,
        user_id: req.user_id,
        label: req.label,
        lookup_key: lookup_key.to_string(),
        plaintext_key: plaintext,
        created_at: row.1,
    })))
}

pub async fn remove(
    State(db): State<SqlitePool>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ClewdrError> {
    let result = sqlx::query("DELETE FROM api_keys WHERE id = ?1")
        .bind(id)
        .execute(&db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ClewdrError::NotFound { msg: "api key not found" });
    }

    Ok(StatusCode::NO_CONTENT)
}
