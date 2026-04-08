use axum::{Extension, extract::State, response::Response};

use crate::{
    billing::{
        BillingContext, RequestType, TerminalLogOptions, check_quota, persist_terminal_request_log,
    },
    error::ClewdrError,
    middleware::claude::{ClaudeCodePreprocess, ClaudeContext},
    providers::{
        LLMProvider,
        claude::{ClaudeInvocation, ClaudeProviderResponse},
    },
    state::AppState,
};

fn error_to_log_status(err: &ClewdrError) -> (&'static str, u16) {
    match err {
        ClewdrError::QuotaExceeded => ("quota_rejected", 429),
        ClewdrError::UserConcurrencyExceeded => ("user_concurrency_rejected", 429),
        ClewdrError::RpmExceeded => ("rpm_rejected", 429),
        ClewdrError::NoCookieAvailable => ("no_account_available", 429),
        ClewdrError::BoundAccountsUnavailable => ("no_account_available", 429),
        ClewdrError::TooManyRetries => ("no_account_available", 504),
        ClewdrError::InvalidCookie { .. } => ("auth_rejected", 400),
        ClewdrError::ClaudeHttpError { code, .. } => ("upstream_error", code.as_u16()),
        _ => ("internal_error", 500),
    }
}

async fn log_error_request(
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
        account_id: None,
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

pub async fn api_claude_code(
    State(state): State<AppState>,
    ClaudeCodePreprocess(params, context): ClaudeCodePreprocess,
) -> Result<(Extension<ClaudeContext>, Response), ClewdrError> {
    let db = &state.db;

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
            log_error_request(
                &state,
                &context,
                RequestType::Messages,
                "quota_rejected",
                429,
                &err_msg,
            )
            .await;
            return Err(e);
        }
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
        match state.user_limiter.acquire(user_id, max_c, rpm).await {
            Ok(permit) => Some(permit),
            Err(e) => return Err(e),
        }
    } else {
        None
    };

    params.stream = Some(false);
    match state
        .code_provider
        .invoke(ClaudeInvocation::count_tokens(params, context.clone()))
        .await
    {
        Ok(ClaudeProviderResponse { response, .. }) => Ok(response),
        Err(e) => Err(e),
    }
}
