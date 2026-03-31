use chrono::{DateTime, Datelike, NaiveDate, Utc};
use sqlx::SqlitePool;
use tracing::warn;

use crate::db::billing::{
    insert_request_log, lookup_model_pricing, upsert_usage_rollup, RequestLogRow,
};

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
}

impl BillingUsage {
    /// Compute cost in nanousd using pure integer arithmetic.
    pub fn cost_nanousd(&self, input_price: i64, output_price: i64) -> i64 {
        let base_input = self.input_tokens as i64 * input_price;
        let cache_create =
            self.cache_creation_tokens as i64 * input_price * CACHE_CREATION_NUM / CACHE_CREATION_DEN;
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
}

/// Canonical alias table for model normalization.
/// Maps known model alias prefixes to pricing_key in model_pricing table.
static KNOWN_ALIASES: &[(&str, &str)] = &[
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
    let m = m.strip_suffix("-thinking").unwrap_or(&m);

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

/// Persist billing data to DB. Called after upstream usage is known.
pub async fn persist_billing_to_db(ctx: &BillingContext, usage: BillingUsage, stream: bool) {
    let normalized = normalize_model(&ctx.model_raw);
    let (input_price, output_price) = if let Some(ref key) = normalized {
        match lookup_model_pricing(&ctx.db, key).await {
            Ok(Some(prices)) => prices,
            Ok(None) => {
                warn!(
                    "No pricing found for normalized model '{}' (raw: '{}'), using fallback",
                    key, ctx.model_raw
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
            ctx.model_raw
        );
        (FALLBACK_INPUT_PRICE, FALLBACK_OUTPUT_PRICE)
    };

    let cost = usage.cost_nanousd(input_price, output_price);
    let now = Utc::now();
    let completed_at = now.to_rfc3339();
    let duration_ms = (now - ctx.started_at).num_milliseconds();

    let log = RequestLogRow {
        request_id: &ctx.request_id,
        request_type: "messages",
        user_id: ctx.user_id,
        api_key_id: ctx.api_key_id,
        account_id: ctx.account_id,
        model_raw: &ctx.model_raw,
        model_normalized: normalized.as_deref(),
        stream,
        started_at: &ctx.started_at.to_rfc3339(),
        completed_at: Some(&completed_at),
        duration_ms: Some(duration_ms),
        status: "ok",
        http_status: Some(200),
        input_tokens: Some(usage.input_tokens as i64),
        output_tokens: Some(usage.output_tokens as i64),
        cache_creation_tokens: Some(usage.cache_creation_tokens as i64),
        cache_read_tokens: Some(usage.cache_read_tokens as i64),
        priced_input_nanousd_per_token: Some(input_price),
        priced_output_nanousd_per_token: Some(output_price),
        cost_nanousd: cost,
        error_code: None,
        error_message: None,
    };

    if let Err(e) = insert_request_log(&ctx.db, &log).await {
        warn!("Failed to insert request log: {e}");
    }

    if let Some(user_id) = ctx.user_id {
        let (week_start, week_end) = current_week_bounds(now);
        let (month_start, month_end) = current_month_bounds(now);

        if let Err(e) =
            upsert_usage_rollup(&ctx.db, user_id, "week", &week_start, &week_end, &usage, cost)
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
            &usage,
            cost,
        )
        .await
        {
            warn!("Failed to upsert monthly rollup: {e}");
        }
    }
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
        let current = match crate::db::billing::get_current_period_cost(db, user_id, "week", &week_start).await {
            Ok(v) => v,
            Err(e) => {
                warn!("Quota check DB error (weekly), failing open: {e}");
                0
            }
        };
        if current >= budget {
            return Err(crate::error::ClewdrError::QuotaExceeded);
        }
    }

    if let Some(budget) = monthly_budget.filter(|&b| b > 0) {
        let (month_start, _) = current_month_bounds(now);
        let current = match crate::db::billing::get_current_period_cost(db, user_id, "month", &month_start).await {
            Ok(v) => v,
            Err(e) => {
                warn!("Quota check DB error (monthly), failing open: {e}");
                0
            }
        };
        if current >= budget {
            return Err(crate::error::ClewdrError::QuotaExceeded);
        }
    }

    Ok(())
}
