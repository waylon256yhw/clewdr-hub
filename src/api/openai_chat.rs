//! `POST /v1/chat/completions` — OpenAI-compatible chat completions handler.
//!
//! Mirrors [`crate::api::api_claude_code`]: same quota / limiter / provider
//! flow, but the request is translated through the OpenAI extractor and the
//! response body is rewrapped in OpenAI shape (non-stream JSON or SSE
//! chunk stream). Request logs continue to use
//! [`RequestType::Messages`] so the existing admin filters and ops
//! statistics remain unchanged.

use axum::{
    Extension, Json,
    extract::State,
    response::{IntoResponse, Response},
};
use http::StatusCode;
use serde_json::{Value, json};

use crate::{
    api::common::{error_to_log_status, log_error_request},
    billing::{RequestType, check_quota},
    error::ClewdrError,
    middleware::{
        claude::ClaudeContext,
        openai::{
            OpenAIChatPreprocess, anthropic_type_to_oai, to_openai_non_stream,
            to_openai_non_stream_keepalive, to_openai_stream,
        },
    },
    providers::{
        LLMProvider,
        claude::{ClaudeInvocation, ClaudeProviderResponse},
    },
    state::AppState,
    types::openai::OpenAIErrorBody,
};

/// Newtype adapter that turns a [`ClewdrError`] into an OpenAI-shaped
/// `{ "error": {...} }` response. Used by [`api_openai_chat_completions`]
/// so the same upstream signal that produces an Anthropic-shape error on
/// `/v1/messages` produces the equivalent OpenAI-shape error here.
pub struct OpenAIError(pub ClewdrError);

impl From<ClewdrError> for OpenAIError {
    fn from(err: ClewdrError) -> Self {
        Self(err)
    }
}

impl IntoResponse for OpenAIError {
    fn into_response(self) -> Response {
        let (status, body) = build_openai_error_payload(self.0);
        (status, Json(json!({ "error": body }))).into_response()
    }
}

fn build_openai_error_payload(err: ClewdrError) -> (StatusCode, OpenAIErrorBody) {
    if let ClewdrError::ClaudeHttpError { code, inner } = &err {
        let body = OpenAIErrorBody {
            message: stringify_value(&inner.message),
            kind: anthropic_type_to_oai(&inner.r#type).to_string(),
            code: None,
            param: None,
        };
        return (*code, body);
    }

    let (log_status, http_status) = error_to_log_status(&err);
    let kind = log_status_to_oai_type(log_status);
    let body = OpenAIErrorBody {
        message: err.to_string(),
        kind: kind.to_string(),
        code: None,
        param: None,
    };
    let status = StatusCode::from_u16(http_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, body)
}

fn stringify_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn log_status_to_oai_type(status: &str) -> &'static str {
    match status {
        "quota_rejected"
        | "user_concurrency_rejected"
        | "rpm_rejected"
        | "no_account_available" => "rate_limit_exceeded",
        "auth_rejected" => "invalid_request_error",
        _ => "api_error",
    }
}

pub async fn api_openai_chat_completions(
    State(state): State<AppState>,
    OpenAIChatPreprocess {
        params,
        context,
        include_reasoning,
        include_usage,
    }: OpenAIChatPreprocess,
) -> Result<(Extension<ClaudeContext>, Response), OpenAIError> {
    let db = &state.db;

    if let Some(user_id) = context.user_id
        && let Err(e) = check_quota(
            db,
            user_id,
            context.weekly_budget_nanousd,
            context.monthly_budget_nanousd,
        )
        .await
    {
        let (status, http_status) = error_to_log_status(&e);
        let err_msg = e.to_string();
        log_error_request(
            &state,
            &context,
            RequestType::Messages,
            status,
            http_status,
            &err_msg,
        )
        .await;
        return Err(OpenAIError(e));
    }

    let permit = if let (Some(user_id), Some(max_c), Some(rpm)) =
        (context.user_id, context.max_concurrent, context.rpm_limit)
    {
        match state.user_limiter.acquire(user_id, max_c, rpm).await {
            Ok(permit) => Some(permit),
            Err(e) => {
                let (status, http_status) = error_to_log_status(&e);
                let err_msg = e.to_string();
                log_error_request(
                    &state,
                    &context,
                    RequestType::Messages,
                    status,
                    http_status,
                    &err_msg,
                )
                .await;
                return Err(OpenAIError(e));
            }
        }
    } else {
        None
    };

    match state
        .code_provider
        .invoke(ClaudeInvocation::messages(params, context.clone()))
        .await
    {
        Ok(ClaudeProviderResponse { context, response }) => {
            let mut oai_resp = if context.stream {
                to_openai_stream(
                    response,
                    include_reasoning,
                    include_usage,
                    context.model_raw.clone(),
                )
            } else if context.non_stream_keepalive {
                to_openai_non_stream_keepalive(response, include_reasoning)
            } else {
                to_openai_non_stream(response, include_reasoning).await
            };
            if let Some(permit) = permit {
                oai_resp.extensions_mut().insert(permit);
            }
            Ok((Extension(context), oai_resp))
        }
        Err(e) => {
            let (status, http_status) = error_to_log_status(&e);
            let err_msg = e.to_string();
            log_error_request(
                &state,
                &context,
                RequestType::Messages,
                status,
                http_status,
                &err_msg,
            )
            .await;
            Err(OpenAIError(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ClaudeErrorBody, ClewdrError};
    use wreq::StatusCode as UpstreamStatus;

    #[test]
    fn claude_http_error_preserves_status_and_type() {
        let err = ClewdrError::ClaudeHttpError {
            code: UpstreamStatus::TOO_MANY_REQUESTS,
            inner: Box::new(ClaudeErrorBody {
                message: json!("slow down"),
                r#type: "rate_limited".to_string(),
                code: Some(429),
                ..Default::default()
            }),
        };
        let (status, body) = build_openai_error_payload(err);
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body.kind, "rate_limit_exceeded");
        assert_eq!(body.message, "slow down");
    }

    #[test]
    fn quota_exceeded_maps_to_rate_limit_exceeded_429() {
        let (status, body) = build_openai_error_payload(ClewdrError::QuotaExceeded);
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body.kind, "rate_limit_exceeded");
    }

    #[test]
    fn no_valid_upstream_accounts_maps_to_rate_limit_exceeded_503() {
        let (status, body) = build_openai_error_payload(ClewdrError::NoValidUpstreamAccounts);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.kind, "rate_limit_exceeded");
    }

    #[test]
    fn upstream_timeout_maps_to_api_error_504() {
        let (status, body) =
            build_openai_error_payload(ClewdrError::UpstreamTimeout { msg: "timed out" });
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body.kind, "api_error");
    }

    #[test]
    fn claude_http_error_object_message_is_stringified() {
        let err = ClewdrError::ClaudeHttpError {
            code: UpstreamStatus::BAD_REQUEST,
            inner: Box::new(ClaudeErrorBody {
                message: json!({"detail": "bad"}),
                r#type: "invalid_request_error".to_string(),
                code: Some(400),
                ..Default::default()
            }),
        };
        let (_, body) = build_openai_error_payload(err);
        assert!(body.message.contains("detail"));
        assert_eq!(body.kind, "invalid_request_error");
    }
}
