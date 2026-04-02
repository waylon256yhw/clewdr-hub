use std::{sync::Arc, time::Instant};

use axum::response::Response;
use colored::Colorize;
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::info;

use super::LLMProvider;
use crate::{
    billing::BillingContext,
    claude_code_state::ClaudeCodeState,
    error::ClewdrError,
    middleware::claude::ClaudeContext,
    services::cookie_actor::CookieActorHandle,
    stealth::SharedStealthProfile,
    types::claude::CreateMessageParams,
    utils::{enabled, print_out_json},
};

#[derive(Clone, Copy)]
pub enum ClaudeOperation {
    Messages,
    CountTokens,
}

#[derive(Clone)]
pub struct ClaudeInvocation {
    pub params: CreateMessageParams,
    pub context: ClaudeContext,
    pub operation: ClaudeOperation,
}

impl ClaudeInvocation {
    pub fn messages(params: CreateMessageParams, context: ClaudeContext) -> Self {
        Self {
            params,
            context,
            operation: ClaudeOperation::Messages,
        }
    }

    pub fn count_tokens(params: CreateMessageParams, context: ClaudeContext) -> Self {
        Self {
            params,
            context,
            operation: ClaudeOperation::CountTokens,
        }
    }
}

pub struct ClaudeProviderResponse {
    pub context: ClaudeContext,
    pub response: Response,
}

struct ClaudeSharedState {
    cookie_actor_handle: CookieActorHandle,
    db: SqlitePool,
    stealth_profile: SharedStealthProfile,
    event_tx: broadcast::Sender<()>,
}

impl ClaudeSharedState {
    fn new(cookie_actor_handle: CookieActorHandle, db: SqlitePool, stealth_profile: SharedStealthProfile, event_tx: broadcast::Sender<()>) -> Self {
        Self {
            cookie_actor_handle,
            db,
            stealth_profile,
            event_tx,
        }
    }
}

#[derive(Clone)]
pub struct ClaudeProviders {
    code: Arc<ClaudeCodeProvider>,
}

impl ClaudeProviders {
    pub fn new(cookie_actor_handle: CookieActorHandle, db: SqlitePool, stealth_profile: SharedStealthProfile, event_tx: broadcast::Sender<()>) -> Self {
        let shared = Arc::new(ClaudeSharedState::new(cookie_actor_handle, db, stealth_profile, event_tx));
        let code = Arc::new(ClaudeCodeProvider::new(shared));
        Self { code }
    }

    pub fn code(&self) -> Arc<ClaudeCodeProvider> {
        self.code.clone()
    }
}

#[derive(Clone)]
pub struct ClaudeCodeProvider {
    shared: Arc<ClaudeSharedState>,
}

impl ClaudeCodeProvider {
    fn new(shared: Arc<ClaudeSharedState>) -> Self {
        Self { shared }
    }
}

#[async_trait::async_trait]
impl LLMProvider for ClaudeCodeProvider {
    type Request = ClaudeInvocation;
    type Output = ClaudeProviderResponse;

    async fn invoke(&self, request: Self::Request) -> Result<Self::Output, ClewdrError> {
        let mut state = ClaudeCodeState::new(
            self.shared.cookie_actor_handle.clone(),
            self.shared.stealth_profile.clone(),
        );
        state.stream = request.context.stream;
        state.system_prompt_hash = request.context.system_prompt_hash;
        state.usage = request.context.usage.to_owned();
        state.session_id = request.context.session_id.clone();
        state.bound_account_ids = request.context.bound_account_ids.clone();

        // Set billing context for cost tracking
        state.billing_ctx = Some(BillingContext {
            db: self.shared.db.clone(),
            user_id: request.context.user_id,
            api_key_id: request.context.api_key_id,
            account_id: None,
            model_raw: request.context.model_raw.clone(),
            request_id: request.context.request_id.clone(),
            started_at: request.context.started_at,
            event_tx: self.shared.event_tx.clone(),
        });

        let ClaudeInvocation {
            params,
            context,
            operation,
        } = request;
        match operation {
            ClaudeOperation::Messages => {
                info!(
                    "[REQ] stream: {}, msgs: {}, model: {}",
                    enabled(state.stream),
                    params.messages.len().to_string().green(),
                    params.model.green(),
                );
                print_out_json(&params, "claude_code_client_req.json");
                let stopwatch = Instant::now();
                let response = state.try_chat(params).await?;
                let elapsed = stopwatch.elapsed();
                info!(
                    "[FIN] elapsed: {}s",
                    format!("{}", elapsed.as_secs_f32()).green()
                );
                Ok(ClaudeProviderResponse { context, response })
            }
            ClaudeOperation::CountTokens => {
                info!(
                    "[TOKENS] msgs: {}, model: {}",
                    params.messages.len().to_string().green(),
                    params.model.green()
                );
                let stopwatch = Instant::now();
                let response = state.try_count_tokens(params).await?;
                let elapsed = stopwatch.elapsed();
                info!(
                    "[TOKENS] elapsed: {}s",
                    format!("{}", elapsed.as_secs_f32()).green()
                );
                Ok(ClaudeProviderResponse { context, response })
            }
        }
    }
}

pub fn build_providers(cookie_actor_handle: CookieActorHandle, db: SqlitePool, stealth_profile: SharedStealthProfile, event_tx: broadcast::Sender<()>) -> ClaudeProviders {
    ClaudeProviders::new(cookie_actor_handle, db, stealth_profile, event_tx)
}
