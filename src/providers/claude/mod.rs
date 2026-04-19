use std::{collections::HashMap, sync::Arc, time::Instant};

use axum::response::Response;
use colored::Colorize;
use sqlx::SqlitePool;
use tokio::sync::{Mutex, broadcast};
use tracing::{info, warn};

use super::LLMProvider;
use crate::{
    billing::BillingContext,
    claude_code_state::ClaudeCodeState,
    db::accounts::{AccountWithRuntime, is_temporarily_unavailable, load_all_accounts},
    error::ClewdrError,
    middleware::claude::ClaudeContext,
    services::account_pool::AccountPoolHandle,
    state::AdminEvent,
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
    account_pool_handle: AccountPoolHandle,
    db: SqlitePool,
    stealth_profile: SharedStealthProfile,
    event_tx: broadcast::Sender<AdminEvent>,
    oauth_pool: Arc<OAuthAccountPool>,
}

#[derive(Default)]
struct OAuthPoolState {
    inflight: HashMap<i64, (u32, u32)>,
    /// Round-robin cursor for the `drain_first = true` subset.
    drain_cursor: Option<i64>,
    /// Round-robin cursor for the normal (non-drain_first) subset.
    /// Tracked independently so that interleaved drain/normal acquires
    /// do not reset each other's round-robin position to the head of the
    /// subset (which would concentrate spillover load on the lowest-
    /// `rr_order` normal account).
    normal_cursor: Option<i64>,
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

        // Partition into drain_first subset (preferred) and normal subset,
        // preserving the input order (which already reflects `rr_order`).
        // Drain-first accounts are tried first; only when all of them are
        // saturated do we fall back to the normal subset.
        let (drain, normal): (Vec<&AccountWithRuntime>, Vec<&AccountWithRuntime>) =
            accounts.iter().partition(|a| a.drain_first);

        if !drain.is_empty() {
            let inner = &mut *inner;
            if let Some(picked) =
                Self::try_acquire_in_subset(&mut inner.inflight, &mut inner.drain_cursor, &drain)
            {
                return Some(picked);
            }
        }
        if !normal.is_empty() {
            let inner = &mut *inner;
            if let Some(picked) =
                Self::try_acquire_in_subset(&mut inner.inflight, &mut inner.normal_cursor, &normal)
            {
                return Some(picked);
            }
        }

        None
    }

    fn try_acquire_in_subset(
        inflight: &mut HashMap<i64, (u32, u32)>,
        cursor: &mut Option<i64>,
        subset: &[&AccountWithRuntime],
    ) -> Option<AccountWithRuntime> {
        let start_idx = (*cursor)
            .and_then(|c| subset.iter().position(|a| a.id == c))
            .map(|idx| (idx + 1) % subset.len())
            .unwrap_or(0);

        for offset in 0..subset.len() {
            let idx = (start_idx + offset) % subset.len();
            let account = subset[idx];
            let slot = inflight
                .entry(account.id)
                .or_insert((0, account.max_slots as u32));
            if slot.0 < slot.1 {
                slot.0 += 1;
                *cursor = Some(account.id);
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
        account_pool_handle: AccountPoolHandle,
        db: SqlitePool,
        stealth_profile: SharedStealthProfile,
        event_tx: broadcast::Sender<AdminEvent>,
    ) -> Self {
        Self {
            account_pool_handle,
            db,
            stealth_profile,
            event_tx,
            oauth_pool: Arc::new(OAuthAccountPool::default()),
        }
    }

    async fn acquire_pure_oauth_account(
        &self,
        bound_account_ids: &[i64],
    ) -> Result<(Option<AccountWithRuntime>, bool), ClewdrError> {
        let relevant_accounts: Vec<_> = load_all_accounts(&self.db)
            .await?
            .into_iter()
            .filter(|account| {
                !matches!(account.status.as_str(), "auth_error" | "disabled")
                    && account.auth_source == "oauth"
                    && account.oauth_token.is_some()
                    && (bound_account_ids.is_empty() || bound_account_ids.contains(&account.id))
            })
            .collect();

        let available_accounts: Vec<_> = relevant_accounts
            .iter()
            .filter(|account| !is_temporarily_unavailable(account))
            .cloned()
            .collect();
        let acquired = self.oauth_pool.acquire(&available_accounts).await;
        let temporarily_unavailable = relevant_accounts.iter().any(is_temporarily_unavailable)
            || (!available_accounts.is_empty() && acquired.is_none());

        Ok((acquired, temporarily_unavailable))
    }
}

fn reconcile_oauth_fallback_error(
    err: ClewdrError,
    oauth_temporarily_unavailable: bool,
) -> ClewdrError {
    if oauth_temporarily_unavailable && matches!(err, ClewdrError::NoValidUpstreamAccounts) {
        ClewdrError::UpstreamCoolingDown
    } else {
        err
    }
}

#[derive(Clone)]
pub struct ClaudeProviders {
    code: Arc<ClaudeCodeProvider>,
}

impl ClaudeProviders {
    pub fn new(
        account_pool_handle: AccountPoolHandle,
        db: SqlitePool,
        stealth_profile: SharedStealthProfile,
        event_tx: broadcast::Sender<AdminEvent>,
    ) -> Self {
        let shared = Arc::new(ClaudeSharedState::new(
            account_pool_handle,
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
            self.shared.account_pool_handle.clone(),
            self.shared.stealth_profile.clone(),
        );
        state.stream = request.context.stream;
        state.system_prompt_hash = request.context.system_prompt_hash;
        state.anthropic_beta_header = request.context.anthropic_beta.clone();
        state.usage = request.context.usage.to_owned();
        state.bound_account_ids = request.context.bound_account_ids.clone();
        state.oauth_pool = Some(self.shared.oauth_pool.clone());

        let (oauth_account, oauth_temporarily_unavailable) = self
            .shared
            .acquire_pure_oauth_account(&request.context.bound_account_ids)
            .await?;
        if let Some(account) = oauth_account {
            state.account_id = Some(account.id);
            state.oauth_token = account.oauth_token.clone();
            state.organization_uuid = account.organization_uuid.clone();
            state.set_proxy_url(account.proxy_url.as_deref());
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
                let response = match state.try_chat(params).await {
                    Ok(response) => response,
                    Err(err) => {
                        let err =
                            reconcile_oauth_fallback_error(err, oauth_temporarily_unavailable);
                        warn!("[ERR] {}", err);
                        return Err(err);
                    }
                };
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
                let response = state.try_count_tokens(params).await.map_err(|err| {
                    reconcile_oauth_fallback_error(err, oauth_temporarily_unavailable)
                })?;
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
    account_pool_handle: AccountPoolHandle,
    db: SqlitePool,
    stealth_profile: SharedStealthProfile,
    event_tx: broadcast::Sender<AdminEvent>,
) -> ClaudeProviders {
    ClaudeProviders::new(account_pool_handle, db, stealth_profile, event_tx)
}

#[cfg(test)]
mod tests {
    use super::{OAuthAccountPool, reconcile_oauth_fallback_error};
    use crate::db::accounts::AccountWithRuntime;
    use crate::error::ClewdrError;

    fn mk_account(id: i64, drain_first: bool) -> AccountWithRuntime {
        AccountWithRuntime {
            id,
            name: format!("acc-{id}"),
            rr_order: id,
            max_slots: 1,
            drain_first,
            status: "active".to_string(),
            auth_source: "oauth".to_string(),
            cookie_blob: None,
            oauth_token: None,
            oauth_expires_at: None,
            last_refresh_at: None,
            last_error: None,
            organization_uuid: None,
            invalid_reason: None,
            email: None,
            account_type: None,
            created_at: None,
            updated_at: None,
            runtime: None,
        }
    }

    #[test]
    fn oauth_temporary_fallback_upgrades_cookie_503_to_429() {
        let err = reconcile_oauth_fallback_error(ClewdrError::NoValidUpstreamAccounts, true);
        assert!(matches!(err, ClewdrError::UpstreamCoolingDown));
    }

    #[test]
    fn oauth_fallback_leaves_other_errors_unchanged() {
        let err = reconcile_oauth_fallback_error(ClewdrError::QuotaExceeded, true);
        assert!(matches!(err, ClewdrError::QuotaExceeded));
    }

    #[tokio::test]
    async fn oauth_acquire_prefers_drain_first_until_saturated() {
        // Two accounts, `a` is drain_first with 1 slot, `b` is normal with 1 slot.
        // Order in the input reflects rr_order: b (normal) comes first to prove
        // drain_first overrides list position.
        let a = mk_account(1, true);
        let b = mk_account(2, false);
        let accounts = vec![b.clone(), a.clone()];

        let pool = OAuthAccountPool::default();

        // First acquire must pick `a` (drain_first) even though `b` is first in list.
        let picked = pool.acquire(&accounts).await.expect("first acquire");
        assert_eq!(picked.id, a.id, "drain_first account should be preferred");

        // `a`'s only slot is now taken → second acquire falls through to `b`.
        let picked = pool.acquire(&accounts).await.expect("second acquire");
        assert_eq!(
            picked.id, b.id,
            "should fall back to normal when drain_first saturated"
        );

        // Both saturated → None (not an error — OAuth layer translates to cooldown).
        assert!(pool.acquire(&accounts).await.is_none());

        // Release `a`, it should become the preferred pick again.
        pool.release(a.id).await;
        let picked = pool.acquire(&accounts).await.expect("after release");
        assert_eq!(
            picked.id, a.id,
            "released drain_first slot should be re-preferred"
        );
    }

    #[tokio::test]
    async fn oauth_acquire_round_robins_within_drain_first_subset() {
        // Two drain_first accounts each with capacity 1, one normal with capacity 1.
        // First two requests should hit the two drain_first accounts, third hits normal.
        let d1 = mk_account(1, true);
        let d2 = mk_account(2, true);
        let n = mk_account(3, false);
        let accounts = vec![d1.clone(), d2.clone(), n.clone()];

        let pool = OAuthAccountPool::default();

        let p1 = pool.acquire(&accounts).await.expect("1");
        let p2 = pool.acquire(&accounts).await.expect("2");
        let p3 = pool.acquire(&accounts).await.expect("3");

        let picks: Vec<i64> = vec![p1.id, p2.id, p3.id];
        // First two must be drain_first ids (order between them not asserted since cursor).
        assert!(picks[..2].contains(&d1.id) && picks[..2].contains(&d2.id));
        // Third must be the normal account.
        assert_eq!(picks[2], n.id);
    }

    #[tokio::test]
    async fn oauth_acquire_behaves_normally_without_drain_first() {
        // Backward-compat check: no drain_first flag set → pure round-robin.
        let a = mk_account(1, false);
        let b = mk_account(2, false);
        let accounts = vec![a.clone(), b.clone()];

        let pool = OAuthAccountPool::default();
        let p1 = pool.acquire(&accounts).await.expect("1");
        let p2 = pool.acquire(&accounts).await.expect("2");
        // Cold start picks idx=0, then cursor advances; both should be acquired exactly once.
        let mut ids = vec![p1.id, p2.id];
        ids.sort();
        assert_eq!(ids, vec![a.id, b.id]);
    }

    #[tokio::test]
    async fn oauth_acquire_preserves_normal_cursor_across_drain_interference() {
        // D1: drain_first, max_slots=1. N1/N2: normal, max_slots=2.
        // Interleaving drain picks with spillover to normal must NOT
        // reset the normal subset's round-robin position — otherwise all
        // spillover requests concentrate on N1.
        let d1 = AccountWithRuntime {
            max_slots: 1,
            ..mk_account(1, true)
        };
        let n1 = AccountWithRuntime {
            max_slots: 2,
            ..mk_account(2, false)
        };
        let n2 = AccountWithRuntime {
            max_slots: 2,
            ..mk_account(3, false)
        };
        let accounts = vec![d1.clone(), n1.clone(), n2.clone()];
        let pool = OAuthAccountPool::default();

        // 1. Preferred pick → D1 (D1 saturated: 1/1).
        assert_eq!(pool.acquire(&accounts).await.unwrap().id, d1.id);
        // 2. D1 full, spill to normal → N1 (normal_cursor advances to N1, N1: 1/2).
        assert_eq!(pool.acquire(&accounts).await.unwrap().id, n1.id);
        // 3. Free D1.
        pool.release(d1.id).await;
        // 4. D1 preferred again. This touches drain_cursor only; normal_cursor
        //    must remain at N1.
        assert_eq!(pool.acquire(&accounts).await.unwrap().id, d1.id);
        // 5. D1 full, spill to normal. With independent cursors, we advance
        //    from N1 → N2. With a shared cursor, it would reset to idx=0
        //    (N1) and pick N1 again (still has 1 free slot), skewing load.
        assert_eq!(
            pool.acquire(&accounts).await.unwrap().id,
            n2.id,
            "normal subset should round-robin independently of drain picks",
        );
    }
}
