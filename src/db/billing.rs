use sqlx::{Executor, Sqlite, SqlitePool};

use crate::billing::BillingUsage;

/// Row to insert into request_logs.
pub struct RequestLogRow<'a> {
    pub request_id: &'a str,
    pub request_type: &'a str,
    pub user_id: Option<i64>,
    pub api_key_id: Option<i64>,
    pub account_id: Option<i64>,
    pub model_raw: Option<&'a str>,
    pub model_normalized: Option<&'a str>,
    pub model_key: &'a str,
    pub usage_accounted: bool,
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
    pub response_body: Option<&'a str>,
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

/// Row shape for the enhanced-audit sidecar table. Inserted in the same
/// transaction as the parent `request_logs` row when (and only when)
/// the API key has `enhanced_audit_enabled = true`. The presence of a
/// row here is the authoritative "this request was audited" signal —
/// we deliberately do not denormalize a flag back onto `request_logs`.
#[derive(Debug)]
pub struct RequestLogAuditRow<'a> {
    pub request_log_id: i64,
    pub peer_ip: Option<&'a str>,
    pub client_ip: Option<&'a str>,
    pub ip_source: Option<&'a str>,
    pub forwarded_chain: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub api_surface: Option<&'a str>,
    pub anthropic_version: Option<&'a str>,
    pub anthropic_beta: Option<&'a str>,
    pub content_length: Option<i64>,
}

/// Insert a request log row. Returns the new row's `id`, retrieved via
/// `last_insert_rowid()` so the caller can attach a sidecar audit row
/// inside the same transaction.
///
/// Accepts any sqlx `Executor` (a pool or a transaction connection) so the
/// terminal write path can bundle log + rollups into a single transaction.
pub async fn insert_request_log<'e, E>(
    executor: E,
    r: &RequestLogRow<'_>,
) -> Result<i64, sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    sqlx::query(
        r#"INSERT INTO request_logs (
            request_id, request_type, user_id, api_key_id, account_id,
            model_raw, model_normalized, model_key, usage_accounted, stream,
            started_at, completed_at, duration_ms, ttft_ms,
            status, http_status,
            input_tokens, output_tokens,
            cache_creation_tokens, cache_read_tokens,
            priced_input_nanousd_per_token, priced_output_nanousd_per_token,
            cost_nanousd, error_code, error_message, response_body
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5,
            ?6, ?7, ?8, ?9, ?10,
            ?11, ?12, ?13, ?14,
            ?15, ?16,
            ?17, ?18,
            ?19, ?20,
            ?21, ?22,
            ?23, ?24, ?25, ?26
        )"#,
    )
    .bind(r.request_id)
    .bind(r.request_type)
    .bind(r.user_id)
    .bind(r.api_key_id)
    .bind(r.account_id)
    .bind(r.model_raw)
    .bind(r.model_normalized)
    .bind(r.model_key)
    .bind(r.usage_accounted as i32)
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
    .bind(r.response_body)
    .execute(executor)
    .await
    .map(|res| res.last_insert_rowid())
}

/// Insert one row into `request_log_audits`. Must run inside the same
/// transaction as the parent `request_logs` insert so a partial failure
/// rolls both back together. `PRAGMA foreign_keys = ON` (set in
/// `db::init_pool`) keeps the FK live.
pub async fn insert_request_log_audit<'e, E>(
    executor: E,
    r: &RequestLogAuditRow<'_>,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    sqlx::query(
        r#"INSERT INTO request_log_audits (
            request_log_id, peer_ip, client_ip, ip_source, forwarded_chain,
            user_agent, api_surface, anthropic_version, anthropic_beta,
            content_length
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5,
            ?6, ?7, ?8, ?9, ?10
        )"#,
    )
    .bind(r.request_log_id)
    .bind(r.peer_ip)
    .bind(r.client_ip)
    .bind(r.ip_source)
    .bind(r.forwarded_chain)
    .bind(r.user_agent)
    .bind(r.api_surface)
    .bind(r.anthropic_version)
    .bind(r.anthropic_beta)
    .bind(r.content_length)
    .execute(executor)
    .await?;
    Ok(())
}

/// Upsert a usage rollup row, incrementing counters on conflict.
pub async fn upsert_usage_rollup<'e, E>(
    executor: E,
    user_id: i64,
    period_type: &str,
    period_start: &str,
    period_end: &str,
    usage: &BillingUsage,
    cost_nanousd: i64,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
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
    .execute(executor)
    .await?;
    Ok(())
}

/// Upsert a per-user lifetime usage total, incrementing counters on conflict.
pub async fn upsert_usage_lifetime_total<'e, E>(
    executor: E,
    user_id: i64,
    usage: &BillingUsage,
    cost_nanousd: i64,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    sqlx::query(
        r#"INSERT INTO usage_lifetime_totals (
            user_id,
            request_count,
            input_tokens,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
            cost_nanousd,
            updated_at
        ) VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, CURRENT_TIMESTAMP)
        ON CONFLICT (user_id) DO UPDATE SET
            request_count = request_count + 1,
            input_tokens = input_tokens + excluded.input_tokens,
            output_tokens = output_tokens + excluded.output_tokens,
            cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
            cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
            cost_nanousd = cost_nanousd + excluded.cost_nanousd,
            updated_at = CURRENT_TIMESTAMP"#,
    )
    .bind(user_id)
    .bind(usage.input_tokens as i64)
    .bind(usage.output_tokens as i64)
    .bind(usage.cache_creation_tokens as i64)
    .bind(usage.cache_read_tokens as i64)
    .bind(cost_nanousd)
    .execute(executor)
    .await?;
    Ok(())
}

/// Upsert a per-(user, model_key, UTC+8 day) rollup, incrementing counters
/// on conflict.
///
/// `bucket_date_local` is a YYYY-MM-DD string for the UTC+8 day boundary,
/// computed by the writer from `started_at`. Keeping this column as plain
/// TEXT avoids any timezone surprises at read time and lets indexes serve
/// 7d / 30d range scans directly.
pub async fn upsert_usage_daily_rollup<'e, E>(
    executor: E,
    user_id: i64,
    model_key: &str,
    bucket_date_local: &str,
    usage: &BillingUsage,
    cost_nanousd: i64,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    sqlx::query(
        r#"INSERT INTO usage_daily_rollups (
            user_id,
            model_key,
            bucket_date_local,
            request_count,
            input_tokens,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
            cost_nanousd,
            updated_at
        ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, CURRENT_TIMESTAMP)
        ON CONFLICT (user_id, model_key, bucket_date_local) DO UPDATE SET
            request_count = request_count + 1,
            input_tokens = input_tokens + excluded.input_tokens,
            output_tokens = output_tokens + excluded.output_tokens,
            cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
            cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
            cost_nanousd = cost_nanousd + excluded.cost_nanousd,
            updated_at = CURRENT_TIMESTAMP"#,
    )
    .bind(user_id)
    .bind(model_key)
    .bind(bucket_date_local)
    .bind(usage.input_tokens as i64)
    .bind(usage.output_tokens as i64)
    .bind(usage.cache_creation_tokens as i64)
    .bind(usage.cache_read_tokens as i64)
    .bind(cost_nanousd)
    .execute(executor)
    .await?;
    Ok(())
}

/// Ensure the single-row `usage_daily_rollup_state` exists. Migrations
/// seed this row on first deploy; this helper covers the
/// default-restore-then-restart path where runtime tables get wiped but
/// the application keeps writing rollups from that moment forward.
pub async fn ensure_daily_rollup_state(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT OR IGNORE INTO usage_daily_rollup_state (
            id, writes_started_at, backfill_available_from
        ) VALUES (1, CURRENT_TIMESTAMP, NULL)"#,
    )
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

/// Delete a single usage_rollups row for the given user/period.
/// Returns rows deleted (0 if no row existed for that period).
pub async fn delete_usage_rollup<'e, E>(
    executor: E,
    user_id: i64,
    period_type: &str,
    period_start: &str,
) -> Result<u64, sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    let result = sqlx::query(
        "DELETE FROM usage_rollups WHERE user_id = ?1 AND period_type = ?2 AND period_start = ?3",
    )
    .bind(user_id)
    .bind(period_type)
    .bind(period_start)
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
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
