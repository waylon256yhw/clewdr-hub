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
    /// Derived from `EXISTS (SELECT 1 FROM request_log_audits ...)` —
    /// the sidecar's existence is the authoritative "this row was
    /// audited" signal. Frontend gates the lazy detail fetch on this
    /// flag to avoid 404s on un-audited rows.
    pub enhanced_audit: bool,
}

#[derive(Serialize)]
pub struct ResponseBodyPayload {
    pub response_body: Option<String>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct RequestAuditPayload {
    pub peer_ip: Option<String>,
    pub client_ip: Option<String>,
    pub ip_source: Option<String>,
    pub forwarded_chain: Option<String>,
    pub user_agent: Option<String>,
    pub api_surface: Option<String>,
    pub anthropic_version: Option<String>,
    pub anthropic_beta: Option<String>,
    pub content_length: Option<i64>,
}

#[derive(Deserialize)]
pub struct RequestListParams {
    pub offset: Option<i64>,
    pub limit: Option<i64>,
    pub request_type: Option<String>,
    pub user_id: Option<i64>,
    pub status: Option<String>,
    /// Legacy substring filter on `model_raw` / `model_normalized`. Kept
    /// for back-compat with bookmarks; new callers should prefer
    /// `model_key` for an exact match.
    pub model: Option<String>,
    /// Exact match on the canonical `model_key` column. Used by Ops
    /// drill-down so the "其他" donut slice can be reliably distinguished
    /// from the substring "其他" appearing inside a model name.
    pub model_key: Option<String>,
    pub started_from: Option<String>,
    pub started_to: Option<String>,
    /// When `Some(true)`, restrict to rows that have a matching
    /// `request_log_audits` sidecar row. `Some(false)` / `None` apply
    /// no constraint.
    pub enhanced_audit: Option<bool>,
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
    if params.model_key.is_some() {
        where_clauses.push(format!("r.model_key = ?{bind_idx}"));
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
    // enhanced_audit=true → only rows with a sidecar audit row. No
    // bind needed; the EXISTS subquery is parameterless.
    if matches!(params.enhanced_audit, Some(true)) {
        where_clauses.push(
            "EXISTS (SELECT 1 FROM request_log_audits a WHERE a.request_log_id = r.id)".to_string(),
        );
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
                  r.error_code, r.error_message,
                  CASE WHEN EXISTS (
                      SELECT 1 FROM request_log_audits a WHERE a.request_log_id = r.id
                  ) THEN 1 ELSE 0 END AS enhanced_audit
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
    if let Some(ref mk) = params.model_key {
        count_query = count_query.bind(mk);
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
    if let Some(ref mk) = params.model_key {
        list_query = list_query.bind(mk);
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

/// Lazy-load the audit sidecar row for a single request log row. The
/// frontend should only call this when the list response indicated
/// `enhanced_audit: true` — un-audited rows return 404 here, which is
/// expected (the sidecar's absence is the "not audited" signal).
pub async fn get_audit(
    State(db): State<SqlitePool>,
    Path(id): Path<i64>,
) -> Result<Json<RequestAuditPayload>, ClewdrError> {
    let row: Option<RequestAuditPayload> = sqlx::query_as(
        r#"SELECT peer_ip, client_ip, ip_source, forwarded_chain,
                  user_agent, api_surface, anthropic_version, anthropic_beta,
                  content_length
           FROM request_log_audits
           WHERE request_log_id = ?1"#,
    )
    .bind(id)
    .fetch_optional(&db)
    .await?;
    let payload = row.ok_or(ClewdrError::NotFound {
        msg: "request log audit not found",
    })?;
    Ok(Json(payload))
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::{
        Json,
        extract::{Query, State},
    };
    use sqlx::SqlitePool;

    use super::{RequestListParams, list};

    async fn fresh_pool() -> SqlitePool {
        let pool = crate::db::init_pool(Path::new(":memory:"))
            .await
            .expect("init_pool");
        crate::db::seed_admin(&pool).await.expect("seed_admin");
        sqlx::query(
            "INSERT INTO users (id, username, display_name, password_hash, role, policy_id)
             VALUES (10, 'alice', 'alice', '$argon2id$dummy', 'member', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn insert_log_row(pool: &SqlitePool, request_id: &str, model_raw: &str, model_key: &str) {
        sqlx::query(
            r#"INSERT INTO request_logs (
                request_id, request_type, user_id, model_raw, model_normalized,
                model_key, usage_accounted, stream,
                started_at, completed_at, status, http_status, cost_nanousd
            ) VALUES (?1, 'messages', 10, ?2, ?2, ?3, 1, 1,
                      '2026-06-01T00:00:00Z', '2026-06-01T00:00:01Z',
                      'ok', 200, 0)"#,
        )
        .bind(request_id)
        .bind(model_raw)
        .bind(model_key)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn list_with(pool: SqlitePool, params: RequestListParams) -> Vec<String> {
        let Json(page) = list(State(pool), Query(params)).await.expect("list ok");
        page.items.into_iter().map(|i| i.request_id).collect()
    }

    /// `model_key=claude-opus-4-7` must match only the canonical row and
    /// reject "claude-opus-4-7-experimental", where the legacy
    /// substring filter (`model=claude-opus-4-7`) would have matched
    /// both.
    #[tokio::test]
    async fn model_key_filter_is_exact_match_not_substring() {
        let pool = fresh_pool().await;
        insert_log_row(
            &pool,
            "exact",
            "claude-opus-4-7-20260101",
            "claude-opus-4-7",
        )
        .await;
        insert_log_row(
            &pool,
            "experimental",
            "claude-opus-4-7-experimental",
            "claude-opus-4-7-experimental",
        )
        .await;

        let matches = list_with(
            pool.clone(),
            RequestListParams {
                offset: None,
                limit: None,
                request_type: None,
                user_id: None,
                status: None,
                model: None,
                model_key: Some("claude-opus-4-7".into()),
                started_from: None,
                started_to: None,
                enhanced_audit: None,
            },
        )
        .await;
        assert_eq!(matches, vec!["exact"], "model_key must be exact");

        // Confirm the legacy substring filter still catches both rows so
        // the migration path doesn't surprise existing bookmarks.
        let mut legacy = list_with(
            pool,
            RequestListParams {
                offset: None,
                limit: None,
                request_type: None,
                user_id: None,
                status: None,
                model: Some("claude-opus-4-7".into()),
                model_key: None,
                started_from: None,
                started_to: None,
                enhanced_audit: None,
            },
        )
        .await;
        legacy.sort();
        assert_eq!(legacy, vec!["exact", "experimental"]);
    }
}
