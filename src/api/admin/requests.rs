use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::common::Paginated;
use crate::error::{ClewdrError, sanitize_account_error_message};

#[derive(Serialize, sqlx::FromRow)]
pub struct RequestLogResponse {
    pub id: i64,
    pub request_id: String,
    pub request_type: String,
    pub user_id: Option<i64>,
    pub username: Option<String>,
    pub api_key_id: Option<i64>,
    pub key_label: Option<String>,
    pub account_id: Option<i64>,
    pub account_name: Option<String>,
    pub model_raw: Option<String>,
    pub model_normalized: Option<String>,
    pub stream: i32,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub status: String,
    pub http_status: Option<i32>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cost_nanousd: i64,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Serialize)]
pub struct ResponseBodyPayload {
    pub response_body: Option<String>,
}

#[derive(Deserialize)]
pub struct RequestListParams {
    pub offset: Option<i64>,
    pub limit: Option<i64>,
    pub request_type: Option<String>,
    pub user_id: Option<i64>,
    pub status: Option<String>,
    pub model: Option<String>,
    pub started_from: Option<String>,
    pub started_to: Option<String>,
}

pub async fn list(
    State(db): State<SqlitePool>,
    Query(params): Query<RequestListParams>,
) -> Result<Json<Paginated<RequestLogResponse>>, ClewdrError> {
    let offset = params.offset.unwrap_or(0).max(0);
    let limit = params.limit.unwrap_or(50).clamp(1, 100);

    let mut where_clauses = Vec::new();
    let mut bind_idx = 1u32;

    if params.request_type.is_some() {
        where_clauses.push(format!("r.request_type = ?{bind_idx}"));
        bind_idx += 1;
    }
    if params.user_id.is_some() {
        where_clauses.push(format!("r.user_id = ?{bind_idx}"));
        bind_idx += 1;
    }
    if params.status.is_some() {
        where_clauses.push(format!("r.status = ?{bind_idx}"));
        bind_idx += 1;
    }
    if params.model.is_some() {
        where_clauses.push(format!(
            "(COALESCE(r.model_raw, '') || ' ' || COALESCE(r.model_normalized, '')) LIKE ?{bind_idx}"
        ));
        bind_idx += 1;
    }
    if params.started_from.is_some() {
        where_clauses.push(format!("r.started_at >= ?{bind_idx}"));
        bind_idx += 1;
    }
    if params.started_to.is_some() {
        where_clauses.push(format!("r.started_at <= ?{bind_idx}"));
        bind_idx += 1;
    }

    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_clauses.join(" AND "))
    };

    let count_sql = format!("SELECT COUNT(*) FROM request_logs r {where_sql}");
    let list_sql = format!(
        r#"SELECT r.id, r.request_id, r.request_type,
                  r.user_id, u.username,
                  r.api_key_id, ak.label as key_label,
                  r.account_id, acc.name as account_name,
                  r.model_raw, r.model_normalized, r.stream,
                  r.started_at, r.completed_at, r.duration_ms, r.ttft_ms,
                  r.status, r.http_status,
                  r.input_tokens, r.output_tokens,
                  r.cache_creation_tokens, r.cache_read_tokens,
                  r.cost_nanousd,
                  r.error_code, r.error_message
           FROM request_logs r
           LEFT JOIN users u ON r.user_id = u.id
           LEFT JOIN api_keys ak ON r.api_key_id = ak.id
           LEFT JOIN accounts acc ON r.account_id = acc.id
           {where_sql}
           ORDER BY r.started_at DESC
           LIMIT ?{bind_idx} OFFSET ?{}"#,
        bind_idx + 1
    );

    // Build and execute count query
    let mut count_query = sqlx::query_as::<_, (i64,)>(&count_sql);
    if let Some(ref request_type) = params.request_type {
        count_query = count_query.bind(request_type);
    }
    if let Some(uid) = params.user_id {
        count_query = count_query.bind(uid);
    }
    if let Some(ref s) = params.status {
        count_query = count_query.bind(s);
    }
    if let Some(ref m) = params.model {
        count_query = count_query.bind(format!("%{m}%"));
    }
    if let Some(ref f) = params.started_from {
        count_query = count_query.bind(f);
    }
    if let Some(ref t) = params.started_to {
        count_query = count_query.bind(t);
    }
    let (total,) = count_query.fetch_one(&db).await?;

    // Build and execute list query
    let mut list_query = sqlx::query_as::<_, RequestLogResponse>(&list_sql);
    if let Some(ref request_type) = params.request_type {
        list_query = list_query.bind(request_type);
    }
    if let Some(uid) = params.user_id {
        list_query = list_query.bind(uid);
    }
    if let Some(ref s) = params.status {
        list_query = list_query.bind(s);
    }
    if let Some(ref m) = params.model {
        list_query = list_query.bind(format!("%{m}%"));
    }
    if let Some(ref f) = params.started_from {
        list_query = list_query.bind(f);
    }
    if let Some(ref t) = params.started_to {
        list_query = list_query.bind(t);
    }
    list_query = list_query.bind(limit).bind(offset);
    let mut items = list_query.fetch_all(&db).await?;
    for item in &mut items {
        if let Some(message) = item.error_message.as_deref() {
            item.error_message = Some(sanitize_account_error_message(message));
        }
    }

    Ok(Json(Paginated {
        items,
        total,
        offset,
        limit,
    }))
}

/// Lazy-load the upstream `response_body` for a single request log row.
/// Kept off the list endpoint because probe rows can store ~256KB JSON each
/// and the list is polled by the admin UI.
pub async fn get_response_body(
    State(db): State<SqlitePool>,
    Path(id): Path<i64>,
) -> Result<Json<ResponseBodyPayload>, ClewdrError> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT response_body FROM request_logs WHERE id = ?1")
            .bind(id)
            .fetch_optional(&db)
            .await?;
    let response_body = row
        .ok_or(ClewdrError::NotFound {
            msg: "request log not found",
        })?
        .0;
    Ok(Json(ResponseBodyPayload { response_body }))
}
