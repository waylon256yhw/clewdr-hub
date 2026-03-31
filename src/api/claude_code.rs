use std::sync::Arc;

use axum::{Extension, extract::State, response::Response};

use crate::{
    error::ClewdrError,
    middleware::claude::{ClaudeCodePreprocess, ClaudeContext},
    providers::{
        LLMProvider,
        claude::{ClaudeCodeProvider, ClaudeInvocation, ClaudeProviderResponse},
    },
    services::user_limiter::UserLimiterMap,
};

pub async fn api_claude_code(
    State(provider): State<Arc<ClaudeCodeProvider>>,
    State(limiter): State<UserLimiterMap>,
    ClaudeCodePreprocess(params, context): ClaudeCodePreprocess,
) -> Result<(Extension<ClaudeContext>, Response), ClewdrError> {
    // Acquire per-user concurrency permit + RPM check (None for legacy auth)
    let permit = if let (Some(user_id), Some(max_c), Some(rpm)) =
        (context.user_id, context.max_concurrent, context.rpm_limit)
    {
        Some(limiter.acquire(user_id, max_c, rpm).await?)
    } else {
        None
    };

    let ClaudeProviderResponse { context, response } = provider
        .invoke(ClaudeInvocation::messages(params, context.clone()))
        .await?;

    // Store permit in response extensions so it lives until body is consumed
    // (streaming: held until SSE stream ends; non-streaming: dropped after body read)
    let mut response = response;
    if let Some(permit) = permit {
        response.extensions_mut().insert(permit);
    }
    Ok((Extension(context), response))
}

pub async fn api_claude_code_count_tokens(
    State(provider): State<Arc<ClaudeCodeProvider>>,
    State(limiter): State<UserLimiterMap>,
    ClaudeCodePreprocess(mut params, context): ClaudeCodePreprocess,
) -> Result<Response, ClewdrError> {
    // count_tokens shares the same per-user limits
    let _permit = if let (Some(user_id), Some(max_c), Some(rpm)) =
        (context.user_id, context.max_concurrent, context.rpm_limit)
    {
        Some(limiter.acquire(user_id, max_c, rpm).await?)
    } else {
        None
    };

    params.stream = Some(false);
    let ClaudeProviderResponse { response, .. } = provider
        .invoke(ClaudeInvocation::count_tokens(params, context))
        .await?;
    Ok(response)
}
