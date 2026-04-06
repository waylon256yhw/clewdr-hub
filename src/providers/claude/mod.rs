use std::{collections::HashMap, sync::Arc, time::Instant};

use axum::response::Response;
use colored::Colorize;
use sqlx::SqlitePool;
use tokio::sync::{Mutex, broadcast};
use tracing::info;

use super::LLMProvider;
use crate::{
    billing::BillingContext,
    claude_code_state::ClaudeCodeState,
    db::accounts::{AccountWithRuntime, load_pure_oauth_accounts},
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
    oauth_pool: Arc<OAuthAccountPool>,
}

#[derive(Default)]
struct OAuthPoolState {
    inflight: HashMap<i64, (u32, u32)>,
    cursor: Option<i64>,
}

#[derive(Default)]
pub(crate) struct OAuthAccountPool {
    inner: Mutex<OAuthPoolState>,
}

impl OAuthAccountPool {
    pub(crate) async fn acquire(
        &self,
        accounts: &[AccountWithRuntime],
    ) -> Option<AccountWithRuntime> {
        if accounts.is_empty() {
            return None;
        }
        let mut inner = self.inner.lock().await;
        for account in accounts {
            inner
                .inflight
                .entry(account.id)
                .and_modify(|slot| slot.1 = account.max_slots as u32)
                .or_insert((0, account.max_slots as u32));
        }
        inner
            .inflight
            .retain(|id, _| accounts.iter().any(|account| account.id == *id));

        let start_idx = inner
            .cursor
            .and_then(|cursor| accounts.iter().position(|account| account.id == cursor))
            .map(|idx| (idx + 1) % accounts.len())
            .unwrap_or(0);

        for offset in 0..accounts.len() {
            let idx = (start_idx + offset) % accounts.len();
            let account = &accounts[idx];
            let slot = inner
                .inflight
                .entry(account.id)
                .or_insert((0, account.max_slots as u32));
            if slot.0 < slot.1 {
                slot.0 += 1;
                inner.cursor = Some(account.id);
                return Some(account.clone());
            }
        }

        None
    }

    pub(crate) async fn release(&self, account_id: i64) {
        let mut inner = self.inner.lock().await;
        if let Some((current, _)) = inner.inflight.get_mut(&account_id)
            && *current > 0
        {
            *current -= 1;
        }
    }
}

impl ClaudeSharedState {
    fn new(
        cookie_actor_handle: CookieActorHandle,
        db: SqlitePool,
        stealth_profile: SharedStealthProfile,
        event_tx: broadcast::Sender<()>,
    ) -> Self {
        Self {
            cookie_actor_handle,
            db,
            stealth_profile,
            event_tx,
            oauth_pool: Arc::new(OAuthAccountPool::default()),
        }
    }
}

#[derive(Clone)]
pub struct ClaudeProviders {
    code: Arc<ClaudeCodeProvider>,
}

impl ClaudeProviders {
    pub fn new(
        cookie_actor_handle: CookieActorHandle,
        db: SqlitePool,
        stealth_profile: SharedStealthProfile,
        event_tx: broadcast::Sender<()>,
    ) -> Self {
        let shared = Arc::new(ClaudeSharedState::new(
            cookie_actor_handle,
            db,
            stealth_profile,
            event_tx,
        ));
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
        state.anthropic_beta_header = request.context.anthropic_beta.clone();
        state.usage = request.context.usage.to_owned();
        state.bound_account_ids = request.context.bound_account_ids.clone();
        state.oauth_pool = Some(self.shared.oauth_pool.clone());

        if let Some(account) = self
            .shared
            .oauth_pool
            .acquire(
                &load_pure_oauth_accounts(&self.shared.db, &request.context.bound_account_ids)
                    .await?,
            )
            .await
        {
            state.account_id = Some(account.id);
            state.oauth_token = account.oauth_token.clone();
            state.organization_uuid = account.organization_uuid.clone();
        }

        // Set billing context for cost tracking
        state.billing_ctx = Some(BillingContext {
            db: self.shared.db.clone(),
            user_id: request.context.user_id,
            api_key_id: request.context.api_key_id,
            account_id: state.account_id,
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

pub fn build_providers(
    cookie_actor_handle: CookieActorHandle,
    db: SqlitePool,
    stealth_profile: SharedStealthProfile,
    event_tx: broadcast::Sender<()>,
) -> ClaudeProviders {
    ClaudeProviders::new(cookie_actor_handle, db, stealth_profile, event_tx)
}
