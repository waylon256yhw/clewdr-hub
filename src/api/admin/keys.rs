use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::common::Paginated;
use crate::error::ClewdrError;

#[derive(Serialize)]
pub struct KeyResponse {
    pub id: i64,
    pub user_id: i64,
    pub username: String,
    pub label: Option<String>,
    pub lookup_key: String,
    pub plaintext_key: Option<String>,
    pub disabled_at: Option<String>,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub last_used_ip: Option<String>,
    pub created_at: String,
    pub bound_account_ids: Vec<i64>,
}

#[derive(Serialize)]
pub struct KeyCreatedResponse {
    pub id: i64,
    pub user_id: i64,
    pub label: Option<String>,
    pub lookup_key: String,
    pub plaintext_key: String,
    pub created_at: String,
    pub bound_account_ids: Vec<i64>,
}

#[derive(sqlx::FromRow)]
struct KeyRow {
    id: i64,
    user_id: i64,
    username: String,
    label: Option<String>,
    lookup_key: String,
    plaintext_key: Option<String>,
    disabled_at: Option<String>,
    expires_at: Option<String>,
    last_used_at: Option<String>,
    last_used_ip: Option<String>,
    created_at: String,
}

#[derive(sqlx::FromRow)]
struct BindingRow {
    api_key_id: i64,
    account_id: i64,
}

#[derive(Deserialize)]
pub struct CreateKeyRequest {
    pub user_id: i64,
    pub label: Option<String>,
    pub bound_account_ids: Option<Vec<i64>>,
}

#[derive(Deserialize)]
pub struct KeyListParams {
    pub user_id: Option<i64>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}

#[derive(Deserialize)]
pub struct UpdateBindingsRequest {
    pub account_ids: Vec<i64>,
}

async fn load_bindings_for_keys(
    db: &SqlitePool,
    key_ids: &[i64],
) -> Result<Vec<BindingRow>, sqlx::Error> {
    if key_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders: String = key_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT api_key_id, account_id FROM api_key_account_bindings WHERE api_key_id IN ({placeholders})"
    );
    let mut query = sqlx::query_as::<_, BindingRow>(&sql);
    for id in key_ids {
        query = query.bind(id);
    }
    query.fetch_all(db).await
}

fn attach_bindings(keys: Vec<KeyRow>, bindings: Vec<BindingRow>) -> Vec<KeyResponse> {
    keys.into_iter()
        .map(|k| {
            let bound: Vec<i64> = bindings
                .iter()
                .filter(|b| b.api_key_id == k.id)
                .map(|b| b.account_id)
                .collect();
            KeyResponse {
                id: k.id,
                user_id: k.user_id,
                username: k.username,
                label: k.label,
                lookup_key: k.lookup_key,
                plaintext_key: k.plaintext_key,
                disabled_at: k.disabled_at,
                expires_at: k.expires_at,
                last_used_at: k.last_used_at,
                last_used_ip: k.last_used_ip,
                created_at: k.created_at,
                bound_account_ids: bound,
            }
        })
        .collect()
}

pub async fn list(
    State(db): State<SqlitePool>,
    Query(params): Query<KeyListParams>,
) -> Result<Json<Paginated<KeyResponse>>, ClewdrError> {
    let offset = params.offset.unwrap_or(0).max(0);
    let limit = params.limit.unwrap_or(50).clamp(1, 100);

    let (total, rows) = if let Some(uid) = params.user_id {
        let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM api_keys WHERE user_id = ?1")
            .bind(uid)
            .fetch_one(&db)
            .await?;
        let items: Vec<KeyRow> = sqlx::query_as(
            r#"SELECT ak.id, ak.user_id, u.username, ak.label, ak.lookup_key, ak.plaintext_key,
                      ak.disabled_at, ak.expires_at, ak.last_used_at, ak.last_used_ip, ak.created_at
               FROM api_keys ak JOIN users u ON ak.user_id = u.id
               WHERE ak.user_id = ?1 ORDER BY ak.id LIMIT ?2 OFFSET ?3"#,
        )
        .bind(uid)
        .bind(limit)
        .bind(offset)
        .fetch_all(&db)
        .await?;
        (total.0, items)
    } else {
        let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM api_keys")
            .fetch_one(&db)
            .await?;
        let items: Vec<KeyRow> = sqlx::query_as(
            r#"SELECT ak.id, ak.user_id, u.username, ak.label, ak.lookup_key, ak.plaintext_key,
                      ak.disabled_at, ak.expires_at, ak.last_used_at, ak.last_used_ip, ak.created_at
               FROM api_keys ak JOIN users u ON ak.user_id = u.id
               ORDER BY ak.id LIMIT ?1 OFFSET ?2"#,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&db)
        .await?;
        (total.0, items)
    };

    let key_ids: Vec<i64> = rows.iter().map(|k| k.id).collect();
    let bindings = load_bindings_for_keys(&db, &key_ids).await?;
    let items = attach_bindings(rows, bindings);

    Ok(Json(Paginated {
        items,
        total,
        offset,
        limit,
    }))
}

pub async fn create(
    State(db): State<SqlitePool>,
    Json(req): Json<CreateKeyRequest>,
) -> Result<(StatusCode, Json<KeyCreatedResponse>), ClewdrError> {
    let user_exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM users WHERE id = ?1")
        .bind(req.user_id)
        .fetch_optional(&db)
        .await?;
    if user_exists.is_none() {
        return Err(ClewdrError::NotFound {
            msg: "user not found",
        });
    }

    let plaintext =
        crate::db::queries::create_api_key(&db, req.user_id, req.label.as_deref()).await?;
    let lookup_key = &plaintext[3..11];

    let row: (i64, String) =
        sqlx::query_as("SELECT id, created_at FROM api_keys WHERE lookup_key = ?1")
            .bind(lookup_key)
            .fetch_one(&db)
            .await?;

    let ak_id = row.0;
    let bound = req.bound_account_ids.unwrap_or_default();
    for aid in &bound {
        sqlx::query(
            "INSERT INTO api_key_account_bindings (api_key_id, account_id) VALUES (?1, ?2)",
        )
        .bind(ak_id)
        .bind(aid)
        .execute(&db)
        .await?;
    }

    Ok((
        StatusCode::CREATED,
        Json(KeyCreatedResponse {
            id: ak_id,
            user_id: req.user_id,
            label: req.label,
            lookup_key: lookup_key.to_string(),
            plaintext_key: plaintext,
            created_at: row.1,
            bound_account_ids: bound,
        }),
    ))
}

pub async fn update_bindings(
    State(db): State<SqlitePool>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateBindingsRequest>,
) -> Result<StatusCode, ClewdrError> {
    let exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM api_keys WHERE id = ?1")
        .bind(id)
        .fetch_optional(&db)
        .await?;
    if exists.is_none() {
        return Err(ClewdrError::NotFound {
            msg: "api key not found",
        });
    }

    sqlx::query("DELETE FROM api_key_account_bindings WHERE api_key_id = ?1")
        .bind(id)
        .execute(&db)
        .await?;

    for aid in &req.account_ids {
        sqlx::query(
            "INSERT INTO api_key_account_bindings (api_key_id, account_id) VALUES (?1, ?2)",
        )
        .bind(id)
        .bind(aid)
        .execute(&db)
        .await?;
    }

    Ok(StatusCode::NO_CONTENT)
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
        return Err(ClewdrError::NotFound {
            msg: "api key not found",
        });
    }

    Ok(StatusCode::NO_CONTENT)
}
