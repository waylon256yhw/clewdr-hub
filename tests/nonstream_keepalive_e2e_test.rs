//! End-to-end coverage for the non-stream idle-timeout bridge.

use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{ConnectInfo, State},
    http::{Request, StatusCode, header},
    response::Response,
    routing::post,
};
use clewdr_hub::{
    db::{self, queries::create_api_key},
    router::RouterBuilder,
};
use futures::StreamExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{net::TcpListener, sync::oneshot};
use tower::ServiceExt;

type CapturedRequest = (Value, Option<String>);
type RequestSender = oneshot::Sender<CapturedRequest>;

#[derive(Clone)]
struct MockState {
    request_tx: Arc<Mutex<Option<RequestSender>>>,
}

async fn slow_messages(
    State(state): State<MockState>,
    headers: header::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let outbound_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if let Some(tx) = state.request_tx.lock().unwrap().take() {
        let _ = tx.send((body, outbound_key));
    }

    let stream = async_stream::stream! {
        yield Ok::<Bytes, Infallible>(Bytes::from_static(
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_slow\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":11,\"output_tokens\":0}}}\n\n",
        ));
        tokio::time::sleep(Duration::from_millis(800)).await;
        yield Ok(Bytes::from_static(
            b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"compacted summary\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":3}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ));
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn reject_messages() -> Response {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "mock request rejected"
                }
            }))
            .unwrap(),
        ))
        .unwrap()
}

async fn seed_user_and_key(pool: &sqlx::SqlitePool) -> String {
    let policy = sqlx::query(
        "INSERT INTO policies (
            name, max_concurrent, rpm_limit,
            weekly_budget_nanousd, monthly_budget_nanousd
         ) VALUES (?1, 5, 30, 0, 0)",
    )
    .bind(format!("keepalive-policy-{}", uuid::Uuid::new_v4()))
    .execute(pool)
    .await
    .unwrap();
    let user = sqlx::query(
        "INSERT INTO users (username, display_name, role, policy_id) VALUES (?1, 'Keepalive Test', 'member', ?2)",
    )
    .bind(format!("keepalive-user-{}", uuid::Uuid::new_v4()))
    .bind(policy.last_insert_rowid())
    .execute(pool)
    .await
    .unwrap();
    create_api_key(pool, user.last_insert_rowid(), Some("keepalive-e2e"))
        .await
        .unwrap()
}

async fn setup_app(mock_base_url: &str) -> (TempDir, Router, String) {
    let tempdir = tempfile::tempdir().unwrap();
    let pool = db::init_pool(&tempdir.path().join("keepalive.db"))
        .await
        .unwrap();
    db::seed_admin(&pool).await.unwrap();
    let app_key = seed_user_and_key(&pool).await;

    sqlx::query(
        "INSERT INTO accounts (
            name, rr_order, max_slots, status, auth_source,
            api_key_base_url, api_key_secret
         ) VALUES (?1, 1, 1, 'active', 'api_key', ?2, 'test-upstream-key')",
    )
    .bind(format!("keepalive-account-{}", uuid::Uuid::new_v4()))
    .bind(mock_base_url)
    .execute(&pool)
    .await
    .unwrap();

    let router = RouterBuilder::new(pool).await.with_default_setup().build();
    (tempdir, router, app_key)
}

#[tokio::test]
async fn non_stream_request_receives_heartbeats_before_final_json() {
    let (request_tx, request_rx) = oneshot::channel();
    let mock = Router::new()
        .route("/v1/messages", post(slow_messages))
        .with_state(MockState {
            request_tx: Arc::new(Mutex::new(Some(request_tx))),
        });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = listener.local_addr().unwrap();
    let mock_task = tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });

    let (_tempdir, router, app_key) = setup_app(&format!("http://{mock_addr}/")).await;
    let request_body = json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 256,
        "messages": [{"role": "user", "content": "Compact this long context slowly"}],
        "stream": false
    });
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {app_key}"))
        .header(header::ACCEPT_ENCODING, "zstd")
        .header("x-clewdr-non-stream-keepalive", "true")
        .header("x-clewdr-non-stream-keepalive-interval-ms", "250")
        .body(Body::from(serde_json::to_vec(&request_body).unwrap()))
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo::<SocketAddr>("127.0.0.1:0".parse().unwrap()));

    let started = Instant::now();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "identity");
    assert_eq!(response.headers()["x-accel-buffering"], "no");
    assert!(response.headers().get(header::CONTENT_LENGTH).is_none());

    let (upstream_body, outbound_key) = request_rx.await.unwrap();
    assert_eq!(
        upstream_body["stream"], true,
        "upstream must be forced to SSE"
    );
    assert_eq!(outbound_key.as_deref(), Some("test-upstream-key"));

    let mut stream = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_millis(100), stream.next())
        .await
        .expect("initial whitespace must arrive immediately")
        .unwrap()
        .unwrap();
    assert!(first.iter().all(u8::is_ascii_whitespace));

    let mut heartbeat_frames = 1usize;
    let mut final_json = Vec::new();
    while let Some(frame) = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("response body stalled")
    {
        let bytes = frame.unwrap();
        if bytes.iter().all(u8::is_ascii_whitespace) {
            heartbeat_frames += 1;
        } else {
            final_json.extend_from_slice(&bytes);
        }
    }

    assert!(started.elapsed() >= Duration::from_millis(700));
    assert!(
        heartbeat_frames >= 3,
        "expected initial + periodic heartbeat frames, got {heartbeat_frames}"
    );
    let final_json: Value = serde_json::from_slice(&final_json).unwrap();
    assert_eq!(final_json["content"][0]["text"], "compacted summary");
    assert_eq!(final_json["stop_reason"], "end_turn");
    assert_eq!(final_json["usage"]["input_tokens"], 11);
    assert_eq!(final_json["usage"]["output_tokens"], 3);

    mock_task.abort();
}

#[tokio::test]
async fn request_time_upstream_error_keeps_real_http_status_without_heartbeat() {
    let mock = Router::new().route("/v1/messages", post(reject_messages));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = listener.local_addr().unwrap();
    let mock_task = tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    let (_tempdir, router, app_key) = setup_app(&format!("http://{mock_addr}/")).await;

    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {app_key}"))
        .header("x-clewdr-non-stream-keepalive", "true")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "claude-sonnet-4-6",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "Reject this upstream"}],
                "stream": false
            }))
            .unwrap(),
        ))
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo::<SocketAddr>("127.0.0.1:0".parse().unwrap()));

    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_ne!(
        response
            .headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok()),
        Some("identity"),
        "request-time errors must bypass the committed keepalive response"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_ne!(body.first(), Some(&b' '));
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"]["message"], "mock request rejected");

    mock_task.abort();
}
