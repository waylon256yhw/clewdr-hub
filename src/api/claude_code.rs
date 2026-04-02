use axum::{Extension, extract::State, response::Response};
use chrono::Utc;
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::warn;

use crate::{
    billing::check_quota,
    db::billing::{insert_request_log, RequestLogRow},
    error::ClewdrError,
    middleware::claude::{ClaudeCodePreprocess, ClaudeContext},
    providers::{
        LLMProvider,
        claude::{ClaudeInvocation, ClaudeProviderResponse},
    },
    state::AppState,
};

fn error_to_log_status(err: &ClewdrError) -> Option<(&'static str, u16)> {
    match err {
        ClewdrError::QuotaExceeded => Some(("quota_rejected", 429)),
        ClewdrError::UserConcurrencyExceeded => Some(("user_concurrency_rejected", 429)),
        ClewdrError::RpmExceeded => Some(("rpm_rejected", 429)),
        ClewdrError::BoundAccountsUnavailable => Some(("no_account_available", 429)),
        ClewdrError::TooManyRetries => Some(("no_account_available", 504)),
        ClewdrError::ClaudeHttpError { code, .. } => Some(("upstream_error", code.as_u16())),
        _ => None,
    }
}

async fn log_error_request(
    db: &SqlitePool,
    ctx: &ClaudeContext,
    status: &'static str,
    http_status: u16,
    err_msg: &str,
    event_tx: &broadcast::Sender<()>,
) {
    let now = Utc::now();
    let started_at = ctx.started_at.to_rfc3339();
    let completed_at = now.to_rfc3339();
    let duration_ms = (now - ctx.started_at).num_milliseconds();

    let log = RequestLogRow {
        request_id: &ctx.request_id,
        request_type: "messages",
        user_id: ctx.user_id,
        api_key_id: ctx.api_key_id,
        account_id: None,
        model_raw: &ctx.model_raw,
        model_normalized: None,
        stream: ctx.stream,
        started_at: &started_at,
        completed_at: Some(&completed_at),
        duration_ms: Some(duration_ms),
        ttft_ms: None,
        status,
        http_status: Some(http_status),
        input_tokens: None,
        output_tokens: None,
        cache_creation_tokens: None,
        cache_read_tokens: None,
        priced_input_nanousd_per_token: None,
        priced_output_nanousd_per_token: None,
        cost_nanousd: 0,
        error_code: Some(status),
        error_message: Some(err_msg),
    };

    if let Err(e) = insert_request_log(db, &log).await {
        warn!("Failed to insert error request log: {e}");
    } else {
        let _ = event_tx.send(());
    }
}

pub async fn api_claude_code(
    State(state): State<AppState>,
    ClaudeCodePreprocess(params, context): ClaudeCodePreprocess,
) -> Result<(Extension<ClaudeContext>, Response), ClewdrError> {
    let db = &state.db;
    let event_tx = &state.event_tx;

    if let Some(user_id) = context.user_id {
        if let Err(e) = check_quota(
            db,
            user_id,
            context.weekly_budget_nanousd,
            context.monthly_budget_nanousd,
        )
        .await
        {
            let err_msg = e.to_string();
            log_error_request(db, &context, "quota_rejected", 429, &err_msg, event_tx).await;
            return Err(e);
        }
    }

    let permit = if let (Some(user_id), Some(max_c), Some(rpm)) =
        (context.user_id, context.max_concurrent, context.rpm_limit)
    {
        match state.user_limiter.acquire(user_id, max_c, rpm).await {
            Ok(permit) => Some(permit),
            Err(e) => {
                if let Some((status, http_status)) = error_to_log_status(&e) {
                    let err_msg = e.to_string();
                    log_error_request(db, &context, status, http_status, &err_msg, event_tx).await;
                }
                return Err(e);
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
            let mut response = response;
            if let Some(permit) = permit {
                response.extensions_mut().insert(permit);
            }
            Ok((Extension(context), response))
        }
        Err(e) => {
            if let Some((status, http_status)) = error_to_log_status(&e) {
                let err_msg = e.to_string();
                log_error_request(db, &context, status, http_status, &err_msg, event_tx).await;
            }
            Err(e)
        }
    }
}

pub async fn api_claude_code_count_tokens(
    State(state): State<AppState>,
    ClaudeCodePreprocess(mut params, context): ClaudeCodePreprocess,
) -> Result<Response, ClewdrError> {
    let _permit = if let (Some(user_id), Some(max_c), Some(rpm)) =
        (context.user_id, context.max_concurrent, context.rpm_limit)
    {
        Some(state.user_limiter.acquire(user_id, max_c, rpm).await?)
    } else {
        None
    };

    params.stream = Some(false);
    let ClaudeProviderResponse { response, .. } = state
        .code_provider
        .invoke(ClaudeInvocation::count_tokens(params, context))
        .await?;
    Ok(response)
}
