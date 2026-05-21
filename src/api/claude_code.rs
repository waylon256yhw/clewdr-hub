use axum::{Extension, extract::State, response::Response};

use crate::{
    api::common::{error_to_log_status, log_error_request},
    billing::{RequestType, check_quota},
    error::ClewdrError,
    middleware::claude::{ClaudeCodePreprocess, ClaudeContext},
    providers::{
        LLMProvider,
        claude::{ClaudeInvocation, ClaudeProviderResponse},
    },
    state::AppState,
};

pub async fn api_claude_code(
    State(state): State<AppState>,
    ClaudeCodePreprocess(params, context): ClaudeCodePreprocess,
) -> Result<(Extension<ClaudeContext>, Response), ClewdrError> {
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
        return Err(e);
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
    if let (Some(user_id), Some(max_c), Some(rpm)) =
        (context.user_id, context.max_concurrent, context.rpm_limit)
    {
        match state.user_limiter.check_rpm(user_id, max_c, rpm).await {
            Ok(()) => {}
            Err(e) => return Err(e),
        }
    }

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
