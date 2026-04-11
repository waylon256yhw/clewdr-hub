use std::time::Duration;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use clewdr_hub::{
    billing::current_week_bounds,
    db::{self, api_key::parse_api_key, queries::create_api_key},
    router::RouterBuilder,
};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tempfile::TempDir;
use tower::ServiceExt;

#[derive(Clone, Copy)]
struct PolicyConfig {
    max_concurrent: i32,
    rpm_limit: i32,
    weekly_budget_nanousd: i64,
    monthly_budget_nanousd: i64,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 5,
            rpm_limit: 30,
            weekly_budget_nanousd: 0,
            monthly_budget_nanousd: 0,
        }
    }
}

struct TestApp {
    _tempdir: TempDir,
    pool: SqlitePool,
    router: Router,
    api_key: String,
    api_key_id: i64,
    user_id: i64,
}

impl TestApp {
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        auth_header: Option<(&str, &str)>,
        extra_headers: &[(&str, &str)],
    ) -> axum::response::Response {
        let mut builder = Request::builder().method(method).uri(path);

        if body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }

        if let Some((name, value)) = auth_header {
            builder = builder.header(name, value);
        }

        for (name, value) in extra_headers {
            builder = builder.header(*name, *value);
        }

        let request = builder
            .body(match body {
                Some(value) => Body::from(serde_json::to_vec(&value).unwrap()),
                None => Body::empty(),
            })
            .unwrap();

        self.router.clone().oneshot(request).await.unwrap()
    }
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn error_message(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap()
}

fn test_message_body() -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "messages": [
            { "role": "user", "content": "Hi" }
        ],
        "stream": false
    })
}

fn real_message_body() -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "messages": [
            { "role": "user", "content": "Hello from integration test" }
        ],
        "stream": false
    })
}

async fn setup_app(policy: PolicyConfig) -> TestApp {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("clewdr-test.db");
    let pool = db::init_pool(&db_path).await.unwrap();
    db::seed_admin(&pool).await.unwrap();

    let policy_name = format!("test-policy-{}", uuid::Uuid::new_v4());
    let policy_result = sqlx::query(
        "INSERT INTO policies (name, max_concurrent, rpm_limit, weekly_budget_nanousd, monthly_budget_nanousd)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(&policy_name)
    .bind(policy.max_concurrent)
    .bind(policy.rpm_limit)
    .bind(policy.weekly_budget_nanousd)
    .bind(policy.monthly_budget_nanousd)
    .execute(&pool)
    .await
    .unwrap();
    let policy_id = policy_result.last_insert_rowid();

    let username = format!("member-{}", uuid::Uuid::new_v4().simple());
    let user_result = sqlx::query(
        "INSERT INTO users (username, display_name, role, policy_id) VALUES (?1, ?2, 'member', ?3)",
    )
    .bind(&username)
    .bind("Integration Test User")
    .bind(policy_id)
    .execute(&pool)
    .await
    .unwrap();
    let user_id = user_result.last_insert_rowid();

    let api_key = create_api_key(&pool, user_id, Some("integration"))
        .await
        .unwrap();
    let (lookup_key, _) = parse_api_key(&api_key).unwrap();
    let api_key_id = sqlx::query_scalar::<_, i64>("SELECT id FROM api_keys WHERE lookup_key = ?1")
        .bind(lookup_key)
        .fetch_one(&pool)
        .await
        .unwrap();

    let router = RouterBuilder::new(pool.clone())
        .await
        .with_default_setup()
        .build();

    TestApp {
        _tempdir: tempdir,
        pool,
        router,
        api_key,
        api_key_id,
        user_id,
    }
}

async fn wait_for_key_touch(pool: &SqlitePool, api_key_id: i64, user_id: i64) -> (String, String) {
    for _ in 0..40 {
        let row = sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
            "SELECT ak.last_used_at, ak.last_used_ip, u.last_seen_at
             FROM api_keys ak
             JOIN users u ON u.id = ak.user_id
             WHERE ak.id = ?1 AND u.id = ?2",
        )
        .bind(api_key_id)
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap();

        if let (Some(_last_used_at), Some(last_used_ip), Some(last_seen_at)) = row {
            return (last_used_ip, last_seen_at);
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    panic!("timed out waiting for async API key touch updates");
}

async fn seed_current_week_cost(pool: &SqlitePool, user_id: i64, cost_nanousd: i64) {
    let now = chrono::Utc::now();
    let (period_start, period_end) = current_week_bounds(now);

    sqlx::query(
        "INSERT INTO usage_rollups (
            user_id, period_type, period_start, period_end,
            request_count, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
            cost_nanousd, updated_at
        ) VALUES (?1, 'week', ?2, ?3, 1, 0, 0, 0, 0, ?4, CURRENT_TIMESTAMP)",
    )
    .bind(user_id)
    .bind(period_start)
    .bind(period_end)
    .bind(cost_nanousd)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_pure_oauth_account(pool: &SqlitePool, reset_time: Option<i64>) -> i64 {
    let name = format!("oauth-{}", uuid::Uuid::new_v4().simple());
    let expires_at = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();

    let result = sqlx::query(
        "INSERT INTO accounts (
            name, rr_order, max_slots, status, auth_source,
            oauth_access_token, oauth_refresh_token, oauth_expires_at, organization_uuid
        ) VALUES (
            ?1, (SELECT COALESCE(MAX(rr_order), 0) + 1 FROM accounts), 1, 'active', 'oauth',
            ?2, ?3, ?4, ?5
        )",
    )
    .bind(&name)
    .bind("test-access-token")
    .bind("test-refresh-token")
    .bind(&expires_at)
    .bind("org-test")
    .execute(pool)
    .await
    .unwrap();

    let account_id = result.last_insert_rowid();
    if let Some(reset_time) = reset_time {
        sqlx::query("INSERT INTO account_runtime_state (account_id, reset_time) VALUES (?1, ?2)")
            .bind(account_id)
            .bind(reset_time)
            .execute(pool)
            .await
            .unwrap();
    }

    account_id
}

async fn latest_request_log(pool: &SqlitePool) -> (String, Option<i64>, Option<String>) {
    sqlx::query_as::<_, (String, Option<i64>, Option<String>)>(
        "SELECT status, http_status, error_message
         FROM request_logs
         ORDER BY id DESC
         LIMIT 1",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn v1_models_list_is_public_and_seeded() {
    let app = setup_app(PolicyConfig::default()).await;

    let response = app
        .request(Method::GET, "/v1/models", None, None, &[])
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert!(body["data"].as_array().unwrap().len() >= 3);
    assert_eq!(body["first_id"], "claude-opus-4-6");
}

#[tokio::test]
async fn v1_models_get_returns_seeded_model() {
    let app = setup_app(PolicyConfig::default()).await;

    let response = app
        .request(Method::GET, "/v1/models/claude-sonnet-4-6", None, None, &[])
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["id"], "claude-sonnet-4-6");
    assert_eq!(body["type"], "model");
}

#[tokio::test]
async fn v1_messages_requires_auth() {
    let app = setup_app(PolicyConfig::default()).await;

    let response = app
        .request(
            Method::POST,
            "/v1/messages",
            Some(test_message_body()),
            None,
            &[],
        )
        .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = response_json(response).await;
    assert_eq!(error_message(&body), "Key/Password Invalid");
}

#[tokio::test]
async fn v1_messages_rejects_invalid_api_key() {
    let app = setup_app(PolicyConfig::default()).await;
    let invalid_key = format!("{}x", &app.api_key[..app.api_key.len() - 1]);

    let response = app
        .request(
            Method::POST,
            "/v1/messages",
            Some(test_message_body()),
            Some(("x-api-key", invalid_key.as_str())),
            &[],
        )
        .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = response_json(response).await;
    assert_eq!(error_message(&body), "Key/Password Invalid");
}

#[tokio::test]
async fn v1_messages_accepts_x_api_key_and_touches_usage_fields() {
    let app = setup_app(PolicyConfig::default()).await;

    let response = app
        .request(
            Method::POST,
            "/v1/messages",
            Some(test_message_body()),
            Some(("x-api-key", app.api_key.as_str())),
            &[("x-real-ip", "203.0.113.10")],
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["type"], "message");
    assert_eq!(
        body["content"][0]["text"],
        "Claude Reverse Proxy is working, please send a real message."
    );

    let (last_used_ip, last_seen_at) =
        wait_for_key_touch(&app.pool, app.api_key_id, app.user_id).await;
    assert_eq!(last_used_ip, "203.0.113.10");
    assert!(!last_seen_at.is_empty());
}

#[tokio::test]
async fn v1_messages_accepts_bearer_auth() {
    let app = setup_app(PolicyConfig::default()).await;
    let bearer = format!("Bearer {}", app.api_key);

    let response = app
        .request(
            Method::POST,
            "/v1/messages",
            Some(test_message_body()),
            Some((header::AUTHORIZATION.as_str(), bearer.as_str())),
            &[],
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["type"], "message");
}

#[tokio::test]
async fn v1_messages_rejects_when_weekly_quota_is_exceeded() {
    let app = setup_app(PolicyConfig {
        weekly_budget_nanousd: 100,
        ..PolicyConfig::default()
    })
    .await;
    seed_current_week_cost(&app.pool, app.user_id, 100).await;

    let response = app
        .request(
            Method::POST,
            "/v1/messages",
            Some(real_message_body()),
            Some(("x-api-key", app.api_key.as_str())),
            &[],
        )
        .await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = response_json(response).await;
    assert_eq!(error_message(&body), "Usage quota exceeded");
}

#[tokio::test]
async fn v1_messages_logs_quota_db_failures_as_internal_error() {
    let app = setup_app(PolicyConfig {
        weekly_budget_nanousd: 100,
        ..PolicyConfig::default()
    })
    .await;
    sqlx::query("DROP TABLE usage_rollups")
        .execute(&app.pool)
        .await
        .unwrap();

    let response = app
        .request(
            Method::POST,
            "/v1/messages",
            Some(real_message_body()),
            Some(("x-api-key", app.api_key.as_str())),
            &[],
        )
        .await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let (status, http_status, error_message) = latest_request_log(&app.pool).await;
    assert_eq!(status, "internal_error");
    assert_eq!(http_status, Some(500));
    assert!(error_message.unwrap().contains("usage_rollups"));
}

#[tokio::test]
async fn v1_messages_returns_429_for_pure_oauth_accounts_in_cooldown() {
    let app = setup_app(PolicyConfig::default()).await;
    seed_pure_oauth_account(&app.pool, Some(chrono::Utc::now().timestamp() + 300)).await;

    let response = app
        .request(
            Method::POST,
            "/v1/messages",
            Some(real_message_body()),
            Some(("x-api-key", app.api_key.as_str())),
            &[],
        )
        .await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = response_json(response).await;
    assert_eq!(
        error_message(&body),
        "All upstream accounts are temporarily unavailable"
    );
}

#[tokio::test]
async fn v1_messages_returns_no_account_available_for_real_requests() {
    let app = setup_app(PolicyConfig::default()).await;

    let response = app
        .request(
            Method::POST,
            "/v1/messages",
            Some(real_message_body()),
            Some(("x-api-key", app.api_key.as_str())),
            &[],
        )
        .await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response_json(response).await;
    assert_eq!(error_message(&body), "No valid upstream accounts available");
}
