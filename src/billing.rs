use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, Utc};
use sqlx::SqlitePool;
use tracing::warn;

use crate::db::billing::{
    RequestLogRow, insert_request_log, lookup_model_pricing, upsert_usage_daily_rollup,
    upsert_usage_lifetime_total, upsert_usage_rollup,
};
use crate::state::AdminEvent;

/// UTC+8 offset used to bucket daily rollups. Matches the migration SQL
/// `strftime('%Y-%m-%d', datetime(started_at, '+8 hours'))` so historical
/// backfill and live writes agree on the day boundary.
const DAILY_BUCKET_OFFSET_SECS: i32 = 8 * 3600;

/// Cache write multiplier (5-min ephemeral cache, 1.25x base input price).
/// Stored as integer fraction: numerator=125, denominator=100.
const CACHE_CREATION_NUM: i64 = 125;
const CACHE_CREATION_DEN: i64 = 100;

/// Cache read multiplier (0.10x base input price).
const CACHE_READ_NUM: i64 = 10;
const CACHE_READ_DEN: i64 = 100;

/// Fallback pricing for unknown models (Opus 4.0/4.1 rates — most expensive).
const FALLBACK_INPUT_PRICE: i64 = 15000;
const FALLBACK_OUTPUT_PRICE: i64 = 75000;

#[derive(Debug, Clone, Default)]
pub struct BillingUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub ttft_ms: Option<i64>,
}

impl BillingUsage {
    /// Compute cost in nanousd using pure integer arithmetic.
    pub fn cost_nanousd(&self, input_price: i64, output_price: i64) -> i64 {
        let base_input = self.input_tokens as i64 * input_price;
        let cache_create = self.cache_creation_tokens as i64 * input_price * CACHE_CREATION_NUM
            / CACHE_CREATION_DEN;
        let cache_read =
            self.cache_read_tokens as i64 * input_price * CACHE_READ_NUM / CACHE_READ_DEN;
        let output = self.output_tokens as i64 * output_price;
        base_input + cache_create + cache_read + output
    }
}

/// Billing context carried through the request lifecycle.
#[derive(Debug, Clone)]
pub struct BillingContext {
    pub db: SqlitePool,
    pub user_id: Option<i64>,
    pub api_key_id: Option<i64>,
    pub account_id: Option<i64>,
    pub model_raw: String,
    pub request_id: String,
    pub started_at: DateTime<Utc>,
    pub event_tx: tokio::sync::broadcast::Sender<AdminEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestType {
    Messages,
    ProbeCookie,
    ProbeOauth,
    ProbeProxy,
    Test,
}

impl RequestType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::ProbeCookie => "probe_cookie",
            Self::ProbeOauth => "probe_oauth",
            Self::ProbeProxy => "probe_proxy",
            Self::Test => "test",
        }
    }
}

pub struct TerminalLogOptions<'a> {
    pub request_type: RequestType,
    pub stream: bool,
    pub status: &'a str,
    pub http_status: Option<u16>,
    pub usage: Option<BillingUsage>,
    pub error_code: Option<&'a str>,
    pub error_message: Option<&'a str>,
    pub update_rollups: bool,
    pub response_body: Option<&'a str>,
}

/// Canonical alias table for model normalization.
/// Maps known model alias prefixes to pricing_key in model_pricing table.
static KNOWN_ALIASES: &[(&str, &str)] = &[
    ("claude-opus-4-8", "claude-opus-4-8"),
    ("claude-opus-4-7", "claude-opus-4-7"),
    ("claude-opus-4-6", "claude-opus-4-6"),
    ("claude-opus-4-5", "claude-opus-4-5"),
    ("claude-opus-4-1", "claude-opus-4-1"),
    ("claude-opus-4-0", "claude-opus-4-0"),
    ("claude-sonnet-4-6", "claude-sonnet-4-6"),
    ("claude-sonnet-4-5", "claude-sonnet-4-5"),
    ("claude-sonnet-4-0", "claude-sonnet-4-0"),
    ("claude-haiku-4-5", "claude-haiku-4-5"),
    ("claude-haiku-3-5", "claude-haiku-3-5"),
    // Legacy API IDs
    ("claude-3-5-sonnet", "claude-sonnet-4-0"),
    ("claude-3-5-haiku", "claude-haiku-3-5"),
    ("claude-3-haiku", "claude-haiku-3-5"),
];

/// Normalize a raw model string to a pricing_key.
/// Returns None if the model cannot be matched (caller should use fallback pricing).
pub fn normalize_model(raw: &str) -> Option<String> {
    let m = raw.to_ascii_lowercase();

    // Exact alias match
    for &(alias, key) in KNOWN_ALIASES {
        if m == alias {
            return Some(key.to_string());
        }
    }
    // Alias + date suffix (e.g. claude-opus-4-6-20260301)
    for &(alias, key) in KNOWN_ALIASES {
        if let Some(rest) = m.strip_prefix(alias)
            && let Some(date_part) = rest.strip_prefix('-')
            && date_part.len() == 8
            && date_part.bytes().all(|b| b.is_ascii_digit())
        {
            return Some(key.to_string());
        }
    }
    None
}

/// Compute UTC week boundaries (Monday 00:00 to next Monday 00:00).
pub fn current_week_bounds(now: DateTime<Utc>) -> (String, String) {
    let weekday = now.weekday().num_days_from_monday(); // 0=Mon
    let monday = now.date_naive() - chrono::Duration::days(weekday as i64);
    let next_monday = monday + chrono::Duration::days(7);
    (
        monday.format("%Y-%m-%dT00:00:00Z").to_string(),
        next_monday.format("%Y-%m-%dT00:00:00Z").to_string(),
    )
}

/// Compute UTC month boundaries (1st 00:00 to next month 1st 00:00).
pub fn current_month_bounds(now: DateTime<Utc>) -> (String, String) {
    let d = now.date_naive();
    let month_start = NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap();
    let next_month = if d.month() == 12 {
        NaiveDate::from_ymd_opt(d.year() + 1, 1, 1).unwrap()
    } else {
        NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1).unwrap()
    };
    (
        month_start.format("%Y-%m-%dT00:00:00Z").to_string(),
        next_month.format("%Y-%m-%dT00:00:00Z").to_string(),
    )
}

/// Canonical `model_key` used by daily rollups and the Ops drill-down.
/// Mirrors the migration's
/// `COALESCE(NULLIF(model_normalized, ''), NULLIF(model_raw, ''), 'unknown')`
/// so new writes and historical backfill share a single rule.
pub fn compute_model_key(model_raw: &str, model_normalized: &Option<String>) -> String {
    if let Some(n) = model_normalized.as_deref()
        && !n.is_empty()
    {
        return n.to_string();
    }
    let raw = model_raw.trim();
    if !raw.is_empty() {
        return raw.to_string();
    }
    "unknown".to_string()
}

/// UTC+8 day bucket key for `usage_daily_rollups` (YYYY-MM-DD).
pub fn daily_bucket_date_local(started_at: DateTime<Utc>) -> String {
    let offset =
        FixedOffset::east_opt(DAILY_BUCKET_OFFSET_SECS).expect("UTC+8 is a valid fixed offset");
    started_at
        .with_timezone(&offset)
        .format("%Y-%m-%d")
        .to_string()
}

async fn lookup_prices(
    raw_model: &str,
    normalized: &Option<String>,
    db: &SqlitePool,
) -> (i64, i64) {
    if let Some(key) = normalized {
        match lookup_model_pricing(db, key).await {
            Ok(Some(prices)) => prices,
            Ok(None) => {
                warn!(
                    "No pricing found for normalized model '{}' (raw: '{}'), using fallback",
                    key, raw_model
                );
                (FALLBACK_INPUT_PRICE, FALLBACK_OUTPUT_PRICE)
            }
            Err(e) => {
                warn!("Failed to lookup model pricing: {e}");
                (FALLBACK_INPUT_PRICE, FALLBACK_OUTPUT_PRICE)
            }
        }
    } else {
        warn!(
            "Unknown model '{}', using fallback (most expensive) pricing",
            raw_model
        );
        (FALLBACK_INPUT_PRICE, FALLBACK_OUTPUT_PRICE)
    }
}

pub async fn persist_terminal_request_log(ctx: &BillingContext, opts: TerminalLogOptions<'_>) {
    let normalized = normalize_model(&ctx.model_raw);
    let (
        priced_input,
        priced_output,
        cost,
        input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
    ) = if let Some(ref usage) = opts.usage {
        let (input_price, output_price) = lookup_prices(&ctx.model_raw, &normalized, &ctx.db).await;
        (
            Some(input_price),
            Some(output_price),
            usage.cost_nanousd(input_price, output_price),
            Some(usage.input_tokens as i64),
            Some(usage.output_tokens as i64),
            Some(usage.cache_creation_tokens as i64),
            Some(usage.cache_read_tokens as i64),
        )
    } else {
        (None, None, 0, None, None, None, None)
    };
    let now = Utc::now();
    let completed_at = now.to_rfc3339();
    let started_at_rfc = ctx.started_at.to_rfc3339();
    let duration_ms = (now - ctx.started_at).num_milliseconds();
    let model_key = compute_model_key(&ctx.model_raw, &normalized);
    let bucket_date = daily_bucket_date_local(ctx.started_at);

    // Plan §3.1: strict three-condition accountability flag.
    // Same predicate the writer applies to update_rollups, just made
    // explicit on the row so Ops queries don't have to re-derive it.
    let usage_accounted = matches!(opts.request_type, RequestType::Messages)
        && opts.usage.is_some()
        && opts.update_rollups;

    let log = RequestLogRow {
        request_id: &ctx.request_id,
        request_type: opts.request_type.as_str(),
        user_id: ctx.user_id,
        api_key_id: ctx.api_key_id,
        account_id: ctx.account_id,
        model_raw: (!ctx.model_raw.is_empty()).then_some(ctx.model_raw.as_str()),
        model_normalized: normalized.as_deref(),
        model_key: &model_key,
        usage_accounted,
        stream: opts.stream,
        started_at: &started_at_rfc,
        completed_at: Some(&completed_at),
        duration_ms: Some(duration_ms),
        ttft_ms: opts.usage.as_ref().and_then(|usage| usage.ttft_ms),
        status: opts.status,
        http_status: opts.http_status,
        input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        priced_input_nanousd_per_token: priced_input,
        priced_output_nanousd_per_token: priced_output,
        cost_nanousd: cost,
        error_code: opts
            .error_code
            .or_else(|| (opts.status != "ok").then_some(opts.status)),
        error_message: opts.error_message,
        response_body: opts.response_body,
    };

    // Single short transaction wrapping log + week/month/lifetime/daily.
    // Any partial failure rolls the whole batch back so Ops queries
    // never see a log row that lacks a matching rollup contribution.
    let tx_result: Result<(), sqlx::Error> = async {
        let mut tx = ctx.db.begin().await?;
        insert_request_log(&mut *tx, &log).await?;

        if usage_accounted && let (Some(user_id), Some(usage)) = (ctx.user_id, opts.usage.as_ref())
        {
            let (week_start, week_end) = current_week_bounds(now);
            let (month_start, month_end) = current_month_bounds(now);

            upsert_usage_rollup(
                &mut *tx,
                user_id,
                "week",
                &week_start,
                &week_end,
                usage,
                cost,
            )
            .await?;
            upsert_usage_rollup(
                &mut *tx,
                user_id,
                "month",
                &month_start,
                &month_end,
                usage,
                cost,
            )
            .await?;
            upsert_usage_lifetime_total(&mut *tx, user_id, usage, cost).await?;
            upsert_usage_daily_rollup(&mut *tx, user_id, &model_key, &bucket_date, usage, cost)
                .await?;
        }

        tx.commit().await
    }
    .await;

    match tx_result {
        Ok(()) => {
            let _ = ctx.event_tx.send(AdminEvent::request_log(
                opts.request_type.as_str(),
                opts.status,
            ));
        }
        Err(e) => {
            // Structured error per plan §5.2. Single-tx replaces ad-hoc
            // per-statement warnings; there is now exactly one failure
            // point per terminal log.
            tracing::error!(
                target: "billing.persist_failed",
                request_id = %ctx.request_id,
                request_type = opts.request_type.as_str(),
                error = %e,
                "failed to persist terminal request log + rollups",
            );
        }
    }
}

/// Persist a successful Claude messages request after upstream usage is known.
pub async fn persist_billing_to_db(ctx: &BillingContext, usage: BillingUsage, stream: bool) {
    persist_terminal_request_log(
        ctx,
        TerminalLogOptions {
            request_type: RequestType::Messages,
            stream,
            status: "ok",
            http_status: Some(200),
            usage: Some(usage),
            error_code: None,
            error_message: None,
            update_rollups: true,
            response_body: None,
        },
    )
    .await;
}

/// Persist a probe row in request_logs with a raw upstream JSON bundle.
pub async fn persist_probe_log(
    ctx: &BillingContext,
    request_type: RequestType,
    status: &str,
    http_status: Option<u16>,
    response_body: &str,
    error_message: Option<&str>,
) {
    persist_terminal_request_log(
        ctx,
        TerminalLogOptions {
            request_type,
            stream: false,
            status,
            http_status,
            usage: None,
            error_code: error_message.map(|_| status),
            error_message,
            update_rollups: false,
            response_body: Some(response_body),
        },
    )
    .await;
}

/// Check if user has exceeded their budget (soft cap).
pub async fn check_quota(
    db: &SqlitePool,
    user_id: i64,
    weekly_budget: Option<i64>,
    monthly_budget: Option<i64>,
) -> Result<(), crate::error::ClewdrError> {
    let now = Utc::now();

    if let Some(budget) = weekly_budget.filter(|&b| b > 0) {
        let (week_start, _) = current_week_bounds(now);
        let current = crate::db::billing::get_current_period_cost(db, user_id, "week", &week_start)
            .await
            .map_err(|e| {
                warn!("Quota check DB error (weekly), failing closed: {e}");
                e
            })?;
        if current >= budget {
            return Err(crate::error::ClewdrError::QuotaExceeded);
        }
    }

    if let Some(budget) = monthly_budget.filter(|&b| b > 0) {
        let (month_start, _) = current_month_bounds(now);
        let current =
            crate::db::billing::get_current_period_cost(db, user_id, "month", &month_start)
                .await
                .map_err(|e| {
                    warn!("Quota check DB error (monthly), failing closed: {e}");
                    e
                })?;
        if current >= budget {
            return Err(crate::error::ClewdrError::QuotaExceeded);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;

    async fn fresh_pool_with_user() -> (SqlitePool, i64, broadcast::Sender<AdminEvent>) {
        let pool = crate::db::init_pool(std::path::Path::new(":memory:"))
            .await
            .expect("init_pool");
        crate::db::seed_admin(&pool).await.expect("seed_admin");
        // Use a non-admin user so the FK from rollups is unambiguous.
        sqlx::query(
            "INSERT INTO users (username, display_name, password_hash, role, policy_id)
             VALUES ('alice', 'Alice', '$argon2id$dummy', 'member', 1)",
        )
        .execute(&pool)
        .await
        .expect("insert alice");
        let (user_id,): (i64,) = sqlx::query_as("SELECT id FROM users WHERE username = 'alice'")
            .fetch_one(&pool)
            .await
            .expect("alice id");
        let (tx, _rx) = broadcast::channel(16);
        (pool, user_id, tx)
    }

    fn billing_ctx(
        pool: SqlitePool,
        user_id: i64,
        tx: broadcast::Sender<AdminEvent>,
        request_id: &str,
    ) -> BillingContext {
        BillingContext {
            db: pool,
            user_id: Some(user_id),
            api_key_id: None,
            account_id: None,
            model_raw: "claude-opus-4-7".to_string(),
            request_id: request_id.to_string(),
            started_at: Utc::now(),
            event_tx: tx,
        }
    }

    fn sample_usage() -> BillingUsage {
        BillingUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 10,
            cache_read_tokens: 5,
            ttft_ms: Some(120),
        }
    }

    #[test]
    fn compute_model_key_prefers_normalized() {
        let k = compute_model_key("claude-opus-4-7-20260101", &Some("claude-opus-4-7".into()));
        assert_eq!(k, "claude-opus-4-7");
    }

    #[test]
    fn compute_model_key_falls_back_to_raw_when_normalized_missing() {
        let k = compute_model_key("custom-model-x", &None);
        assert_eq!(k, "custom-model-x");
    }

    #[test]
    fn compute_model_key_returns_unknown_for_blank_inputs() {
        let k = compute_model_key("", &Some("".into()));
        assert_eq!(k, "unknown");
        let k = compute_model_key("   ", &None);
        assert_eq!(k, "unknown");
    }

    #[test]
    fn daily_bucket_date_local_uses_utc_plus_8() {
        // 2026-06-01 17:00 UTC == 2026-06-02 01:00 UTC+8 → bucket on the 2nd.
        let dt: DateTime<Utc> = "2026-06-01T17:00:00Z".parse().unwrap();
        assert_eq!(daily_bucket_date_local(dt), "2026-06-02");

        // 2026-06-01 15:00 UTC == 2026-06-01 23:00 UTC+8 → still the 1st.
        let dt: DateTime<Utc> = "2026-06-01T15:00:00Z".parse().unwrap();
        assert_eq!(daily_bucket_date_local(dt), "2026-06-01");
    }

    #[tokio::test]
    async fn persist_messages_writes_log_plus_four_aggregates() {
        let (pool, user_id, tx) = fresh_pool_with_user().await;
        let ctx = billing_ctx(pool.clone(), user_id, tx, "req-ok-1");

        persist_billing_to_db(&ctx, sample_usage(), false).await;

        // request_logs row exists, is accounted, and has a model_key.
        let (count, accounted, model_key): (i64, i64, String) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(MAX(usage_accounted),0), COALESCE(MAX(model_key),'')
             FROM request_logs WHERE request_id = 'req-ok-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "log row written");
        assert_eq!(
            accounted, 1,
            "messages + usage + update_rollups → accounted"
        );
        assert_eq!(model_key, "claude-opus-4-7");

        // Weekly + monthly rollups exist with request_count=1.
        let (week_count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM usage_rollups
             WHERE user_id = ?1 AND period_type = 'week' AND request_count = 1",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(week_count, 1);

        let (month_count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM usage_rollups
             WHERE user_id = ?1 AND period_type = 'month' AND request_count = 1",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(month_count, 1);

        // Lifetime row exists with request_count=1.
        let (lifetime,): (i64,) =
            sqlx::query_as("SELECT request_count FROM usage_lifetime_totals WHERE user_id = ?1")
                .bind(user_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(lifetime, 1);

        // Daily rollup exists for the UTC+8 bucket and matches model_key.
        let (daily_count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM usage_daily_rollups
             WHERE user_id = ?1 AND model_key = 'claude-opus-4-7' AND request_count = 1",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(daily_count, 1);
    }

    #[tokio::test]
    async fn persist_messages_rolls_back_when_log_insert_conflicts() {
        // Two calls with the same request_id: the second must fail at
        // insert_request_log (UNIQUE constraint), and the transaction must
        // roll back the daily/lifetime/rollup increments so totals reflect
        // exactly one accounted request.
        let (pool, user_id, tx) = fresh_pool_with_user().await;
        let ctx1 = billing_ctx(pool.clone(), user_id, tx.clone(), "req-dup");
        let ctx2 = billing_ctx(pool.clone(), user_id, tx, "req-dup");

        persist_billing_to_db(&ctx1, sample_usage(), false).await;
        persist_billing_to_db(&ctx2, sample_usage(), false).await;

        let (log_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM request_logs WHERE request_id = 'req-dup'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(log_count, 1, "duplicate request_id must not insert twice");

        let (lifetime_count,): (i64,) =
            sqlx::query_as("SELECT request_count FROM usage_lifetime_totals WHERE user_id = ?1")
                .bind(user_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            lifetime_count, 1,
            "rolled-back second call must not have incremented lifetime"
        );

        let (daily_count,): (i64,) =
            sqlx::query_as("SELECT request_count FROM usage_daily_rollups WHERE user_id = ?1")
                .bind(user_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            daily_count, 1,
            "rolled-back second call must not have incremented daily"
        );

        let (week_count,): (i64,) = sqlx::query_as(
            "SELECT request_count FROM usage_rollups
             WHERE user_id = ?1 AND period_type = 'week'",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(week_count, 1, "weekly rollup must not double-count");
    }

    #[tokio::test]
    async fn persist_probe_writes_log_but_no_rollups() {
        let (pool, user_id, tx) = fresh_pool_with_user().await;
        let ctx = billing_ctx(pool.clone(), user_id, tx, "probe-1");

        persist_probe_log(
            &ctx,
            RequestType::ProbeCookie,
            "ok",
            Some(200),
            "{\"orgs\":[]}",
            None,
        )
        .await;

        let (log_count, accounted): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(MAX(usage_accounted),0)
             FROM request_logs WHERE request_id = 'probe-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(log_count, 1);
        assert_eq!(accounted, 0, "probe must not be flagged accounted");

        let (rollup_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM usage_rollups WHERE user_id = ?1")
                .bind(user_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rollup_count, 0, "probe must not write usage_rollups");

        let (lifetime_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM usage_lifetime_totals WHERE user_id = ?1")
                .bind(user_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(lifetime_count, 0, "probe must not write lifetime");

        let (daily_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM usage_daily_rollups WHERE user_id = ?1")
                .bind(user_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(daily_count, 0, "probe must not write daily");
    }

    #[tokio::test]
    async fn ensure_daily_rollup_state_is_idempotent() {
        let pool = crate::db::init_pool(std::path::Path::new(":memory:"))
            .await
            .expect("init_pool");
        // Migration already seeded the row; seed_admin's call to
        // ensure_daily_rollup_state below must remain a no-op.
        crate::db::seed_admin(&pool).await.expect("seed_admin");

        // Call again explicitly to prove the INSERT OR IGNORE clamps to one row.
        crate::db::billing::ensure_daily_rollup_state(&pool)
            .await
            .expect("ensure idempotent");
        crate::db::billing::ensure_daily_rollup_state(&pool)
            .await
            .expect("ensure idempotent again");

        let (state_rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM usage_daily_rollup_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state_rows, 1, "single-row state table must stay at one row");
    }

    #[tokio::test]
    async fn empty_db_migration_seeds_state_with_null_backfill() {
        // Fresh DB, no request_logs rows: the migration's seed INSERT runs
        // `SELECT MIN(bucket_date_local) FROM usage_daily_rollups` over an
        // empty table, which yields NULL — exactly what we want to surface
        // as "no historical backfill window available".
        let pool = crate::db::init_pool(std::path::Path::new(":memory:"))
            .await
            .expect("init_pool");

        let (writes_started_at, backfill_available_from): (String, Option<String>) =
            sqlx::query_as(
                "SELECT writes_started_at, backfill_available_from
                 FROM usage_daily_rollup_state WHERE id = 1",
            )
            .fetch_one(&pool)
            .await
            .expect("state row seeded by migration");

        assert!(
            !writes_started_at.is_empty(),
            "writes_started_at must be populated by migration"
        );
        assert!(
            backfill_available_from.is_none(),
            "empty request_logs → no backfill window: got {:?}",
            backfill_available_from
        );
    }

    /// Exercise the migration's historical backfill SQL on a controlled
    /// fixture. We can't replay the actual migration at this point — sqlx
    /// has already advanced past it — so we manually reset the columns to
    /// their pre-backfill state and re-run the same SQL blocks. This
    /// catches drift between the migration's predicates and the writer's
    /// model_key / usage_accounted rules.
    #[tokio::test]
    async fn migration_backfill_sql_classifies_history_correctly() {
        let pool = crate::db::init_pool(std::path::Path::new(":memory:"))
            .await
            .expect("init_pool");
        crate::db::seed_admin(&pool).await.expect("seed_admin");
        sqlx::query(
            "INSERT INTO users (id, username, display_name, password_hash, role, policy_id)
             VALUES (10, 'bob', 'Bob', '$argon2id$dummy', 'member', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Fixture rows simulating various pre-backfill request_logs:
        // - normalized present + raw present → model_key = normalized
        // - raw present, normalized NULL/empty → model_key = raw
        // - raw NULL, normalized NULL → model_key = 'unknown'
        // - messages + tokens → usage_accounted = 1
        // - probe / messages-without-tokens → usage_accounted = 0
        // - UTC+8 day boundary: 17:00 UTC on day N counts as N+1
        sqlx::query(
            r#"INSERT INTO request_logs
               (request_id, request_type, user_id, model_raw, model_normalized,
                stream, started_at, status, http_status,
                input_tokens, output_tokens, cost_nanousd,
                model_key, usage_accounted)
               VALUES
               -- 1: normalized wins
               ('r1', 'messages', 10, 'claude-opus-4-7-20260101', 'claude-opus-4-7',
                1, '2026-06-01T10:00:00Z', 'ok', 200, 100, 50, 1000, 'unknown', 0),
               -- 2: raw fallback when normalized empty
               ('r2', 'messages', 10, 'custom-model', '',
                1, '2026-06-01T11:00:00Z', 'ok', 200, 200, 100, 2000, 'unknown', 0),
               -- 3: 'unknown' when both missing
               ('r3', 'messages', 10, NULL, NULL,
                1, '2026-06-01T17:30:00Z', 'ok', 200, 50, 25, 500, 'unknown', 0),
               -- 4: probe must stay unaccounted even after backfill
               ('r4', 'probe_cookie', 10, NULL, NULL,
                0, '2026-06-01T12:00:00Z', 'ok', 200, NULL, NULL, 0, 'unknown', 0),
               -- 5: messages without tokens must stay unaccounted
               ('r5', 'messages', 10, 'claude-opus-4-7', 'claude-opus-4-7',
                1, '2026-06-01T13:00:00Z', 'upstream_error', 502, NULL, NULL, 0,
                'unknown', 0)"#,
        )
        .execute(&pool)
        .await
        .unwrap();

        // Re-run the migration's backfill block.
        sqlx::query(
            "UPDATE request_logs
             SET model_key = COALESCE(
                 NULLIF(model_normalized, ''),
                 NULLIF(model_raw, ''),
                 'unknown'
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE request_logs
             SET usage_accounted = 1
             WHERE request_type = 'messages'
               AND input_tokens IS NOT NULL",
        )
        .execute(&pool)
        .await
        .unwrap();

        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT request_id, model_key, usage_accounted
             FROM request_logs ORDER BY request_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("r1".to_string(), "claude-opus-4-7".to_string(), 1),
                ("r2".to_string(), "custom-model".to_string(), 1),
                ("r3".to_string(), "unknown".to_string(), 1),
                ("r4".to_string(), "unknown".to_string(), 0),
                ("r5".to_string(), "claude-opus-4-7".to_string(), 0),
            ],
            "model_key + usage_accounted backfill must match plan §3.1 / §4.1"
        );

        // Clear daily and re-run the migration's daily INSERT SELECT.
        sqlx::query("DELETE FROM usage_daily_rollups")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            r#"INSERT INTO usage_daily_rollups (
                user_id, model_key, bucket_date_local,
                request_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens, cost_nanousd
            )
            SELECT
                user_id, model_key,
                strftime('%Y-%m-%d', datetime(started_at, '+8 hours')),
                COUNT(*),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cost_nanousd), 0)
            FROM request_logs
            WHERE usage_accounted = 1 AND user_id IS NOT NULL
            GROUP BY user_id, model_key,
                     strftime('%Y-%m-%d', datetime(started_at, '+8 hours'))"#,
        )
        .execute(&pool)
        .await
        .unwrap();

        // r1 (10:00 UTC → 18:00 UTC+8) and r2 (11:00 UTC → 19:00 UTC+8) both
        // fall in 2026-06-01 UTC+8. r3 (17:30 UTC → 01:30 UTC+8 next day)
        // crosses into 2026-06-02. r4/r5 are unaccounted so they don't show.
        let buckets: Vec<(String, String, i64, i64)> = sqlx::query_as(
            "SELECT model_key, bucket_date_local, request_count, input_tokens
             FROM usage_daily_rollups
             ORDER BY bucket_date_local, model_key",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            buckets,
            vec![
                // 2026-06-01: r1 (100 input) for opus-4-7, r2 (200 input) for custom
                ("claude-opus-4-7".into(), "2026-06-01".into(), 1, 100),
                ("custom-model".into(), "2026-06-01".into(), 1, 200),
                // 2026-06-02: r3 (50 input) for unknown
                ("unknown".into(), "2026-06-02".into(), 1, 50),
            ],
            "daily backfill must group by UTC+8 day and accountable rows only"
        );
    }
}
