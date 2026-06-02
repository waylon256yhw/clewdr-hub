//! Tests for the Ops API.
//!
//! Coverage focus:
//! - 24h reads request_logs WHERE usage_accounted=1 (not status='ok')
//! - 7d / 30d read usage_daily_rollups (so retention pruning is invisible)
//! - window_totals / distribution / ranking / series share a data source
//!   for any given range
//! - metric=tokens|requests reorders distribution and ranking
//! - selected user_id flips ranking + series to model dimension
//! - is_other_bucket synthesised once Top-N is exceeded
//! - 24h coverage clamp uses max(now-retention, MIN(started_at))
//! - daily 7d coverage is gated by writes_started_at

use std::path::Path;

use axum::{
    Json,
    extract::{Query, State},
};
use chrono::{Duration, Utc};
use sqlx::SqlitePool;

use super::*;

async fn fresh_pool() -> SqlitePool {
    let pool = crate::db::init_pool(Path::new(":memory:"))
        .await
        .expect("init_pool");
    crate::db::seed_admin(&pool).await.expect("seed_admin");
    pool
}

async fn insert_user(pool: &SqlitePool, id: i64, name: &str) {
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, role, policy_id)
         VALUES (?1, ?2, ?2, '$argon2id$dummy', 'member', 1)",
    )
    .bind(id)
    .bind(name)
    .execute(pool)
    .await
    .expect("insert user");
}

/// Insert a `usage_accounted = 1` request_logs row tagged for `model_key`
/// and the given `started_at`. `request_id` must be unique per call.
#[allow(clippy::too_many_arguments)]
async fn insert_log(
    pool: &SqlitePool,
    request_id: &str,
    user_id: i64,
    model_key: &str,
    started_at: chrono::DateTime<Utc>,
    input_tokens: i64,
    output_tokens: i64,
    cost_nanousd: i64,
) {
    sqlx::query(
        r#"INSERT INTO request_logs (
            request_id, request_type, user_id, model_raw, model_normalized,
            model_key, usage_accounted, stream,
            started_at, completed_at, duration_ms, status, http_status,
            input_tokens, output_tokens,
            cache_creation_tokens, cache_read_tokens, cost_nanousd
        ) VALUES (
            ?1, 'messages', ?2, ?3, ?3, ?3, 1, 1,
            ?4, ?4, 100, 'ok', 200,
            ?5, ?6, 0, 0, ?7
        )"#,
    )
    .bind(request_id)
    .bind(user_id)
    .bind(model_key)
    .bind(started_at.to_rfc3339())
    .bind(input_tokens)
    .bind(output_tokens)
    .bind(cost_nanousd)
    .execute(pool)
    .await
    .expect("insert log");
}

#[allow(clippy::too_many_arguments)]
async fn insert_daily(
    pool: &SqlitePool,
    user_id: i64,
    model_key: &str,
    bucket_date_local: &str,
    request_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost_nanousd: i64,
) {
    sqlx::query(
        r#"INSERT INTO usage_daily_rollups (
            user_id, model_key, bucket_date_local,
            request_count, input_tokens, output_tokens, cost_nanousd
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
    )
    .bind(user_id)
    .bind(model_key)
    .bind(bucket_date_local)
    .bind(request_count)
    .bind(input_tokens)
    .bind(output_tokens)
    .bind(cost_nanousd)
    .execute(pool)
    .await
    .expect("insert daily");
}

async fn insert_lifetime(
    pool: &SqlitePool,
    user_id: i64,
    request_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost_nanousd: i64,
) {
    sqlx::query(
        r#"INSERT INTO usage_lifetime_totals (
            user_id, request_count, input_tokens, output_tokens,
            cache_creation_tokens, cache_read_tokens, cost_nanousd
        ) VALUES (?1, ?2, ?3, ?4, 0, 0, ?5)"#,
    )
    .bind(user_id)
    .bind(request_count)
    .bind(input_tokens)
    .bind(output_tokens)
    .bind(cost_nanousd)
    .execute(pool)
    .await
    .expect("insert lifetime");
}

/// Call the `usage` handler with a [`OpsUsageParams`] built from the
/// optional fields. Strips axum extractors so tests assert on response
/// shape directly.
async fn call_usage(
    pool: SqlitePool,
    range: Option<&str>,
    metric: Option<&str>,
    user_id: Option<i64>,
    top_users: Option<usize>,
) -> OpsUsageResponse {
    let params = OpsUsageParams {
        range: range.map(String::from),
        metric: metric.map(String::from),
        top_users,
        user_id,
    };
    let Json(resp) = super::usage(State(pool), Query(params))
        .await
        .expect("usage handler");
    resp
}

fn shanghai_today_string() -> String {
    Utc::now()
        .with_timezone(&shanghai_offset())
        .format("%Y-%m-%d")
        .to_string()
}

fn shanghai_days_ago_string(days: i64) -> String {
    (Utc::now() - Duration::days(days))
        .with_timezone(&shanghai_offset())
        .format("%Y-%m-%d")
        .to_string()
}

#[tokio::test]
async fn metric_kind_defaults_and_whitelist() {
    assert_eq!(MetricKind::from_query(None), MetricKind::Cost);
    assert_eq!(MetricKind::from_query(Some("cost")), MetricKind::Cost);
    assert_eq!(MetricKind::from_query(Some("tokens")), MetricKind::Tokens);
    assert_eq!(
        MetricKind::from_query(Some("requests")),
        MetricKind::Requests
    );
    // Unknown strings must NOT panic or empty the page — fall back to cost.
    assert_eq!(
        MetricKind::from_query(Some("'; DROP TABLE--")),
        MetricKind::Cost
    );
    assert_eq!(
        MetricKind::from_query(Some("totally-bogus")),
        MetricKind::Cost
    );
}

#[tokio::test]
async fn shift_window_back_does_not_overlap_current() {
    let now = Utc::now();
    let window = build_window(RangePreset::Last7Days, now);
    let prev = shift_window_back(&window);
    assert_eq!(prev.end_utc, window.start_utc, "prev ends at current start");
    assert_eq!(
        window.end_utc - window.start_utc,
        prev.end_utc - prev.start_utc,
        "prev window spans the same duration",
    );
    assert_eq!(prev.partial_bucket_idx, prev.buckets.len());
}

#[tokio::test]
async fn h24_reads_request_logs_and_ignores_unaccounted_rows() {
    let pool = fresh_pool().await;
    insert_user(&pool, 10, "alice").await;

    let now = Utc::now();
    insert_log(
        &pool,
        "r1",
        10,
        "claude-opus-4-7",
        now - Duration::minutes(30),
        100,
        50,
        1_000_000,
    )
    .await;
    insert_log(
        &pool,
        "r2",
        10,
        "claude-sonnet-4-6",
        now - Duration::hours(5),
        200,
        100,
        2_000_000,
    )
    .await;
    // Probe row that the new usage_accounted filter must reject.
    sqlx::query(
        r#"INSERT INTO request_logs (
            request_id, request_type, user_id, model_raw, model_key,
            usage_accounted, stream, started_at, status, http_status, cost_nanousd
        ) VALUES ('probe1', 'probe_cookie', 10, NULL, 'unknown', 0, 0, ?1, 'ok', 200, 0)"#,
    )
    .bind((now - Duration::hours(1)).to_rfc3339())
    .execute(&pool)
    .await
    .unwrap();

    let resp = call_usage(pool, Some("24h"), None, None, None).await;
    assert_eq!(resp.range, "24h");
    assert_eq!(resp.metric, "cost");
    assert_eq!(resp.bucket_unit, "hour");
    assert_eq!(resp.dimension, "user");
    assert_eq!(
        resp.window_totals.request_count, 2,
        "probe row must not count"
    );
    assert_eq!(resp.window_totals.cost_nanousd, 3_000_000);

    let labels: Vec<String> = resp.distribution.iter().map(|d| d.label.clone()).collect();
    assert!(labels.contains(&"claude-opus-4-7".to_string()));
    assert!(labels.contains(&"claude-sonnet-4-6".to_string()));
    assert!(
        !labels.contains(&"unknown".to_string()),
        "probe row leaked into distribution"
    );

    assert_eq!(resp.ranking.len(), 1);
    assert_eq!(resp.ranking[0].kind, "user");
    assert_eq!(resp.ranking[0].user_id, Some(10));
}

#[tokio::test]
async fn d7_reads_daily_rollups_not_request_logs() {
    let pool = fresh_pool().await;
    insert_user(&pool, 10, "alice").await;
    let four_days_ago = shanghai_days_ago_string(4);
    insert_daily(
        &pool,
        10,
        "claude-opus-4-7",
        &four_days_ago,
        5,
        5000,
        2500,
        12_345_678,
    )
    .await;

    let resp = call_usage(pool, Some("7d"), None, None, None).await;
    assert_eq!(resp.range, "7d");
    assert_eq!(resp.bucket_unit, "day");
    assert_eq!(resp.window_totals.request_count, 5);
    assert_eq!(resp.window_totals.cost_nanousd, 12_345_678);
    assert_eq!(resp.buckets.len(), 7);
}

#[tokio::test]
async fn d30_window_survives_log_retention_pruning() {
    let pool = fresh_pool().await;
    insert_user(&pool, 10, "alice").await;
    let twenty_days_ago = shanghai_days_ago_string(20);
    insert_daily(
        &pool,
        10,
        "claude-opus-4-7",
        &twenty_days_ago,
        3,
        3000,
        1500,
        6_000_000,
    )
    .await;

    let resp = call_usage(pool, Some("30d"), None, None, None).await;
    assert_eq!(resp.window_totals.request_count, 3);
    assert_eq!(resp.buckets.len(), 30);
}

#[tokio::test]
async fn metric_switch_reorders_ranking() {
    let pool = fresh_pool().await;
    insert_user(&pool, 10, "alice").await;
    insert_user(&pool, 11, "bob").await;
    let today = shanghai_today_string();
    insert_daily(&pool, 10, "m", &today, 1, 100, 50, 10_000).await;
    insert_daily(&pool, 11, "m", &today, 1, 10_000, 5_000, 1_000).await;

    let by_cost = call_usage(pool.clone(), Some("7d"), Some("cost"), None, None).await;
    let by_tokens = call_usage(pool, Some("7d"), Some("tokens"), None, None).await;

    assert_eq!(by_cost.ranking[0].label, "alice");
    assert_eq!(by_tokens.ranking[0].label, "bob");
}

#[tokio::test]
async fn user_filter_switches_dimension_to_model() {
    let pool = fresh_pool().await;
    insert_user(&pool, 10, "alice").await;
    let today = shanghai_today_string();
    insert_daily(&pool, 10, "claude-opus-4-7", &today, 1, 500, 250, 5_000_000).await;
    insert_daily(
        &pool,
        10,
        "claude-sonnet-4-6",
        &today,
        2,
        1000,
        500,
        1_000_000,
    )
    .await;

    let resp = call_usage(pool, Some("7d"), None, Some(10), None).await;
    assert_eq!(resp.dimension, "model");
    assert_eq!(resp.selected_user_id, Some(10));

    let labels: Vec<String> = resp.ranking.iter().map(|d| d.label.clone()).collect();
    assert_eq!(labels.len(), 2);
    assert_eq!(labels[0], "claude-opus-4-7");
    assert_eq!(resp.series.len(), 2);
    assert!(resp.series.iter().all(|s| s.kind == "model"));
}

#[tokio::test]
async fn distribution_collapses_excess_into_other_bucket() {
    let pool = fresh_pool().await;
    insert_user(&pool, 10, "alice").await;
    let today = shanghai_today_string();
    for i in 0..9 {
        let cost = ((9 - i) as i64) * 1_000_000;
        insert_daily(
            &pool,
            10,
            &format!("model-{i:02}"),
            &today,
            1,
            100,
            50,
            cost,
        )
        .await;
    }
    let resp = call_usage(pool, Some("7d"), None, None, None).await;
    assert_eq!(resp.distribution.len(), 9, "Top 8 + 1 other");
    let other = resp.distribution.last().unwrap();
    assert!(other.is_other_bucket);
    assert_eq!(other.label, "其他");
    assert_eq!(other.cost_nanousd, 1_000_000);
}

#[tokio::test]
async fn lifetime_totals_stay_independent_of_window_source() {
    let pool = fresh_pool().await;
    insert_user(&pool, 10, "alice").await;
    insert_lifetime(&pool, 10, 99, 100_000, 50_000, 99_000_000).await;
    let resp = call_usage(pool, Some("7d"), None, None, None).await;
    assert_eq!(resp.window_totals.request_count, 0);
    assert_eq!(resp.lifetime_totals.request_count, 99);
    assert_eq!(resp.lifetime_totals.cost_nanousd, 99_000_000);
    assert_eq!(resp.totals.request_count, 99);
}

#[tokio::test]
async fn coverage_24h_uses_min_started_at_clamp() {
    let pool = fresh_pool().await;
    insert_user(&pool, 10, "alice").await;
    let six_h_ago = Utc::now() - Duration::hours(6);
    insert_log(
        &pool,
        "r1",
        10,
        "claude-opus-4-7",
        six_h_ago,
        100,
        50,
        1_000_000,
    )
    .await;

    let resp = call_usage(pool, Some("24h"), None, None, None).await;
    assert!(resp.coverage.logs_available_from.is_some());
    let avail = resp.coverage.logs_available_from.as_deref().unwrap();
    let avail_dt = parse_timestamp_flexible(avail).unwrap();
    assert!(
        (avail_dt - six_h_ago).num_seconds().abs() < 5,
        "logs_available_from should clamp to the earliest row, got {avail}"
    );
    assert!(!resp.coverage.complete);
}

#[tokio::test]
async fn coverage_7d_gated_by_writes_started_at() {
    let pool = fresh_pool().await;
    insert_user(&pool, 10, "alice").await;
    let three_days_ago = Utc::now() - Duration::days(3);
    sqlx::query("UPDATE usage_daily_rollup_state SET writes_started_at = ?1 WHERE id = 1")
        .bind(three_days_ago.to_rfc3339())
        .execute(&pool)
        .await
        .unwrap();

    let resp = call_usage(pool, Some("7d"), None, None, None).await;
    assert!(!resp.coverage.complete);
    assert!(!resp.coverage.comparison_complete);
    assert_eq!(
        resp.coverage.writes_started_at.as_deref(),
        Some(three_days_ago.to_rfc3339().as_str())
    );
}

#[tokio::test]
async fn comparison_ratio_null_when_previous_window_empty() {
    let pool = fresh_pool().await;
    insert_user(&pool, 10, "alice").await;
    let today = shanghai_today_string();
    insert_daily(&pool, 10, "m", &today, 5, 1000, 500, 5_000_000).await;
    let resp = call_usage(pool, Some("7d"), Some("cost"), None, None).await;
    assert!(resp.comparison.cost_ratio.is_none());
    assert!(resp.comparison.total_tokens_ratio.is_none());
    assert!(resp.comparison.request_count_ratio.is_none());
    assert_eq!(resp.comparison.expected_bucket_count, 7);
}

#[tokio::test]
async fn comparison_ratio_computed_when_previous_window_has_data() {
    let pool = fresh_pool().await;
    insert_user(&pool, 10, "alice").await;
    let today = shanghai_today_string();
    let ten_days_ago = shanghai_days_ago_string(10);
    insert_daily(&pool, 10, "m", &today, 2, 200, 100, 2_000_000).await;
    insert_daily(&pool, 10, "m", &ten_days_ago, 1, 100, 50, 1_000_000).await;
    let resp = call_usage(pool, Some("7d"), Some("cost"), None, None).await;
    assert_eq!(resp.previous_window_totals.cost_nanousd, 1_000_000);
    assert_eq!(resp.window_totals.cost_nanousd, 2_000_000);
    assert!((resp.comparison.cost_ratio.unwrap() - 2.0).abs() < 1e-9);
}

#[tokio::test]
async fn buckets_mark_only_trailing_as_partial() {
    let pool = fresh_pool().await;
    let resp = call_usage(pool, Some("24h"), None, None, None).await;
    assert_eq!(resp.buckets.len(), 24);
    assert_eq!(resp.bucket_labels.len(), 24);
    for (i, bucket) in resp.bucket_labels.iter().enumerate() {
        if i + 1 == resp.bucket_labels.len() {
            assert!(bucket.partial, "trailing bucket must be partial");
        } else {
            assert!(
                !bucket.partial,
                "non-trailing bucket {i} should not be partial"
            );
        }
    }
}

#[tokio::test]
async fn series_points_align_with_bucket_keys() {
    let pool = fresh_pool().await;
    insert_user(&pool, 10, "alice").await;
    let today = shanghai_today_string();
    insert_daily(&pool, 10, "m", &today, 3, 300, 150, 3_000_000).await;

    let resp = call_usage(pool, Some("7d"), None, None, None).await;
    assert_eq!(resp.series.len(), 1);
    let s = &resp.series[0];
    assert_eq!(s.points.len(), 7);
    let today_point = s.points.iter().find(|p| p.bucket == today).unwrap();
    assert_eq!(today_point.request_count, 3);
    assert!(today_point.partial);
    let other = s.points.iter().find(|p| p.bucket != today).unwrap();
    assert_eq!(other.request_count, 0);
    assert!(!other.partial);
}

#[test]
fn parse_timestamp_flexible_accepts_sqlite_and_rfc3339() {
    // SQLite CURRENT_TIMESTAMP shape — what PR-A's migration seed and
    // ensure_daily_rollup_state both write.
    let sqlite_shape =
        parse_timestamp_flexible("2026-06-02 12:34:56").expect("SQLite format must parse");
    assert_eq!(
        sqlite_shape.format("%Y-%m-%d %H:%M:%S").to_string(),
        "2026-06-02 12:34:56"
    );
    // RFC3339 shape — what `chrono::DateTime<Utc>::to_rfc3339` emits, used
    // by tests and any future Rust-side writers.
    let rfc3339 = parse_timestamp_flexible("2026-06-02T12:34:56Z").expect("RFC3339 must parse");
    assert_eq!(rfc3339, sqlite_shape);
    // RFC3339 with offset normalises to UTC.
    let with_offset = parse_timestamp_flexible("2026-06-02T20:34:56+08:00").expect("offset");
    assert_eq!(with_offset, sqlite_shape);
    // Malformed strings return None instead of panicking.
    assert!(parse_timestamp_flexible("not-a-timestamp").is_none());
    assert!(parse_timestamp_flexible("").is_none());
}

#[tokio::test]
async fn coverage_7d_accepts_sqlite_writes_started_at_format() {
    // Regression: PR-A's migration seeds writes_started_at with SQLite
    // CURRENT_TIMESTAMP, which has no `T` separator and no `Z` suffix.
    // PR-B's first parse pass only accepted RFC3339, so 7d coverage on
    // any real deploy would have permanently reported "incomplete".
    let pool = fresh_pool().await;
    insert_user(&pool, 10, "alice").await;
    // 30 days in the past, written in SQLite's native CURRENT_TIMESTAMP
    // shape. This must be old enough that the 7d window and its previous
    // 7d sit entirely after writes_started_at.
    sqlx::query(
        "UPDATE usage_daily_rollup_state
         SET writes_started_at = strftime('%Y-%m-%d %H:%M:%S', datetime('now', '-30 days'))
         WHERE id = 1",
    )
    .execute(&pool)
    .await
    .unwrap();

    let resp = call_usage(pool, Some("7d"), None, None, None).await;
    assert!(
        resp.coverage.complete,
        "30d-old SQLite-format writes_started_at must cover the 7d window"
    );
    assert!(
        resp.coverage.comparison_complete,
        "30d-old writes_started_at must also cover the previous 7d window"
    );
    let written = resp.coverage.writes_started_at.expect("seeded by UPDATE");
    assert!(
        !written.contains('T'),
        "writes_started_at should be returned verbatim in SQLite shape: {written}"
    );
}

#[tokio::test]
async fn ranking_and_series_stay_aligned_under_metric_ties() {
    // When the SQL ORDER BY can't choose between two users with
    // identical metric values, the table and the chart must still pick
    // the same subjects. We force a 4-user tie on cost, ask for Top 2
    // by cost, and verify series subjects are a subset of the
    // non-other-bucket ranking entries.
    let pool = fresh_pool().await;
    insert_user(&pool, 10, "alice").await;
    insert_user(&pool, 11, "bob").await;
    insert_user(&pool, 12, "carol").await;
    insert_user(&pool, 13, "dave").await;
    let today = shanghai_today_string();
    insert_daily(&pool, 10, "m", &today, 1, 100, 50, 5_000_000).await;
    insert_daily(&pool, 11, "m", &today, 1, 100, 50, 5_000_000).await;
    insert_daily(&pool, 12, "m", &today, 1, 100, 50, 5_000_000).await;
    insert_daily(&pool, 13, "m", &today, 1, 100, 50, 5_000_000).await;

    let resp = call_usage(pool, Some("7d"), Some("cost"), None, Some(2)).await;
    let ranking_users: std::collections::BTreeSet<i64> = resp
        .ranking
        .iter()
        .filter(|item| !item.is_other_bucket)
        .filter_map(|item| item.user_id)
        .collect();
    let series_users: std::collections::BTreeSet<i64> =
        resp.series.iter().filter_map(|s| s.user_id).collect();
    assert_eq!(
        ranking_users, series_users,
        "ranking subjects and series subjects must agree even with tied metric values"
    );
    assert_eq!(series_users.len(), 2, "Top 2 honored");
}
