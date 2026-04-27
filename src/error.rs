use std::fmt::Display;

use axum::{
    Json,
    extract::rejection::{JsonRejection, PathRejection, QueryRejection},
    response::IntoResponse,
};
use chrono::Utc;
use colored::Colorize;
use oauth2::{RequestTokenError, StandardErrorResponse, basic::BasicErrorResponseType};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use snafu::Location;
use strum::IntoStaticStr;
use tokio::sync::oneshot;
use tracing::{debug, error};
use wreq::{Response, StatusCode, header::InvalidHeaderValue};

use crate::config::Reason;

#[derive(Debug, IntoStaticStr, snafu::Snafu)]
#[snafu(visibility(pub(crate)))]
#[strum(serialize_all = "snake_case")]
pub enum ClewdrError {
    #[snafu(display("HTTP error: {}, at: {}", source, loc))]
    #[snafu(context(false))]
    HttpError {
        #[snafu(implicit)]
        loc: Location,
        source: http::Error,
    },
    #[snafu(display("Ractor error: {}", msg))]
    RactorError {
        #[snafu(implicit)]
        loc: Location,
        msg: String,
    },
    #[snafu(display("Error requesting token: {}", source))]
    #[snafu(context(false))]
    RequestTokenError {
        #[snafu(implicit)]
        loc: Location,
        source: RequestTokenError<
            oauth2::HttpClientError<wreq::Error>,
            StandardErrorResponse<BasicErrorResponseType>,
        >,
    },
    #[snafu(display("URL parse error: {}, at: {}", source, loc))]
    UrlError {
        #[snafu(implicit)]
        loc: Location,
        url: String,
        source: url::ParseError,
    },
    #[snafu(display("Parse cookie error: {}, at: {}", msg, loc))]
    ParseCookieError {
        #[snafu(implicit)]
        loc: Location,
        msg: &'static str,
    },
    #[snafu(display("Invalid URI: {}", uri))]
    InvalidUri {
        uri: String,
        source: http::uri::InvalidUri,
    },
    #[snafu(display("Empty choices"))]
    EmptyChoices,
    #[snafu(display("JSON error: {}", source))]
    #[snafu(context(false))]
    JsonError { source: serde_json::Error },
    #[snafu(transparent)]
    PathRejection { source: PathRejection },
    #[snafu(transparent)]
    QueryRejection { source: QueryRejection },
    #[snafu(display("Test Message"))]
    TestMessage,
    #[snafu(display("FmtError: {}", source))]
    #[snafu(context(false))]
    FmtError {
        #[snafu(implicit)]
        loc: Location,
        source: std::fmt::Error,
    },
    #[snafu(display("Invalid header value: {}", source))]
    #[snafu(context(false))]
    InvalidHeaderValue { source: InvalidHeaderValue },
    #[snafu(display("Bad request: {}", msg))]
    BadRequest { msg: &'static str },
    #[snafu(display("Retries exceeded"))]
    TooManyRetries,
    #[snafu(display("EventSource error: {}", source))]
    #[snafu(context(false))]
    EventSourceAxumError {
        source: eventsource_stream::EventStreamError<axum::Error>,
    },
    #[snafu(context(false))]
    EventSourceRquestError {
        source: eventsource_stream::EventStreamError<wreq::Error>,
    },
    #[snafu(display("Zip error: {}", source))]
    #[snafu(context(false))]
    #[cfg(feature = "portable")]
    ZipError { source: zip::result::ZipError },
    #[snafu(display("Asset Error: {}", msg))]
    AssetError { msg: String },
    #[snafu(display("Invalid version: {}", version))]
    InvalidVersion { version: String },
    #[snafu(display("ParseInt error: {}", source))]
    #[snafu(context(false))]
    ParseIntError { source: std::num::ParseIntError },
    #[snafu(display("Account dispatch error: {}", source))]
    #[snafu(context(false))]
    AccountDispatchError { source: oneshot::error::RecvError },
    #[snafu(display("All upstream accounts are temporarily unavailable"))]
    UpstreamCoolingDown,
    #[snafu(display("No valid upstream accounts available"))]
    NoValidUpstreamAccounts,
    #[snafu(display("{}", reason))]
    #[snafu(context(false))]
    InvalidCookie {
        #[snafu(source)]
        reason: Reason,
    },
    #[snafu(display("Failed to parse TOML: {}", source))]
    #[snafu(context(false))]
    TomlDeError { source: toml::de::Error },
    #[snafu(transparent)]
    TomlSeError { source: toml::ser::Error },
    #[snafu(transparent)]
    JsonRejection { source: JsonRejection },
    #[snafu(display("Rquest error: {}, source: {}", msg, source))]
    WreqError {
        msg: &'static str,
        source: wreq::Error,
    },
    #[snafu(display("UTF-8 error: {}", source))]
    #[snafu(context(false))]
    UTF8Error {
        #[snafu(implicit)]
        loc: Location,
        source: std::string::FromUtf8Error,
    },
    #[snafu(display("Http error: code: {}, body: {}", code.to_string().red(), inner.to_string()))]
    ClaudeHttpError {
        code: StatusCode,
        // Boxed because `ClaudeErrorBody` carries Step 3.5 C3c diagnostic
        // metadata fields and pushes the variant past clippy's
        // `result_large_err` threshold otherwise.
        inner: Box<ClaudeErrorBody>,
    },
    #[snafu(display("Unexpected None: {}", msg))]
    UnexpectedNone { msg: &'static str },
    #[snafu(display("IO error: {}", source))]
    #[snafu(context(false))]
    IoError {
        #[snafu(implicit)]
        loc: Location,
        source: std::io::Error,
    },
    #[snafu(display("{}", msg))]
    PathNotFound { msg: String },
    #[snafu(display("Invalid timestamp: {}", timestamp))]
    TimestampError { timestamp: i64 },
    #[snafu(display("Key/Password Invalid"))]
    InvalidAuth,
    #[snafu(display("User concurrency limit exceeded"))]
    UserConcurrencyExceeded,
    #[snafu(display("Request rate limit exceeded"))]
    RpmExceeded,
    #[snafu(display("Usage quota exceeded"))]
    QuotaExceeded,

    #[snafu(display("Not found: {}", msg))]
    NotFound { msg: &'static str },
    #[snafu(display("Conflict: {}", msg))]
    Conflict { msg: &'static str },
    #[snafu(display("Conflict: {}", msg))]
    ConflictMessage { msg: String },
    #[snafu(display("Database error: {}", source))]
    #[snafu(context(false))]
    SqlxError {
        #[snafu(implicit)]
        loc: Location,
        source: sqlx::Error,
    },
    #[snafu(display("Migration error: {}", source))]
    #[snafu(context(false))]
    MigrateError {
        #[snafu(implicit)]
        loc: Location,
        source: sqlx::migrate::MigrateError,
    },
    #[snafu(whatever, display("{}: {}", message, source.as_ref().map_or_else(|| "Unknown error".into(), |e| e.to_string())))]
    Whatever {
        message: String,
        #[snafu(source(from(Box<dyn std::error::Error + Send>, Some)))]
        source: Option<Box<dyn std::error::Error + Send>>,
    },
}

pub fn display_account_invalid_reason(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    Reason::from_db_string_checked(trimmed)
        .map(|reason| reason.to_string())
        .unwrap_or_else(|| sanitize_account_error_message(trimmed))
}

pub fn sanitize_account_error_message(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let replacements = [
        ("Invalid Cookie: Null", Reason::Null.to_string()),
        ("Invalid Cookie: Banned", Reason::Banned.to_string()),
        ("Invalid Cookie: Free account", Reason::Free.to_string()),
        (
            "Invalid Cookie: Organization Disabled",
            Reason::Disabled.to_string(),
        ),
    ];
    let mut message = trimmed.to_string();
    for (legacy, replacement) in replacements {
        message = message.replace(legacy, &replacement);
    }
    message = message.replace(
        "Invalid Cookie: Restricted/Warning: until",
        "Account restricted until",
    );
    message = message.replace(
        "Invalid Cookie: 429 Too many request: until",
        "Account cooling down until",
    );
    message
}

impl IntoResponse for ClewdrError {
    fn into_response(self) -> axum::response::Response {
        // Step 3.5 C3a: account-class errors no longer collapse to the
        // snake_case enum name `invalid_cookie` on the API response.
        // The classifier produces a stable `normalized_reason` that
        // distinguishes oauth_refresh_invalid / organization_disabled /
        // rate_limited / etc., and we expose it as the response
        // `type` field.
        //
        // Step 3.5 C3c: also surface `failure_source / failure_stage /
        // upstream_http_status / normalized_reason_type` as additive
        // metadata on the response body. We deliberately do NOT
        // surface `raw_message` here — `err.to_string()` is the
        // sanitized display form callers should rely on, and raw
        // upstream bodies may carry sensitive context. raw_message is
        // only persisted in the admin-domain `AccountHealth.last_failure`.
        if matches!(&self, ClewdrError::InvalidCookie { .. }) {
            use crate::services::account_error::{FailureSource, classify_account_failure};
            let ctx = classify_account_failure(&self, FailureSource::Messages, None);
            let inner = ClaudeErrorBody {
                message: json!(self.to_string()),
                r#type: ctx.normalized_reason.as_type_str().to_string(),
                code: Some(StatusCode::BAD_REQUEST.as_u16()),
                failure_source: Some(ctx.source.as_str().to_string()),
                failure_stage: ctx.stage.map(|s| s.to_string()),
                upstream_http_status: ctx.upstream_http_status,
                normalized_reason_type: Some(ctx.normalized_reason.as_type_str().to_string()),
            };
            return (StatusCode::BAD_REQUEST, Json(ClaudeError { error: inner })).into_response();
        }

        let (status, msg) = match self {
            ClewdrError::UrlError {
                loc,
                source,
                ref url,
            } => (
                StatusCode::BAD_REQUEST,
                json!(format!("{}: {} (URL: {})", loc, source, url)),
            ),
            ClewdrError::ParseCookieError { .. } => {
                (StatusCode::BAD_REQUEST, json!(self.to_string()))
            }
            ClewdrError::InvalidUri { .. } => (StatusCode::BAD_REQUEST, json!(self.to_string())),
            ClewdrError::PathRejection { ref source } => {
                (source.status(), json!(source.body_text()))
            }
            ClewdrError::QueryRejection { ref source } => {
                (source.status(), json!(source.body_text()))
            }
            ClewdrError::ClaudeHttpError { code, inner } => {
                return (code, Json(ClaudeError { error: *inner })).into_response();
            }
            ClewdrError::TestMessage => {
                return (
                    StatusCode::OK,
                    Json(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "text", "text": "Claude Reverse Proxy is working, please send a real message."}],
                    })),
                )
                    .into_response();
            }
            ClewdrError::JsonRejection { ref source } => {
                (source.status(), json!(source.body_text()))
            }
            ClewdrError::TooManyRetries => (StatusCode::GATEWAY_TIMEOUT, json!(self.to_string())),
            ClewdrError::PathNotFound { .. } => (StatusCode::NOT_FOUND, json!(self.to_string())),
            ClewdrError::InvalidAuth => (StatusCode::UNAUTHORIZED, json!(self.to_string())),
            ClewdrError::UpstreamCoolingDown
            | ClewdrError::UserConcurrencyExceeded
            | ClewdrError::RpmExceeded
            | ClewdrError::QuotaExceeded => {
                let inner = ClaudeErrorBody {
                    message: json!(self.to_string()),
                    r#type: "rate_limit_error".to_string(),
                    code: Some(429),
                    ..Default::default()
                };
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(ClaudeError { error: inner }),
                )
                    .into_response();
            }
            ClewdrError::NoValidUpstreamAccounts => {
                let inner = ClaudeErrorBody {
                    message: json!(self.to_string()),
                    r#type: "api_error".to_string(),
                    code: Some(503),
                    ..Default::default()
                };
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ClaudeError { error: inner }),
                )
                    .into_response();
            }
            ClewdrError::NotFound { .. } => (StatusCode::NOT_FOUND, json!(self.to_string())),
            ClewdrError::Conflict { .. } | ClewdrError::ConflictMessage { .. } => {
                (StatusCode::CONFLICT, json!(self.to_string()))
            }
            ClewdrError::BadRequest { .. } => (StatusCode::BAD_REQUEST, json!(self.to_string())),
            ClewdrError::InvalidHeaderValue { .. } => {
                (StatusCode::BAD_REQUEST, json!(self.to_string()))
            }
            ClewdrError::EmptyChoices => (StatusCode::NO_CONTENT, json!(self.to_string())),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, json!(self.to_string())),
        };
        let err = ClaudeError {
            error: ClaudeErrorBody {
                message: msg,
                r#type: <&str>::from(self).into(),
                code: Some(status.as_u16()),
                ..Default::default()
            },
        };
        (status, Json(err)).into_response()
    }
}

/// HTTP error response
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClaudeError {
    pub error: ClaudeErrorBody,
}

/// Inner HTTP error response
#[derive(Debug, Serialize, Clone, Default)]
pub struct ClaudeErrorBody {
    pub message: Value,
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<u16>,
    /// Step 3.5 C3c: classifier `FailureSource` snake_case name. Only
    /// populated when this body is the `InvalidCookie` IntoResponse path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_source: Option<String>,
    /// Step 3.5 C3c: optional sub-stage refinement (e.g., "refresh",
    /// "profile"). Only populated alongside `failure_source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_stage: Option<String>,
    /// Step 3.5 C3c: HTTP status from the upstream (Anthropic / OAuth)
    /// when the failure carried one. Distinct from `code`, which is the
    /// status of *our* response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_http_status: Option<u16>,
    /// Step 3.5 C3c: stable snake_case name from
    /// `AccountNormalizedReason::as_type_str()`. This duplicates the
    /// `type` field on the `InvalidCookie` path (where they are equal),
    /// but exists as its own field so consumers can read it without
    /// ambiguity even if `type` is repurposed later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_reason_type: Option<String>,
}

/// Raw Inner HTTP error response
#[derive(Debug, Deserialize)]
struct RawBody {
    pub message: String,
    pub r#type: String,
    #[serde(default)]
    pub failure_source: Option<String>,
    #[serde(default)]
    pub failure_stage: Option<String>,
    #[serde(default)]
    pub upstream_http_status: Option<u16>,
    #[serde(default)]
    pub normalized_reason_type: Option<String>,
}

impl<'de> Deserialize<'de> for ClaudeErrorBody {
    /// when message is a json string, try parse it as a object
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawBody::deserialize(deserializer)?;
        let message =
            serde_json::from_str::<Value>(&raw.message).unwrap_or_else(|_| json!(raw.message));
        Ok(ClaudeErrorBody {
            message,
            r#type: raw.r#type,
            code: None,
            failure_source: raw.failure_source,
            failure_stage: raw.failure_stage,
            upstream_http_status: raw.upstream_http_status,
            normalized_reason_type: raw.normalized_reason_type,
        })
    }
}

impl Display for ClaudeErrorBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        serde_json::to_string_pretty(self)
            .map_err(|_| std::fmt::Error)?
            .fmt(f)
    }
}

pub trait CheckClaudeErr
where
    Self: Sized,
{
    fn check_claude(self) -> impl Future<Output = Result<Self, ClewdrError>>;
}

impl CheckClaudeErr for Response {
    /// Checks response from Claude Web API for errors
    /// Validates HTTP status codes and parses error messages from responses
    ///
    /// # Arguments
    /// * `res` - The HTTP response to check
    ///
    /// # Returns
    /// * `Ok(Response)` if the request was successful
    /// * `Err(ClewdrError)` if the request failed, with details about the failure
    async fn check_claude(self) -> Result<Self, ClewdrError> {
        let status = self.status();
        if status.is_success() {
            return Ok(self);
        }
        let reset_header = self
            .headers()
            .get("anthropic-ratelimit-unified-reset")
            .cloned();
        debug!("Error response status: {}", status);
        if status == 302 {
            // blocked by cloudflare
            let error = ClaudeErrorBody {
                message: json!("Blocked, check your IP address"),
                r#type: "error".to_string(),
                code: Some(status.as_u16()),
                ..Default::default()
            };
            return Err(ClewdrError::ClaudeHttpError {
                code: status,
                inner: Box::new(error),
            });
        }
        let text = match self.text().await {
            Ok(text) => text,
            Err(err) => {
                let error = ClaudeErrorBody {
                    message: json!(err.to_string()),
                    r#type: "error_get_error_body".to_string(),
                    code: Some(status.as_u16()),
                    ..Default::default()
                };
                return Err(ClewdrError::ClaudeHttpError {
                    code: status,
                    inner: Box::new(error),
                });
            }
        };
        let Ok(err) = serde_json::from_str::<ClaudeError>(&text) else {
            let error = ClaudeErrorBody {
                message: format!("Unknown error: {text}").into(),
                r#type: "error_parse_error_body".to_string(),
                code: Some(status.as_u16()),
                ..Default::default()
            };
            return Err(ClewdrError::ClaudeHttpError {
                code: status,
                inner: Box::new(error),
            });
        };
        const OAUTH_PHRASE: &str =
            "oauth authentication is currently not allowed for this organization";
        let msg_lower = err
            .error
            .message
            .as_str()
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_else(|| err.error.message.to_string().to_ascii_lowercase());
        if status == 400 && msg_lower.contains("organization has been disabled") {
            // account disabled
            return Err(Reason::Disabled.into());
        }
        if (status == 401 || status == 403) && msg_lower.contains(OAUTH_PHRASE) {
            return Err(Reason::Null.into());
        }
        let inner_error = err.error;
        // check if the error is a rate limit error
        if status == 429 {
            // Some Anthropic feature-gating errors also use 429; keep them as HTTP
            // errors instead of cooling down the account as if it were rate-limited.
            let msg_lower = inner_error
                .message
                .as_str()
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_else(|| inner_error.message.to_string().to_ascii_lowercase());
            if msg_lower.contains("extra usage is required for long context requests") {
                return Err(ClewdrError::ClaudeHttpError {
                    code: status,
                    inner: Box::new(inner_error),
                });
            }

            // get the reset time from the error message
            let ts = inner_error.message["resetsAt"]
                .as_i64()
                .or_else(|| reset_header.and_then(|h| h.to_str().ok()?.parse::<i64>().ok()));
            if let Some(ts) = ts {
                let reset_time = chrono::DateTime::from_timestamp(ts, 0)
                    .ok_or(ClewdrError::TimestampError { timestamp: ts })?
                    .to_utc();
                let now = chrono::Utc::now();
                let diff = reset_time - now;
                let mins = diff.num_minutes();
                error!(
                    "Rate limit exceeded, expires in {} hours",
                    mins as f64 / 60.0
                );
                return Err(ClewdrError::InvalidCookie {
                    reason: Reason::TooManyRequest(ts),
                });
            } else {
                error!("Rate limit exceeded, but no reset time provided");
                return Err(ClewdrError::InvalidCookie {
                    reason: Reason::TooManyRequest(Utc::now().timestamp() + 18000),
                });
            }
        }
        Err(ClewdrError::ClaudeHttpError {
            code: status,
            inner: Box::new(inner_error),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClaudeError, ClewdrError, display_account_invalid_reason, sanitize_account_error_message,
    };
    use crate::config::Reason;
    use axum::response::IntoResponse;

    #[test]
    fn sanitizes_legacy_invalid_cookie_messages() {
        assert_eq!(
            sanitize_account_error_message("Invalid Cookie: Null"),
            "Account unavailable"
        );
        assert_eq!(
            sanitize_account_error_message("OAuth: Invalid Cookie: Banned"),
            "OAuth: Account banned"
        );
        assert_eq!(
            sanitize_account_error_message(
                "Invalid Cookie: 429 Too many request: until UTC 2026-04-21 11:00:00",
            ),
            "Account cooling down until UTC 2026-04-21 11:00:00"
        );
    }

    #[test]
    fn renders_stored_invalid_reasons_as_account_messages() {
        assert_eq!(
            display_account_invalid_reason("null"),
            "Account unavailable"
        );
        assert_eq!(display_account_invalid_reason("banned"), "Account banned");
        assert_eq!(
            display_account_invalid_reason("too_many_request:1735689600"),
            "Account cooling down until UTC 2025-01-01 00:00:00"
        );
    }

    /// Step 3.5 C3a: IntoResponse for `InvalidCookie` no longer emits the
    /// snake_case enum name `invalid_cookie` as the response `type`.
    /// Instead each Reason maps to a stable `AccountNormalizedReason`
    /// snake_case name that distinguishes the failure cause.
    async fn invalid_cookie_response_type(reason: Reason) -> String {
        let err = ClewdrError::InvalidCookie { reason };
        let response = err.into_response();
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let parsed: ClaudeError = serde_json::from_slice(&body).unwrap();
        parsed.error.r#type
    }

    /// Step 3.5 C3c helper: full body so tests can assert metadata fields.
    async fn invalid_cookie_response_body(reason: Reason) -> super::ClaudeErrorBody {
        let err = ClewdrError::InvalidCookie { reason };
        let response = err.into_response();
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let parsed: ClaudeError = serde_json::from_slice(&body).unwrap();
        parsed.error
    }

    #[tokio::test]
    async fn into_response_invalid_cookie_emits_normalized_type() {
        // Each Reason produces a distinct `type` string — none of them
        // collapse to "invalid_cookie" any more.
        assert_eq!(
            invalid_cookie_response_type(Reason::Disabled).await,
            "organization_disabled"
        );
        assert_eq!(
            invalid_cookie_response_type(Reason::Banned).await,
            "account_banned"
        );
        assert_eq!(
            invalid_cookie_response_type(Reason::Free).await,
            "free_tier"
        );
        assert_eq!(
            invalid_cookie_response_type(Reason::TooManyRequest(123)).await,
            "rate_limited"
        );
        assert_eq!(
            invalid_cookie_response_type(Reason::Restricted(456)).await,
            "restricted"
        );
        // Reason::Null with default source falls back to the
        // catch-all oauth-org-not-allowed bucket; still NOT
        // "invalid_cookie".
        let null_type = invalid_cookie_response_type(Reason::Null).await;
        assert_ne!(null_type, "invalid_cookie");
        assert!(!null_type.is_empty());
    }

    /// Step 3.5 C3c: InvalidCookie IntoResponse populates additive
    /// diagnostic fields (`failure_source`, `normalized_reason_type`)
    /// on the response body. raw_message is intentionally NOT carried
    /// to the user-facing API.
    #[tokio::test]
    async fn into_response_invalid_cookie_emits_failure_metadata() {
        let body = invalid_cookie_response_body(Reason::TooManyRequest(123)).await;
        // failure_source is always "messages" on this path — the
        // IntoResponse impl uses the Messages default since it cannot
        // see the calling entry point.
        assert_eq!(body.failure_source.as_deref(), Some("messages"));
        // normalized_reason_type duplicates `type` field for stable
        // consumer access.
        assert_eq!(body.normalized_reason_type.as_deref(), Some("rate_limited"));
        assert_eq!(body.r#type, "rate_limited");
        // failure_stage is None for the messages path (no sub-stage).
        assert!(body.failure_stage.is_none());
    }

    /// Step 3.5 C3c: 401/403 ClaudeHttpError that's not an account
    /// failure does not get the normalized metadata — only the
    /// InvalidCookie path does.
    #[tokio::test]
    async fn into_response_non_account_error_skips_failure_metadata() {
        let err = ClewdrError::NoValidUpstreamAccounts;
        let response = err.into_response();
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let parsed: ClaudeError = serde_json::from_slice(&body).unwrap();
        assert!(parsed.error.failure_source.is_none());
        assert!(parsed.error.failure_stage.is_none());
        assert!(parsed.error.upstream_http_status.is_none());
        assert!(parsed.error.normalized_reason_type.is_none());
    }

    /// Step 3.5 C3c: round-trip serde — `RawBody` accepts the new
    /// fields and `ClaudeErrorBody::deserialize` restores them.
    #[test]
    fn claude_error_body_round_trips_failure_metadata() {
        let body = super::ClaudeErrorBody {
            message: serde_json::json!("hello"),
            r#type: "rate_limited".to_string(),
            code: Some(400),
            failure_source: Some("messages".to_string()),
            failure_stage: Some("refresh".to_string()),
            upstream_http_status: Some(429),
            normalized_reason_type: Some("rate_limited".to_string()),
        };
        let json = serde_json::to_string(&body).unwrap();
        let restored: super::ClaudeErrorBody = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.failure_source.as_deref(), Some("messages"));
        assert_eq!(restored.failure_stage.as_deref(), Some("refresh"));
        assert_eq!(restored.upstream_http_status, Some(429));
        assert_eq!(
            restored.normalized_reason_type.as_deref(),
            Some("rate_limited")
        );
    }
}
