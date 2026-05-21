//! Shared API helpers reused by Anthropic and OpenAI surfaces.
//!
//! The error → log mapping and the terminal request-log persistence are
//! kept here so the `/v1/messages` and `/v1/chat/completions` paths report
//! identical statuses for the same upstream signal.

use crate::{
    billing::{BillingContext, RequestType, TerminalLogOptions, persist_terminal_request_log},
    error::ClewdrError,
    middleware::claude::ClaudeContext,
    services::account_error::{AccountFailureAction, FailureSource, classify_account_failure},
    state::AppState,
};

pub fn error_to_log_status(err: &ClewdrError) -> (&'static str, u16) {
    // Non-account variants own their status / http_status — these are
    // user-side limits and pool-level signals, not upstream account
    // failures, so they bypass the classifier.
    match err {
        ClewdrError::QuotaExceeded => return ("quota_rejected", 429),
        ClewdrError::UserConcurrencyExceeded => return ("user_concurrency_rejected", 429),
        ClewdrError::RpmExceeded => return ("rpm_rejected", 429),
        ClewdrError::UpstreamCoolingDown => return ("no_account_available", 429),
        ClewdrError::NoValidUpstreamAccounts => return ("no_account_available", 503),
        ClewdrError::TooManyRetries => return ("no_account_available", 504),
        ClewdrError::UpstreamTimeout { .. } => return ("upstream_error", 504),
        _ => {}
    }

    // Account-side failures: route through the unified classifier so
    // every entry point (messages / count_tokens / test / probe /
    // refresh) reports the same status for the same upstream signal.
    let ctx = classify_account_failure(err, FailureSource::Messages, None);
    let status = ctx.action.to_log_status();
    let http_status = match ctx.action {
        AccountFailureAction::TerminalAuth | AccountFailureAction::TerminalDisabled => {
            // Preserve the upstream HTTP status when we have one (e.g.
            // 401/403 from a bare ClaudeHttpError). InvalidCookie carries
            // no upstream status, fall back to 400.
            ctx.upstream_http_status.unwrap_or(400)
        }
        AccountFailureAction::Cooldown { .. } => 429,
        AccountFailureAction::TransientUpstream => ctx.upstream_http_status.unwrap_or(502),
        AccountFailureAction::InternalError => 500,
    };
    (status, http_status)
}

pub async fn log_error_request(
    state: &AppState,
    ctx: &ClaudeContext,
    request_type: RequestType,
    status: &'static str,
    http_status: u16,
    err_msg: &str,
) {
    let billing_ctx = BillingContext {
        db: state.db.clone(),
        user_id: ctx.user_id,
        api_key_id: ctx.api_key_id,
        account_id: ctx.selected_account_id(),
        model_raw: ctx.model_raw.clone(),
        request_id: ctx.request_id.clone(),
        started_at: ctx.started_at,
        event_tx: state.event_tx.clone(),
    };
    persist_terminal_request_log(
        &billing_ctx,
        TerminalLogOptions {
            request_type,
            stream: ctx.stream,
            status,
            http_status: Some(http_status),
            usage: None,
            error_code: Some(status),
            error_message: Some(err_msg),
            update_rollups: false,
            response_body: None,
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::error_to_log_status;
    use crate::{
        config::Reason,
        error::{ClaudeErrorBody, ClewdrError},
    };
    use serde_json::json;
    use wreq::StatusCode;

    fn http(status: u16) -> ClewdrError {
        ClewdrError::ClaudeHttpError {
            code: StatusCode::from_u16(status).unwrap(),
            inner: Box::new(ClaudeErrorBody {
                message: json!("upstream"),
                r#type: "error".to_string(),
                code: Some(status),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn cooldown_invalid_cookie_logs_as_no_account_available() {
        assert_eq!(
            error_to_log_status(&ClewdrError::InvalidCookie {
                reason: Reason::TooManyRequest(123),
            }),
            ("no_account_available", 429)
        );
        assert_eq!(
            error_to_log_status(&ClewdrError::InvalidCookie {
                reason: Reason::Restricted(456),
            }),
            ("no_account_available", 429)
        );
    }

    #[test]
    fn http_401_403_logs_as_auth_rejected_with_upstream_status() {
        assert_eq!(error_to_log_status(&http(401)), ("auth_rejected", 401));
        assert_eq!(error_to_log_status(&http(403)), ("auth_rejected", 403));
    }

    #[test]
    fn http_500_logs_as_upstream_error_with_upstream_status() {
        assert_eq!(error_to_log_status(&http(500)), ("upstream_error", 500));
        assert_eq!(error_to_log_status(&http(502)), ("upstream_error", 502));
    }

    #[test]
    fn invalid_cookie_terminal_classes_log_as_auth_rejected_400() {
        assert_eq!(
            error_to_log_status(&ClewdrError::InvalidCookie {
                reason: Reason::Null,
            }),
            ("auth_rejected", 400)
        );
        assert_eq!(
            error_to_log_status(&ClewdrError::InvalidCookie {
                reason: Reason::Disabled,
            }),
            ("auth_rejected", 400)
        );
        assert_eq!(
            error_to_log_status(&ClewdrError::InvalidCookie {
                reason: Reason::Free,
            }),
            ("auth_rejected", 400)
        );
    }

    #[test]
    fn user_side_quota_variants_keep_their_dedicated_status() {
        assert_eq!(
            error_to_log_status(&ClewdrError::QuotaExceeded),
            ("quota_rejected", 429)
        );
        assert_eq!(
            error_to_log_status(&ClewdrError::RpmExceeded),
            ("rpm_rejected", 429)
        );
        assert_eq!(
            error_to_log_status(&ClewdrError::UserConcurrencyExceeded),
            ("user_concurrency_rejected", 429)
        );
    }

    #[test]
    fn oauth_refresh_invalid_grant_logs_as_auth_rejected() {
        let err = ClewdrError::Whatever {
            message: "oauth refresh failed: invalid_grant".to_string(),
            source: None,
        };
        assert_eq!(error_to_log_status(&err), ("auth_rejected", 400));
    }
}
