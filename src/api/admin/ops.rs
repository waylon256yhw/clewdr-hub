//! Admin Ops API.
//!
//! Three-layer response per the Ops overhaul plan (lazy-squishing-wand.md):
//!
//! 1. `window_totals` + `distribution` + `ranking` + `series` all read
//!    from a single source per range:
//!    - 24h: `request_logs` filtered by `usage_accounted = 1`
//!    - 7d / 30d: `usage_daily_rollups`
//! 2. `previous_window_totals` reads from the same source shifted back by
//!    one window so the comparison can never mix data sources.
//! 3. `lifetime_totals` reads from `usage_lifetime_totals` and is shown
//!    alongside, never folded into the window KPI.
//!
//! `metric` (cost / tokens / requests) is a backend-validated enum that
//! drives both the rendering hint and the sort key used by
//! distribution / ranking / series. The frontend echoes the resolved
//! metric in its `metric` selector.
//!
//! When `user_id` is passed the response switches the ranking / series
//! dimension from "users" to "this user's models", which is the only way
//! the page can let an operator drill into per-model spend for a single
//! account. `distribution` is always model-keyed.
//!
//! Legacy fields (`totals`, `model_distribution`, `top_users`,
//! `user_series`, `coverage_limited`) are populated for one release so
//! the current Ops.tsx keeps rendering; PR-C removes them.

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{Query, State},
};
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::db::billing::get_setting;
use crate::error::ClewdrError;

const DEFAULT_RETENTION_DAYS: i64 = 7;
const DEFAULT_TOP_USERS: usize = 5;
const MAX_TOP_USERS: usize = 8;
const MODEL_DISTRIBUTION_TOP_N: usize = 8;
const SHANGHAI_OFFSET_SECONDS: i32 = 8 * 60 * 60;

#[derive(Deserialize)]
pub struct OpsUsageParams {
    pub range: Option<String>,
    pub metric: Option<String>,
    pub top_users: Option<usize>,
    pub user_id: Option<i64>,
}

#[derive(Serialize)]
pub struct OpsUsageResponse {
    pub range: String,
    pub metric: String,
    pub bucket_unit: String,
    pub dimension: String,
    pub selected_user_id: Option<i64>,
    pub retention_days: i64,
    pub window_started_at: String,
    pub window_ended_at: String,
    pub previous_window_started_at: String,
    pub previous_window_ended_at: String,
    pub buckets: Vec<String>,
    pub bucket_labels: Vec<BucketLabel>,

    pub window_totals: UsageTotals,
    pub previous_window_totals: UsageTotals,
    pub lifetime_totals: UsageTotals,

    pub comparison: ComparisonInfo,
    pub coverage: CoverageInfo,

    pub distribution: Vec<DimensionItem>,
    pub ranking: Vec<DimensionItem>,
    pub series: Vec<DimensionSeries>,

    // ---- Legacy fields: populated for forward-compat with the current
    // Ops.tsx. PR-C will switch the frontend over and then we can delete
    // these in a follow-up.
    pub coverage_limited: bool,
    pub totals: UsageTotals,
    pub model_distribution: Vec<ModelDistributionItem>,
    pub top_users: Vec<UserAggregate>,
    pub user_series: Vec<UserSeries>,
}

#[derive(Serialize, Default, Clone)]
pub struct UsageTotals {
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    pub cost_nanousd: i64,
}

impl UsageTotals {
    fn from_parts(
        request_count: i64,
        input_tokens: i64,
        output_tokens: i64,
        cache_creation_tokens: i64,
        cache_read_tokens: i64,
        cost_nanousd: i64,
    ) -> Self {
        Self {
            request_count,
            input_tokens,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
            total_tokens: input_tokens + output_tokens + cache_creation_tokens + cache_read_tokens,
            cost_nanousd,
        }
    }
}

#[derive(Serialize)]
pub struct BucketLabel {
    pub key: String,
    pub partial: bool,
}

#[derive(Serialize)]
pub struct DimensionItem {
    /// "user" or "model" — explicit so the frontend can render the right
    /// label without re-deriving from the presence of `user_id`.
    pub kind: String,
    pub user_id: Option<i64>,
    pub model_key: Option<String>,
    pub label: String,
    /// True for the synthesized "其他" aggregate row; frontend disables
    /// click-drilldown on these because they don't correspond to a
    /// single user/model.
    pub is_other_bucket: bool,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    pub cost_nanousd: i64,
}

#[derive(Serialize)]
pub struct DimensionSeries {
    pub kind: String,
    pub user_id: Option<i64>,
    pub model_key: Option<String>,
    pub label: String,
    pub points: Vec<DimensionSeriesPoint>,
}

#[derive(Serialize)]
pub struct DimensionSeriesPoint {
    pub bucket: String,
    pub partial: bool,
    pub request_count: i64,
    pub total_tokens: i64,
    pub cost_nanousd: i64,
}

#[derive(Serialize)]
pub struct ComparisonInfo {
    /// Both windows are fully covered by the data source, so the ratio is
    /// meaningful. When false the frontend should show "数据积累中" instead
    /// of an arrow.
    pub complete: bool,
    /// Current window's completed buckets (i.e. excluding the partial
    /// trailing bucket). Used by PR-C to qualify the comparison label.
    pub current_bucket_count: i64,
    /// How many buckets a fully-covered window should have. 24 for 24h,
    /// 7 for 7d, 30 for 30d.
    pub expected_bucket_count: i64,
    pub window_label: String,
    pub cost_ratio: Option<f64>,
    pub total_tokens_ratio: Option<f64>,
    pub request_count_ratio: Option<f64>,
}

#[derive(Serialize)]
pub struct CoverageInfo {
    /// Current window is fully backed by the data source.
    pub complete: bool,
    /// Previous window is fully backed too — used for the comparison
    /// gate. Distinct from `complete` because a 7d coverage start that
    /// lands mid-window leaves the comparison incomplete even if the
    /// current window itself is complete.
    pub comparison_complete: bool,
    pub writes_started_at: Option<String>,
    pub backfill_available_from: Option<String>,
    pub retention_days: i64,
    /// 24h-only: the earliest started_at that the live request_logs can
    /// still surface, accounting for retention pruning. Computed as
    /// `max(now - retention_days, MIN(request_logs.started_at WHERE
    /// usage_accounted = 1))`. Null for 7d/30d.
    pub logs_available_from: Option<String>,
}

// Legacy item shapes — exact field set of the pre-PR-B response so the
// existing frontend keeps deserialising.
#[derive(Serialize)]
pub struct ModelDistributionItem {
    pub model: String,
    pub request_count: i64,
    pub total_tokens: i64,
    pub cost_nanousd: i64,
}

#[derive(Serialize)]
pub struct UserAggregate {
    pub user_id: i64,
    pub username: String,
    pub request_count: i64,
    pub total_tokens: i64,
    pub cost_nanousd: i64,
}

#[derive(Serialize)]
pub struct UserSeries {
    pub user_id: i64,
    pub username: String,
    pub points: Vec<UserSeriesPoint>,
}

#[derive(Serialize)]
pub struct UserSeriesPoint {
    pub bucket: String,
    pub request_count: i64,
    pub total_tokens: i64,
    pub cost_nanousd: i64,
}

// ---- Internal types ------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetricKind {
    Cost,
    Tokens,
    Requests,
}

impl MetricKind {
    fn from_query(raw: Option<&str>) -> Self {
        match raw {
            Some("tokens") => Self::Tokens,
            Some("requests") => Self::Requests,
            // cost is the default; any unknown string also falls back to
            // cost so a typo in the URL never empties the page.
            _ => Self::Cost,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Cost => "cost",
            Self::Tokens => "tokens",
            Self::Requests => "requests",
        }
    }

    /// Composite ORDER BY suitable for SQL. Whitelist-only — `metric` is
    /// never interpolated as raw user input. The token expression is
    /// inlined because the SELECT clauses don't pre-compute a
    /// `total_tokens` column (the response builder adds that field in
    /// Rust).
    fn order_by_sql(self) -> &'static str {
        match self {
            Self::Cost => {
                "cost_nanousd DESC, \
                 (input_tokens + output_tokens + cache_creation_tokens + cache_read_tokens) DESC, \
                 request_count DESC"
            }
            Self::Tokens => {
                "(input_tokens + output_tokens + cache_creation_tokens + cache_read_tokens) DESC, \
                 cost_nanousd DESC, request_count DESC"
            }
            Self::Requests => {
                "request_count DESC, cost_nanousd DESC, \
                 (input_tokens + output_tokens + cache_creation_tokens + cache_read_tokens) DESC"
            }
        }
    }

    /// In-memory comparator mirroring `order_by_sql` for collections we
    /// build via aggregation in Rust (e.g. when merging "其他" rows).
    fn cmp_items(self, a: &DimensionItem, b: &DimensionItem) -> std::cmp::Ordering {
        let (ka, kb) = match self {
            Self::Cost => (a.cost_nanousd, b.cost_nanousd),
            Self::Tokens => (a.total_tokens, b.total_tokens),
            Self::Requests => (a.request_count, b.request_count),
        };
        kb.cmp(&ka)
            .then_with(|| b.cost_nanousd.cmp(&a.cost_nanousd))
            .then_with(|| b.total_tokens.cmp(&a.total_tokens))
            .then_with(|| b.request_count.cmp(&a.request_count))
            .then_with(|| a.label.cmp(&b.label))
    }
}

#[derive(Clone, Copy, Debug)]
enum RangePreset {
    Last24Hours,
    Last7Days,
    Last30Days,
}

impl RangePreset {
    fn from_query(raw: Option<&str>) -> Self {
        match raw {
            Some("24h") => Self::Last24Hours,
            Some("30d") => Self::Last30Days,
            _ => Self::Last7Days,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Last24Hours => "24h",
            Self::Last7Days => "7d",
            Self::Last30Days => "30d",
        }
    }

    fn bucket_unit(self) -> &'static str {
        match self {
            Self::Last24Hours => "hour",
            Self::Last7Days | Self::Last30Days => "day",
        }
    }

    fn expected_bucket_count(self) -> i64 {
        match self {
            Self::Last24Hours => 24,
            Self::Last7Days => 7,
            Self::Last30Days => 30,
        }
    }
}

#[derive(Clone, Copy)]
enum WindowSource {
    /// 24h: query `request_logs` directly. Carries the UTC RFC3339
    /// boundaries used by the `started_at >= ? AND started_at < ?` filter.
    RequestLogs,
    /// 7d / 30d: query `usage_daily_rollups`. Carries the UTC+8 day
    /// strings used by the `bucket_date_local >= ? AND < ?` filter.
    DailyRollups,
}

struct WindowSpec {
    preset: RangePreset,
    /// UTC range of the window. Used directly by the request_logs source
    /// and surfaced in the response.
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
    /// UTC+8 day strings for the daily source. For 24h windows these are
    /// still computed but the daily source isn't queried.
    start_local_day: String,
    /// Exclusive end day (the day after the window ends). For 24h windows
    /// this isn't directly used.
    end_local_day_exclusive: String,
    /// Ordered list of bucket keys ("YYYY-MM-DD HH:00" for 24h,
    /// "YYYY-MM-DD" for 7d/30d). The trailing entry is the current
    /// partial bucket.
    buckets: Vec<String>,
    /// Bucket index of the current partial bucket (last one), or
    /// `buckets.len()` when the window has fully closed (unusual).
    partial_bucket_idx: usize,
}

impl WindowSpec {
    fn source(&self) -> WindowSource {
        match self.preset {
            RangePreset::Last24Hours => WindowSource::RequestLogs,
            RangePreset::Last7Days | RangePreset::Last30Days => WindowSource::DailyRollups,
        }
    }
}

// ---- Window construction -------------------------------------------------

fn shanghai_offset() -> FixedOffset {
    FixedOffset::east_opt(SHANGHAI_OFFSET_SECONDS).expect("+08:00 is a valid offset")
}

fn build_window(preset: RangePreset, now: DateTime<Utc>) -> WindowSpec {
    let shanghai = shanghai_offset();
    let now_local = now.with_timezone(&shanghai);

    match preset {
        RangePreset::Last24Hours => {
            let current_hour = now_local
                .date_naive()
                .and_hms_opt(now_local.hour(), 0, 0)
                .expect("valid current hour");
            let end_local = current_hour + chrono::Duration::hours(1);
            let start_local = end_local - chrono::Duration::hours(24);
            let start_utc = shanghai
                .from_local_datetime(&start_local)
                .single()
                .expect("fixed offset local start")
                .with_timezone(&Utc);
            let end_utc = shanghai
                .from_local_datetime(&end_local)
                .single()
                .expect("fixed offset local end")
                .with_timezone(&Utc);
            let buckets = build_hour_buckets(start_local, end_local);
            // The current hour (containing `now`) is the last bucket and
            // is always partial.
            let partial_bucket_idx = buckets.len().saturating_sub(1);
            WindowSpec {
                preset,
                start_utc,
                end_utc,
                start_local_day: start_local.format("%Y-%m-%d").to_string(),
                end_local_day_exclusive: end_local.format("%Y-%m-%d").to_string(),
                buckets,
                partial_bucket_idx,
            }
        }
        RangePreset::Last7Days | RangePreset::Last30Days => {
            let window_days = preset.expected_bucket_count();
            let tomorrow = now_local
                .date_naive()
                .succ_opt()
                .expect("valid next day")
                .and_hms_opt(0, 0, 0)
                .expect("valid midnight");
            let start_local = tomorrow - chrono::Duration::days(window_days);
            let start_utc = shanghai
                .from_local_datetime(&start_local)
                .single()
                .expect("fixed offset local start")
                .with_timezone(&Utc);
            let end_utc = shanghai
                .from_local_datetime(&tomorrow)
                .single()
                .expect("fixed offset local end")
                .with_timezone(&Utc);
            let buckets = build_day_buckets(start_local, tomorrow);
            let partial_bucket_idx = buckets.len().saturating_sub(1);
            WindowSpec {
                preset,
                start_utc,
                end_utc,
                start_local_day: start_local.format("%Y-%m-%d").to_string(),
                end_local_day_exclusive: tomorrow.format("%Y-%m-%d").to_string(),
                buckets,
                partial_bucket_idx,
            }
        }
    }
}

fn shift_window_back(spec: &WindowSpec) -> WindowSpec {
    let shanghai = shanghai_offset();
    let span = spec.end_utc - spec.start_utc;
    let new_end_utc = spec.start_utc;
    let new_start_utc = new_end_utc - span;
    let new_start_local = new_start_utc.with_timezone(&shanghai).naive_local();
    let new_end_local = new_end_utc.with_timezone(&shanghai).naive_local();
    let buckets = match spec.preset {
        RangePreset::Last24Hours => build_hour_buckets(new_start_local, new_end_local),
        RangePreset::Last7Days | RangePreset::Last30Days => {
            build_day_buckets(new_start_local, new_end_local)
        }
    };
    WindowSpec {
        preset: spec.preset,
        start_utc: new_start_utc,
        end_utc: new_end_utc,
        start_local_day: new_start_local.format("%Y-%m-%d").to_string(),
        end_local_day_exclusive: new_end_local.format("%Y-%m-%d").to_string(),
        // The previous window is fully closed by construction (it ends
        // exactly at the current window's start), so it has no partial
        // bucket. We mark partial_bucket_idx == len to signal "no partial".
        partial_bucket_idx: buckets.len(),
        buckets,
    }
}

fn build_hour_buckets(start: NaiveDateTime, end: NaiveDateTime) -> Vec<String> {
    let mut buckets = Vec::new();
    let mut cursor = start;
    while cursor < end {
        buckets.push(cursor.format("%Y-%m-%d %H:00").to_string());
        cursor += chrono::Duration::hours(1);
    }
    buckets
}

fn build_day_buckets(start: NaiveDateTime, end: NaiveDateTime) -> Vec<String> {
    let mut buckets = Vec::new();
    let mut cursor = start;
    while cursor < end {
        buckets.push(cursor.format("%Y-%m-%d").to_string());
        cursor += chrono::Duration::days(1);
    }
    buckets
}

// ---- HTTP handler --------------------------------------------------------

pub async fn usage(
    State(db): State<SqlitePool>,
    Query(params): Query<OpsUsageParams>,
) -> Result<Json<OpsUsageResponse>, ClewdrError> {
    let now = Utc::now();
    let preset = RangePreset::from_query(params.range.as_deref());
    let metric = MetricKind::from_query(params.metric.as_deref());
    let top_n = params
        .top_users
        .unwrap_or(DEFAULT_TOP_USERS)
        .clamp(1, MAX_TOP_USERS);
    let selected_user_id = params.user_id;

    let window = build_window(preset, now);
    let previous_window = shift_window_back(&window);

    let retention_days = read_retention_days(&db).await?;
    let state = read_daily_state(&db).await?;

    // Window-source aggregates. All four (window_totals, distribution,
    // ranking, series) must come from the same data source per plan §6.1.
    let window_totals = load_window_totals(&db, &window, selected_user_id).await?;
    let previous_window_totals =
        load_window_totals(&db, &previous_window, selected_user_id).await?;
    let distribution = load_distribution(&db, &window, selected_user_id, metric).await?;
    let (ranking, series) =
        load_ranking_and_series(&db, &window, selected_user_id, metric, top_n).await?;

    // Lifetime is always sourced from usage_lifetime_totals.
    let lifetime_totals = load_lifetime_totals(&db, selected_user_id).await?;

    let coverage = build_coverage(&db, &window, &previous_window, retention_days, &state).await?;
    let comparison = build_comparison(&window, &window_totals, &previous_window_totals, &coverage);

    let dimension = if selected_user_id.is_some() {
        "model"
    } else {
        "user"
    };

    // ---- Legacy projection.
    let (top_users_legacy, user_series_legacy, model_distribution_legacy) =
        legacy_projection(&db, &window, selected_user_id, metric, top_n).await?;
    let coverage_limited = !coverage.complete;

    Ok(Json(OpsUsageResponse {
        range: preset.label().to_string(),
        metric: metric.as_str().to_string(),
        bucket_unit: preset.bucket_unit().to_string(),
        dimension: dimension.to_string(),
        selected_user_id,
        retention_days,
        window_started_at: window.start_utc.to_rfc3339(),
        window_ended_at: window.end_utc.to_rfc3339(),
        previous_window_started_at: previous_window.start_utc.to_rfc3339(),
        previous_window_ended_at: previous_window.end_utc.to_rfc3339(),
        buckets: window.buckets.clone(),
        bucket_labels: window
            .buckets
            .iter()
            .enumerate()
            .map(|(idx, key)| BucketLabel {
                key: key.clone(),
                partial: idx == window.partial_bucket_idx,
            })
            .collect(),
        window_totals: window_totals.clone(),
        previous_window_totals,
        lifetime_totals: lifetime_totals.clone(),
        comparison,
        coverage,
        distribution,
        ranking,
        series,
        coverage_limited,
        totals: lifetime_totals,
        model_distribution: model_distribution_legacy,
        top_users: top_users_legacy,
        user_series: user_series_legacy,
    }))
}

// ---- Loaders -------------------------------------------------------------

async fn read_retention_days(db: &SqlitePool) -> Result<i64, ClewdrError> {
    Ok(get_setting(db, "log_retention_days")
        .await?
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(DEFAULT_RETENTION_DAYS))
}

#[derive(Default, Debug, Clone)]
struct DailyRollupState {
    writes_started_at: Option<String>,
    backfill_available_from: Option<String>,
}

async fn read_daily_state(db: &SqlitePool) -> Result<DailyRollupState, ClewdrError> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT writes_started_at, backfill_available_from
         FROM usage_daily_rollup_state WHERE id = 1",
    )
    .fetch_optional(db)
    .await?;
    Ok(match row {
        Some((Some(w), b)) => DailyRollupState {
            writes_started_at: Some(w),
            backfill_available_from: b,
        },
        Some((None, b)) => DailyRollupState {
            writes_started_at: None,
            backfill_available_from: b,
        },
        None => DailyRollupState::default(),
    })
}

async fn load_lifetime_totals(
    db: &SqlitePool,
    selected_user_id: Option<i64>,
) -> Result<UsageTotals, ClewdrError> {
    let (
        request_count,
        input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        cost_nanousd,
    ): (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        r#"SELECT
               COALESCE(SUM(request_count), 0),
               COALESCE(SUM(input_tokens), 0),
               COALESCE(SUM(output_tokens), 0),
               COALESCE(SUM(cache_creation_tokens), 0),
               COALESCE(SUM(cache_read_tokens), 0),
               COALESCE(SUM(cost_nanousd), 0)
           FROM usage_lifetime_totals
           WHERE (?1 IS NULL OR user_id = ?1)"#,
    )
    .bind(selected_user_id)
    .fetch_one(db)
    .await?;
    Ok(UsageTotals::from_parts(
        request_count,
        input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        cost_nanousd,
    ))
}

async fn load_window_totals(
    db: &SqlitePool,
    window: &WindowSpec,
    selected_user_id: Option<i64>,
) -> Result<UsageTotals, ClewdrError> {
    match window.source() {
        WindowSource::RequestLogs => {
            let (
                request_count,
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
                cost_nanousd,
            ): (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
                r#"SELECT
                       COUNT(*),
                       COALESCE(SUM(input_tokens), 0),
                       COALESCE(SUM(output_tokens), 0),
                       COALESCE(SUM(cache_creation_tokens), 0),
                       COALESCE(SUM(cache_read_tokens), 0),
                       COALESCE(SUM(cost_nanousd), 0)
                   FROM request_logs
                   WHERE usage_accounted = 1
                     AND started_at >= ?1
                     AND started_at < ?2
                     AND (?3 IS NULL OR user_id = ?3)"#,
            )
            .bind(window.start_utc.to_rfc3339())
            .bind(window.end_utc.to_rfc3339())
            .bind(selected_user_id)
            .fetch_one(db)
            .await?;
            Ok(UsageTotals::from_parts(
                request_count,
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
                cost_nanousd,
            ))
        }
        WindowSource::DailyRollups => {
            let (
                request_count,
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
                cost_nanousd,
            ): (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
                r#"SELECT
                       COALESCE(SUM(request_count), 0),
                       COALESCE(SUM(input_tokens), 0),
                       COALESCE(SUM(output_tokens), 0),
                       COALESCE(SUM(cache_creation_tokens), 0),
                       COALESCE(SUM(cache_read_tokens), 0),
                       COALESCE(SUM(cost_nanousd), 0)
                   FROM usage_daily_rollups
                   WHERE bucket_date_local >= ?1
                     AND bucket_date_local < ?2
                     AND (?3 IS NULL OR user_id = ?3)"#,
            )
            .bind(&window.start_local_day)
            .bind(&window.end_local_day_exclusive)
            .bind(selected_user_id)
            .fetch_one(db)
            .await?;
            Ok(UsageTotals::from_parts(
                request_count,
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
                cost_nanousd,
            ))
        }
    }
}

#[derive(sqlx::FromRow)]
struct ModelAggregateRow {
    model_key: String,
    request_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
    cost_nanousd: i64,
}

async fn load_distribution(
    db: &SqlitePool,
    window: &WindowSpec,
    selected_user_id: Option<i64>,
    metric: MetricKind,
) -> Result<Vec<DimensionItem>, ClewdrError> {
    // Distribution always groups by model_key, regardless of dimension.
    let rows: Vec<ModelAggregateRow> = match window.source() {
        WindowSource::RequestLogs => {
            let query = format!(
                r#"SELECT
                       model_key,
                       COUNT(*) AS request_count,
                       COALESCE(SUM(input_tokens), 0) AS input_tokens,
                       COALESCE(SUM(output_tokens), 0) AS output_tokens,
                       COALESCE(SUM(cache_creation_tokens), 0) AS cache_creation_tokens,
                       COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
                       COALESCE(SUM(cost_nanousd), 0) AS cost_nanousd
                   FROM request_logs
                   WHERE usage_accounted = 1
                     AND started_at >= ?1
                     AND started_at < ?2
                     AND (?3 IS NULL OR user_id = ?3)
                   GROUP BY model_key
                   ORDER BY {order}"#,
                order = metric.order_by_sql(),
            );
            sqlx::query_as(&query)
                .bind(window.start_utc.to_rfc3339())
                .bind(window.end_utc.to_rfc3339())
                .bind(selected_user_id)
                .fetch_all(db)
                .await?
        }
        WindowSource::DailyRollups => {
            let query = format!(
                r#"SELECT
                       model_key,
                       COALESCE(SUM(request_count), 0) AS request_count,
                       COALESCE(SUM(input_tokens), 0) AS input_tokens,
                       COALESCE(SUM(output_tokens), 0) AS output_tokens,
                       COALESCE(SUM(cache_creation_tokens), 0) AS cache_creation_tokens,
                       COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
                       COALESCE(SUM(cost_nanousd), 0) AS cost_nanousd
                   FROM usage_daily_rollups
                   WHERE bucket_date_local >= ?1
                     AND bucket_date_local < ?2
                     AND (?3 IS NULL OR user_id = ?3)
                   GROUP BY model_key
                   ORDER BY {order}"#,
                order = metric.order_by_sql(),
            );
            sqlx::query_as(&query)
                .bind(&window.start_local_day)
                .bind(&window.end_local_day_exclusive)
                .bind(selected_user_id)
                .fetch_all(db)
                .await?
        }
    };

    let mut items: Vec<DimensionItem> = rows
        .into_iter()
        .map(|row| DimensionItem {
            kind: "model".to_string(),
            user_id: None,
            label: row.model_key.clone(),
            model_key: Some(row.model_key),
            is_other_bucket: false,
            request_count: row.request_count,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cache_creation_tokens: row.cache_creation_tokens,
            cache_read_tokens: row.cache_read_tokens,
            total_tokens: row.input_tokens
                + row.output_tokens
                + row.cache_creation_tokens
                + row.cache_read_tokens,
            cost_nanousd: row.cost_nanousd,
        })
        .collect();

    Ok(roll_up_into_other(
        items.split_off(0),
        MODEL_DISTRIBUTION_TOP_N,
        metric,
    ))
}

/// If there are more than `top_n` items, keep the top N and synthesize an
/// "其他" aggregate carrying the rest. Frontend uses `is_other_bucket=true`
/// to disable drill-down on it.
fn roll_up_into_other(
    mut items: Vec<DimensionItem>,
    top_n: usize,
    metric: MetricKind,
) -> Vec<DimensionItem> {
    items.sort_by(|a, b| metric.cmp_items(a, b));
    if items.len() <= top_n {
        return items;
    }
    let mut rest = items.split_off(top_n);
    let mut other = DimensionItem {
        kind: rest[0].kind.clone(),
        user_id: None,
        model_key: None,
        label: "其他".to_string(),
        is_other_bucket: true,
        request_count: 0,
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        total_tokens: 0,
        cost_nanousd: 0,
    };
    for item in rest.drain(..) {
        other.request_count += item.request_count;
        other.input_tokens += item.input_tokens;
        other.output_tokens += item.output_tokens;
        other.cache_creation_tokens += item.cache_creation_tokens;
        other.cache_read_tokens += item.cache_read_tokens;
        other.total_tokens += item.total_tokens;
        other.cost_nanousd += item.cost_nanousd;
    }
    items.push(other);
    items
}

#[derive(sqlx::FromRow)]
struct UserAggregateRow {
    user_id: i64,
    username: String,
    request_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
    cost_nanousd: i64,
}

#[derive(sqlx::FromRow)]
struct UserBucketRow {
    user_id: i64,
    bucket: String,
    request_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
    cost_nanousd: i64,
}

#[derive(sqlx::FromRow)]
struct ModelBucketRow {
    model_key: String,
    bucket: String,
    request_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
    cost_nanousd: i64,
}

async fn load_ranking_and_series(
    db: &SqlitePool,
    window: &WindowSpec,
    selected_user_id: Option<i64>,
    metric: MetricKind,
    top_n: usize,
) -> Result<(Vec<DimensionItem>, Vec<DimensionSeries>), ClewdrError> {
    if let Some(uid) = selected_user_id {
        // Per-user model dimension.
        let (ranking, series) =
            load_model_ranking_and_series(db, window, uid, metric, top_n).await?;
        Ok((ranking, series))
    } else {
        // Cross-user dimension.
        let (ranking, series) = load_user_ranking_and_series(db, window, metric, top_n).await?;
        Ok((ranking, series))
    }
}

async fn load_user_ranking_and_series(
    db: &SqlitePool,
    window: &WindowSpec,
    metric: MetricKind,
    top_n: usize,
) -> Result<(Vec<DimensionItem>, Vec<DimensionSeries>), ClewdrError> {
    let ranking_rows: Vec<UserAggregateRow> = match window.source() {
        WindowSource::RequestLogs => {
            let query = format!(
                r#"SELECT
                       r.user_id AS user_id,
                       COALESCE(u.username, 'user#' || CAST(r.user_id AS TEXT)) AS username,
                       COUNT(*) AS request_count,
                       COALESCE(SUM(r.input_tokens), 0) AS input_tokens,
                       COALESCE(SUM(r.output_tokens), 0) AS output_tokens,
                       COALESCE(SUM(r.cache_creation_tokens), 0) AS cache_creation_tokens,
                       COALESCE(SUM(r.cache_read_tokens), 0) AS cache_read_tokens,
                       COALESCE(SUM(r.cost_nanousd), 0) AS cost_nanousd
                   FROM request_logs r
                   LEFT JOIN users u ON r.user_id = u.id
                   WHERE r.usage_accounted = 1
                     AND r.user_id IS NOT NULL
                     AND r.started_at >= ?1
                     AND r.started_at < ?2
                   GROUP BY r.user_id, username
                   ORDER BY {order}"#,
                order = metric.order_by_sql(),
            );
            sqlx::query_as(&query)
                .bind(window.start_utc.to_rfc3339())
                .bind(window.end_utc.to_rfc3339())
                .fetch_all(db)
                .await?
        }
        WindowSource::DailyRollups => {
            let query = format!(
                r#"SELECT
                       d.user_id AS user_id,
                       COALESCE(u.username, 'user#' || CAST(d.user_id AS TEXT)) AS username,
                       COALESCE(SUM(d.request_count), 0) AS request_count,
                       COALESCE(SUM(d.input_tokens), 0) AS input_tokens,
                       COALESCE(SUM(d.output_tokens), 0) AS output_tokens,
                       COALESCE(SUM(d.cache_creation_tokens), 0) AS cache_creation_tokens,
                       COALESCE(SUM(d.cache_read_tokens), 0) AS cache_read_tokens,
                       COALESCE(SUM(d.cost_nanousd), 0) AS cost_nanousd
                   FROM usage_daily_rollups d
                   LEFT JOIN users u ON d.user_id = u.id
                   WHERE d.bucket_date_local >= ?1
                     AND d.bucket_date_local < ?2
                   GROUP BY d.user_id, username
                   ORDER BY {order}"#,
                order = metric.order_by_sql(),
            );
            sqlx::query_as(&query)
                .bind(&window.start_local_day)
                .bind(&window.end_local_day_exclusive)
                .fetch_all(db)
                .await?
        }
    };

    let mut ranking: Vec<DimensionItem> = ranking_rows
        .iter()
        .map(|row| DimensionItem {
            kind: "user".to_string(),
            user_id: Some(row.user_id),
            model_key: None,
            label: row.username.clone(),
            is_other_bucket: false,
            request_count: row.request_count,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cache_creation_tokens: row.cache_creation_tokens,
            cache_read_tokens: row.cache_read_tokens,
            total_tokens: row.input_tokens
                + row.output_tokens
                + row.cache_creation_tokens
                + row.cache_read_tokens,
            cost_nanousd: row.cost_nanousd,
        })
        .collect();
    ranking = roll_up_into_other(ranking, top_n, metric);

    // Series picks its subjects from the *final* ranking (sans the
    // synthetic "其他" row) instead of re-running the SQL ORDER BY. The
    // raw SQL would re-tiebreak with whatever indexscan order it picks
    // for equal metric values, which can land on a different user than
    // the one shown in the ranking table.
    let top_user_ids: Vec<i64> = ranking
        .iter()
        .filter(|item| !item.is_other_bucket)
        .filter_map(|item| item.user_id)
        .collect();
    if top_user_ids.is_empty() {
        return Ok((ranking, Vec::new()));
    }

    let user_bucket_rows = load_user_bucket_rows(db, window, &top_user_ids).await?;
    let series = assemble_user_series(window, &top_user_ids, ranking_rows, user_bucket_rows);

    Ok((ranking, series))
}

async fn load_user_bucket_rows(
    db: &SqlitePool,
    window: &WindowSpec,
    user_ids: &[i64],
) -> Result<Vec<UserBucketRow>, ClewdrError> {
    if user_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = (0..user_ids.len())
        .map(|i| format!("?{}", i + 3))
        .collect::<Vec<_>>()
        .join(", ");
    let rows: Vec<UserBucketRow> = match window.source() {
        WindowSource::RequestLogs => {
            let query = format!(
                r#"SELECT
                       r.user_id AS user_id,
                       COALESCE(u.username, 'user#' || CAST(r.user_id AS TEXT)) AS username,
                       strftime('%Y-%m-%d %H:00', datetime(r.started_at, '+8 hours')) AS bucket,
                       COUNT(*) AS request_count,
                       COALESCE(SUM(r.input_tokens), 0) AS input_tokens,
                       COALESCE(SUM(r.output_tokens), 0) AS output_tokens,
                       COALESCE(SUM(r.cache_creation_tokens), 0) AS cache_creation_tokens,
                       COALESCE(SUM(r.cache_read_tokens), 0) AS cache_read_tokens,
                       COALESCE(SUM(r.cost_nanousd), 0) AS cost_nanousd
                   FROM request_logs r
                   LEFT JOIN users u ON r.user_id = u.id
                   WHERE r.usage_accounted = 1
                     AND r.started_at >= ?1
                     AND r.started_at < ?2
                     AND r.user_id IN ({placeholders})
                   GROUP BY r.user_id, username, bucket"#,
            );
            let mut q = sqlx::query_as::<_, UserBucketRow>(&query)
                .bind(window.start_utc.to_rfc3339())
                .bind(window.end_utc.to_rfc3339());
            for uid in user_ids {
                q = q.bind(*uid);
            }
            q.fetch_all(db).await?
        }
        WindowSource::DailyRollups => {
            let query = format!(
                r#"SELECT
                       d.user_id AS user_id,
                       COALESCE(u.username, 'user#' || CAST(d.user_id AS TEXT)) AS username,
                       d.bucket_date_local AS bucket,
                       COALESCE(SUM(d.request_count), 0) AS request_count,
                       COALESCE(SUM(d.input_tokens), 0) AS input_tokens,
                       COALESCE(SUM(d.output_tokens), 0) AS output_tokens,
                       COALESCE(SUM(d.cache_creation_tokens), 0) AS cache_creation_tokens,
                       COALESCE(SUM(d.cache_read_tokens), 0) AS cache_read_tokens,
                       COALESCE(SUM(d.cost_nanousd), 0) AS cost_nanousd
                   FROM usage_daily_rollups d
                   LEFT JOIN users u ON d.user_id = u.id
                   WHERE d.bucket_date_local >= ?1
                     AND d.bucket_date_local < ?2
                     AND d.user_id IN ({placeholders})
                   GROUP BY d.user_id, username, d.bucket_date_local"#,
            );
            let mut q = sqlx::query_as::<_, UserBucketRow>(&query)
                .bind(&window.start_local_day)
                .bind(&window.end_local_day_exclusive);
            for uid in user_ids {
                q = q.bind(*uid);
            }
            q.fetch_all(db).await?
        }
    };
    Ok(rows)
}

fn assemble_user_series(
    window: &WindowSpec,
    top_user_ids: &[i64],
    rankings: Vec<UserAggregateRow>,
    bucket_rows: Vec<UserBucketRow>,
) -> Vec<DimensionSeries> {
    let mut by_user: BTreeMap<i64, Vec<UserBucketRow>> = BTreeMap::new();
    for row in bucket_rows {
        by_user.entry(row.user_id).or_default().push(row);
    }
    let name_lookup: BTreeMap<i64, String> = rankings
        .iter()
        .map(|r| (r.user_id, r.username.clone()))
        .collect();

    top_user_ids
        .iter()
        .map(|uid| {
            let username = name_lookup.get(uid).cloned().unwrap_or_default();
            let rows = by_user.remove(uid).unwrap_or_default();
            let points = align_points(&window.buckets, window.partial_bucket_idx, rows);
            DimensionSeries {
                kind: "user".to_string(),
                user_id: Some(*uid),
                model_key: None,
                label: username,
                points,
            }
        })
        .collect()
}

fn align_points(
    buckets: &[String],
    partial_idx: usize,
    rows: Vec<UserBucketRow>,
) -> Vec<DimensionSeriesPoint> {
    let mut by_bucket: BTreeMap<String, UserBucketRow> = BTreeMap::new();
    for row in rows {
        by_bucket.insert(row.bucket.clone(), row);
    }
    buckets
        .iter()
        .enumerate()
        .map(|(idx, key)| {
            let row = by_bucket.remove(key);
            let (req, input, out, cc, cr, cost) = match row {
                Some(r) => (
                    r.request_count,
                    r.input_tokens,
                    r.output_tokens,
                    r.cache_creation_tokens,
                    r.cache_read_tokens,
                    r.cost_nanousd,
                ),
                None => (0, 0, 0, 0, 0, 0),
            };
            DimensionSeriesPoint {
                bucket: key.clone(),
                partial: idx == partial_idx,
                request_count: req,
                total_tokens: input + out + cc + cr,
                cost_nanousd: cost,
            }
        })
        .collect()
}

async fn load_model_ranking_and_series(
    db: &SqlitePool,
    window: &WindowSpec,
    user_id: i64,
    metric: MetricKind,
    top_n: usize,
) -> Result<(Vec<DimensionItem>, Vec<DimensionSeries>), ClewdrError> {
    let ranking_rows: Vec<ModelAggregateRow> = match window.source() {
        WindowSource::RequestLogs => {
            let query = format!(
                r#"SELECT
                       model_key,
                       COUNT(*) AS request_count,
                       COALESCE(SUM(input_tokens), 0) AS input_tokens,
                       COALESCE(SUM(output_tokens), 0) AS output_tokens,
                       COALESCE(SUM(cache_creation_tokens), 0) AS cache_creation_tokens,
                       COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
                       COALESCE(SUM(cost_nanousd), 0) AS cost_nanousd
                   FROM request_logs
                   WHERE usage_accounted = 1
                     AND user_id = ?1
                     AND started_at >= ?2
                     AND started_at < ?3
                   GROUP BY model_key
                   ORDER BY {order}"#,
                order = metric.order_by_sql(),
            );
            sqlx::query_as(&query)
                .bind(user_id)
                .bind(window.start_utc.to_rfc3339())
                .bind(window.end_utc.to_rfc3339())
                .fetch_all(db)
                .await?
        }
        WindowSource::DailyRollups => {
            let query = format!(
                r#"SELECT
                       model_key,
                       COALESCE(SUM(request_count), 0) AS request_count,
                       COALESCE(SUM(input_tokens), 0) AS input_tokens,
                       COALESCE(SUM(output_tokens), 0) AS output_tokens,
                       COALESCE(SUM(cache_creation_tokens), 0) AS cache_creation_tokens,
                       COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
                       COALESCE(SUM(cost_nanousd), 0) AS cost_nanousd
                   FROM usage_daily_rollups
                   WHERE user_id = ?1
                     AND bucket_date_local >= ?2
                     AND bucket_date_local < ?3
                   GROUP BY model_key
                   ORDER BY {order}"#,
                order = metric.order_by_sql(),
            );
            sqlx::query_as(&query)
                .bind(user_id)
                .bind(&window.start_local_day)
                .bind(&window.end_local_day_exclusive)
                .fetch_all(db)
                .await?
        }
    };

    let mut ranking: Vec<DimensionItem> = ranking_rows
        .iter()
        .map(|row| DimensionItem {
            kind: "model".to_string(),
            user_id: Some(user_id),
            label: row.model_key.clone(),
            model_key: Some(row.model_key.clone()),
            is_other_bucket: false,
            request_count: row.request_count,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cache_creation_tokens: row.cache_creation_tokens,
            cache_read_tokens: row.cache_read_tokens,
            total_tokens: row.input_tokens
                + row.output_tokens
                + row.cache_creation_tokens
                + row.cache_read_tokens,
            cost_nanousd: row.cost_nanousd,
        })
        .collect();
    ranking = roll_up_into_other(ranking, top_n, metric);

    // Mirror the user-dimension fix: derive series subjects from the
    // final ranking so a metric tie can never desync the table and the
    // chart.
    let top_models: Vec<String> = ranking
        .iter()
        .filter(|item| !item.is_other_bucket)
        .filter_map(|item| item.model_key.clone())
        .collect();
    if top_models.is_empty() {
        return Ok((ranking, Vec::new()));
    }

    let model_bucket_rows = load_model_bucket_rows(db, window, user_id, &top_models).await?;
    let series = assemble_model_series(window, user_id, &top_models, model_bucket_rows);

    Ok((ranking, series))
}

async fn load_model_bucket_rows(
    db: &SqlitePool,
    window: &WindowSpec,
    user_id: i64,
    models: &[String],
) -> Result<Vec<ModelBucketRow>, ClewdrError> {
    if models.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = (0..models.len())
        .map(|i| format!("?{}", i + 4))
        .collect::<Vec<_>>()
        .join(", ");
    let rows: Vec<ModelBucketRow> = match window.source() {
        WindowSource::RequestLogs => {
            let query = format!(
                r#"SELECT
                       model_key,
                       strftime('%Y-%m-%d %H:00', datetime(started_at, '+8 hours')) AS bucket,
                       COUNT(*) AS request_count,
                       COALESCE(SUM(input_tokens), 0) AS input_tokens,
                       COALESCE(SUM(output_tokens), 0) AS output_tokens,
                       COALESCE(SUM(cache_creation_tokens), 0) AS cache_creation_tokens,
                       COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
                       COALESCE(SUM(cost_nanousd), 0) AS cost_nanousd
                   FROM request_logs
                   WHERE usage_accounted = 1
                     AND user_id = ?1
                     AND started_at >= ?2
                     AND started_at < ?3
                     AND model_key IN ({placeholders})
                   GROUP BY model_key, bucket"#,
            );
            let mut q = sqlx::query_as::<_, ModelBucketRow>(&query)
                .bind(user_id)
                .bind(window.start_utc.to_rfc3339())
                .bind(window.end_utc.to_rfc3339());
            for m in models {
                q = q.bind(m);
            }
            q.fetch_all(db).await?
        }
        WindowSource::DailyRollups => {
            let query = format!(
                r#"SELECT
                       model_key,
                       bucket_date_local AS bucket,
                       COALESCE(SUM(request_count), 0) AS request_count,
                       COALESCE(SUM(input_tokens), 0) AS input_tokens,
                       COALESCE(SUM(output_tokens), 0) AS output_tokens,
                       COALESCE(SUM(cache_creation_tokens), 0) AS cache_creation_tokens,
                       COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
                       COALESCE(SUM(cost_nanousd), 0) AS cost_nanousd
                   FROM usage_daily_rollups
                   WHERE user_id = ?1
                     AND bucket_date_local >= ?2
                     AND bucket_date_local < ?3
                     AND model_key IN ({placeholders})
                   GROUP BY model_key, bucket_date_local"#,
            );
            let mut q = sqlx::query_as::<_, ModelBucketRow>(&query)
                .bind(user_id)
                .bind(&window.start_local_day)
                .bind(&window.end_local_day_exclusive);
            for m in models {
                q = q.bind(m);
            }
            q.fetch_all(db).await?
        }
    };
    Ok(rows)
}

fn assemble_model_series(
    window: &WindowSpec,
    user_id: i64,
    top_models: &[String],
    bucket_rows: Vec<ModelBucketRow>,
) -> Vec<DimensionSeries> {
    let mut by_model: BTreeMap<String, Vec<ModelBucketRow>> = BTreeMap::new();
    for row in bucket_rows {
        by_model.entry(row.model_key.clone()).or_default().push(row);
    }
    top_models
        .iter()
        .map(|model| {
            let rows = by_model.remove(model).unwrap_or_default();
            let points = align_model_points(&window.buckets, window.partial_bucket_idx, rows);
            DimensionSeries {
                kind: "model".to_string(),
                user_id: Some(user_id),
                model_key: Some(model.clone()),
                label: model.clone(),
                points,
            }
        })
        .collect()
}

fn align_model_points(
    buckets: &[String],
    partial_idx: usize,
    rows: Vec<ModelBucketRow>,
) -> Vec<DimensionSeriesPoint> {
    let mut by_bucket: BTreeMap<String, ModelBucketRow> = BTreeMap::new();
    for row in rows {
        by_bucket.insert(row.bucket.clone(), row);
    }
    buckets
        .iter()
        .enumerate()
        .map(|(idx, key)| {
            let row = by_bucket.remove(key);
            let (req, input, out, cc, cr, cost) = match row {
                Some(r) => (
                    r.request_count,
                    r.input_tokens,
                    r.output_tokens,
                    r.cache_creation_tokens,
                    r.cache_read_tokens,
                    r.cost_nanousd,
                ),
                None => (0, 0, 0, 0, 0, 0),
            };
            DimensionSeriesPoint {
                bucket: key.clone(),
                partial: idx == partial_idx,
                request_count: req,
                total_tokens: input + out + cc + cr,
                cost_nanousd: cost,
            }
        })
        .collect()
}

// ---- Coverage + comparison ----------------------------------------------

async fn build_coverage(
    db: &SqlitePool,
    window: &WindowSpec,
    previous_window: &WindowSpec,
    retention_days: i64,
    state: &DailyRollupState,
) -> Result<CoverageInfo, ClewdrError> {
    match window.preset {
        RangePreset::Last24Hours => {
            // 24h reads request_logs. The earliest started_at we could
            // possibly surface is max(now-retention, MIN(usage_accounted=1
            // row's started_at)). The MIN clamp guards against a fresh
            // deploy whose log history is shorter than retention_days.
            let min_started: Option<String> = sqlx::query_as(
                "SELECT MIN(started_at) FROM request_logs WHERE usage_accounted = 1",
            )
            .fetch_one(db)
            .await
            .map(|(v,): (Option<String>,)| v)?;
            let retention_cutoff = Utc::now() - chrono::Duration::days(retention_days);
            let logs_available_from =
                effective_logs_available_from(min_started.as_deref(), retention_cutoff);
            let complete = logs_available_from <= window.start_utc;
            let comparison_complete = logs_available_from <= previous_window.start_utc;
            Ok(CoverageInfo {
                complete,
                comparison_complete,
                writes_started_at: state.writes_started_at.clone(),
                backfill_available_from: state.backfill_available_from.clone(),
                retention_days,
                logs_available_from: Some(logs_available_from.to_rfc3339()),
            })
        }
        RangePreset::Last7Days | RangePreset::Last30Days => {
            // 7d / 30d read usage_daily_rollups. The data source covers
            // the window iff `writes_started_at` lands before the window
            // start. We do NOT include `backfill_available_from` in the
            // completeness check: per plan §6.6 it is a UI hint only.
            let coverage_start = state
                .writes_started_at
                .as_deref()
                .and_then(parse_timestamp_flexible);
            let complete = match coverage_start {
                Some(start) => start <= window.start_utc,
                // No state row → assume not complete; this is the
                // post-restore-before-first-write case.
                None => false,
            };
            let comparison_complete = match coverage_start {
                Some(start) => start <= previous_window.start_utc,
                None => false,
            };
            Ok(CoverageInfo {
                complete,
                comparison_complete,
                writes_started_at: state.writes_started_at.clone(),
                backfill_available_from: state.backfill_available_from.clone(),
                retention_days,
                logs_available_from: None,
            })
        }
    }
}

fn effective_logs_available_from(
    min_started: Option<&str>,
    retention_cutoff: DateTime<Utc>,
) -> DateTime<Utc> {
    match min_started.and_then(parse_timestamp_flexible) {
        Some(min) => min.max(retention_cutoff),
        None => retention_cutoff,
    }
}

/// Parse a timestamp produced either by sqlx (RFC3339 with `T` and a
/// trailing `Z`) or by SQLite's `CURRENT_TIMESTAMP` (`YYYY-MM-DD
/// HH:MM:SS` in UTC, no separator, no offset).
///
/// PR-A seeds `usage_daily_rollup_state.writes_started_at` via
/// `CURRENT_TIMESTAMP`, so a strict RFC3339 parse here would make the
/// 7d / 30d coverage check fail on every normal deployment and the page
/// would permanently report "数据积累中". Tests cover both shapes.
fn parse_timestamp_flexible(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    let s = s.trim();
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .ok()
        .map(|naive| Utc.from_utc_datetime(&naive))
}

fn build_comparison(
    window: &WindowSpec,
    window_totals: &UsageTotals,
    previous_totals: &UsageTotals,
    coverage: &CoverageInfo,
) -> ComparisonInfo {
    let expected = window.preset.expected_bucket_count();
    // Closed buckets: everything before the partial trailing bucket.
    let current_bucket_count = window.partial_bucket_idx.min(window.buckets.len()) as i64;

    let complete = coverage.comparison_complete && current_bucket_count == expected - 1;

    let ratio = |cur: i64, prev: i64| -> Option<f64> {
        if prev == 0 {
            return None;
        }
        Some(cur as f64 / prev as f64)
    };

    ComparisonInfo {
        complete,
        current_bucket_count,
        expected_bucket_count: expected,
        window_label: window_label(window),
        cost_ratio: ratio(window_totals.cost_nanousd, previous_totals.cost_nanousd),
        total_tokens_ratio: ratio(window_totals.total_tokens, previous_totals.total_tokens),
        request_count_ratio: ratio(window_totals.request_count, previous_totals.request_count),
    }
}

fn window_label(window: &WindowSpec) -> String {
    let shanghai = shanghai_offset();
    let start_local = window.start_utc.with_timezone(&shanghai);
    let end_local = window.end_utc.with_timezone(&shanghai);
    match window.preset {
        RangePreset::Last24Hours => format!(
            "{} → {} (UTC+8)",
            start_local.format("%m-%d %H:00"),
            end_local.format("%m-%d %H:00"),
        ),
        RangePreset::Last7Days | RangePreset::Last30Days => {
            // Inclusive day range: end is the day AFTER the window ends.
            let last_included = end_local.naive_local() - chrono::Duration::days(1);
            format!(
                "{} → {} (UTC+8)",
                start_local.format("%Y-%m-%d"),
                last_included.format("%Y-%m-%d"),
            )
        }
    }
}

// ---- Legacy projection --------------------------------------------------

async fn legacy_projection(
    db: &SqlitePool,
    window: &WindowSpec,
    selected_user_id: Option<i64>,
    metric: MetricKind,
    top_n: usize,
) -> Result<
    (
        Vec<UserAggregate>,
        Vec<UserSeries>,
        Vec<ModelDistributionItem>,
    ),
    ClewdrError,
> {
    // Legacy ranking + series always stay on the user dimension regardless
    // of selected_user_id (matching pre-PR-B behavior — when a single user
    // is filtered, the legacy table just shows one row). PR-C deletes
    // these fields.
    let window_for_user_filter = window;
    let user_rows: Vec<UserAggregateRow> = match window.source() {
        WindowSource::RequestLogs => {
            let query = format!(
                r#"SELECT
                       r.user_id AS user_id,
                       COALESCE(u.username, 'user#' || CAST(r.user_id AS TEXT)) AS username,
                       COUNT(*) AS request_count,
                       COALESCE(SUM(r.input_tokens), 0) AS input_tokens,
                       COALESCE(SUM(r.output_tokens), 0) AS output_tokens,
                       COALESCE(SUM(r.cache_creation_tokens), 0) AS cache_creation_tokens,
                       COALESCE(SUM(r.cache_read_tokens), 0) AS cache_read_tokens,
                       COALESCE(SUM(r.cost_nanousd), 0) AS cost_nanousd
                   FROM request_logs r
                   LEFT JOIN users u ON r.user_id = u.id
                   WHERE r.usage_accounted = 1
                     AND r.user_id IS NOT NULL
                     AND r.started_at >= ?1
                     AND r.started_at < ?2
                     AND (?3 IS NULL OR r.user_id = ?3)
                   GROUP BY r.user_id, username
                   ORDER BY {order}
                   LIMIT {limit}"#,
                order = metric.order_by_sql(),
                limit = top_n,
            );
            sqlx::query_as(&query)
                .bind(window_for_user_filter.start_utc.to_rfc3339())
                .bind(window_for_user_filter.end_utc.to_rfc3339())
                .bind(selected_user_id)
                .fetch_all(db)
                .await?
        }
        WindowSource::DailyRollups => {
            let query = format!(
                r#"SELECT
                       d.user_id AS user_id,
                       COALESCE(u.username, 'user#' || CAST(d.user_id AS TEXT)) AS username,
                       COALESCE(SUM(d.request_count), 0) AS request_count,
                       COALESCE(SUM(d.input_tokens), 0) AS input_tokens,
                       COALESCE(SUM(d.output_tokens), 0) AS output_tokens,
                       COALESCE(SUM(d.cache_creation_tokens), 0) AS cache_creation_tokens,
                       COALESCE(SUM(d.cache_read_tokens), 0) AS cache_read_tokens,
                       COALESCE(SUM(d.cost_nanousd), 0) AS cost_nanousd
                   FROM usage_daily_rollups d
                   LEFT JOIN users u ON d.user_id = u.id
                   WHERE d.bucket_date_local >= ?1
                     AND d.bucket_date_local < ?2
                     AND (?3 IS NULL OR d.user_id = ?3)
                   GROUP BY d.user_id, username
                   ORDER BY {order}
                   LIMIT {limit}"#,
                order = metric.order_by_sql(),
                limit = top_n,
            );
            sqlx::query_as(&query)
                .bind(&window_for_user_filter.start_local_day)
                .bind(&window_for_user_filter.end_local_day_exclusive)
                .bind(selected_user_id)
                .fetch_all(db)
                .await?
        }
    };

    let top_user_ids: Vec<i64> = user_rows.iter().map(|r| r.user_id).collect();
    let bucket_rows = load_user_bucket_rows(db, window, &top_user_ids).await?;

    let top_users: Vec<UserAggregate> = user_rows
        .iter()
        .map(|row| UserAggregate {
            user_id: row.user_id,
            username: row.username.clone(),
            request_count: row.request_count,
            total_tokens: row.input_tokens
                + row.output_tokens
                + row.cache_creation_tokens
                + row.cache_read_tokens,
            cost_nanousd: row.cost_nanousd,
        })
        .collect();

    let mut by_user: BTreeMap<i64, Vec<UserBucketRow>> = BTreeMap::new();
    for row in bucket_rows {
        by_user.entry(row.user_id).or_default().push(row);
    }
    let user_series: Vec<UserSeries> = user_rows
        .iter()
        .map(|row| {
            let rows = by_user.remove(&row.user_id).unwrap_or_default();
            let mut by_bucket: BTreeMap<String, UserBucketRow> = BTreeMap::new();
            for r in rows {
                by_bucket.insert(r.bucket.clone(), r);
            }
            let points = window
                .buckets
                .iter()
                .map(|key| {
                    let r = by_bucket.remove(key);
                    let (req, input, out, cc, cr, cost) = match r {
                        Some(r) => (
                            r.request_count,
                            r.input_tokens,
                            r.output_tokens,
                            r.cache_creation_tokens,
                            r.cache_read_tokens,
                            r.cost_nanousd,
                        ),
                        None => (0, 0, 0, 0, 0, 0),
                    };
                    UserSeriesPoint {
                        bucket: key.clone(),
                        request_count: req,
                        total_tokens: input + out + cc + cr,
                        cost_nanousd: cost,
                    }
                })
                .collect();
            UserSeries {
                user_id: row.user_id,
                username: row.username.clone(),
                points,
            }
        })
        .collect();

    // Legacy model_distribution: keep the original shape but feed from the
    // new data source. Top 8 with no "其他" row.
    let model_dist_rows: Vec<ModelAggregateRow> = match window.source() {
        WindowSource::RequestLogs => {
            let query = format!(
                r#"SELECT
                       model_key,
                       COUNT(*) AS request_count,
                       COALESCE(SUM(input_tokens), 0) AS input_tokens,
                       COALESCE(SUM(output_tokens), 0) AS output_tokens,
                       COALESCE(SUM(cache_creation_tokens), 0) AS cache_creation_tokens,
                       COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
                       COALESCE(SUM(cost_nanousd), 0) AS cost_nanousd
                   FROM request_logs
                   WHERE usage_accounted = 1
                     AND started_at >= ?1
                     AND started_at < ?2
                     AND (?3 IS NULL OR user_id = ?3)
                   GROUP BY model_key
                   ORDER BY {order}
                   LIMIT 8"#,
                order = metric.order_by_sql(),
            );
            sqlx::query_as(&query)
                .bind(window.start_utc.to_rfc3339())
                .bind(window.end_utc.to_rfc3339())
                .bind(selected_user_id)
                .fetch_all(db)
                .await?
        }
        WindowSource::DailyRollups => {
            let query = format!(
                r#"SELECT
                       model_key,
                       COALESCE(SUM(request_count), 0) AS request_count,
                       COALESCE(SUM(input_tokens), 0) AS input_tokens,
                       COALESCE(SUM(output_tokens), 0) AS output_tokens,
                       COALESCE(SUM(cache_creation_tokens), 0) AS cache_creation_tokens,
                       COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
                       COALESCE(SUM(cost_nanousd), 0) AS cost_nanousd
                   FROM usage_daily_rollups
                   WHERE bucket_date_local >= ?1
                     AND bucket_date_local < ?2
                     AND (?3 IS NULL OR user_id = ?3)
                   GROUP BY model_key
                   ORDER BY {order}
                   LIMIT 8"#,
                order = metric.order_by_sql(),
            );
            sqlx::query_as(&query)
                .bind(&window.start_local_day)
                .bind(&window.end_local_day_exclusive)
                .bind(selected_user_id)
                .fetch_all(db)
                .await?
        }
    };
    let model_distribution: Vec<ModelDistributionItem> = model_dist_rows
        .into_iter()
        .map(|row| ModelDistributionItem {
            model: row.model_key,
            request_count: row.request_count,
            total_tokens: row.input_tokens
                + row.output_tokens
                + row.cache_creation_tokens
                + row.cache_read_tokens,
            cost_nanousd: row.cost_nanousd,
        })
        .collect();

    Ok((top_users, user_series, model_distribution))
}

#[cfg(test)]
mod tests;
