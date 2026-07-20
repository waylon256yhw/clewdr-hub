//! Compression opt-out contract for the non-stream keepalive path.
//!
//! The non-stream keepalive builder emits periodic whitespace heartbeats on an
//! `application/json` response so a slow compaction request does not trip the
//! client's idle/read timeout. Those heartbeats only reach the client if
//! `tower_http::compression::CompressionLayer` leaves the body untouched —
//! otherwise the streaming compressor buffers the long whitespace runs and the
//! heartbeat never flushes.
//!
//! The agreed opt-out is to pre-tag the response with `Content-Encoding: identity`:
//! tower-http skips any response that already carries a `Content-Encoding` header.
//! This test pins that behavior for the exact tower-http version in the tree.
//!
//! Note: the project enables ONLY `compression-zstd` (see Cargo.toml), so the
//! negotiated algorithm is zstd — a client sending `Accept-Encoding: gzip` would
//! not be compressed at all here. The control case therefore negotiates zstd.

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
    routing::get,
};
use tower::ServiceExt; // oneshot
use tower_http::compression::CompressionLayer;

/// Large, highly compressible body — clears tower-http's default size gate and
/// makes a "was it compressed" check unambiguous.
fn payload() -> String {
    let mut s = String::from("{\"data\":\"");
    s.push_str(&"a".repeat(4096));
    s.push_str("\"}");
    s
}

/// Plain body, no pre-set encoding: the layer is free to compress it.
async fn plain_json() -> Response {
    let body = payload();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

/// Pre-tagged `identity` — mirrors what the keepalive response builder sets.
async fn identity_json() -> Response {
    let body = payload();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_ENCODING, "identity")
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

fn app() -> Router {
    Router::new()
        .route("/plain", get(plain_json))
        .route("/identity", get(identity_json))
        .layer(CompressionLayer::new())
}

fn content_encoding(resp: &Response) -> Option<String> {
    resp.headers()
        .get(header::CONTENT_ENCODING)
        .map(|v| v.to_str().unwrap().to_owned())
}

/// Control: proves the layer is actually live for a zstd-accepting client, so
/// the opt-out assertion below is meaningful rather than vacuous.
#[tokio::test]
async fn plain_response_is_compressed_for_zstd_client() {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri("/plain")
                .header(header::ACCEPT_ENCODING, "zstd")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        content_encoding(&resp).as_deref(),
        Some("zstd"),
        "plain response should be zstd-compressed under Accept-Encoding: zstd"
    );
}

/// The contract the keepalive relies on: a response pre-tagged
/// `Content-Encoding: identity` is passed through untouched, so heartbeat
/// whitespace is never swallowed by the compressor.
#[tokio::test]
async fn identity_tagged_response_is_left_uncompressed() {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri("/identity")
                .header(header::ACCEPT_ENCODING, "zstd")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        content_encoding(&resp).as_deref(),
        Some("identity"),
        "identity-tagged response must be left uncompressed by CompressionLayer"
    );

    // Body must be the raw, uncompressed bytes (not a zstd frame).
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert!(
        body.starts_with(b"{\"data\":\"aaaaaaaa"),
        "identity body should be the untouched payload, got {} bytes starting {:?}",
        body.len(),
        &body[..body.len().min(16)]
    );
}
