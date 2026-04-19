use chrono::{DateTime, Datelike, NaiveDate, Utc};
use sqlx::SqlitePool;
use tracing::warn;

use crate::db::billing::{
    RequestLogRow, insert_request_log, lookup_model_pricing, upsert_usage_lifetime_total,
    upsert_usage_rollup,
};
use crate::state::AdminEvent;

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
        if let Some(rest) = m.strip_prefix(alias) {
            if let Some(date_part) = rest.strip_prefix('-') {
                if date_part.len() == 8 && date_part.bytes().all(|b| b.is_ascii_digit()) {
                    return Some(key.to_string());
                }
            }
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
    let duration_ms = (now - ctx.started_at).num_milliseconds();

    let log = RequestLogRow {
        request_id: &ctx.request_id,
        request_type: opts.request_type.as_str(),
        user_id: ctx.user_id,
        api_key_id: ctx.api_key_id,
        account_id: ctx.account_id,
        model_raw: (!ctx.model_raw.is_empty()).then_some(ctx.model_raw.as_str()),
        model_normalized: normalized.as_deref(),
        stream: opts.stream,
        started_at: &ctx.started_at.to_rfc3339(),
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

    if let Err(e) = insert_request_log(&ctx.db, &log).await {
        warn!("Failed to insert request log: {e}");
    } else {
        let _ = ctx.event_tx.send(AdminEvent::request_log(
            opts.request_type.as_str(),
            opts.status,
        ));
    }

    if opts.update_rollups
        && let (Some(user_id), Some(usage)) = (ctx.user_id, opts.usage.as_ref())
    {
        let (week_start, week_end) = current_week_bounds(now);
        let (month_start, month_end) = current_month_bounds(now);

        if let Err(e) = upsert_usage_rollup(
            &ctx.db,
            user_id,
            "week",
            &week_start,
            &week_end,
            usage,
            cost,
        )
        .await
        {
            warn!("Failed to upsert weekly rollup: {e}");
        }
        if let Err(e) = upsert_usage_rollup(
            &ctx.db,
            user_id,
            "month",
            &month_start,
            &month_end,
            usage,
            cost,
        )
        .await
        {
            warn!("Failed to upsert monthly rollup: {e}");
        }
        if let Err(e) = upsert_usage_lifetime_total(&ctx.db, user_id, usage, cost).await {
            warn!("Failed to upsert lifetime usage total: {e}");
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
