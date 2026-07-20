use axum::{
    Json,
    body::{Body, Bytes},
    response::{IntoResponse, Sse, sse::Event as SseEvent},
};
use colored::Colorize;
use eventsource_stream::Eventsource;
use futures::{StreamExt, TryStreamExt};
use http::header::{
    ACCEPT, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING,
    USER_AGENT,
};
use snafu::{GenerateImplicitData, ResultExt};
use std::{
    collections::HashMap,
    convert::Infallible,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
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
        set_account_auth_error, set_account_disabled, set_account_last_failure_logged,
        set_account_reset_time, update_account_metadata_unchecked, upsert_account_oauth,
        upsert_oauth_snapshot_runtime_fields,
    },
    error::{CheckClaudeErr, ClewdrError, WreqSnafu},
    mimicry::STAINLESS_HEADERS,
    oauth::refresh_oauth_token,
    services::account_error::{
        AccountFailureAction, AccountFailureContextPersisted, AccountNormalizedReason,
        FailureSource, classify_account_failure,
    },
    services::account_pool::{AccountPoolHandle, CredentialFingerprint},
    stealth,
    types::claude::{CountMessageTokensResponse, CreateMessageParams, Role},
};

// Re-exported so existing call sites (`chat::is_reserved_api_key_extra_header`,
// the `crate::claude_code_state` re-export, and this module's tests) keep
// resolving after the definition moved to `crate::mimicry`.
pub(crate) use crate::mimicry::is_reserved_api_key_extra_header;

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const MAX_RETRIES: usize = 5;
const MESSAGES_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const COUNT_TOKENS_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(60);
const CLAUDE_BETA_BASE: &str = "oauth-2025-04-20";
const CLAUDE_BETA_CONTEXT_1M_TOKEN: &str = "context-1m-2025-08-07";
const CLAUDE_API_VERSION: &str = "2023-06-01";

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

/// Rebuild a regular Messages JSON response from an upstream SSE sequence.
///
/// Values are accumulated as JSON rather than through the typed content-block
/// enum so new upstream block fields survive the bridge unchanged. Only the
/// delta-bearing fields need explicit merge behavior.
#[derive(Default)]
struct NonStreamMessageAccumulator {
    message: Option<serde_json::Value>,
    partial_json: HashMap<usize, String>,
}

impl NonStreamMessageAccumulator {
    fn apply(&mut self, data: &str) -> Result<bool, String> {
        let event: serde_json::Value = serde_json::from_str(data)
            .map_err(|err| format!("invalid upstream SSE JSON: {err}"))?;
        let event_type = event
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        match event_type {
            "message_start" => {
                self.message = event.get("message").cloned();
                if self.message.is_none() {
                    return Err("upstream message_start omitted message".to_string());
                }
            }
            "content_block_start" => {
                let index = event
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| "content_block_start omitted index".to_string())?
                    as usize;
                let block = event
                    .get("content_block")
                    .cloned()
                    .ok_or_else(|| "content_block_start omitted content_block".to_string())?;
                self.set_content_block(index, block)?;
            }
            "content_block_delta" => {
                let index = event
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| "content_block_delta omitted index".to_string())?
                    as usize;
                let delta = event
                    .get("delta")
                    .and_then(serde_json::Value::as_object)
                    .ok_or_else(|| "content_block_delta omitted delta".to_string())?;
                let delta_type = delta
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                match delta_type {
                    "text_delta" => self.append_block_string(index, "text", delta, "text")?,
                    "thinking_delta" => {
                        self.append_block_string(index, "thinking", delta, "thinking")?
                    }
                    "signature_delta" => {
                        self.append_block_string(index, "signature", delta, "signature")?
                    }
                    "input_json_delta" => {
                        if let Some(fragment) = delta
                            .get("partial_json")
                            .and_then(serde_json::Value::as_str)
                        {
                            self.partial_json
                                .entry(index)
                                .or_default()
                                .push_str(fragment);
                        }
                    }
                    "citations_delta" => {
                        if let Some(citation) = delta.get("citation") {
                            let block = self
                                .content_mut()?
                                .get_mut(index)
                                .and_then(serde_json::Value::as_object_mut)
                                .ok_or_else(|| format!("content block {index} was missing"))?;
                            let citations = block
                                .entry("citations")
                                .or_insert_with(|| serde_json::json!([]))
                                .as_array_mut()
                                .ok_or_else(|| {
                                    format!("content block {index}.citations was not an array")
                                })?;
                            citations.push(citation.clone());
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(index) = event.get("index").and_then(serde_json::Value::as_u64) {
                    self.finish_partial_json(index as usize)?;
                }
            }
            "message_delta" => {
                let message = self
                    .message
                    .as_mut()
                    .and_then(serde_json::Value::as_object_mut)
                    .ok_or_else(|| "message_delta arrived before message_start".to_string())?;
                if let Some(delta) = event.get("delta").and_then(serde_json::Value::as_object) {
                    for key in ["stop_reason", "stop_sequence"] {
                        if let Some(value) = delta.get(key) {
                            message.insert(key.to_string(), value.clone());
                        }
                    }
                }
                if let Some(usage) = event.get("usage").and_then(serde_json::Value::as_object) {
                    let target = message
                        .entry("usage")
                        .or_insert_with(|| serde_json::json!({}));
                    let target = target
                        .as_object_mut()
                        .ok_or_else(|| "message usage was not an object".to_string())?;
                    for (key, value) in usage {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
            "message_stop" => {
                let remaining = self.partial_json.keys().copied().collect::<Vec<_>>();
                for index in remaining {
                    self.finish_partial_json(index)?;
                }
                return Ok(true);
            }
            "error" => {
                let error = event.get("error").unwrap_or(&event);
                let message = error
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("upstream stream returned an error");
                return Err(message.to_string());
            }
            _ => {}
        }
        Ok(false)
    }

    fn set_content_block(&mut self, index: usize, block: serde_json::Value) -> Result<(), String> {
        let content = self.content_mut()?;
        if index > content.len() {
            return Err(format!(
                "upstream content block index {index} skipped index {}",
                content.len()
            ));
        }
        if index == content.len() {
            content.push(block);
        } else {
            content[index] = block;
        }
        Ok(())
    }

    fn content_mut(&mut self) -> Result<&mut Vec<serde_json::Value>, String> {
        self.message
            .as_mut()
            .and_then(|message| message.get_mut("content"))
            .and_then(serde_json::Value::as_array_mut)
            .ok_or_else(|| "upstream message_start omitted content array".to_string())
    }

    fn append_block_string(
        &mut self,
        index: usize,
        target_key: &str,
        delta: &serde_json::Map<String, serde_json::Value>,
        delta_key: &str,
    ) -> Result<(), String> {
        let Some(fragment) = delta.get(delta_key).and_then(serde_json::Value::as_str) else {
            return Ok(());
        };
        let block = self
            .content_mut()?
            .get_mut(index)
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| format!("content block {index} was missing"))?;
        let target = block
            .entry(target_key)
            .or_insert_with(|| serde_json::Value::String(String::new()));
        let target = target
            .as_str()
            .ok_or_else(|| format!("content block {index}.{target_key} was not a string"))?;
        let mut merged = String::with_capacity(target.len() + fragment.len());
        merged.push_str(target);
        merged.push_str(fragment);
        block.insert(target_key.to_string(), serde_json::Value::String(merged));
        Ok(())
    }

    fn finish_partial_json(&mut self, index: usize) -> Result<(), String> {
        let Some(raw) = self.partial_json.remove(&index) else {
            return Ok(());
        };
        let input = serde_json::from_str(&raw)
            .map_err(|err| format!("invalid tool input JSON in block {index}: {err}"))?;
        let block = self
            .content_mut()?
            .get_mut(index)
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| format!("tool content block {index} was missing"))?;
        block.insert("input".to_string(), input);
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>, String> {
        let message = self
            .message
            .ok_or_else(|| "upstream stream ended before message_start".to_string())?;
        serde_json::to_vec(&message).map_err(|err| format!("failed to serialize response: {err}"))
    }
}

struct BridgedNonStreamDropGuard {
    slot: Option<SelectedSlotHandle>,
    completed: Arc<AtomicBool>,
    upstream_failed: Arc<AtomicBool>,
    error_message: Arc<Mutex<Option<String>>>,
    billing_ctx: Option<crate::billing::BillingContext>,
    cookie: Option<crate::config::AccountSlot>,
    family: ModelFamily,
    input_tokens: Arc<AtomicU64>,
    output_tokens: Arc<AtomicU64>,
    cache_creation_tokens: Arc<AtomicU64>,
    cache_read_tokens: Arc<AtomicU64>,
    ttft_ms: Arc<AtomicI64>,
    saw_upstream_usage: Arc<AtomicBool>,
}

impl Drop for BridgedNonStreamDropGuard {
    fn drop(&mut self) {
        if self.completed.load(Ordering::Relaxed) {
            return;
        }
        let slot = self.slot.clone();
        let upstream_failed = self.upstream_failed.load(Ordering::Relaxed);
        let error_message = self
            .error_message
            .lock()
            .ok()
            .and_then(|message| message.clone())
            .unwrap_or_else(|| {
                if upstream_failed {
                    "upstream stream ended before message_stop".to_string()
                } else {
                    "non-stream keepalive response dropped before completion".to_string()
                }
            });
        let billing_ctx = self.billing_ctx.clone();
        let cookie = self.cookie.clone();
        let family = self.family;
        let input = self.input_tokens.load(Ordering::Relaxed);
        let output = self.output_tokens.load(Ordering::Relaxed);
        let cache_creation = self.cache_creation_tokens.load(Ordering::Relaxed);
        let cache_read = self.cache_read_tokens.load(Ordering::Relaxed);
        let ttft = self.ttft_ms.load(Ordering::Relaxed);
        let has_usage = self.saw_upstream_usage.load(Ordering::Relaxed)
            || output > 0
            || cache_creation > 0
            || cache_read > 0;

        tokio::spawn(async move {
            if let (Some(mut cookie), Some(slot)) = (cookie, slot.as_ref())
                && cookie.auth_method != AuthMethod::ApiKey
                && has_usage
            {
                ClaudeCodeState::update_cookie_boundaries_if_due(&mut cookie, &slot.state.handle)
                    .await;
                cookie.add_and_bucket_usage(input, output, family);
                if let Some(account_id) = cookie.account_id {
                    let update = cookie.to_runtime_params();
                    let fingerprint = CredentialFingerprint::from_slot(&cookie);
                    let _ = slot
                        .state
                        .handle
                        .release_runtime(account_id, update, None, fingerprint)
                        .await;
                }
            }
            if let Some(ctx) = billing_ctx {
                let usage = has_usage.then_some(crate::billing::BillingUsage {
                    input_tokens: input,
                    output_tokens: output,
                    cache_creation_tokens: cache_creation,
                    cache_read_tokens: cache_read,
                    ttft_ms: (ttft >= 0).then_some(ttft),
                });
                let status = if upstream_failed {
                    "upstream_error"
                } else {
                    "client_abort"
                };
                crate::billing::persist_terminal_request_log(
                    &ctx,
                    TerminalLogOptions {
                        request_type: RequestType::Messages,
                        stream: false,
                        status,
                        http_status: Some(if upstream_failed { 502 } else { 499 }),
                        usage,
                        error_code: Some(status),
                        error_message: Some(&error_message),
                        update_rollups: has_usage,
                        response_body: None,
                    },
                )
                .await;
            }
            if let Some(slot) = slot {
                slot.release_slot_only().await;
            }
        });
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

/// One-shot verdict for a pure-OAuth slot failure, produced by
/// [`ClaudeCodeState::classify_oauth_failure`]. Owned (no borrow of the
/// `!Sync` `ClewdrError`) so the retry loops can hold it across `.await`.
struct OAuthFailureVerdict {
    /// Upstream says the organization is disabled → `disabled` transition.
    disabled: bool,
    /// Cooldown verdict with its reset time → `reset_time` transition and
    /// an `UpstreamCoolingDown` surface error.
    cooldown_until: Option<i64>,
    /// Legacy `Reason` for the pool's release / invalidate API; `None`
    /// for transient/internal classes (do not change account state).
    pool_reason: Option<Reason>,
    /// Structured context for `accounts.last_failure_json`.
    persisted: AccountFailureContextPersisted,
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
    /// verdicts. Runs `classify_account_failure` exactly once per error and
    /// derives every field the pure-OAuth failure arms need, instead of the
    /// previous three helpers that each re-classified the same error.
    ///
    /// Behavior notes preserved from those helpers:
    /// - `disabled`: only the `OrganizationDisabled` normalized reason —
    ///   `FreeTier` keeps a separate path even though it also maps to
    ///   `AccountFailureAction::TerminalDisabled`.
    /// - `cooldown_until`: equivalent to the previous `InvalidCookie +
    ///   Reason::TooManyRequest|Restricted` match — any classifier path
    ///   that produces `AccountFailureAction::Cooldown` reports its
    ///   `reset_time`.
    /// - `pool_reason`: `to_reason()` bridges the normalized reason back to
    ///   the legacy `Reason` enum used by the pool's invalidate / collect
    ///   API; transient and internal classes yield `None` so callers do
    ///   not change account state.
    ///
    /// Returns an owned struct (no borrow of the `!Sync` error) so the
    /// caller can cross `.await` points with a Send future — same rule as
    /// [`Self::classify_persisted`].
    fn classify_oauth_failure(err: &ClewdrError, source: FailureSource) -> OAuthFailureVerdict {
        let ctx = classify_account_failure(err, source, None, None);
        OAuthFailureVerdict {
            disabled: matches!(
                ctx.normalized_reason,
                AccountNormalizedReason::OrganizationDisabled
            ),
            cooldown_until: match ctx.action {
                AccountFailureAction::Cooldown { reset_time } => Some(reset_time),
                _ => None,
            },
            pool_reason: match ctx.action {
                AccountFailureAction::TerminalAuth
                | AccountFailureAction::TerminalDisabled
                | AccountFailureAction::Cooldown { .. } => ctx.normalized_reason.to_reason(),
                AccountFailureAction::TransientUpstream | AccountFailureAction::InternalError => {
                    None
                }
            },
            persisted: AccountFailureContextPersisted::from(&ctx),
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
        set_account_last_failure_logged(&db, account_id, Some(&persisted)).await;
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
        set_account_last_failure_logged(&db, account_id, Some(&persisted)).await;
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
        set_account_last_failure_logged(&db, account_id, Some(&persisted)).await;
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
        set_account_last_failure_logged(&db, account_id, Some(&persisted)).await;
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
        set_account_last_failure_logged(&db, account_id, Some(&persisted)).await;
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
        if !upsert_account_oauth(
            &db,
            account_id,
            Some(&refreshed.token),
            None,
            Some(&current.refresh_token),
        )
        .await?
        {
            if let Some(token) = crate::db::accounts::get_account_by_id(&db, account_id)
                .await?
                .and_then(|account| account.oauth_token)
            {
                self.organization_uuid = Some(token.organization.uuid.clone());
                self.oauth_token = Some(token);
                return Ok(());
            }
            return Err(ClewdrError::InvalidAuth);
        }
        if !self
            .account_pool_handle
            .update_credential_if_current(
                account_id,
                &current.refresh_token,
                Some(refreshed.token.clone()),
            )
            .await?
        {
            if let Some(token) = crate::db::accounts::get_account_by_id(&db, account_id)
                .await?
                .and_then(|account| account.oauth_token)
            {
                self.organization_uuid = Some(token.organization.uuid.clone());
                self.oauth_token = Some(token);
                return Ok(());
            }
            return Err(ClewdrError::InvalidAuth);
        }
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
        // Merge the refreshed upstream snapshot without clobbering local
        // counters or applying it to a concurrently rotated credential.
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
                    if self.stream || self.non_stream_keepalive {
                        // Streaming and bridged non-stream responses use their
                        // own body drop guards; this acquire-time guard only
                        // covers failures before response handoff.
                        slot_guard.disarm();
                    } else {
                        slot_guard.finish().await;
                    }
                    return Ok(res);
                }
                Err(e) => {
                    if is_pure_oauth_slot {
                        let verdict = Self::classify_oauth_failure(&e, FailureSource::Messages);
                        if let Some(aid) = account_id {
                            if verdict.disabled {
                                state
                                    .mark_oauth_account_disabled(aid, verdict.persisted)
                                    .await;
                            } else if let Some(reset_time) = verdict.cooldown_until {
                                state.mark_oauth_account_cooldown(aid, reset_time).await;
                            } else if Self::is_oauth_auth_failure(&e) {
                                let message = e.to_string();
                                state
                                    .mark_oauth_account_auth_error(aid, message, verdict.persisted)
                                    .await;
                            }
                        }
                        slot_guard.finish().await;
                        if verdict.pool_reason.is_some() {
                            state.release_account(verdict.pool_reason).await;
                        }
                        if verdict.cooldown_until.is_some() {
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
        mut p: CreateMessageParams,
        slot_guard: Option<SelectedSlotHandle>,
    ) -> Result<axum::response::Response, ClewdrError> {
        let model_family = Self::classify_model(&p.model);
        if !self.stream && self.non_stream_keepalive {
            info!(
                "[NON_STREAM_KEEPALIVE] forcing upstream SSE; interval={}ms",
                self.non_stream_keepalive_interval_ms
            );
            p.stream = Some(true);
        }
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
    ///
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

        // Opt-in third-party relay cloak: dispatch to the Claude-Cloak-aligned
        // wire profile instead of the clean passthrough below.
        if self.mimicry_mode.is_third_party() {
            let is_count_tokens = path.ends_with("count_tokens");
            return self
                .execute_third_party_request(url, body, is_count_tokens, error_context)
                .await;
        }

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

        // Per-account body injection (e.g. Pioneer's `models: [...]` pool),
        // shallow-merged over the serialized body. Only on `/v1/messages` —
        // the count_tokens endpoint rejects extra top-level inputs. Reserved
        // keys (`messages`/`system`) are skipped by `merge_extra_body`.
        let req = match self
            .api_key_extra_body
            .as_ref()
            .filter(|_| !path.ends_with("count_tokens"))
        {
            Some(extra) => {
                let mut value = serde_json::to_value(&body)?;
                crate::mimicry::merge_extra_body(&mut value, extra);
                req.header("content-type", "application/json")
                    .body(serde_json::to_vec(&value)?)
            }
            None => req.json(&body),
        };

        req.send()
            .await
            .context(WreqSnafu { msg: error_context })?
            .check_claude()
            .await
    }

    /// Third-party relay cloak send (`mimicry_mode == ThirdParty`). Applies the
    /// Claude-Cloak-aligned wire profile: full Claude Code header set + body
    /// identity/billing cloak on `/v1/messages`; headers-only on count_tokens
    /// (the real CLI's count request carries no billing/metadata/session body).
    ///
    /// Distinct from the official path on purpose (see `mimicry::third_party`):
    /// Bearer auth by default, synthesized `anthropic-beta` without
    /// `oauth-2025-04-20`, and `cch` left as the literal `00000` (no xxh64
    /// rewrite — relays whitelist/recompute it).
    async fn execute_third_party_request(
        &self,
        url: url::Url,
        body: &CreateMessageParams,
        is_count_tokens: bool,
        error_context: &'static str,
    ) -> Result<wreq::Response, ClewdrError> {
        let cfg = self.mimicry_config.clone().unwrap_or_default();
        let empty_headers = std::collections::BTreeMap::new();
        let extra_headers = self
            .api_key_extra_headers
            .as_ref()
            .map(|h| h.as_map())
            .unwrap_or(&empty_headers);
        let req = crate::mimicry::third_party::build_cloak_request(
            &self.client,
            &url,
            self.api_key.as_deref().unwrap_or(""),
            &cfg,
            extra_headers,
            self.api_key_extra_body.as_ref(),
            body,
            is_count_tokens,
        )?;
        req.send()
            .await
            .context(WreqSnafu { msg: error_context })?
            .check_claude()
            .await
    }

    /// Build the OFFICIAL (Cookie/OAuth) outbound body: inject the deterministic
    /// `metadata.user_id` bound to the selected account, serialize, and overwrite
    /// the billing-header `cch=00000;` placeholder with a self-consistent xxh64
    /// checksum over the FINAL bytes (`stealth::cch_rewrite`). Returns the wire
    /// bytes plus the derived session id (also emitted as
    /// `x-claude-code-session-id`, so metadata.user_id and the header carry one
    /// value, mirroring the real CLI). Failover rotates the identity because it
    /// is bound to the selected account.
    ///
    /// This is intentionally NOT reused by the third-party cloak: third-party
    /// keeps `cch=00000` literal (no rewrite) and derives Claude-Cloak-style
    /// metadata, so it reuses only the lower-level `stealth` helpers, not this.
    fn build_official_identity_body(
        &self,
        body: &CreateMessageParams,
        profile: &stealth::StealthProfile,
    ) -> Result<(Vec<u8>, uuid::Uuid), ClewdrError> {
        let api_key_id = self
            .billing_ctx
            .as_ref()
            .and_then(|c| c.api_key_id)
            .unwrap_or(0);
        let device_id = stealth::derive_device_id(&profile.billing_salt, api_key_id);
        let account_uuid = self.account_uuid().unwrap_or_default();
        let seed = self.session_seed(body);
        let session_id = stealth::derive_session_id(
            &profile.billing_salt,
            self.account_id,
            api_key_id,
            &seed,
            chrono::Utc::now(),
        );

        let mut send_body = body.clone();
        send_body
            .metadata
            .get_or_insert_with(Default::default)
            .fields
            .insert(
                "user_id".to_string(),
                stealth::build_user_id_metadata(&device_id, &account_uuid, &session_id),
            );
        let mut bytes = serde_json::to_vec(&send_body)?;
        // The messages body always carries exactly one billing block (prepended
        // in middleware), so the placeholder must be present. A false return
        // means an unexpected body shape — surface it rather than silently
        // shipping a literal `cch=00000;`.
        if !stealth::cch_rewrite(&mut bytes) {
            warn!(
                "cch placeholder not rewritten on messages body (unexpected billing-block shape)"
            );
        }
        Ok((bytes, session_id))
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

        let (bytes, session_id) = self.build_official_identity_body(body, &profile)?;

        let mut req = self
            .client
            .post(url.to_string())
            .bearer_auth(access_token)
            .header("content-type", "application/json")
            .header(USER_AGENT, profile.user_agent())
            .header("anthropic-beta", beta_header)
            .header("anthropic-version", CLAUDE_API_VERSION)
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header("x-app", "cli")
            .header("x-claude-code-session-id", session_id.to_string())
            // Client-generated per-attempt request id the real Claude Code CLI
            // sends on every `/v1/messages` call (documented as the
            // `client_request_id` telemetry attribute — a fresh value per retry
            // attempt). Generated here so each send/retry of this function gets
            // its own id, matching the CLI. Scoped to the OAuth/Cookie model
            // path: ApiKey requests can target a third-party relay (handled in
            // `execute_api_key_request`, which omits it) and count_tokens is not
            // a model attempt.
            .header("x-client-request-id", uuid::Uuid::new_v4().to_string());
        for (name, value) in STAINLESS_HEADERS {
            req = req.header(*name, *value);
        }
        req.body(bytes)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to send chat message",
            })?
            .check_claude()
            .await
    }

    /// The selected account's real organization UUID, used for
    /// `metadata.user_id.account_uuid`. Prefers the refreshed
    /// `organization_uuid`, falling back to the credential's token org so a
    /// freshly-loaded (non-refreshed) valid slot still carries it.
    fn account_uuid(&self) -> Option<String> {
        self.organization_uuid.clone().or_else(|| {
            self.cookie
                .as_ref()
                .and_then(|c| c.token.as_ref())
                .map(|t| t.organization.uuid.clone())
        })
    }

    /// Pick the conversation seed for `session_id` derivation, in priority
    /// order: ① the inbound `X-Claude-Code-Session-Id` header (the most direct
    /// session signal, threaded from middleware), ② an inbound
    /// `metadata.user_id.session_id` (real CLI client), ③ a hash of `system` +
    /// first user message (2api multi-turn stays stable while the first turn is
    /// unchanged), ④ key+time fallback.
    fn session_seed(&self, body: &CreateMessageParams) -> stealth::SessionSeed {
        // ① inbound session header (matches the affinity key used to pick the
        // account, so the outbound session stays stable across same-session
        // turns even when their prompts differ).
        if let Some(sid) = self
            .inbound_session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return stealth::SessionSeed::InboundSession(sid.to_string());
        }
        // ② inbound metadata.user_id.session_id, if the caller speaks Claude Code.
        if let Some(uid) = body.metadata.as_ref().and_then(|m| m.fields.get("user_id"))
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(uid)
            && let Some(sid) = v
                .get("session_id")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
        {
            return stealth::SessionSeed::InboundSession(sid.to_string());
        }
        // ③ content hash of system + first user message text.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        if let Some(system) = body.system.as_ref() {
            system.to_string().hash(&mut hasher);
        }
        let first_user = body.messages.iter().find(|m| m.role == Role::User);
        if let Some(msg) = first_user {
            serde_json::to_string(&msg.content)
                .unwrap_or_default()
                .hash(&mut hasher);
            return stealth::SessionSeed::ContentHash(hasher.finish());
        }
        // ④ no usable content → key + time window.
        stealth::SessionSeed::KeyTimeWindow
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
                        let verdict = Self::classify_oauth_failure(&e, FailureSource::CountTokens);
                        if let Some(aid) = account_id {
                            if verdict.disabled {
                                state
                                    .mark_oauth_account_disabled(aid, verdict.persisted)
                                    .await;
                            } else if let Some(reset_time) = verdict.cooldown_until {
                                state.mark_oauth_account_cooldown(aid, reset_time).await;
                            } else if Self::is_oauth_auth_failure(&e) {
                                let message = e.to_string();
                                state
                                    .mark_oauth_account_auth_error(aid, message, verdict.persisted)
                                    .await;
                            }
                        }
                        slot_guard.finish().await;
                        if verdict.pool_reason.is_some() {
                            state.release_account(verdict.pool_reason).await;
                        }
                        if verdict.cooldown_until.is_some() {
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
        let upstream_is_sse = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"));

        if !self.stream && self.non_stream_keepalive && upstream_is_sse {
            self.forward_stream_as_non_stream(response, model_family, slot_guard)
                .await
        } else if !self.stream {
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
            // should not keep an account unavailable for dispatch. Spawned —
            // mirroring the streaming path's message_stop persistence — so the
            // client response also stops waiting on the SQLite rollup
            // transaction (single-writer; the dominant tail latency under
            // concurrent load).
            if let Some(ctx) = self.billing_ctx.clone() {
                tokio::spawn(async move {
                    crate::billing::persist_billing_to_db(&ctx, bu, false).await;
                });
            }
            Ok(resp)
        } else {
            return self.forward_stream_with_usage(response, model_family).await;
        }
    }

    /// Convert an upstream Messages SSE response into one final Messages JSON
    /// document while emitting JSON whitespace often enough to reset a
    /// downstream per-read idle timeout.
    async fn forward_stream_as_non_stream(
        &mut self,
        response: wreq::Response,
        family: ModelFamily,
        slot: Option<SelectedSlotHandle>,
    ) -> Result<axum::response::Response, ClewdrError> {
        let status = response.status();
        let mut headers = response.headers().clone();
        headers.remove(CONTENT_LENGTH);
        headers.remove(TRANSFER_ENCODING);
        headers.insert(
            CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        // tower-http never recompresses a response that already carries this
        // header. Compressing one-byte whitespace frames would buffer them and
        // defeat the keepalive.
        headers.insert(CONTENT_ENCODING, http::HeaderValue::from_static("identity"));
        headers.insert(
            CACHE_CONTROL,
            http::HeaderValue::from_static("no-cache, no-transform"),
        );
        headers.insert(
            http::HeaderName::from_static("x-accel-buffering"),
            http::HeaderValue::from_static("no"),
        );

        let input_tokens = Arc::new(AtomicU64::new(self.usage.input_tokens as u64));
        let output_tokens = Arc::new(AtomicU64::new(0));
        let cache_creation_tokens = Arc::new(AtomicU64::new(0));
        let cache_read_tokens = Arc::new(AtomicU64::new(0));
        let ttft_ms = Arc::new(AtomicI64::new(-1));
        let saw_upstream_usage = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let upstream_failed = Arc::new(AtomicBool::new(false));
        let error_message = Arc::new(Mutex::new(None::<String>));
        let billing_ctx = self.billing_ctx.clone();
        let cookie = self.cookie.clone();
        let ttft_started_at = billing_ctx.as_ref().map(|ctx| ctx.started_at);
        let keepalive_interval =
            Duration::from_millis(self.non_stream_keepalive_interval_ms.max(1));

        let guard = BridgedNonStreamDropGuard {
            slot: slot.clone(),
            completed: completed.clone(),
            upstream_failed: upstream_failed.clone(),
            error_message: error_message.clone(),
            billing_ctx: billing_ctx.clone(),
            cookie: cookie.clone(),
            family,
            input_tokens: input_tokens.clone(),
            output_tokens: output_tokens.clone(),
            cache_creation_tokens: cache_creation_tokens.clone(),
            cache_read_tokens: cache_read_tokens.clone(),
            ttft_ms: ttft_ms.clone(),
            saw_upstream_usage: saw_upstream_usage.clone(),
        };

        let mut upstream = response.bytes_stream().eventsource();
        let pool_handle = self.account_pool_handle.clone();
        let body_stream = async_stream::stream! {
            let _guard = guard;
            let mut accumulator = NonStreamMessageAccumulator::default();
            let mut heartbeat = tokio::time::interval(keepalive_interval);
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Consume interval's immediate first tick. The explicit initial
            // whitespace below commits headers/body without waiting six seconds.
            heartbeat.tick().await;
            yield Ok::<Bytes, Infallible>(Bytes::from_static(b" "));

            loop {
                tokio::select! {
                    _ = heartbeat.tick() => {
                        yield Ok(Bytes::from_static(b" "));
                    }
                    next = upstream.next() => {
                        let Some(next) = next else {
                            upstream_failed.store(true, Ordering::Relaxed);
                            if let Ok(mut target) = error_message.lock() {
                                *target = Some("upstream stream ended before message_stop".to_string());
                            }
                            let error = serde_json::json!({
                                "type": "error",
                                "error": {
                                    "type": "api_error",
                                    "message": "upstream stream ended before message_stop"
                                }
                            });
                            yield Ok(Bytes::from(serde_json::to_vec(&error).unwrap_or_default()));
                            break;
                        };
                        let event = match next {
                            Ok(event) => event,
                            Err(err) => {
                                let message = format!("failed to read upstream SSE: {err}");
                                upstream_failed.store(true, Ordering::Relaxed);
                                if let Ok(mut target) = error_message.lock() {
                                    *target = Some(message.clone());
                                }
                                let error = serde_json::json!({
                                    "type": "error",
                                    "error": { "type": "api_error", "message": message }
                                });
                                yield Ok(Bytes::from(serde_json::to_vec(&error).unwrap_or_default()));
                                break;
                            }
                        };

                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&event.data) {
                            Self::update_bridge_usage(
                                &value,
                                &input_tokens,
                                &output_tokens,
                                &cache_creation_tokens,
                                &cache_read_tokens,
                                &saw_upstream_usage,
                            );
                            if value.get("type").and_then(serde_json::Value::as_str)
                                == Some("content_block_delta")
                                && let Some(started) = ttft_started_at
                            {
                                let elapsed = (chrono::Utc::now() - started).num_milliseconds();
                                if elapsed >= 0 {
                                    let _ = ttft_ms.compare_exchange(
                                        -1,
                                        elapsed,
                                        Ordering::Relaxed,
                                        Ordering::Relaxed,
                                    );
                                }
                            }
                        }

                        match accumulator.apply(&event.data) {
                            Ok(false) => {}
                            Ok(true) => {
                                let bytes = match accumulator.finish() {
                                    Ok(bytes) => bytes,
                                    Err(message) => {
                                        upstream_failed.store(true, Ordering::Relaxed);
                                        if let Ok(mut target) = error_message.lock() {
                                            *target = Some(message.clone());
                                        }
                                        let error = serde_json::json!({
                                            "type": "error",
                                            "error": { "type": "api_error", "message": message }
                                        });
                                        yield Ok(Bytes::from(serde_json::to_vec(&error).unwrap_or_default()));
                                        break;
                                    }
                                };
                                let measured_ttft = {
                                    let value = ttft_ms.load(Ordering::Relaxed);
                                    (value >= 0).then_some(value)
                                };
                                let mut usage = Self::extract_usage_from_bytes(&bytes).unwrap_or(
                                    crate::billing::BillingUsage {
                                        input_tokens: input_tokens.load(Ordering::Relaxed),
                                        output_tokens: output_tokens.load(Ordering::Relaxed),
                                        cache_creation_tokens: cache_creation_tokens.load(Ordering::Relaxed),
                                        cache_read_tokens: cache_read_tokens.load(Ordering::Relaxed),
                                        ttft_ms: measured_ttft,
                                    },
                                );
                                usage.ttft_ms = usage.ttft_ms.or(measured_ttft);

                                let cookie_for_finish = cookie.clone();
                                let pool_for_finish = pool_handle.clone();
                                let slot_for_finish = slot.clone();
                                let usage_for_runtime = usage.clone();
                                tokio::spawn(async move {
                                    if let Some(mut cookie) = cookie_for_finish
                                        && cookie.auth_method != AuthMethod::ApiKey
                                    {
                                        Self::update_cookie_boundaries_if_due(
                                            &mut cookie,
                                            &pool_for_finish,
                                        )
                                        .await;
                                        cookie.add_and_bucket_usage(
                                            usage_for_runtime.input_tokens,
                                            usage_for_runtime.output_tokens,
                                            family,
                                        );
                                        if let Some(account_id) = cookie.account_id {
                                            let update = cookie.to_runtime_params();
                                            let fingerprint = CredentialFingerprint::from_slot(&cookie);
                                            let _ = pool_for_finish
                                                .release_runtime(
                                                    account_id,
                                                    update,
                                                    None,
                                                    fingerprint,
                                                )
                                                .await;
                                        }
                                    }
                                    if let Some(slot) = slot_for_finish {
                                        slot.release_slot_only().await;
                                    }
                                });
                                if let Some(ctx) = billing_ctx.clone() {
                                    tokio::spawn(async move {
                                        crate::billing::persist_billing_to_db(&ctx, usage, false).await;
                                    });
                                }
                                completed.store(true, Ordering::Relaxed);
                                yield Ok(Bytes::from(bytes));
                                break;
                            }
                            Err(message) => {
                                upstream_failed.store(true, Ordering::Relaxed);
                                if let Ok(mut target) = error_message.lock() {
                                    *target = Some(message.clone());
                                }
                                let error = serde_json::json!({
                                    "type": "error",
                                    "error": { "type": "api_error", "message": message }
                                });
                                yield Ok(Bytes::from(serde_json::to_vec(&error).unwrap_or_default()));
                                break;
                            }
                        }
                    }
                }
            }
        };

        let mut builder = http::Response::builder().status(status);
        *builder.headers_mut().expect("response builder headers") = headers;
        builder
            .body(Body::from_stream(body_stream))
            .map_err(|source| ClewdrError::HttpError {
                loc: snafu::Location::generate(),
                source,
            })
    }

    fn update_bridge_usage(
        event: &serde_json::Value,
        input: &AtomicU64,
        output: &AtomicU64,
        cache_creation: &AtomicU64,
        cache_read: &AtomicU64,
        saw_upstream_usage: &AtomicBool,
    ) {
        let usage = match event.get("type").and_then(serde_json::Value::as_str) {
            Some("message_start") => event
                .get("message")
                .and_then(|message| message.get("usage")),
            Some("message_delta") => event.get("usage"),
            _ => None,
        };
        let Some(usage) = usage else { return };
        saw_upstream_usage.store(true, Ordering::Relaxed);
        if let Some(value) = usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
        {
            input.store(value, Ordering::Relaxed);
        }
        if let Some(value) = usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
        {
            output.store(value, Ordering::Relaxed);
        }
        if let Some(value) = usage
            .get("cache_creation_input_tokens")
            .and_then(serde_json::Value::as_u64)
        {
            cache_creation.store(value, Ordering::Relaxed);
        }
        if let Some(value) = usage
            .get("cache_read_input_tokens")
            .and_then(serde_json::Value::as_u64)
        {
            cache_read.store(value, Ordering::Relaxed);
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
        // Parse the body once. The typed fallback below reuses the parsed
        // tree via `from_value` instead of re-lexing the whole body; a body
        // that fails the `Value` parse could never satisfy the typed parse
        // either, so bailing out early is behavior-preserving.
        let value = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
        if let Some(usage) = value.get("usage") {
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
        let parsed =
            serde_json::from_value::<crate::types::claude::CreateMessageResponse>(value).ok()?;
        Some(crate::billing::BillingUsage {
            input_tokens: 0,
            output_tokens: parsed.count_tokens() as u64,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            ttft_ms: None,
        })
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
        // count_tokens carries the fixed fingerprint HTTP headers but NO body
        // cloak: the real CLI's count request is just {model, messages, tools,
        // betas?, thinking?} — no billing block (skipped in middleware), no
        // metadata (Anthropic rejects it: "Extra inputs are not permitted"), no
        // session id. Body is sent as-is.
        let mut req = self
            .client
            .post(url.to_string())
            .bearer_auth(access_token)
            .header(USER_AGENT, profile.user_agent())
            .header("anthropic-beta", beta_header)
            .header("anthropic-version", CLAUDE_API_VERSION)
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header("x-app", "cli");
        for (name, value) in STAINLESS_HEADERS {
            req = req.header(*name, *value);
        }
        req.json(body)
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
        let (mut opus_ts, mut opus_util) = parse_window("seven_day_opus");
        let (mut sonnet_ts, mut sonnet_util) = parse_window("seven_day_sonnet");

        // Anthropic replaced the fixed seven_day_opus/seven_day_sonnet fields
        // (now always null) with a generic usage.limits[] array of
        // kind == "weekly_scoped" entries, scoped to an arbitrary model name.
        // Parse the full list, and backfill the legacy fields above when a
        // scoped entry's name matches "opus"/"sonnet" for backward compat.
        let scoped_limits = crate::config::parse_weekly_scoped_limits(&usage);
        if let Some((r, u)) = crate::config::scoped_legacy_backfill(&scoped_limits, "opus") {
            opus_ts = opus_ts.or(r);
            opus_util = opus_util.or(u);
        }
        if let Some((r, u)) = crate::config::scoped_legacy_backfill(&scoped_limits, "sonnet") {
            sonnet_ts = sonnet_ts.or(r);
            sonnet_util = sonnet_util.or(u);
        }
        cookie.weekly_scoped_limits = scoped_limits;

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
    use super::{ClaudeCodeState, NonStreamMessageAccumulator};
    use crate::{config::Reason, error::ClewdrError, services::account_error::FailureSource};

    fn classify(err: &ClewdrError) -> super::OAuthFailureVerdict {
        ClaudeCodeState::classify_oauth_failure(err, FailureSource::Messages)
    }

    #[test]
    fn non_stream_accumulator_rebuilds_text_thinking_tool_and_usage() {
        let events = [
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"claude-test","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"plan"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hello"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"tool_1","name":"calc","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"1}"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":9}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let mut accumulator = NonStreamMessageAccumulator::default();
        for (index, event) in events.iter().enumerate() {
            assert_eq!(accumulator.apply(event).unwrap(), index + 1 == events.len());
        }
        let response: serde_json::Value =
            serde_json::from_slice(&accumulator.finish().unwrap()).unwrap();
        assert_eq!(response["content"][0]["thinking"], "plan");
        assert_eq!(response["content"][0]["signature"], "sig");
        assert_eq!(response["content"][1]["text"], "hello");
        assert_eq!(response["content"][2]["input"], serde_json::json!({"x": 1}));
        assert_eq!(response["stop_reason"], "tool_use");
        assert_eq!(response["usage"]["input_tokens"], 12);
        assert_eq!(response["usage"]["output_tokens"], 9);
    }

    #[test]
    fn oauth_cooldown_detects_temporary_invalid_cookie_reasons() {
        assert_eq!(
            classify(&ClewdrError::InvalidCookie {
                reason: Reason::TooManyRequest(123),
            })
            .cooldown_until,
            Some(123)
        );
        assert_eq!(
            classify(&ClewdrError::InvalidCookie {
                reason: Reason::Restricted(456),
            })
            .cooldown_until,
            Some(456)
        );
        assert_eq!(
            classify(&ClewdrError::InvalidCookie {
                reason: Reason::Null,
            })
            .cooldown_until,
            None
        );
    }

    #[test]
    fn oauth_pool_reason_maps_auth_and_cooldown_errors_for_pool_eviction() {
        assert_eq!(
            classify(&ClewdrError::InvalidCookie {
                reason: Reason::Restricted(456),
            })
            .pool_reason,
            Some(Reason::Restricted(456))
        );
        assert_eq!(
            classify(&ClewdrError::InvalidCookie {
                reason: Reason::Disabled,
            })
            .pool_reason,
            Some(Reason::Disabled)
        );
        assert_eq!(
            classify(&ClewdrError::Whatever {
                message: "invalid_grant".to_string(),
                source: None,
            })
            .pool_reason,
            Some(Reason::Null)
        );
    }

    /// Step 3.5 C2 regression guards. The classifier rewrite must preserve
    /// these subtle edges:
    /// 1. `Reason::Free` is `TerminalDisabled` at the action level but is
    ///    NOT a `disabled` verdict — it goes through a separate Free-tier
    ///    path that must not converge with the org-disabled branch.
    /// 2. Bare 401/403 (no phrase) is auth-rejected and must produce
    ///    `Reason::Null` for pool eviction.
    /// 3. Transient / internal errors must not produce a pool reason.
    #[test]
    fn oauth_disabled_failure_distinguishes_free_from_org_disabled() {
        assert!(
            classify(&ClewdrError::InvalidCookie {
                reason: Reason::Disabled,
            })
            .disabled
        );
        assert!(
            !classify(&ClewdrError::InvalidCookie {
                reason: Reason::Free,
            })
            .disabled
        );
        assert!(
            !classify(&ClewdrError::InvalidCookie {
                reason: Reason::Banned,
            })
            .disabled
        );
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
        assert_eq!(classify(&http(401)).pool_reason, Some(Reason::Null));
        assert_eq!(classify(&http(403)).pool_reason, Some(Reason::Null));
        // 5xx is transient — must NOT evict.
        assert_eq!(classify(&http(500)).pool_reason, None);
        // Local logic errors must not evict either.
        assert_eq!(
            classify(&ClewdrError::Whatever {
                message: "unrelated local failure".to_string(),
                source: None,
            })
            .pool_reason,
            None
        );
    }

    /// The count_tokens retry loop classifies with
    /// `FailureSource::CountTokens` where the pre-consolidation helpers
    /// hardcoded `Messages`. Pin that the scheduler-facing fields are
    /// source-independent so the consolidation is behavior-preserving.
    #[test]
    fn oauth_verdict_scheduler_fields_are_source_independent() {
        for err in [
            ClewdrError::InvalidCookie {
                reason: Reason::TooManyRequest(123),
            },
            ClewdrError::InvalidCookie {
                reason: Reason::Disabled,
            },
            ClewdrError::InvalidCookie {
                reason: Reason::Null,
            },
            ClewdrError::Whatever {
                message: "invalid_grant".to_string(),
                source: None,
            },
        ] {
            let messages = ClaudeCodeState::classify_oauth_failure(&err, FailureSource::Messages);
            let count = ClaudeCodeState::classify_oauth_failure(&err, FailureSource::CountTokens);
            assert_eq!(messages.disabled, count.disabled);
            assert_eq!(messages.cooldown_until, count.cooldown_until);
            assert_eq!(messages.pool_reason, count.pool_reason);
        }
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
            "accept",
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
