use sqlx::SqlitePool;

use crate::billing::BillingUsage;

/// Row to insert into request_logs.
pub struct RequestLogRow<'a> {
    pub request_id: &'a str,
    pub request_type: &'a str,
    pub user_id: Option<i64>,
    pub api_key_id: Option<i64>,
    pub account_id: Option<i64>,
    pub model_raw: &'a str,
    pub model_normalized: Option<&'a str>,
    pub stream: bool,
    pub started_at: &'a str,
    pub completed_at: Option<&'a str>,
    pub duration_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub status: &'a str,
    pub http_status: Option<u16>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub priced_input_nanousd_per_token: Option<i64>,
    pub priced_output_nanousd_per_token: Option<i64>,
    pub cost_nanousd: i64,
    pub error_code: Option<&'a str>,
    pub error_message: Option<&'a str>,
}

/// Look up model pricing by pricing_key. Returns (input_nanousd, output_nanousd).
pub async fn lookup_model_pricing(
    pool: &SqlitePool,
    pricing_key: &str,
) -> Result<Option<(i64, i64)>, sqlx::Error> {
    let row: Option<(i64, i64)> = sqlx::query_as(
        "SELECT input_nanousd_per_token, output_nanousd_per_token FROM model_pricing WHERE pricing_key = ?1",
    )
    .bind(pricing_key)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Insert a request log row.
pub async fn insert_request_log(
    pool: &SqlitePool,
    r: &RequestLogRow<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO request_logs (
            request_id, request_type, user_id, api_key_id, account_id,
            model_raw, model_normalized, stream,
            started_at, completed_at, duration_ms, ttft_ms,
            status, http_status,
            input_tokens, output_tokens,
            cache_creation_tokens, cache_read_tokens,
            priced_input_nanousd_per_token, priced_output_nanousd_per_token,
            cost_nanousd, error_code, error_message
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5,
            ?6, ?7, ?8,
            ?9, ?10, ?11, ?12,
            ?13, ?14,
            ?15, ?16,
            ?17, ?18,
            ?19, ?20,
            ?21, ?22, ?23
        )"#,
    )
    .bind(r.request_id)
    .bind(r.request_type)
    .bind(r.user_id)
    .bind(r.api_key_id)
    .bind(r.account_id)
    .bind(r.model_raw)
    .bind(r.model_normalized)
    .bind(r.stream as i32)
    .bind(r.started_at)
    .bind(r.completed_at)
    .bind(r.duration_ms)
    .bind(r.ttft_ms)
    .bind(r.status)
    .bind(r.http_status.map(|v| v as i32))
    .bind(r.input_tokens)
    .bind(r.output_tokens)
    .bind(r.cache_creation_tokens)
    .bind(r.cache_read_tokens)
    .bind(r.priced_input_nanousd_per_token)
    .bind(r.priced_output_nanousd_per_token)
    .bind(r.cost_nanousd)
    .bind(r.error_code)
    .bind(r.error_message)
    .execute(pool)
    .await?;
    Ok(())
}

/// Upsert a usage rollup row, incrementing counters on conflict.
pub async fn upsert_usage_rollup(
    pool: &SqlitePool,
    user_id: i64,
    period_type: &str,
    period_start: &str,
    period_end: &str,
    usage: &BillingUsage,
    cost_nanousd: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO usage_rollups (
            user_id, period_type, period_start, period_end,
            request_count, input_tokens, output_tokens,
            cache_creation_tokens, cache_read_tokens,
            cost_nanousd, updated_at
        ) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, ?9, CURRENT_TIMESTAMP)
        ON CONFLICT (user_id, period_type, period_start) DO UPDATE SET
            request_count = request_count + 1,
            input_tokens = input_tokens + excluded.input_tokens,
            output_tokens = output_tokens + excluded.output_tokens,
            cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
            cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
            cost_nanousd = cost_nanousd + excluded.cost_nanousd,
            updated_at = CURRENT_TIMESTAMP"#,
    )
    .bind(user_id)
    .bind(period_type)
    .bind(period_start)
    .bind(period_end)
    .bind(usage.input_tokens as i64)
    .bind(usage.output_tokens as i64)
    .bind(usage.cache_creation_tokens as i64)
    .bind(usage.cache_read_tokens as i64)
    .bind(cost_nanousd)
    .execute(pool)
    .await?;
    Ok(())
}

/// Get current period cost for quota checking.
pub async fn get_current_period_cost(
    pool: &SqlitePool,
    user_id: i64,
    period_type: &str,
    period_start: &str,
) -> Result<i64, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT cost_nanousd FROM usage_rollups WHERE user_id = ?1 AND period_type = ?2 AND period_start = ?3",
    )
    .bind(user_id)
    .bind(period_type)
    .bind(period_start)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(c,)| c).unwrap_or(0))
}

/// Delete request logs older than retention_days. Returns rows deleted.
pub async fn delete_old_request_logs(
    pool: &SqlitePool,
    retention_days: i64,
) -> Result<u64, sqlx::Error> {
    // Compute cutoff as RFC3339 string to match stored format
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(retention_days)).to_rfc3339();
    let result = sqlx::query("DELETE FROM request_logs WHERE started_at < ?1")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Read a setting value from the settings KV table.
pub async fn get_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(v,)| v))
}
