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
const SHANGHAI_OFFSET_SECONDS: i32 = 8 * 60 * 60;

#[derive(Deserialize)]
pub struct OpsUsageParams {
    pub range: Option<String>,
    pub top_users: Option<usize>,
    pub user_id: Option<i64>,
}

#[derive(Serialize)]
pub struct OpsUsageResponse {
    pub range: String,
    pub bucket_unit: String,
    pub selected_user_id: Option<i64>,
    pub retention_days: i64,
    pub coverage_limited: bool,
    pub window_started_at: String,
    pub window_ended_at: String,
    pub buckets: Vec<String>,
    pub totals: UsageTotals,
    pub model_distribution: Vec<ModelDistributionItem>,
    pub top_users: Vec<UserAggregate>,
    pub user_series: Vec<UserSeries>,
}

#[derive(Serialize)]
pub struct UsageTotals {
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    pub cost_nanousd: i64,
}

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

#[derive(sqlx::FromRow)]
struct ModelDistributionRow {
    model: String,
    request_count: i64,
    total_tokens: i64,
    cost_nanousd: i64,
}

#[derive(sqlx::FromRow)]
struct UserBucketRow {
    user_id: i64,
    username: String,
    bucket: String,
    request_count: i64,
    total_tokens: i64,
    cost_nanousd: i64,
}

struct UserAccumulator {
    username: String,
    request_count: i64,
    total_tokens: i64,
    cost_nanousd: i64,
    points: Vec<UserSeriesPoint>,
}

struct WindowSpec {
    range: &'static str,
    bucket_unit: &'static str,
    window_days: i64,
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
    buckets: Vec<String>,
}

#[derive(Clone, Copy)]
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

    fn build_window(self, now: DateTime<Utc>) -> WindowSpec {
        let shanghai = FixedOffset::east_opt(SHANGHAI_OFFSET_SECONDS).expect("valid +08:00");
        let now_local = now.with_timezone(&shanghai);

        match self {
            Self::Last24Hours => {
                let current_hour = now_local
                    .date_naive()
                    .and_hms_opt(now_local.hour(), 0, 0)
                    .expect("valid current hour");
                let end_local = current_hour + chrono::Duration::hours(1);
                let start_local = end_local - chrono::Duration::hours(24);
                WindowSpec {
                    range: "24h",
                    bucket_unit: "hour",
                    window_days: 1,
                    start_utc: shanghai
                        .from_local_datetime(&start_local)
                        .single()
                        .expect("fixed offset local start")
                        .with_timezone(&Utc),
                    end_utc: shanghai
                        .from_local_datetime(&end_local)
                        .single()
                        .expect("fixed offset local end")
                        .with_timezone(&Utc),
                    buckets: build_hour_buckets(start_local, end_local),
                }
            }
            Self::Last7Days | Self::Last30Days => {
                let window_days = if matches!(self, Self::Last7Days) {
                    7
                } else {
                    30
                };
                let tomorrow = now_local
                    .date_naive()
                    .succ_opt()
                    .expect("valid next day")
                    .and_hms_opt(0, 0, 0)
                    .expect("valid midnight");
                let start_local = tomorrow - chrono::Duration::days(window_days);
                WindowSpec {
                    range: if matches!(self, Self::Last7Days) {
                        "7d"
                    } else {
                        "30d"
                    },
                    bucket_unit: "day",
                    window_days,
                    start_utc: shanghai
                        .from_local_datetime(&start_local)
                        .single()
                        .expect("fixed offset local start")
                        .with_timezone(&Utc),
                    end_utc: shanghai
                        .from_local_datetime(&tomorrow)
                        .single()
                        .expect("fixed offset local end")
                        .with_timezone(&Utc),
                    buckets: build_day_buckets(start_local, tomorrow),
                }
            }
        }
    }

    fn bucket_sql(self) -> &'static str {
        match self {
            Self::Last24Hours => "strftime('%Y-%m-%d %H:00', datetime(r.started_at, '+8 hours'))",
            Self::Last7Days | Self::Last30Days => {
                "strftime('%Y-%m-%d', datetime(r.started_at, '+8 hours'))"
            }
        }
    }
}

pub async fn usage(
    State(db): State<SqlitePool>,
    Query(params): Query<OpsUsageParams>,
) -> Result<Json<OpsUsageResponse>, ClewdrError> {
    let now = Utc::now();
    let preset = RangePreset::from_query(params.range.as_deref());
    let window = preset.build_window(now);
    let top_users_limit = params
        .top_users
        .unwrap_or(DEFAULT_TOP_USERS)
        .clamp(1, MAX_TOP_USERS);
    let selected_user_id = params.user_id;
    let retention_days = read_retention_days(&db).await?;

    let totals = load_lifetime_totals(&db, selected_user_id).await?;
    let model_distribution = load_model_distribution(&db, &window, selected_user_id).await?;
    let (top_users, user_series) =
        load_user_series(&db, preset, &window, top_users_limit, selected_user_id).await?;

    Ok(Json(OpsUsageResponse {
        range: window.range.to_string(),
        bucket_unit: window.bucket_unit.to_string(),
        selected_user_id,
        retention_days,
        coverage_limited: retention_days < window.window_days,
        window_started_at: window.start_utc.to_rfc3339(),
        window_ended_at: window.end_utc.to_rfc3339(),
        buckets: window.buckets,
        totals,
        model_distribution,
        top_users,
        user_series,
    }))
}

async fn read_retention_days(db: &SqlitePool) -> Result<i64, ClewdrError> {
    Ok(get_setting(db, "log_retention_days")
        .await?
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(DEFAULT_RETENTION_DAYS))
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

    Ok(UsageTotals {
        request_count,
        input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        total_tokens: input_tokens + output_tokens + cache_creation_tokens + cache_read_tokens,
        cost_nanousd,
    })
}

async fn load_model_distribution(
    db: &SqlitePool,
    window: &WindowSpec,
    selected_user_id: Option<i64>,
) -> Result<Vec<ModelDistributionItem>, ClewdrError> {
    let query = format!(
        r#"SELECT
               COALESCE(NULLIF(r.model_normalized, ''), NULLIF(r.model_raw, ''), 'unknown') AS model,
               COUNT(*) AS request_count,
               COALESCE(SUM(
                   COALESCE(r.input_tokens, 0) +
                   COALESCE(r.output_tokens, 0) +
                   COALESCE(r.cache_creation_tokens, 0) +
                   COALESCE(r.cache_read_tokens, 0)
               ), 0) AS total_tokens,
               COALESCE(SUM(r.cost_nanousd), 0) AS cost_nanousd
           FROM request_logs r
           WHERE r.request_type = 'messages'
             AND r.status = 'ok'
             AND r.started_at >= ?1
             AND r.started_at < ?2
             AND (?3 IS NULL OR r.user_id = ?3)
           GROUP BY model
           ORDER BY cost_nanousd DESC, request_count DESC
           LIMIT 8"#,
    );

    let rows: Vec<ModelDistributionRow> = sqlx::query_as(&query)
        .bind(window.start_utc.to_rfc3339())
        .bind(window.end_utc.to_rfc3339())
        .bind(selected_user_id)
        .fetch_all(db)
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| ModelDistributionItem {
            model: row.model,
            request_count: row.request_count,
            total_tokens: row.total_tokens,
            cost_nanousd: row.cost_nanousd,
        })
        .collect())
}

async fn load_user_series(
    db: &SqlitePool,
    preset: RangePreset,
    window: &WindowSpec,
    top_users_limit: usize,
    selected_user_id: Option<i64>,
) -> Result<(Vec<UserAggregate>, Vec<UserSeries>), ClewdrError> {
    let query = format!(
        r#"SELECT
               r.user_id AS user_id,
               COALESCE(u.username, 'user#' || CAST(r.user_id AS TEXT)) AS username,
               {bucket} AS bucket,
               COUNT(*) AS request_count,
               COALESCE(SUM(
                   COALESCE(r.input_tokens, 0) +
                   COALESCE(r.output_tokens, 0) +
                   COALESCE(r.cache_creation_tokens, 0) +
                   COALESCE(r.cache_read_tokens, 0)
               ), 0) AS total_tokens,
               COALESCE(SUM(r.cost_nanousd), 0) AS cost_nanousd
           FROM request_logs r
           LEFT JOIN users u ON r.user_id = u.id
           WHERE r.request_type = 'messages'
             AND r.status = 'ok'
             AND r.user_id IS NOT NULL
             AND r.started_at >= ?1
             AND r.started_at < ?2
             AND (?3 IS NULL OR r.user_id = ?3)
           GROUP BY r.user_id, username, bucket
           ORDER BY bucket ASC, cost_nanousd DESC, request_count DESC"#,
        bucket = preset.bucket_sql(),
    );

    let rows: Vec<UserBucketRow> = sqlx::query_as(&query)
        .bind(window.start_utc.to_rfc3339())
        .bind(window.end_utc.to_rfc3339())
        .bind(selected_user_id)
        .fetch_all(db)
        .await?;

    let mut users = BTreeMap::<i64, UserAccumulator>::new();
    for row in rows {
        let user = users.entry(row.user_id).or_insert_with(|| UserAccumulator {
            username: row.username.clone(),
            request_count: 0,
            total_tokens: 0,
            cost_nanousd: 0,
            points: Vec::new(),
        });
        user.request_count += row.request_count;
        user.total_tokens += row.total_tokens;
        user.cost_nanousd += row.cost_nanousd;
        user.points.push(UserSeriesPoint {
            bucket: row.bucket,
            request_count: row.request_count,
            total_tokens: row.total_tokens,
            cost_nanousd: row.cost_nanousd,
        });
    }

    let mut ranked: Vec<(i64, UserAccumulator)> = users.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.cost_nanousd
            .cmp(&a.1.cost_nanousd)
            .then_with(|| b.1.total_tokens.cmp(&a.1.total_tokens))
            .then_with(|| b.1.request_count.cmp(&a.1.request_count))
            .then_with(|| a.1.username.cmp(&b.1.username))
    });
    ranked.truncate(top_users_limit);

    let top_users = ranked
        .iter()
        .map(|(user_id, user)| UserAggregate {
            user_id: *user_id,
            username: user.username.clone(),
            request_count: user.request_count,
            total_tokens: user.total_tokens,
            cost_nanousd: user.cost_nanousd,
        })
        .collect();

    let user_series = ranked
        .into_iter()
        .map(|(user_id, user)| UserSeries {
            user_id,
            username: user.username,
            points: user.points,
        })
        .collect();

    Ok((top_users, user_series))
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
