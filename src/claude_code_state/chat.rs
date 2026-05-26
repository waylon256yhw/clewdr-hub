use axum::{
    Json,
    response::{IntoResponse, Sse, sse::Event as SseEvent},
};
use colored::Colorize;
use eventsource_stream::Eventsource;
use futures::{StreamExt, TryStreamExt};
use http::header::{ACCEPT, USER_AGENT};
use snafu::{GenerateImplicitData, ResultExt};
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tracing::{Instrument, error, info, warn};
use wreq::Method;

use crate::{
    billing::{RequestType, TerminalLogOptions},
    claude_code_state::{ClaudeCodeState, TokenStatus},
    config::{AuthMethod, ModelFamily, Reason},
    db::accounts::{
        set_account_auth_error, set_account_disabled, set_account_last_failure,
        set_account_reset_time, update_account_metadata_unchecked, upsert_account_oauth,
        upsert_oauth_snapshot_runtime_fields,
    },
    error::{CheckClaudeErr, ClewdrError, WreqSnafu},
    oauth::refresh_oauth_token,
    services::account_error::{
        AccountFailureAction, AccountFailureContextPersisted, AccountNormalizedReason,
        FailureSource, classify_account_failure,
    },
    services::account_pool::{AccountPoolHandle, CredentialFingerprint},
    types::claude::{CountMessageTokensResponse, CreateMessageParams},
};

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const MAX_RETRIES: usize = 5;
const MESSAGES_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const COUNT_TOKENS_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(60);
const CLAUDE_BETA_BASE: &str = "oauth-2025-04-20";
const CLAUDE_BETA_CONTEXT_1M_TOKEN: &str = "context-1m-2025-08-07";
const CLAUDE_API_VERSION: &str = "2023-06-01";

/// Header names that MUST NOT come from per-account extra_headers on
/// an ApiKey send: either we set them ourselves (`x-api-key`,
/// `anthropic-version`, `anthropic-beta`, `content-type`) or they
/// reintroduce subscription-shaped behavior the ApiKey dispatch is
/// designed to remove (`user-agent` would silently restore the CC
/// stealth UA we deliberately omit) or they belong to the transport
/// layer (`host`, `content-length`, `accept-encoding`) and overriding
/// them breaks the request.
///
/// Admin write-time validation (C10) is the primary guardrail; this
/// send-time filter is defense in depth for the case of a manual DB
/// edit that bypasses validation.
const API_KEY_RESERVED_EXTRA_HEADERS: &[&str] = &[
    "x-api-key",
    "authorization",
    "anthropic-version",
    "anthropic-beta",
    "user-agent",
    "host",
    "content-length",
    "content-type",
    "accept-encoding",
];

pub(crate) fn is_reserved_api_key_extra_header(name: &str) -> bool {
    API_KEY_RESERVED_EXTRA_HEADERS
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(name))
}

/// Compose the `anthropic-beta` header for an ApiKey send.
///
/// Unlike `merge_anthropic_beta_header` (the subscription path), this
/// does NOT prepend `CLAUDE_BETA_BASE` (`oauth-2025-04-20`) — that
/// token is meaningful only on the OAuth subscription endpoint and
/// upstream API key services either ignore or reject it. We also do
/// NOT filter `CLAUDE_BETA_CONTEXT_1M_TOKEN`; the 1M-token context
/// beta is a legitimate request-level capability the caller is
/// entitled to opt into on direct-API.
///
/// Returns `None` if the caller-supplied string is empty or contained
/// only the stripped `oauth-2025-04-20` token. Caller must omit the
/// `anthropic-beta` header entirely in that case — sending an empty
/// value is worse than not sending it.
fn api_key_beta_header(extra: Option<&str>) -> Option<String> {
    let cleaned: Vec<&str> = extra
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case(CLAUDE_BETA_BASE))
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.join(","))
    }
}

struct SelectedSlotState {
    handle: AccountPoolHandle,
    account_id: Option<i64>,
    slot_released: AtomicBool,
}

#[derive(Clone)]
struct SelectedSlotAbortLog {
    ctx: crate::billing::BillingContext,
    stream: bool,
}

#[derive(Clone)]
struct SelectedSlotHandle {
    state: Arc<SelectedSlotState>,
}

struct SelectedSlotGuard {
    state: Arc<SelectedSlotState>,
    abort_log: Option<SelectedSlotAbortLog>,
    completed: bool,
}

impl SelectedSlotGuard {
    fn new(
        handle: AccountPoolHandle,
        account_id: Option<i64>,
        abort_log: Option<SelectedSlotAbortLog>,
    ) -> Self {
        let state = Arc::new(SelectedSlotState {
            handle,
            account_id,
            slot_released: AtomicBool::new(false),
        });
        Self {
            state,
            abort_log,
            completed: false,
        }
    }

    fn handle(&self) -> SelectedSlotHandle {
        SelectedSlotHandle {
            state: self.state.clone(),
        }
    }

    async fn finish(&mut self) {
        self.handle().release_slot_only().await;
        self.disarm();
    }

    fn disarm(&mut self) {
        self.completed = true;
    }
}

impl SelectedSlotHandle {
    async fn release_slot_only(&self) {
        if let Some(account_id) = self.state.account_id
            && !self.state.slot_released.swap(true, Ordering::Relaxed)
        {
            self.state.handle.release_slot(account_id).await;
        }
    }
}

impl Drop for SelectedSlotGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let account_id = self.state.account_id;
        let should_release_slot =
            account_id.is_some() && !self.state.slot_released.swap(true, Ordering::Relaxed);
        let handle = self.state.handle.clone();
        let abort_log = self.abort_log.clone();
        tokio::spawn(async move {
            if should_release_slot && let Some(account_id) = account_id {
                handle.release_slot(account_id).await;
            }
            if let Some(log) = abort_log {
                crate::billing::persist_terminal_request_log(
                    &log.ctx,
                    TerminalLogOptions {
                        request_type: RequestType::Messages,
                        stream: log.stream,
                        status: "client_abort",
                        http_status: Some(499),
                        usage: None,
                        error_code: Some("client_abort"),
                        error_message: Some("request task dropped before response completed"),
                        update_rollups: false,
                        response_body: None,
                    },
                )
                .await;
            }
        });
    }
}

impl ClaudeCodeState {
    async fn timeout_upstream<T, F>(
        timeout: Duration,
        label: &'static str,
        future: F,
    ) -> Result<T, ClewdrError>
    where
        F: Future<Output = Result<T, ClewdrError>>,
    {
        tokio::time::timeout(timeout, future)
            .await
            .map_err(|_| ClewdrError::UpstreamTimeout { msg: label })?
    }

    fn is_oauth_auth_failure(err: &ClewdrError) -> bool {
        super::is_oauth_auth_failure(err)
    }

    /// Step 3.5: routed through the unified classifier so every entry point
    /// (messages / count_tokens / probe / refresh / test) reaches the same
    /// "this account is disabled" verdict. Behavior is preserved: only the
    /// `OrganizationDisabled` normalized reason triggers the disabled
    /// branch — `FreeTier` keeps a separate path even though it also maps
    /// to `AccountFailureAction::TerminalDisabled`.
    fn is_oauth_disabled_failure(err: &ClewdrError) -> bool {
        let ctx = classify_account_failure(err, FailureSource::Messages, None, None);
        matches!(
            ctx.normalized_reason,
            AccountNormalizedReason::OrganizationDisabled
        )
    }

    /// Step 3.5: routed through the unified classifier. Equivalent to the
    /// previous `InvalidCookie + Reason::TooManyRequest|Restricted` match —
    /// any classifier path that produces `AccountFailureAction::Cooldown`
    /// reports its `reset_time` here.
    fn oauth_cooldown_until(err: &ClewdrError) -> Option<i64> {
        match classify_account_failure(err, FailureSource::Messages, None, None).action {
            AccountFailureAction::Cooldown { reset_time } => Some(reset_time),
            _ => None,
        }
    }

    /// Step 3.5: routed through the unified classifier. The classifier
    /// produces a normalized reason for every terminal/cooldown error and
    /// `to_reason()` bridges it back to the legacy `Reason` enum used by
    /// the pool's invalidate / collect API. Transient and internal
    /// classes return `None` so callers do not change account state.
    fn oauth_pool_reason(err: &ClewdrError) -> Option<Reason> {
        let ctx = classify_account_failure(err, FailureSource::Messages, None, None);
        match ctx.action {
            AccountFailureAction::TerminalAuth
            | AccountFailureAction::TerminalDisabled
            | AccountFailureAction::Cooldown { .. } => ctx.normalized_reason.to_reason(),
            AccountFailureAction::TransientUpstream | AccountFailureAction::InternalError => None,
        }
    }

    fn should_retry_api_key_transient(err: &ClewdrError) -> bool {
        let ClewdrError::ClaudeHttpError { code, .. } = err else {
            return true;
        };

        match code.as_u16() {
            408 | 409 | 425 | 429 => true,
            status if (400..500).contains(&status) => false,
            _ => true,
        }
    }

    /// Step 3.5 C4b: persist a pre-classified structured failure
    /// context to `accounts.last_failure_json` for AccountHealth
    /// display. Used by both OAuth and cookie failure paths in the
    /// messages / count_tokens flow.
    ///
    /// Best-effort: a serialization or DB error logs and returns
    /// without affecting the surrounding state transition. The
    /// in-pool legacy `Reason` carrier is unrelated and lives on
    /// `set_account_auth_error` / `set_account_disabled` /
    /// `collect_by_id`.
    ///
    /// Takes the owned `AccountFailureContextPersisted` rather than
    /// `&ClewdrError` because `ClewdrError: !Sync` (the `Whatever`
    /// variant's `dyn Error + Send` source has no `+ Sync`), so a
    /// borrow held across `.await` makes the surrounding future
    /// non-Send.
    async fn persist_last_failure(
        &self,
        account_id: i64,
        persisted: AccountFailureContextPersisted,
    ) {
        let Some(db) = self.billing_ctx.as_ref().map(|ctx| ctx.db.clone()) else {
            return;
        };
        if let Err(db_err) = set_account_last_failure(&db, account_id, Some(&persisted)).await {
            warn!("Failed to persist last_failure for account {account_id}: {db_err}");
        }
    }

    /// Step 3.5 C4b: classify a borrowed `ClewdrError` to the owned
    /// persistence DTO before any `.await`, so the caller can drop
    /// the borrow before crossing into a non-Send-bound future.
    fn classify_persisted(
        err: &ClewdrError,
        source: FailureSource,
    ) -> AccountFailureContextPersisted {
        let ctx = classify_account_failure(err, source, None, None);
        AccountFailureContextPersisted::from(&ctx)
    }

    async fn mark_oauth_account_auth_error(
        &mut self,
        account_id: i64,
        message: String,
        persisted: AccountFailureContextPersisted,
    ) {
        let Some(db) = self.billing_ctx.as_ref().map(|ctx| ctx.db.clone()) else {
            return;
        };
        if let Err(db_err) = set_account_auth_error(&db, account_id, &message).await {
            warn!("Failed to set OAuth auth_error for account {account_id}: {db_err}");
            return;
        }
        // Step 3.5 C4b: persist structured failure context alongside the
        // legacy auth_error transition so AccountHealth.last_failure can
        // read source/stage/upstream_http_status without losing the
        // failure scene.
        if let Err(db_err) = set_account_last_failure(&db, account_id, Some(&persisted)).await {
            warn!("Failed to persist last_failure for account {account_id}: {db_err}");
        }
        // DB is authoritative; converge the pool's in-memory view so the
        // account stops being dispatched and any affinity pointing at it is
        // cleared.
        self.account_pool_handle
            .invalidate(account_id, Reason::Null)
            .await;
    }

    async fn mark_oauth_account_disabled(
        &mut self,
        account_id: i64,
        persisted: AccountFailureContextPersisted,
    ) {
        let Some(db) = self.billing_ctx.as_ref().map(|ctx| ctx.db.clone()) else {
            return;
        };
        if let Err(db_err) = set_account_disabled(&db, account_id, "disabled").await {
            warn!("Failed to set OAuth account {account_id} disabled: {db_err}");
            return;
        }
        // Step 3.5 C4b: persist structured failure context alongside the
        // legacy disabled transition.
        if let Err(db_err) = set_account_last_failure(&db, account_id, Some(&persisted)).await {
            warn!("Failed to persist last_failure for account {account_id}: {db_err}");
        }
        self.account_pool_handle
            .invalidate(account_id, Reason::Disabled)
            .await;
    }

    async fn mark_oauth_account_cooldown(&mut self, account_id: i64, reset_time: i64) {
        let Some(db) = self.billing_ctx.as_ref().map(|ctx| ctx.db.clone()) else {
            return;
        };
        if let Err(db_err) = set_account_reset_time(&db, account_id, reset_time).await {
            warn!("Failed to set OAuth cooldown for account {account_id}: {db_err}");
        }
    }

    /// Mark an ApiKey account as `auth_error` after a terminal-auth
    /// failure (401/403 from upstream). Same DB shape as the OAuth
    /// sibling — `auth_error` status + `last_failure` context — minus
    /// the oauth-flavored token clearing (ApiKey credentials live in
    /// `api_key_secret`, not the `oauth_*` columns).
    ///
    /// The pool `invalidate` step kicks the account out of dispatch
    /// and clears any affinity pointing at it, so subsequent
    /// `try_chat` calls do not re-pick this slot until an admin
    /// re-enables it.
    async fn mark_api_key_account_auth_error(
        &mut self,
        account_id: i64,
        message: String,
        persisted: AccountFailureContextPersisted,
    ) {
        let Some(db) = self.billing_ctx.as_ref().map(|ctx| ctx.db.clone()) else {
            return;
        };
        if let Err(db_err) = set_account_auth_error(&db, account_id, &message).await {
            warn!("Failed to set ApiKey auth_error for account {account_id}: {db_err}");
            return;
        }
        if let Err(db_err) = set_account_last_failure(&db, account_id, Some(&persisted)).await {
            warn!("Failed to persist ApiKey last_failure for account {account_id}: {db_err}");
        }
        self.account_pool_handle
            .invalidate(account_id, Reason::Null)
            .await;
    }

    /// Mark an ApiKey account disabled after an upstream terminal-disabled
    /// verdict. This mirrors the OAuth disabled transition but keeps the
    /// ApiKey credential columns intact for an admin to inspect or rotate.
    async fn mark_api_key_account_disabled(
        &mut self,
        account_id: i64,
        reason: Reason,
        persisted: AccountFailureContextPersisted,
    ) {
        let Some(db) = self.billing_ctx.as_ref().map(|ctx| ctx.db.clone()) else {
            return;
        };
        if let Err(db_err) = set_account_disabled(&db, account_id, &reason.to_db_string()).await {
            warn!("Failed to set ApiKey account {account_id} disabled: {db_err}");
            return;
        }
        if let Err(db_err) = set_account_last_failure(&db, account_id, Some(&persisted)).await {
            warn!("Failed to persist ApiKey last_failure for account {account_id}: {db_err}");
        }
        self.account_pool_handle
            .invalidate(account_id, reason)
            .await;
    }

    async fn persist_oauth_refresh(&mut self, account_id: i64) -> Result<(), ClewdrError> {
        let Some(fallback) = self.oauth_token.clone() else {
            return Ok(());
        };

        // Serialize concurrent refreshes for the same account — Anthropic's
        // refresh tokens are single-use, so two concurrent refreshes with the
        // same stored RT would both fail after the first one rotates it.
        let _guard = crate::services::oauth_refresh_guard::guard()
            .lock(account_id)
            .await;

        // After acquiring the guard, re-read the latest token: a peer may have
        // already refreshed while we were waiting. This is the singleflight
        // fast-path — avoids re-calling upstream and avoids burning another
        // refresh-token rotation. If the pool has no in-memory entry (e.g. the
        // account was moved to `state.invalid` by a concurrent auth_error),
        // fall back to a fresh DB read under the guard so we don't drive
        // refresh with the `fallback` clone captured before the guard.
        let db = self.billing_ctx.as_ref().map(|ctx| ctx.db.clone()).ok_or(
            ClewdrError::UnexpectedNone {
                msg: "Missing billing context database",
            },
        )?;
        let current = if let Some(t) = self
            .account_pool_handle
            .get_token(account_id)
            .await
            .unwrap_or(None)
        {
            t
        } else {
            match crate::db::accounts::get_account_by_id(&db, account_id).await {
                Ok(Some(acc)) => acc.oauth_token.unwrap_or(fallback),
                _ => fallback,
            }
        };
        if !current.is_expired() {
            self.oauth_token = Some(current.clone());
            self.organization_uuid = Some(current.organization.uuid.clone());
            if let Ok(Some(account)) = crate::db::accounts::get_account_by_id(&db, account_id).await
                && let (Some(slot), Some(runtime)) =
                    (self.cookie.as_mut(), account.runtime.as_ref())
            {
                slot.apply_oauth_snapshot_runtime(&runtime.to_params());
            }
            return Ok(());
        }

        let refreshed = refresh_oauth_token(&current, self.proxy_url.as_deref()).await?;
        upsert_account_oauth(&db, account_id, Some(&refreshed.token), None).await?;
        update_account_metadata_unchecked(
            &db,
            account_id,
            crate::db::accounts::AccountMetadataUpdate {
                email: refreshed.snapshot.email.as_deref(),
                account_type: refreshed.snapshot.account_type.as_deref(),
                organization_uuid: Some(refreshed.snapshot.organization_uuid.as_str()),
                rate_limit_tier: refreshed.snapshot.rate_limit_tier.as_deref(),
                subscription_created_at: refreshed.snapshot.subscription_created_at.as_deref(),
                billing_type: refreshed.snapshot.billing_type.as_deref(),
            },
        )
        .await?;
        upsert_oauth_snapshot_runtime_fields(&db, account_id, &refreshed.snapshot.runtime).await?;
        // Mirror the new token into the pool's in-memory slot so concurrent
        // dispatches see the fresh credential without waiting for reload.
        self.account_pool_handle
            .update_credential(account_id, Some(refreshed.token.clone()))
            .await;
        self.account_pool_handle
            .release_oauth_snapshot_runtime(
                account_id,
                refreshed.snapshot.runtime.clone(),
                Some(CredentialFingerprint::from_oauth_refresh_token(
                    &refreshed.token.refresh_token,
                )),
            )
            .await?;
        if let Some(slot) = self.cookie.as_mut() {
            slot.apply_oauth_snapshot_runtime(&refreshed.snapshot.runtime);
        }
        self.oauth_token = Some(refreshed.token);
        self.organization_uuid = Some(refreshed.snapshot.organization_uuid);
        Ok(())
    }

    /// Attempts to send a chat message to Claude API with retry mechanism
    ///
    /// This method handles the complete chat flow including:
    /// - Request preparation and logging
    /// - Cookie management for authentication
    /// - Executing the chat request with automatic retries on failure
    /// - Response transformation according to the specified API format
    /// - Error handling and cleanup
    ///
    /// The method implements a sophisticated retry mechanism to handle transient failures,
    /// and manages conversation cleanup to prevent resource leaks. It also includes
    /// performance tracking to measure response times.
    ///
    /// # Arguments
    /// * `p` - The client request body containing messages and configuration
    ///
    /// # Returns
    /// * `Result<axum::response::Response, ClewdrError>` - Formatted response or error
    pub async fn try_chat(
        &mut self,
        p: CreateMessageParams,
    ) -> Result<axum::response::Response, ClewdrError> {
        for i in 0..MAX_RETRIES + 1 {
            if i > 0 {
                info!("[RETRY] attempt: {}", i.to_string().green());
            }
            let mut state = self.to_owned();
            let p = p.to_owned();

            let cookie = state.acquire_account().await?;
            let account_id = cookie.account_id;
            let is_pure_oauth_slot = cookie.auth_method == AuthMethod::OAuth;
            let is_api_key_slot = cookie.auth_method == AuthMethod::ApiKey;
            // Pure oauth slots have no real cookie-backed reauth path, so hoist
            // their token into `oauth_token`. Cookie-backed slots keep using the
            // historic cookie/token path. ApiKey slots have no bearer at all.
            if is_pure_oauth_slot {
                state.oauth_token = cookie.token.clone();
            } else {
                state.oauth_token = None;
            }
            state.account_id = account_id;
            if let Some(ref mut ctx) = state.billing_ctx {
                ctx.account_id = account_id;
            }
            let mut slot_guard = SelectedSlotGuard::new(
                state.account_pool_handle.clone(),
                account_id,
                state.billing_ctx.clone().map(|ctx| SelectedSlotAbortLog {
                    ctx,
                    stream: state.stream,
                }),
            );
            let slot_handle = slot_guard.handle();

            let retry = async {
                // ApiKey bypasses the entire bearer-token ladder: the
                // slot has no expiring bearer to refresh and no cookie
                // exchange to perform. `execute_claude_request`'s
                // ApiKey arm reads `self.api_key` directly and ignores
                // the access_token param, so empty-string passes
                // through cleanly.
                if is_api_key_slot {
                    return state
                        .send_chat(String::new(), p, Some(slot_handle.clone()))
                        .await;
                }
                match state.check_token() {
                    TokenStatus::None => {
                        if is_pure_oauth_slot {
                            return Err(ClewdrError::UnexpectedNone {
                                msg: "OAuth token missing for oauth-bearing slot",
                            });
                        }
                        info!("No token found, requesting new token");
                        let org = state.get_organization().await?;
                        let code_res = state.exchange_code(&org).await?;
                        state.exchange_token(code_res).await?;
                        state.release_account(None).await;
                    }
                    TokenStatus::Expired => {
                        if is_pure_oauth_slot {
                            info!("OAuth token expired, refreshing");
                            let aid = account_id.ok_or(ClewdrError::UnexpectedNone {
                                msg: "OAuth refresh requires account id",
                            })?;
                            state.persist_oauth_refresh(aid).await?;
                            // Keep the slot's copy in sync so a later release/flush
                            // doesn't overwrite the DB with the stale token.
                            if let Some(slot) = state.cookie.as_mut() {
                                slot.token = state.oauth_token.clone();
                            }
                        } else {
                            info!("Token expired, refreshing token");
                            state.refresh_token().await?;
                            state.release_account(None).await;
                        }
                    }
                    TokenStatus::Valid => {
                        info!("Token is valid, proceeding with request");
                    }
                }
                let access_token = state
                    .oauth_token
                    .as_ref()
                    .map(|t| t.access_token.clone())
                    .or_else(|| {
                        state
                            .cookie
                            .as_ref()
                            .and_then(|c| c.token.as_ref())
                            .map(|t| t.access_token.clone())
                    })
                    .ok_or(ClewdrError::UnexpectedNone {
                        msg: "No access token found in cookie",
                    })?;
                state
                    .send_chat(access_token, p, Some(slot_handle.clone()))
                    .await
            }
            .instrument(tracing::info_span!(
                "claude_code",
                "cookie" = cookie.credential_label()
            ));
            let retry_result = Self::timeout_upstream(
                MESSAGES_UPSTREAM_TIMEOUT,
                "Claude messages request exceeded 600 seconds before response handoff",
                retry,
            )
            .await;
            match retry_result {
                Ok(res) => {
                    if self.stream {
                        // Streaming uses its own SlotDropGuard on the response body;
                        // this acquire-time guard only covers failures before handoff.
                        slot_guard.disarm();
                    } else {
                        slot_guard.finish().await;
                    }
                    return Ok(res);
                }
                Err(e) => {
                    if is_pure_oauth_slot {
                        let pool_reason = Self::oauth_pool_reason(&e);
                        if let Some(aid) = account_id {
                            if Self::is_oauth_disabled_failure(&e) {
                                let persisted =
                                    Self::classify_persisted(&e, FailureSource::Messages);
                                state.mark_oauth_account_disabled(aid, persisted).await;
                            } else if let Some(reset_time) = Self::oauth_cooldown_until(&e) {
                                state.mark_oauth_account_cooldown(aid, reset_time).await;
                            } else if Self::is_oauth_auth_failure(&e) {
                                let message = e.to_string();
                                let persisted =
                                    Self::classify_persisted(&e, FailureSource::Messages);
                                state
                                    .mark_oauth_account_auth_error(aid, message, persisted)
                                    .await;
                            }
                        }
                        slot_guard.finish().await;
                        if pool_reason.is_some() {
                            state.release_account(pool_reason).await;
                        }
                        if Self::oauth_cooldown_until(&e).is_some() {
                            return Err(ClewdrError::UpstreamCoolingDown);
                        }
                        return Err(e);
                    }
                    if is_api_key_slot {
                        slot_guard.finish().await;
                        error!(
                            "[{}] {}",
                            state.cookie.as_ref().unwrap().credential_label().green(),
                            e
                        );
                        // Classify through the unified pipeline. The
                        // classifier's 4th `auth_method` param handles
                        // the Cooldown→TransientUpstream demotion for
                        // ApiKey at the single chokepoint (PRD
                        // Decision 2: no cooldown semantics on
                        // pay-as-you-go).
                        let verdict = classify_account_failure(
                            &e,
                            FailureSource::Messages,
                            None,
                            Some(AuthMethod::ApiKey),
                        );
                        match verdict.action {
                            AccountFailureAction::TerminalAuth => {
                                if let Some(aid) = account_id {
                                    let persisted = AccountFailureContextPersisted::from(&verdict);
                                    state
                                        .mark_api_key_account_auth_error(
                                            aid,
                                            e.to_string(),
                                            persisted,
                                        )
                                        .await;
                                }
                                return Err(e);
                            }
                            AccountFailureAction::TerminalDisabled => {
                                if let Some(aid) = account_id {
                                    let reason = verdict
                                        .normalized_reason
                                        .to_reason()
                                        .unwrap_or(Reason::Disabled);
                                    let persisted = AccountFailureContextPersisted::from(&verdict);
                                    state
                                        .mark_api_key_account_disabled(aid, reason, persisted)
                                        .await;
                                }
                                return Err(e);
                            }
                            AccountFailureAction::TransientUpstream => {
                                // No cooldown reason — return slot to
                                // the `valid` bucket. Retry only for
                                // statuses that may succeed against a
                                // different account or after a short
                                // upstream blip; caller/request 4xx
                                // errors should surface directly.
                                state.release_account(None).await;
                                if !Self::should_retry_api_key_transient(&e) {
                                    return Err(e);
                                }
                                continue;
                            }
                            AccountFailureAction::Cooldown { .. } => {
                                // Unreachable: classifier demoted to
                                // TransientUpstream because we passed
                                // Some(AuthMethod::ApiKey).
                                unreachable!("classifier should have demoted Cooldown for ApiKey");
                            }
                            AccountFailureAction::InternalError => {
                                // Classifier signaled "do not change
                                // account state" (local logic error,
                                // not an upstream verdict). Surface
                                // the error without mutating the slot
                                // and without retrying — a retry would
                                // hit the same path.
                                return Err(e);
                            }
                        }
                    }
                    slot_guard.finish().await;
                    error!(
                        "[{}] {}",
                        state.cookie.as_ref().unwrap().credential_label().green(),
                        e
                    );
                    // 429 error
                    if let ClewdrError::InvalidCookie { reason } = e {
                        // Step 3.5 C4b: cookie flow's invalid path persists
                        // structured failure context to DB before the pool
                        // flush eventually writes the legacy invalid_reason.
                        // collect_by_id only carries `Reason`, so the rich
                        // context must be written here while we still have
                        // the original ClewdrError.
                        if let Some(aid) = account_id {
                            let persisted = Self::classify_persisted(
                                &ClewdrError::InvalidCookie {
                                    reason: reason.clone(),
                                },
                                FailureSource::Messages,
                            );
                            state.persist_last_failure(aid, persisted).await;
                        }
                        state.release_account(Some(reason.to_owned())).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(ClewdrError::TooManyRetries)
    }

    async fn send_chat(
        &mut self,
        access_token: String,
        p: CreateMessageParams,
        slot_guard: Option<SelectedSlotHandle>,
    ) -> Result<axum::response::Response, ClewdrError> {
        let model_family = Self::classify_model(&p.model);
        let response = self.execute_claude_request(&access_token, &p).await?;
        self.handle_success_response(response, model_family, slot_guard)
            .await
    }

    /// Send a request to an ApiKey account's normalized base URL.
    /// Used by both `execute_claude_request` (`v1/messages`) and
    /// `execute_claude_count_tokens_request` (`v1/messages/count_tokens`)
    /// — the two paths are byte-identical apart from the URL segment
    /// and the error-context string, so they share this helper rather
    /// than duplicating the auth-header / body-strip / extra-header
    /// dance per call site.
    ///
    /// Body clone is unavoidable: callers hand us `&CreateMessageParams`
    /// (the cookie/OAuth send paths only read the body), and we must
    /// mutate the `system` block to strip the CC billing header that
    /// the extractor (`<ClaudeCodePreprocess as FromRequest>::from_request`
    /// in `middleware/claude/request.rs`) prepends before account
    /// dispatch is aware of the slot's auth_method. Direct-API
    /// upstreams reject (or worse, log) that block as a spurious
    /// system prompt.
    ///
    /// Header set (intentionally minimal vs the subscription path):
    ///   - `x-api-key`: from `self.api_key` (populated at acquire time).
    ///   - `anthropic-version`: required by the API.
    ///   - `anthropic-beta`: from `api_key_beta_header(...)`, omitted
    ///     if empty (an empty value is worse than no header).
    ///   - Per-account extras after the reserved-name filter.
    /// Notably absent: `User-Agent` (Chrome stealth UA is anti-detection
    /// for subscription reverse-proxy, meaningless on direct API and
    /// trip-wire for strict corporate proxies); `Authorization` (auth
    /// flows via `x-api-key`, not bearer).
    async fn execute_api_key_request(
        &self,
        path: &str,
        body: &CreateMessageParams,
        error_context: &'static str,
    ) -> Result<wreq::Response, ClewdrError> {
        let mut url = self.endpoint.join(path).expect("Url parse error");
        url.set_query(Some("beta=true"));

        let mut body = body.clone();
        crate::middleware::claude::strip_billing_headers_from_system(&mut body);

        let mut req = self
            .client
            .post(url.to_string())
            .header("x-api-key", self.api_key.as_deref().unwrap_or(""))
            .header("anthropic-version", CLAUDE_API_VERSION);

        if let Some(beta) = api_key_beta_header(self.anthropic_beta_header.as_deref()) {
            req = req.header("anthropic-beta", beta);
        }

        if let Some(extras) = self.api_key_extra_headers.as_ref() {
            for (k, v) in extras.iter() {
                if is_reserved_api_key_extra_header(k) {
                    continue;
                }
                req = req.header(k.as_str(), v.as_str());
            }
        }

        req.json(&body)
            .send()
            .await
            .context(WreqSnafu { msg: error_context })?
            .check_claude()
            .await
    }

    async fn execute_claude_request(
        &mut self,
        access_token: &str,
        body: &CreateMessageParams,
    ) -> Result<wreq::Response, ClewdrError> {
        if self
            .cookie
            .as_ref()
            .is_some_and(|s| s.auth_method == AuthMethod::ApiKey)
        {
            return self
                .execute_api_key_request("v1/messages", body, "Failed to send chat message")
                .await;
        }
        let profile = self.stealth_profile.load();
        let beta_header = Self::merge_anthropic_beta_header(self.anthropic_beta_header.as_deref());
        let mut url = self.endpoint.join("v1/messages").expect("Url parse error");
        url.set_query(Some("beta=true"));
        self.client
            .post(url.to_string())
            .bearer_auth(access_token)
            .header(USER_AGENT, profile.user_agent())
            .header("anthropic-beta", beta_header)
            .header("anthropic-version", CLAUDE_API_VERSION)
            .json(body)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to send chat message",
            })?
            .check_claude()
            .await
    }

    async fn persist_count_tokens_allowed(&mut self, value: bool) {
        // ApiKey accounts always allow count_tokens via direct API — the
        // `count_tokens_allowed` field is subscription-runtime metadata
        // (cookie/OAuth slots may be gated by the upstream's per-account
        // permission), so persisting it for ApiKey is a no-op write that
        // wastes a DB roundtrip + slot flush.
        if self
            .cookie
            .as_ref()
            .is_some_and(|s| s.auth_method == AuthMethod::ApiKey)
        {
            return;
        }
        if let Some(cookie) = self.cookie.as_mut() {
            if cookie.count_tokens_allowed == Some(value) {
                return;
            }
            cookie.set_count_tokens_allowed(Some(value));
            let Some(account_id) = cookie.account_id else {
                return;
            };
            let update = cookie.to_runtime_params();
            let fingerprint = CredentialFingerprint::from_slot(cookie);
            if let Err(err) = self
                .account_pool_handle
                .release_runtime(account_id, update, None, fingerprint)
                .await
            {
                warn!("Failed to persist count_tokens permission: {}", err);
            }
        }
    }

    pub async fn fetch_usage_metrics(&mut self) -> Result<serde_json::Value, ClewdrError> {
        match self.check_token() {
            TokenStatus::None => {
                let org = self.get_organization().await?;
                let code = self.exchange_code(&org).await?;
                self.exchange_token(code).await?;
            }
            TokenStatus::Expired => {
                self.refresh_token().await?;
            }
            TokenStatus::Valid => {}
        }

        let access_token = self
            .cookie
            .as_ref()
            .and_then(|c| c.token.as_ref())
            .ok_or(ClewdrError::UnexpectedNone {
                msg: "No access token available",
            })?
            .access_token
            .to_owned();

        let profile = self.stealth_profile.load();

        self.client
            .request(Method::GET, CLAUDE_USAGE_URL)
            .bearer_auth(access_token)
            .header(ACCEPT, "application/json, text/plain, */*")
            .header(USER_AGENT, profile.user_agent())
            .header("anthropic-beta", CLAUDE_BETA_BASE)
            .header("anthropic-version", CLAUDE_API_VERSION)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to fetch usage metrics",
            })?
            .check_claude()
            .await?
            .json::<serde_json::Value>()
            .await
            .context(WreqSnafu {
                msg: "Failed to parse usage metrics response",
            })
    }

    pub async fn try_count_tokens(
        &mut self,
        p: CreateMessageParams,
    ) -> Result<axum::response::Response, ClewdrError> {
        for i in 0..MAX_RETRIES + 1 {
            if i > 0 {
                info!("[TOKENS][RETRY] attempt: {}", i.to_string().green());
            }
            let mut state = self.to_owned();
            let p = p.to_owned();

            let cookie = state.acquire_account().await?;
            let account_id = cookie.account_id;
            let is_pure_oauth_slot = cookie.auth_method == AuthMethod::OAuth;
            let is_api_key_slot = cookie.auth_method == AuthMethod::ApiKey;
            if is_pure_oauth_slot {
                state.oauth_token = cookie.token.clone();
            } else {
                state.oauth_token = None;
            }
            state.account_id = account_id;
            if let Some(ref mut ctx) = state.billing_ctx {
                ctx.account_id = account_id;
            }
            // count_tokens does not have a request_logs terminal row today;
            // this guard only protects the account-pool inflight slot.
            let mut slot_guard =
                SelectedSlotGuard::new(state.account_pool_handle.clone(), account_id, None);
            // `count_tokens_allowed` is subscription-runtime metadata
            // (per-cookie permission gate). ApiKey accounts always
            // allow upstream count_tokens; the field should be None on
            // a freshly-loaded ApiKey slot but a stale `Some(false)`
            // left over from a previous cookie/oauth life (e.g. on
            // bundle-import paths that pre-date the loader's runtime
            // skip for ApiKey) would otherwise route this request to
            // the local estimator instead of the upstream endpoint.
            // Skip the gate explicitly for ApiKey.
            let cookie_disallows =
                !is_api_key_slot && matches!(cookie.count_tokens_allowed, Some(false));
            if cookie_disallows {
                slot_guard.finish().await;
                state.persist_count_tokens_allowed(false).await;
                let (response, _) = Self::local_count_tokens_response(&p);
                return Ok(response);
            }
            let retry = async {
                // Mirror of try_chat's ApiKey bypass: no bearer-token
                // ladder, no access_token extraction. The send arm
                // (execute_claude_count_tokens_request's ApiKey
                // branch, C7) consumes self.api_key directly.
                if is_api_key_slot {
                    return state.perform_count_tokens(String::new(), p).await;
                }
                match state.check_token() {
                    TokenStatus::None => {
                        if is_pure_oauth_slot {
                            return Err(ClewdrError::UnexpectedNone {
                                msg: "OAuth token missing for oauth-bearing slot",
                            });
                        }
                        info!("No token found, requesting new token");
                        let org = state.get_organization().await?;
                        let code_res = state.exchange_code(&org).await?;
                        state.exchange_token(code_res).await?;
                        state.release_account(None).await;
                    }
                    TokenStatus::Expired => {
                        if is_pure_oauth_slot {
                            info!("OAuth token expired, refreshing");
                            let aid = account_id.ok_or(ClewdrError::UnexpectedNone {
                                msg: "OAuth refresh requires account id",
                            })?;
                            state.persist_oauth_refresh(aid).await?;
                            if let Some(slot) = state.cookie.as_mut() {
                                slot.token = state.oauth_token.clone();
                            }
                        } else {
                            info!("Token expired, refreshing token");
                            state.refresh_token().await?;
                            state.release_account(None).await;
                        }
                    }
                    TokenStatus::Valid => {
                        info!("Token is valid, proceeding with count_tokens");
                    }
                }
                let access_token = state
                    .oauth_token
                    .as_ref()
                    .map(|t| t.access_token.clone())
                    .or_else(|| {
                        state
                            .cookie
                            .as_ref()
                            .and_then(|c| c.token.as_ref())
                            .map(|t| t.access_token.clone())
                    })
                    .ok_or(ClewdrError::UnexpectedNone {
                        msg: "No access token found in cookie",
                    })?;
                state.perform_count_tokens(access_token, p).await
            }
            .instrument(tracing::info_span!(
                "claude_code_tokens",
                "cookie" = cookie.credential_label()
            ));
            let retry_result = Self::timeout_upstream(
                COUNT_TOKENS_UPSTREAM_TIMEOUT,
                "Claude count_tokens request exceeded 60 seconds",
                retry,
            )
            .await;
            match retry_result {
                Ok((res, _)) => {
                    slot_guard.finish().await;
                    return Ok(res);
                }
                Err(e) => {
                    if is_pure_oauth_slot {
                        let pool_reason = Self::oauth_pool_reason(&e);
                        if let Some(aid) = account_id {
                            if Self::is_oauth_disabled_failure(&e) {
                                let persisted =
                                    Self::classify_persisted(&e, FailureSource::CountTokens);
                                state.mark_oauth_account_disabled(aid, persisted).await;
                            } else if let Some(reset_time) = Self::oauth_cooldown_until(&e) {
                                state.mark_oauth_account_cooldown(aid, reset_time).await;
                            } else if Self::is_oauth_auth_failure(&e) {
                                let message = e.to_string();
                                let persisted =
                                    Self::classify_persisted(&e, FailureSource::CountTokens);
                                state
                                    .mark_oauth_account_auth_error(aid, message, persisted)
                                    .await;
                            }
                        }
                        slot_guard.finish().await;
                        if pool_reason.is_some() {
                            state.release_account(pool_reason).await;
                        }
                        if Self::oauth_cooldown_until(&e).is_some() {
                            return Err(ClewdrError::UpstreamCoolingDown);
                        }
                        return Err(e);
                    }
                    if is_api_key_slot {
                        slot_guard.finish().await;
                        error!(
                            "[{}][TOKENS] {}",
                            state.cookie.as_ref().unwrap().credential_label().green(),
                            e
                        );
                        // Mirror of try_chat's ApiKey error arm — see
                        // the equivalent comment there. Differences:
                        // FailureSource::CountTokens so AccountHealth
                        // reports the right entry point.
                        let verdict = classify_account_failure(
                            &e,
                            FailureSource::CountTokens,
                            None,
                            Some(AuthMethod::ApiKey),
                        );
                        match verdict.action {
                            AccountFailureAction::TerminalAuth => {
                                if let Some(aid) = account_id {
                                    let persisted = AccountFailureContextPersisted::from(&verdict);
                                    state
                                        .mark_api_key_account_auth_error(
                                            aid,
                                            e.to_string(),
                                            persisted,
                                        )
                                        .await;
                                }
                                return Err(e);
                            }
                            AccountFailureAction::TerminalDisabled => {
                                if let Some(aid) = account_id {
                                    let reason = verdict
                                        .normalized_reason
                                        .to_reason()
                                        .unwrap_or(Reason::Disabled);
                                    let persisted = AccountFailureContextPersisted::from(&verdict);
                                    state
                                        .mark_api_key_account_disabled(aid, reason, persisted)
                                        .await;
                                }
                                return Err(e);
                            }
                            AccountFailureAction::TransientUpstream => {
                                state.release_account(None).await;
                                if !Self::should_retry_api_key_transient(&e) {
                                    return Err(e);
                                }
                                continue;
                            }
                            AccountFailureAction::Cooldown { .. } => {
                                unreachable!("classifier should have demoted Cooldown for ApiKey");
                            }
                            AccountFailureAction::InternalError => {
                                return Err(e);
                            }
                        }
                    }
                    slot_guard.finish().await;
                    error!(
                        "[{}][TOKENS] {}",
                        state.cookie.as_ref().unwrap().credential_label().green(),
                        e
                    );
                    if let ClewdrError::InvalidCookie { reason } = e {
                        // Step 3.5 C4b: cookie flow's invalid path persists
                        // structured failure context to DB. See sibling
                        // comment in `claude_code_messages` retry loop.
                        if let Some(aid) = account_id {
                            let persisted = Self::classify_persisted(
                                &ClewdrError::InvalidCookie {
                                    reason: reason.clone(),
                                },
                                FailureSource::CountTokens,
                            );
                            state.persist_last_failure(aid, persisted).await;
                        }
                        state.release_account(Some(reason.to_owned())).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(ClewdrError::TooManyRetries)
    }

    async fn perform_count_tokens(
        &mut self,
        access_token: String,
        mut p: CreateMessageParams,
    ) -> Result<(axum::response::Response, u64), ClewdrError> {
        p.stream = Some(false);
        match self
            .execute_claude_count_tokens_request(&access_token, &p)
            .await
        {
            Ok(response) => {
                self.persist_count_tokens_allowed(true).await;
                let (resp, count) = Self::materialize_count_tokens_response(response).await?;
                Ok((resp, count.input_tokens as u64))
            }
            Err(err) => {
                if Self::is_count_tokens_unauthorized(&err) {
                    self.persist_count_tokens_allowed(false).await;
                }
                Err(err)
            }
        }
    }

    async fn handle_success_response(
        &mut self,
        response: wreq::Response,
        model_family: ModelFamily,
        slot_guard: Option<SelectedSlotHandle>,
    ) -> Result<axum::response::Response, ClewdrError> {
        if !self.stream {
            let (resp, billing_usage) = Self::materialize_non_stream_response(response).await?;
            let bu = billing_usage.unwrap_or(crate::billing::BillingUsage {
                input_tokens: self.usage.input_tokens as u64,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                ttft_ms: None,
            });
            self.persist_usage_totals(bu.input_tokens, bu.output_tokens, model_family)
                .await;
            if let Some(guard) = slot_guard.as_ref() {
                guard.release_slot_only().await;
            }
            // Billing/request log writes are intentionally after slot release:
            // runtime state has already been queued, and accounting DB latency
            // should not keep an account unavailable for dispatch.
            if let Some(ref ctx) = self.billing_ctx {
                crate::billing::persist_billing_to_db(ctx, bu, false).await;
            }
            Ok(resp)
        } else {
            return self.forward_stream_with_usage(response, model_family).await;
        }
    }

    async fn persist_usage_totals(&mut self, input: u64, output: u64, family: ModelFamily) {
        if input == 0 && output == 0 {
            return;
        }
        // ApiKey accounts have no quota window / usage buckets — the
        // subscription runtime fields (`*_input_tokens`, `*_output_tokens`,
        // boundaries) are unused. The per-request billing tally written
        // by `persist_billing_to_db` in `handle_success_response` is
        // account-agnostic and continues to run; this site only persists
        // the cookie-specific quota counters.
        if self
            .cookie
            .as_ref()
            .is_some_and(|s| s.auth_method == AuthMethod::ApiKey)
        {
            return;
        }
        if let Some(cookie) = self.cookie.as_mut() {
            // Lazy boundary refresh if due, then reset period counters and start fresh
            Self::update_cookie_boundaries_if_due(cookie, &self.account_pool_handle).await;
            cookie.add_and_bucket_usage(input, output, family);
            let Some(account_id) = cookie.account_id else {
                return;
            };
            let update = cookie.to_runtime_params();
            let fingerprint = CredentialFingerprint::from_slot(cookie);
            if let Err(err) = self
                .account_pool_handle
                .release_runtime(account_id, update, None, fingerprint)
                .await
            {
                warn!("Failed to persist usage statistics: {}", err);
            }
        }
    }

    async fn forward_stream_with_usage(
        &mut self,
        response: wreq::Response,
        family: ModelFamily,
    ) -> Result<axum::response::Response, ClewdrError> {
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        };

        let input_tokens = self.usage.input_tokens as u64;
        let output_sum = Arc::new(AtomicU64::new(0));
        let input_sum = Arc::new(AtomicU64::new(input_tokens));
        let cache_create_sum = Arc::new(AtomicU64::new(0));
        let cache_read_sum = Arc::new(AtomicU64::new(0));
        let ttft_ms = Arc::new(AtomicI64::new(-1));
        let handle = self.account_pool_handle.clone();
        let cookie = self.cookie.clone();
        let billing_ctx = self.billing_ctx.clone();
        let billing_ctx_for_stream = billing_ctx.clone();
        // TTFT zero point: earliest time clewdr knew about this request (set in middleware).
        // Measuring from here (instead of after upstream response headers arrive) makes the
        // metric immune to reverse-proxy response buffering and also reflects clewdr's own
        // cookie-selection / token-refresh / handshake overhead — i.e. the real user-perceived
        // time to first token.
        let ttft_started_at = billing_ctx.as_ref().map(|c| c.started_at);
        let request_id_for_stream = billing_ctx
            .as_ref()
            .map(|ctx| ctx.request_id.clone())
            .unwrap_or_default();
        let stream_account_id = self
            .account_id
            .or(cookie.as_ref().and_then(|c| c.account_id));
        let slot_released = Arc::new(AtomicBool::new(false));
        let slot_released_inner = slot_released.clone();
        let stream_completed = Arc::new(AtomicBool::new(false));
        let saw_upstream_usage = Arc::new(AtomicBool::new(false));
        let upstream_failed = Arc::new(AtomicBool::new(false));
        let abort_error = Arc::new(Mutex::new(None::<String>));

        let osum = output_sum.clone();
        let isum = input_sum.clone();
        let ccsum = cache_create_sum.clone();
        let crsum = cache_read_sum.clone();
        let ttft = ttft_ms.clone();
        let completed = stream_completed.clone();
        let saw_usage = saw_upstream_usage.clone();
        let upstream_failed_for_events = upstream_failed.clone();
        let abort_error_for_events = abort_error.clone();
        let request_id_for_events = request_id_for_stream.clone();
        let stream = response
            .bytes_stream()
            .eventsource()
            .map_ok(move |event| {
                if let Ok(parsed) =
                    serde_json::from_str::<crate::types::claude::StreamEvent>(&event.data)
                {
                    match parsed {
                        crate::types::claude::StreamEvent::MessageStart { message } => {
                            // Capture authoritative input/cache usage from upstream
                            if let Some(u) = message.usage {
                                saw_usage.store(true, Ordering::Relaxed);
                                isum.store(u.input_tokens as u64, Ordering::Relaxed);
                                if let Some(cc) = u.cache_creation_input_tokens {
                                    ccsum.store(cc as u64, Ordering::Relaxed);
                                }
                                if let Some(cr) = u.cache_read_input_tokens {
                                    crsum.store(cr as u64, Ordering::Relaxed);
                                }
                            }
                        }
                        crate::types::claude::StreamEvent::ContentBlockDelta { .. } => {
                            if let Some(started) = ttft_started_at {
                                let elapsed = (chrono::Utc::now() - started).num_milliseconds();
                                if elapsed >= 0 {
                                    let _ = ttft.compare_exchange(
                                        -1,
                                        elapsed,
                                        Ordering::Relaxed,
                                        Ordering::Relaxed,
                                    );
                                }
                            }
                        }
                        crate::types::claude::StreamEvent::MessageDelta {
                            usage: Some(u), ..
                        } => {
                            // usage fields in message_delta are cumulative, use store not add
                            saw_usage.store(true, Ordering::Relaxed);
                            osum.store(u.output_tokens as u64, Ordering::Relaxed);
                            // message_delta also carries final input/cache values
                            if u.input_tokens > 0 {
                                isum.store(u.input_tokens as u64, Ordering::Relaxed);
                            }
                            if let Some(cc) = u.cache_creation_input_tokens {
                                ccsum.store(cc as u64, Ordering::Relaxed);
                            }
                            if let Some(cr) = u.cache_read_input_tokens {
                                crsum.store(cr as u64, Ordering::Relaxed);
                            }
                        }
                        crate::types::claude::StreamEvent::Error { error } => {
                            upstream_failed_for_events.store(true, Ordering::Relaxed);
                            warn!(
                                "[STREAM][ERR] request_id={} upstream returned SSE error: {}",
                                request_id_for_events, error.message
                            );
                            if let Ok(mut msg) = abort_error_for_events.lock() {
                                *msg = Some(error.message);
                            }
                        }
                        crate::types::claude::StreamEvent::MessageStop => {
                            completed.store(true, Ordering::Relaxed);
                            let total_input = isum.load(Ordering::Relaxed);
                            let total_out = osum.load(Ordering::Relaxed);
                            let total_cc = ccsum.load(Ordering::Relaxed);
                            let total_cr = crsum.load(Ordering::Relaxed);

                            // Cookie persistence + slot release.
                            // ApiKey skips the runtime persist (no
                            // quota buckets to add into, no boundary
                            // refresh) but still must release the slot
                            // back to the pool. Billing persistence
                            // below is account-agnostic.
                            if let (Some(cookie), handle) = (cookie.clone(), handle.clone()) {
                                let mut c = cookie.clone();
                                let aid = stream_account_id;
                                let released = slot_released_inner.clone();
                                let skip_runtime = c.auth_method == AuthMethod::ApiKey;
                                tokio::spawn(async move {
                                    if !skip_runtime {
                                        ClaudeCodeState::update_cookie_boundaries_if_due(
                                            &mut c, &handle,
                                        )
                                        .await;
                                        c.add_and_bucket_usage(total_input, total_out, family);
                                        if let Some(account_id) = c.account_id {
                                            let update = c.to_runtime_params();
                                            let fingerprint = CredentialFingerprint::from_slot(&c);
                                            let _ = handle
                                                .release_runtime(
                                                    account_id,
                                                    update,
                                                    None,
                                                    fingerprint,
                                                )
                                                .await;
                                        }
                                    }
                                    if let Some(aid) = aid
                                        && !released.swap(true, Ordering::Relaxed)
                                    {
                                        handle.release_slot(aid).await;
                                    }
                                });
                            }

                            // Billing persistence
                            if let Some(ctx) = billing_ctx_for_stream.clone() {
                                let ttft_val = ttft.load(Ordering::Relaxed);
                                let usage = crate::billing::BillingUsage {
                                    input_tokens: total_input,
                                    output_tokens: total_out,
                                    cache_creation_tokens: total_cc,
                                    cache_read_tokens: total_cr,
                                    ttft_ms: if ttft_val >= 0 { Some(ttft_val) } else { None },
                                };
                                tokio::spawn(async move {
                                    crate::billing::persist_billing_to_db(&ctx, usage, true).await;
                                });
                            }
                        }
                        _ => {}
                    }
                }
                // mirror upstream SSE event unchanged
                let e = SseEvent::default().event(event.event).id(event.id);
                let e = if let Some(retry) = event.retry {
                    e.retry(retry)
                } else {
                    e
                };
                e.data(event.data)
            })
            .map_err({
                let upstream_failed = upstream_failed.clone();
                let abort_error = abort_error.clone();
                let request_id_for_stream = request_id_for_stream.clone();
                move |err| {
                    upstream_failed.store(true, Ordering::Relaxed);
                    warn!(
                        "[STREAM][ERR] request_id={} eventsource stream error: {}",
                        request_id_for_stream, err
                    );
                    if let Ok(mut msg) = abort_error.lock() {
                        *msg = Some(err.to_string());
                    }
                    err
                }
            });

        // Drop guard: release slot when stream ends abnormally (client disconnect, upstream error)
        struct SlotDropGuard {
            released: Arc<AtomicBool>,
            completed: Arc<AtomicBool>,
            account_id: Option<i64>,
            handle: AccountPoolHandle,
            cookie: Option<crate::config::AccountSlot>,
            family: ModelFamily,
            billing_ctx: Option<crate::billing::BillingContext>,
            input_sum: Arc<AtomicU64>,
            output_sum: Arc<AtomicU64>,
            cache_create_sum: Arc<AtomicU64>,
            cache_read_sum: Arc<AtomicU64>,
            ttft_ms: Arc<AtomicI64>,
            saw_upstream_usage: Arc<AtomicBool>,
            upstream_failed: Arc<AtomicBool>,
            abort_error: Arc<Mutex<Option<String>>>,
        }
        impl Drop for SlotDropGuard {
            fn drop(&mut self) {
                let completed = self.completed.load(Ordering::Relaxed);
                let total_input = self.input_sum.load(Ordering::Relaxed);
                let total_output = self.output_sum.load(Ordering::Relaxed);
                let total_cache_create = self.cache_create_sum.load(Ordering::Relaxed);
                let total_cache_read = self.cache_read_sum.load(Ordering::Relaxed);
                let saw_upstream_usage = self.saw_upstream_usage.load(Ordering::Relaxed);
                let upstream_failed = self.upstream_failed.load(Ordering::Relaxed);
                let ttft_val = self.ttft_ms.load(Ordering::Relaxed);
                let status = if upstream_failed {
                    "upstream_error"
                } else {
                    "client_abort"
                };
                let http_status = if upstream_failed { 502 } else { 499 };
                let error_message = self
                    .abort_error
                    .lock()
                    .ok()
                    .and_then(|msg| msg.clone())
                    .unwrap_or_else(|| "stream ended before message_stop".to_string());
                let should_persist_usage = saw_upstream_usage
                    || total_output > 0
                    || total_cache_create > 0
                    || total_cache_read > 0;

                if let Some(aid) = self.account_id {
                    if !self.released.swap(true, Ordering::Relaxed) {
                        let h = self.handle.clone();
                        let cookie = self.cookie.clone();
                        let family = self.family;
                        let billing_ctx = self.billing_ctx.clone();
                        tokio::spawn(async move {
                            if !completed {
                                if let Some(mut cookie) = cookie {
                                    // ApiKey: skip subscription-runtime
                                    // persistence (no quota buckets, no
                                    // boundary refresh). Billing
                                    // terminal log + slot release below
                                    // still run.
                                    if cookie.auth_method != AuthMethod::ApiKey {
                                        if should_persist_usage {
                                            ClaudeCodeState::update_cookie_boundaries_if_due(
                                                &mut cookie,
                                                &h,
                                            )
                                            .await;
                                            cookie.add_and_bucket_usage(
                                                total_input,
                                                total_output,
                                                family,
                                            );
                                        }
                                        if let Some(account_id) = cookie.account_id {
                                            let update = cookie.to_runtime_params();
                                            let fingerprint =
                                                CredentialFingerprint::from_slot(&cookie);
                                            let _ = h
                                                .release_runtime(
                                                    account_id,
                                                    update,
                                                    None,
                                                    fingerprint,
                                                )
                                                .await;
                                        }
                                    }
                                }
                                if let Some(ctx) = billing_ctx {
                                    let usage = should_persist_usage.then_some(
                                        crate::billing::BillingUsage {
                                            input_tokens: total_input,
                                            output_tokens: total_output,
                                            cache_creation_tokens: total_cache_create,
                                            cache_read_tokens: total_cache_read,
                                            ttft_ms: if ttft_val >= 0 {
                                                Some(ttft_val)
                                            } else {
                                                None
                                            },
                                        },
                                    );
                                    crate::billing::persist_terminal_request_log(
                                        &ctx,
                                        TerminalLogOptions {
                                            request_type: RequestType::Messages,
                                            stream: true,
                                            status,
                                            http_status: Some(http_status),
                                            usage,
                                            error_code: Some(status),
                                            error_message: Some(error_message.as_str()),
                                            update_rollups: should_persist_usage,
                                            response_body: None,
                                        },
                                    )
                                    .await;
                                }
                            }
                            h.release_slot(aid).await;
                        });
                    }
                } else if !completed {
                    let billing_ctx = self.billing_ctx.clone();
                    tokio::spawn(async move {
                        if let Some(ctx) = billing_ctx {
                            let usage =
                                should_persist_usage.then_some(crate::billing::BillingUsage {
                                    input_tokens: total_input,
                                    output_tokens: total_output,
                                    cache_creation_tokens: total_cache_create,
                                    cache_read_tokens: total_cache_read,
                                    ttft_ms: if ttft_val >= 0 { Some(ttft_val) } else { None },
                                });
                            crate::billing::persist_terminal_request_log(
                                &ctx,
                                TerminalLogOptions {
                                    request_type: RequestType::Messages,
                                    stream: true,
                                    status,
                                    http_status: Some(http_status),
                                    usage,
                                    error_code: Some(status),
                                    error_message: Some(error_message.as_str()),
                                    update_rollups: should_persist_usage,
                                    response_body: None,
                                },
                            )
                            .await;
                        }
                    });
                }
            }
        }
        let guard = SlotDropGuard {
            released: slot_released,
            completed: stream_completed,
            account_id: stream_account_id,
            handle: self.account_pool_handle.clone(),
            cookie: self.cookie.clone(),
            family,
            billing_ctx,
            input_sum,
            output_sum,
            cache_create_sum,
            cache_read_sum,
            ttft_ms,
            saw_upstream_usage,
            upstream_failed,
            abort_error,
        };
        let stream = stream.map(move |item| {
            let _ = &guard;
            item
        });

        Ok(Sse::new(stream)
            .keep_alive(Default::default())
            .into_response())
    }

    async fn materialize_non_stream_response(
        response: wreq::Response,
    ) -> Result<
        (
            axum::response::Response,
            Option<crate::billing::BillingUsage>,
        ),
        ClewdrError,
    > {
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.context(WreqSnafu {
            msg: "Failed to read Claude response body",
        })?;
        let usage = Self::extract_usage_from_bytes(&bytes);

        let mut builder = http::Response::builder().status(status);
        for (key, value) in headers.iter() {
            builder = builder.header(key, value);
        }
        let response =
            builder
                .body(axum::body::Body::from(bytes))
                .map_err(|e| ClewdrError::HttpError {
                    loc: snafu::Location::generate(),
                    source: e,
                })?;
        Ok((response, usage))
    }

    async fn materialize_count_tokens_response(
        response: wreq::Response,
    ) -> Result<(axum::response::Response, CountMessageTokensResponse), ClewdrError> {
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.context(WreqSnafu {
            msg: "Failed to read Claude count_tokens response body",
        })?;
        let parsed = serde_json::from_slice::<CountMessageTokensResponse>(&bytes)
            .map_err(|source| ClewdrError::JsonError { source })?;

        let mut builder = http::Response::builder().status(status);
        for (key, value) in headers.iter() {
            builder = builder.header(key, value);
        }
        let response =
            builder
                .body(axum::body::Body::from(bytes))
                .map_err(|e| ClewdrError::HttpError {
                    loc: snafu::Location::generate(),
                    source: e,
                })?;
        Ok((response, parsed))
    }

    fn extract_usage_from_bytes(bytes: &[u8]) -> Option<crate::billing::BillingUsage> {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes)
            && let Some(usage) = value.get("usage")
        {
            let get_u64 = |key: &str| {
                usage
                    .get(key)
                    .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|n| n.max(0) as u64)))
            };
            if let (Some(input), Some(output)) = (get_u64("input_tokens"), get_u64("output_tokens"))
            {
                return Some(crate::billing::BillingUsage {
                    input_tokens: input,
                    output_tokens: output,
                    cache_creation_tokens: get_u64("cache_creation_input_tokens").unwrap_or(0),
                    cache_read_tokens: get_u64("cache_read_input_tokens").unwrap_or(0),
                    ttft_ms: None,
                });
            }
        }

        // Fallback: estimate output tokens from the Claude response content
        if let Ok(parsed) =
            serde_json::from_slice::<crate::types::claude::CreateMessageResponse>(bytes)
        {
            let output_tokens = parsed.count_tokens() as u64;
            return Some(crate::billing::BillingUsage {
                input_tokens: 0,
                output_tokens,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                ttft_ms: None,
            });
        }
        None
    }

    async fn execute_claude_count_tokens_request(
        &mut self,
        access_token: &str,
        body: &CreateMessageParams,
    ) -> Result<wreq::Response, ClewdrError> {
        if self
            .cookie
            .as_ref()
            .is_some_and(|s| s.auth_method == AuthMethod::ApiKey)
        {
            return self
                .execute_api_key_request(
                    "v1/messages/count_tokens",
                    body,
                    "Failed to call Claude count_tokens",
                )
                .await;
        }
        let profile = self.stealth_profile.load();
        let beta_header = Self::merge_anthropic_beta_header(self.anthropic_beta_header.as_deref());
        let mut url = self
            .endpoint
            .join("v1/messages/count_tokens")
            .expect("Url parse error");
        url.set_query(Some("beta=true"));
        self.client
            .post(url.to_string())
            .bearer_auth(access_token)
            .header(USER_AGENT, profile.user_agent())
            .header("anthropic-beta", beta_header)
            .header("anthropic-version", CLAUDE_API_VERSION)
            .json(body)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to call Claude count_tokens",
            })?
            .check_claude()
            .await
    }

    fn merge_anthropic_beta_header(extra: Option<&str>) -> String {
        let mut seen = std::collections::HashSet::new();
        let mut merged = Vec::new();
        let mut push = |token: &str| {
            let trimmed = token.trim();
            if trimmed.is_empty() {
                return;
            }
            let key = trimmed.to_ascii_lowercase();
            if key == CLAUDE_BETA_CONTEXT_1M_TOKEN {
                return;
            }
            if seen.insert(key) {
                merged.push(trimmed.to_string());
            }
        };

        push(CLAUDE_BETA_BASE);
        if let Some(extra) = extra {
            for token in extra.split(',') {
                push(token);
            }
        }

        merged.join(",")
    }

    fn classify_model(model: &str) -> ModelFamily {
        let m = model.to_ascii_lowercase();
        if m.contains("opus") {
            ModelFamily::Opus
        } else if m.contains("sonnet") {
            ModelFamily::Sonnet
        } else {
            ModelFamily::Other
        }
    }

    // ---------------------------------------------
    // Lazy boundary refresh (no timers, fetch-on-due)
    // ---------------------------------------------
    async fn update_cookie_boundaries_if_due(
        cookie: &mut crate::config::AccountSlot,
        handle: &crate::services::account_pool::AccountPoolHandle,
    ) {
        let now = chrono::Utc::now().timestamp();
        const SESSION_WINDOW_SECS: i64 = 5 * 60 * 60; // 5h
        const WEEKLY_WINDOW_SECS: i64 = 7 * 24 * 60 * 60; // 7d

        let tracked = |flag: Option<bool>| flag == Some(true);
        let unknown = |flag: Option<bool>| flag.is_none();
        let due = |ts: Option<i64>| ts.map(|t| now >= t).unwrap_or(false);

        let session_tracked = tracked(cookie.session_has_reset);
        let weekly_tracked = tracked(cookie.weekly_has_reset);
        let sonnet_tracked = tracked(cookie.weekly_sonnet_has_reset);
        let opus_tracked = tracked(cookie.weekly_opus_has_reset);

        let session_due = session_tracked && due(cookie.session_resets_at);
        let weekly_due = weekly_tracked && due(cookie.weekly_resets_at);
        let sonnet_due = sonnet_tracked && due(cookie.weekly_sonnet_resets_at);
        let opus_due = opus_tracked && due(cookie.weekly_opus_resets_at);

        let need_probe_unknown = unknown(cookie.session_has_reset)
            || unknown(cookie.weekly_has_reset)
            || unknown(cookie.weekly_sonnet_has_reset)
            || unknown(cookie.weekly_opus_has_reset);
        let any_due = session_due || weekly_due || sonnet_due || opus_due;

        if !(need_probe_unknown || any_due) {
            return;
        }

        cookie.resets_last_checked_at = Some(now);
        let fetched = tokio::time::timeout(
            Duration::from_secs(15),
            Self::fetch_usage_resets(cookie, handle),
        )
        .await
        .ok()
        .flatten();

        if let Some((sess, week, opus, sonnet)) = fetched {
            // Unknown -> decide track/not-track
            if unknown(cookie.session_has_reset) {
                cookie.session_has_reset = Some(sess.is_some());
            }
            if unknown(cookie.weekly_has_reset) {
                cookie.weekly_has_reset = Some(week.is_some());
            }
            if unknown(cookie.weekly_sonnet_has_reset) {
                cookie.weekly_sonnet_has_reset = Some(sonnet.is_some());
            }
            if unknown(cookie.weekly_opus_has_reset) {
                cookie.weekly_opus_has_reset = Some(opus.is_some());
            }

            // Handle due tracked windows: reset usage then update boundaries if provided
            if session_due {
                cookie.session_usage = crate::config::UsageBreakdown::default();
            }
            if weekly_due {
                cookie.weekly_usage = crate::config::UsageBreakdown::default();
            }
            if sonnet_due {
                cookie.weekly_sonnet_usage = crate::config::UsageBreakdown::default();
            }
            if opus_due {
                cookie.weekly_opus_usage = crate::config::UsageBreakdown::default();
            }

            // Update/reset boundaries for tracked windows
            if cookie.session_has_reset == Some(true) {
                if let Some(ts) = sess {
                    cookie.session_resets_at = Some(ts);
                } else {
                    // Server indicates no boundary -> stop tracking and clear ts
                    cookie.session_has_reset = Some(false);
                    cookie.session_resets_at = None;
                }
            }
            if cookie.weekly_has_reset == Some(true) {
                if let Some(ts) = week {
                    cookie.weekly_resets_at = Some(ts);
                } else {
                    cookie.weekly_has_reset = Some(false);
                    cookie.weekly_resets_at = None;
                }
            }
            if cookie.weekly_sonnet_has_reset == Some(true) {
                if let Some(ts) = sonnet {
                    cookie.weekly_sonnet_resets_at = Some(ts);
                } else {
                    cookie.weekly_sonnet_has_reset = Some(false);
                    cookie.weekly_sonnet_resets_at = None;
                }
            }
            if cookie.weekly_opus_has_reset == Some(true) {
                if let Some(ts) = opus {
                    cookie.weekly_opus_resets_at = Some(ts);
                } else {
                    cookie.weekly_opus_has_reset = Some(false);
                    cookie.weekly_opus_resets_at = None;
                }
            }
        } else {
            // Network/parse failure: apply fallback only for windows we currently track
            if session_due && session_tracked {
                cookie.session_usage = crate::config::UsageBreakdown::default();
                cookie.session_resets_at = Some(now + SESSION_WINDOW_SECS);
            }
            if weekly_due && weekly_tracked {
                cookie.weekly_usage = crate::config::UsageBreakdown::default();
                cookie.weekly_resets_at = Some(now + WEEKLY_WINDOW_SECS);
            }
            if sonnet_due && sonnet_tracked {
                cookie.weekly_sonnet_usage = crate::config::UsageBreakdown::default();
                cookie.weekly_sonnet_resets_at = Some(now + WEEKLY_WINDOW_SECS);
            }
            if opus_due && opus_tracked {
                cookie.weekly_opus_usage = crate::config::UsageBreakdown::default();
                cookie.weekly_opus_resets_at = Some(now + WEEKLY_WINDOW_SECS);
            }
        }
    }

    async fn fetch_usage_resets(
        cookie: &mut crate::config::AccountSlot,
        handle: &AccountPoolHandle,
    ) -> Option<(Option<i64>, Option<i64>, Option<i64>, Option<i64>)> {
        let profile = crate::stealth::global_profile().clone();
        let mut state =
            ClaudeCodeState::from_credential(handle.clone(), cookie.clone(), profile).ok()?;
        let usage = state.fetch_usage_metrics().await.ok()?;
        state.release_account(None).await;
        if let Some(updated) = state.cookie.clone() {
            *cookie = updated;
        }

        let parse_window = |obj_key: &str| -> (Option<i64>, Option<f64>) {
            let obj = usage.get(obj_key);
            let resets_at = obj
                .and_then(|o| o.get("resets_at"))
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.timestamp());
            let utilization = obj
                .and_then(|o| o.get("utilization"))
                .and_then(|v| v.as_f64());
            (resets_at, utilization)
        };

        let (sess_ts, sess_util) = parse_window("five_hour");
        let (week_ts, week_util) = parse_window("seven_day");
        let (opus_ts, opus_util) = parse_window("seven_day_opus");
        let (sonnet_ts, sonnet_util) = parse_window("seven_day_sonnet");

        cookie.session_utilization = sess_util;
        cookie.weekly_utilization = week_util;
        cookie.weekly_opus_utilization = opus_util;
        cookie.weekly_sonnet_utilization = sonnet_util;

        Some((sess_ts, week_ts, opus_ts, sonnet_ts))
    }

    fn local_count_tokens_response(
        body: &CreateMessageParams,
    ) -> (axum::response::Response, CountMessageTokensResponse) {
        let estimate = CountMessageTokensResponse {
            input_tokens: body.count_tokens(),
        };
        (Json(estimate.clone()).into_response(), estimate)
    }

    fn is_count_tokens_unauthorized(error: &ClewdrError) -> bool {
        if let ClewdrError::ClaudeHttpError { code, .. } = error {
            return matches!(code.as_u16(), 401 | 403 | 404);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::ClaudeCodeState;
    use crate::{config::Reason, error::ClewdrError};

    #[test]
    fn oauth_cooldown_detects_temporary_invalid_cookie_reasons() {
        assert_eq!(
            ClaudeCodeState::oauth_cooldown_until(&ClewdrError::InvalidCookie {
                reason: Reason::TooManyRequest(123),
            }),
            Some(123)
        );
        assert_eq!(
            ClaudeCodeState::oauth_cooldown_until(&ClewdrError::InvalidCookie {
                reason: Reason::Restricted(456),
            }),
            Some(456)
        );
        assert_eq!(
            ClaudeCodeState::oauth_cooldown_until(&ClewdrError::InvalidCookie {
                reason: Reason::Null,
            }),
            None
        );
    }

    #[test]
    fn oauth_pool_reason_maps_auth_and_cooldown_errors_for_pool_eviction() {
        assert_eq!(
            ClaudeCodeState::oauth_pool_reason(&ClewdrError::InvalidCookie {
                reason: Reason::Restricted(456),
            }),
            Some(Reason::Restricted(456))
        );
        assert_eq!(
            ClaudeCodeState::oauth_pool_reason(&ClewdrError::InvalidCookie {
                reason: Reason::Disabled,
            }),
            Some(Reason::Disabled)
        );
        assert_eq!(
            ClaudeCodeState::oauth_pool_reason(&ClewdrError::Whatever {
                message: "invalid_grant".to_string(),
                source: None,
            }),
            Some(Reason::Null)
        );
    }

    /// Step 3.5 C2 regression guards. The classifier rewrite must preserve
    /// these subtle edges:
    /// 1. `Reason::Free` is `TerminalDisabled` at the action level but is
    ///    NOT `is_oauth_disabled_failure` — it goes through a separate
    ///    Free-tier path that must not converge with the org-disabled
    ///    branch.
    /// 2. Bare 401/403 (no phrase) is auth-rejected and must produce
    ///    `Reason::Null` for pool eviction.
    /// 3. Transient / internal errors must not produce a pool reason.
    #[test]
    fn oauth_disabled_failure_distinguishes_free_from_org_disabled() {
        assert!(ClaudeCodeState::is_oauth_disabled_failure(
            &ClewdrError::InvalidCookie {
                reason: Reason::Disabled,
            }
        ));
        assert!(!ClaudeCodeState::is_oauth_disabled_failure(
            &ClewdrError::InvalidCookie {
                reason: Reason::Free,
            }
        ));
        assert!(!ClaudeCodeState::is_oauth_disabled_failure(
            &ClewdrError::InvalidCookie {
                reason: Reason::Banned,
            }
        ));
    }

    #[test]
    fn oauth_pool_reason_treats_unphrased_401_403_as_null_eviction() {
        use crate::error::ClaudeErrorBody;
        use serde_json::json;
        use wreq::StatusCode;

        let http = |status: u16| ClewdrError::ClaudeHttpError {
            code: StatusCode::from_u16(status).unwrap(),
            inner: Box::new(ClaudeErrorBody {
                message: json!("upstream"),
                r#type: "error".to_string(),
                code: Some(status),
                ..Default::default()
            }),
        };
        assert_eq!(
            ClaudeCodeState::oauth_pool_reason(&http(401)),
            Some(Reason::Null)
        );
        assert_eq!(
            ClaudeCodeState::oauth_pool_reason(&http(403)),
            Some(Reason::Null)
        );
        // 5xx is transient — must NOT evict.
        assert_eq!(ClaudeCodeState::oauth_pool_reason(&http(500)), None);
        // Local logic errors must not evict either.
        assert_eq!(
            ClaudeCodeState::oauth_pool_reason(&ClewdrError::Whatever {
                message: "unrelated local failure".to_string(),
                source: None,
            }),
            None
        );
    }

    #[test]
    fn api_key_transient_retry_policy_does_not_retry_caller_4xx() {
        use crate::error::ClaudeErrorBody;
        use serde_json::json;
        use wreq::StatusCode;

        let http = |status: u16, kind: &str| ClewdrError::ClaudeHttpError {
            code: StatusCode::from_u16(status).unwrap(),
            inner: Box::new(ClaudeErrorBody {
                message: json!("upstream"),
                r#type: kind.to_string(),
                code: Some(status),
                ..Default::default()
            }),
        };

        assert!(!ClaudeCodeState::should_retry_api_key_transient(&http(
            400,
            "invalid_request_error"
        )));
        assert!(!ClaudeCodeState::should_retry_api_key_transient(&http(
            422,
            "invalid_request_error"
        )));
        assert!(ClaudeCodeState::should_retry_api_key_transient(&http(
            429,
            "rate_limit_error"
        )));
        assert!(ClaudeCodeState::should_retry_api_key_transient(&http(
            500,
            "api_error"
        )));
        assert!(ClaudeCodeState::should_retry_api_key_transient(
            &ClewdrError::Whatever {
                message: "temporary transport wrapper".to_string(),
                source: None,
            }
        ));
    }

    /// `try_chat` and `try_count_tokens` route to the OAuth bearer path
    /// when `slot.auth_method == AuthMethod::OAuth`. A cookie-backed slot
    /// that has acquired a short-lived bearer token via `exchange_token`
    /// (slot.token = Some(_)) must STILL be classified as cookie — token
    /// presence is not a kind discriminator. This codifies the regression
    /// guard against re-introducing token-based dispatch logic.
    #[test]
    fn dispatch_decision_is_driven_by_auth_method_not_token_presence() {
        use crate::config::{AccountSlot, AuthMethod, TokenInfo};

        let cookie_str = format!(
            "sk-ant-sid01-{}-aaaaaaAA",
            std::iter::repeat_n('a', 86).collect::<String>()
        );

        // Cookie account post-`exchange_token`: token is set but kind is Cookie.
        let mut cookie_with_bearer = AccountSlot::new(&cookie_str, None).unwrap();
        cookie_with_bearer.token = Some(TokenInfo::from_parts(
            "at".into(),
            "rt".into(),
            std::time::Duration::from_secs(3600),
            "org-uuid".into(),
        ));
        assert_eq!(cookie_with_bearer.auth_method, AuthMethod::Cookie);
        let is_pure_oauth = cookie_with_bearer.auth_method == AuthMethod::OAuth;
        assert!(
            !is_pure_oauth,
            "cookie account holding a bearer token must NOT be sent down the OAuth path"
        );

        // OAuth account: kind is OAuth regardless of token state.
        let oauth_slot = AccountSlot {
            auth_method: AuthMethod::OAuth,
            ..AccountSlot::default()
        };
        assert!(oauth_slot.auth_method == AuthMethod::OAuth);
    }

    /// Step 3.5 C4b: `classify_persisted` produces a Send-safe owned
    /// DTO that the caller can carry across `.await` boundaries. This
    /// is the convention used by both messages / count_tokens
    /// failure paths.
    #[test]
    fn classify_persisted_produces_owned_send_dto() {
        use crate::error::ClewdrError;
        use crate::services::account_error::FailureSource;

        let err = ClewdrError::InvalidCookie {
            reason: Reason::TooManyRequest(123),
        };
        let persisted = super::ClaudeCodeState::classify_persisted(&err, FailureSource::Messages);

        // Sanity: source threaded through, normalized_reason_type is
        // the stable string consumers will actually read.
        assert_eq!(persisted.source, FailureSource::Messages);
        assert_eq!(persisted.normalized_reason_type, "rate_limited");

        // Send check: we can move the persisted into a tokio::spawn
        // body. If the field types regress to non-Send (e.g.,
        // accidentally adopting `Rc` or borrowing static refs),
        // this test fails to compile.
        fn assert_send<T: Send + 'static>(_: &T) {}
        assert_send(&persisted);
    }

    /// Step 3.5 C4b: a CountTokens-source classification carries the
    /// distinct source through to the persisted DTO so AccountHealth
    /// can show "this failed during count_tokens, not messages".
    #[test]
    fn classify_persisted_distinguishes_count_tokens_source() {
        use crate::error::ClewdrError;
        use crate::services::account_error::FailureSource;

        let err = ClewdrError::InvalidCookie {
            reason: Reason::Null,
        };
        let messages = super::ClaudeCodeState::classify_persisted(&err, FailureSource::Messages);
        let count = super::ClaudeCodeState::classify_persisted(&err, FailureSource::CountTokens);
        assert_eq!(messages.source, FailureSource::Messages);
        assert_eq!(count.source, FailureSource::CountTokens);
        // Same Reason::Null + same default stage → same normalized
        // type; only `source` differs.
        assert_eq!(
            messages.normalized_reason_type,
            count.normalized_reason_type
        );
    }

    /// Step 5 C7: the ApiKey beta-header composer drops the OAuth
    /// subscription base (`oauth-2025-04-20`) but, unlike the
    /// subscription path's `merge_anthropic_beta_header`, must NOT
    /// filter `context-1m-2025-08-07` — that token is a legitimate
    /// request-level capability the client is entitled to opt into
    /// on direct-API.
    #[test]
    fn api_key_beta_header_strips_oauth_base_keeps_others() {
        use super::api_key_beta_header;

        // Pure oauth base → header omitted (None) so caller does not
        // emit an empty `anthropic-beta:` line.
        assert_eq!(api_key_beta_header(Some("oauth-2025-04-20")), None);
        assert_eq!(api_key_beta_header(Some("OAUTH-2025-04-20")), None);
        assert_eq!(api_key_beta_header(None), None);
        assert_eq!(api_key_beta_header(Some("")), None);
        assert_eq!(api_key_beta_header(Some("  ,  ,")), None);

        // The 1M context beta survives — regression guard against
        // re-using the subscription-path merge that silently strips it.
        assert_eq!(
            api_key_beta_header(Some("context-1m-2025-08-07")).as_deref(),
            Some("context-1m-2025-08-07"),
        );

        // Mixed: oauth base dropped, others preserved in order with
        // whitespace normalized.
        assert_eq!(
            api_key_beta_header(Some(
                "oauth-2025-04-20, context-1m-2025-08-07, fine-grained-tool-streaming-2025-05-14"
            ))
            .as_deref(),
            Some("context-1m-2025-08-07,fine-grained-tool-streaming-2025-05-14"),
        );

        // Empty tokens dropped between commas.
        assert_eq!(
            api_key_beta_header(Some("context-1m-2025-08-07,, ,prompt-caching-2024-07-31"))
                .as_deref(),
            Some("context-1m-2025-08-07,prompt-caching-2024-07-31"),
        );
    }

    /// Step 5 C7: send-time reserved-name filter is case-insensitive
    /// and covers every header the ApiKey dispatch sets itself or the
    /// transport layer owns. Defense in depth — the admin write-time
    /// validator (C10) is the primary guard, but a manual DB edit
    /// could slip past it and we must not let extra_headers re-inject
    /// e.g. `User-Agent` (which would silently restore the CC stealth
    /// UA the ApiKey path deliberately omits).
    #[test]
    fn reserved_api_key_extra_header_filter_is_case_insensitive() {
        use super::is_reserved_api_key_extra_header;
        for name in [
            "x-api-key",
            "X-API-KEY",
            "x-Api-Key",
            "authorization",
            "Authorization",
            "anthropic-version",
            "anthropic-beta",
            "user-agent",
            "USER-AGENT",
            "host",
            "content-length",
            "content-type",
            "accept-encoding",
        ] {
            assert!(
                is_reserved_api_key_extra_header(name),
                "{name} should be reserved",
            );
        }
        for name in [
            "anthropic-workspace-id",
            "x-request-id",
            "x-custom",
            "accept",
            "cache-control",
            "",
        ] {
            assert!(
                !is_reserved_api_key_extra_header(name),
                "{name} should NOT be reserved",
            );
        }
    }
}
