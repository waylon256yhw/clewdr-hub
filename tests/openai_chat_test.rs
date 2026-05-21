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
    let _api_key_id = sqlx::query_scalar::<_, i64>("SELECT id FROM api_keys WHERE lookup_key = ?1")
        .bind(lookup_key)
        .fetch_one(&pool)
        .await
        .unwrap();

    let builder = RouterBuilder::new(pool.clone()).await.with_default_setup();
    let router = builder.build();

    TestApp {
        _tempdir: tempdir,
        pool,
        router,
        api_key,
        user_id,
    }
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

fn basic_chat_body() -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "messages": [
            { "role": "user", "content": "Hi from integration test" }
        ]
    })
}

// ---------------------------------------------------------------------------
// /v1/chat/completions auth surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_completions_requires_auth() {
    let app = setup_app(PolicyConfig::default()).await;
    let response = app
        .request(
            Method::POST,
            "/v1/chat/completions",
            Some(basic_chat_body()),
            None,
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn chat_completions_accepts_bearer_auth_and_returns_oai_error_shape_when_no_accounts() {
    let app = setup_app(PolicyConfig::default()).await;
    let bearer = format!("Bearer {}", app.api_key);
    let response = app
        .request(
            Method::POST,
            "/v1/chat/completions",
            Some(basic_chat_body()),
            Some((header::AUTHORIZATION.as_str(), bearer.as_str())),
            &[],
        )
        .await;
    // No upstream accounts seeded -> rate_limit_exceeded(503) in OAI shape.
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response_json(response).await;
    assert!(body["error"].is_object());
    assert_eq!(body["error"]["type"], "rate_limit_exceeded");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("account")
    );
}

#[tokio::test]
async fn chat_completions_accepts_x_api_key_auth() {
    let app = setup_app(PolicyConfig::default()).await;
    let response = app
        .request(
            Method::POST,
            "/v1/chat/completions",
            Some(basic_chat_body()),
            Some(("x-api-key", app.api_key.as_str())),
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response_json(response).await;
    assert!(body["error"].is_object());
    assert_eq!(body["error"]["type"], "rate_limit_exceeded");
}

#[tokio::test]
async fn chat_completions_rejects_n_gt_1_with_400_oai_shape() {
    let app = setup_app(PolicyConfig::default()).await;
    let mut body = basic_chat_body();
    body["n"] = json!(2);
    let response = app
        .request(
            Method::POST,
            "/v1/chat/completions",
            Some(body),
            Some(("x-api-key", app.api_key.as_str())),
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["message"].as_str().unwrap().contains("n"));
}

#[tokio::test]
async fn chat_completions_rejects_logprobs_true_with_400() {
    let app = setup_app(PolicyConfig::default()).await;
    let mut body = basic_chat_body();
    body["logprobs"] = json!(true);
    let response = app
        .request(
            Method::POST,
            "/v1/chat/completions",
            Some(body),
            Some(("x-api-key", app.api_key.as_str())),
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("logprobs")
    );
}

#[tokio::test]
async fn chat_completions_rejects_unsupported_image_media_type() {
    let app = setup_app(PolicyConfig::default()).await;
    let body = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": "data:image/tiff;base64,AAAA"}}
            ]
        }]
    });
    let response = app
        .request(
            Method::POST,
            "/v1/chat/completions",
            Some(body),
            Some(("x-api-key", app.api_key.as_str())),
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("image")
    );
}

#[tokio::test]
async fn chat_completions_returns_oai_shape_when_quota_exceeded() {
    let app = setup_app(PolicyConfig {
        weekly_budget_nanousd: 1_000_000,
        ..PolicyConfig::default()
    })
    .await;
    seed_current_week_cost(&app.pool, app.user_id, 2_000_000).await;

    let response = app
        .request(
            Method::POST,
            "/v1/chat/completions",
            Some(basic_chat_body()),
            Some(("x-api-key", app.api_key.as_str())),
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "rate_limit_exceeded");
}

#[tokio::test]
async fn chat_completions_silently_accepts_ignored_fields() {
    // frequency/presence penalty, seed, service_tier, store, logit_bias,
    // top_logprobs, non-string metadata — all silently dropped, request
    // must reach the upstream pool (which then 503s because no accounts).
    let app = setup_app(PolicyConfig::default()).await;
    let body = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{ "role": "user", "content": "hi" }],
        "frequency_penalty": 0.5,
        "presence_penalty": -0.3,
        "logit_bias": {"42": -1},
        "seed": 7,
        "service_tier": "auto",
        "store": true,
        "top_logprobs": 3,
        "metadata": {"app": "tester", "version": 1}
    });
    let response = app
        .request(
            Method::POST,
            "/v1/chat/completions",
            Some(body),
            Some(("x-api-key", app.api_key.as_str())),
            &[],
        )
        .await;
    // Reaches upstream pool (no 400 from translate_request).
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn chat_completions_invalid_json_returns_oai_shape_400() {
    let app = setup_app(PolicyConfig::default()).await;
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-api-key", app.api_key.as_str())
        .body(Body::from("{not json"))
        .unwrap();
    let response = app.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

// ---------------------------------------------------------------------------
// /v1/models format negotiation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn models_default_returns_compat_superset() {
    let app = setup_app(PolicyConfig::default()).await;
    let response = app
        .request(Method::GET, "/v1/models", None, None, &[])
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["object"], "list");
    assert!(body["has_more"].is_boolean());
    let first = &body["data"][0];
    assert_eq!(first["object"], "model");
    assert_eq!(first["type"], "model");
    assert_eq!(first["owned_by"], "anthropic");
    assert!(first["display_name"].is_string());
    assert!(first["created"].as_i64().unwrap() >= 0);
}

#[tokio::test]
async fn models_format_openai_returns_strict_shape() {
    let app = setup_app(PolicyConfig::default()).await;
    let response = app
        .request(Method::GET, "/v1/models?format=openai", None, None, &[])
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["object"], "list");
    assert!(body.get("has_more").is_none());
    assert!(body.get("first_id").is_none());
    let first = &body["data"][0];
    assert_eq!(first["object"], "model");
    assert_eq!(first["owned_by"], "anthropic");
    assert!(first.get("display_name").is_none());
    assert!(first.get("type").is_none());
}

#[tokio::test]
async fn models_format_anthropic_returns_legacy_shape() {
    let app = setup_app(PolicyConfig::default()).await;
    let response = app
        .request(Method::GET, "/v1/models?format=anthropic", None, None, &[])
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert!(body.get("object").is_none());
    assert!(body["has_more"].is_boolean());
    let first = &body["data"][0];
    assert_eq!(first["type"], "model");
    assert!(first.get("object").is_none());
    assert!(first.get("owned_by").is_none());
}

#[tokio::test]
async fn models_invalid_format_returns_400() {
    let app = setup_app(PolicyConfig::default()).await;
    let response = app
        .request(Method::GET, "/v1/models?format=gemini", None, None, &[])
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn models_get_supports_all_formats() {
    let app = setup_app(PolicyConfig::default()).await;

    let compat = app
        .request(Method::GET, "/v1/models/claude-sonnet-4-6", None, None, &[])
        .await;
    assert_eq!(compat.status(), StatusCode::OK);
    let body = response_json(compat).await;
    assert_eq!(body["id"], "claude-sonnet-4-6");
    assert_eq!(body["type"], "model");
    assert_eq!(body["object"], "model");

    let openai = app
        .request(
            Method::GET,
            "/v1/models/claude-sonnet-4-6?format=openai",
            None,
            None,
            &[],
        )
        .await;
    assert_eq!(openai.status(), StatusCode::OK);
    let body = response_json(openai).await;
    assert_eq!(body["object"], "model");
    assert!(body.get("display_name").is_none());
    assert!(body.get("type").is_none());

    let anthropic = app
        .request(
            Method::GET,
            "/v1/models/claude-sonnet-4-6?format=anthropic",
            None,
            None,
            &[],
        )
        .await;
    assert_eq!(anthropic.status(), StatusCode::OK);
    let body = response_json(anthropic).await;
    assert_eq!(body["type"], "model");
    assert!(body.get("object").is_none());
}

#[tokio::test]
async fn models_get_unknown_id_returns_404() {
    let app = setup_app(PolicyConfig::default()).await;
    let response = app
        .request(
            Method::GET,
            "/v1/models/totally-fake-model",
            None,
            None,
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// CORS preflight on /v1/chat/completions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_completions_cors_preflight_allows_openai_headers() {
    let app = setup_app(PolicyConfig::default()).await;
    let response = app
        .request(
            Method::OPTIONS,
            "/v1/chat/completions",
            None,
            None,
            &[
                ("origin", "https://example.com"),
                ("access-control-request-method", "POST"),
                (
                    "access-control-request-headers",
                    "authorization, openai-beta, openai-organization, openai-project",
                ),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let allow_headers = response
        .headers()
        .get("access-control-allow-headers")
        .map(|v| v.to_str().unwrap().to_lowercase())
        .unwrap_or_default();
    assert!(allow_headers.contains("openai-beta"));
    assert!(allow_headers.contains("openai-organization"));
    assert!(allow_headers.contains("openai-project"));
}

#[tokio::test]
async fn chat_completions_handler_completes_within_reasonable_time() {
    // Smoke test: the no-account path returns promptly (no upstream call
    // means no real timeout in play). Guards against accidental
    // infinite-poll regressions in future refactors.
    let app = setup_app(PolicyConfig::default()).await;
    let bearer = format!("Bearer {}", app.api_key);
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        app.request(
            Method::POST,
            "/v1/chat/completions",
            Some(basic_chat_body()),
            Some((header::AUTHORIZATION.as_str(), bearer.as_str())),
            &[],
        ),
    )
    .await
    .expect("request should resolve quickly");
    assert_eq!(result.status(), StatusCode::SERVICE_UNAVAILABLE);
}
