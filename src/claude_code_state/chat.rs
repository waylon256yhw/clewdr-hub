use axum::{
    Json,
    response::{IntoResponse, Sse, sse::Event as SseEvent},
};
use colored::Colorize;
use eventsource_stream::Eventsource;
use futures::{StreamExt, TryStreamExt};
use http::header::{ACCEPT, USER_AGENT};
use snafu::{GenerateImplicitData, ResultExt};
use tracing::{Instrument, error, info, warn};
use wreq::Method;

use crate::{
    billing::{RequestType, TerminalLogOptions},
    claude_code_state::{ClaudeCodeState, TokenStatus},
    config::{ModelFamily, Reason},
    db::accounts::{
        batch_upsert_runtime_states, set_account_auth_error, set_account_disabled,
        update_account_metadata_unchecked, upsert_account_oauth,
    },
    error::{CheckClaudeErr, ClewdrError, WreqSnafu},
    oauth::refresh_oauth_token,
    services::cookie_actor::CookieActorHandle,
    types::claude::{CountMessageTokensResponse, CreateMessageParams},
};

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const MAX_RETRIES: usize = 5;
const CLAUDE_BETA_BASE: &str = "oauth-2025-04-20";
const CLAUDE_BETA_CONTEXT_1M_TOKEN: &str = "context-1m-2025-08-07";
const CLAUDE_API_VERSION: &str = "2023-06-01";

impl ClaudeCodeState {
    fn is_oauth_auth_failure(err: &ClewdrError) -> bool {
        match err {
            ClewdrError::ClaudeHttpError { code, .. } => {
                matches!(code.as_u16(), 401 | 403)
            }
            ClewdrError::Whatever { message, .. } => {
                let msg = message.to_ascii_lowercase();
                msg.contains("invalid_grant")
                    || msg.contains("refresh token not found")
                    || msg.contains("refresh token")
                        && (msg.contains("invalid") || msg.contains("expired"))
                    || msg.contains("status 401")
                    || msg.contains("status 403")
                    || msg.contains("access token")
                        && (msg.contains("expired") || msg.contains("invalid"))
            }
            _ => false,
        }
    }

    fn is_oauth_disabled_failure(err: &ClewdrError) -> bool {
        match err {
            ClewdrError::InvalidCookie { reason } => matches!(reason, Reason::Disabled),
            ClewdrError::ClaudeHttpError { code, inner } => {
                if code.as_u16() != 400 {
                    return false;
                }
                let msg = inner
                    .message
                    .as_str()
                    .map(|s| s.to_ascii_lowercase())
                    .unwrap_or_else(|| inner.message.to_string().to_ascii_lowercase());
                msg.contains("organization has been disabled")
            }
            _ => false,
        }
    }

    async fn mark_oauth_account_auth_error(&mut self, account_id: i64, message: String) {
        let Some(db) = self.billing_ctx.as_ref().map(|ctx| ctx.db.clone()) else {
            return;
        };
        if let Err(db_err) = set_account_auth_error(&db, account_id, &message).await {
            warn!("Failed to set OAuth auth_error for account {account_id}: {db_err}");
            return;
        }
    }

    async fn mark_oauth_account_disabled(&mut self, account_id: i64) {
        let Some(db) = self.billing_ctx.as_ref().map(|ctx| ctx.db.clone()) else {
            return;
        };
        if let Err(db_err) = set_account_disabled(&db, account_id, "disabled").await {
            warn!("Failed to set OAuth account {account_id} disabled: {db_err}");
        }
    }

    async fn release_selected_slot(&self, account_id: Option<i64>) {
        if let Some(aid) = account_id {
            if self.oauth_token.is_some() && self.cookie.is_none() {
                if let Some(pool) = &self.oauth_pool {
                    pool.release(aid).await;
                }
            } else {
                self.cookie_actor_handle.release_slot(aid).await;
            }
        }
    }

    async fn persist_oauth_refresh(&mut self, account_id: i64) -> Result<(), ClewdrError> {
        let Some(token) = self.oauth_token.as_ref() else {
            return Ok(());
        };
        let refreshed = refresh_oauth_token(token).await?;
        let db = self.billing_ctx.as_ref().map(|ctx| ctx.db.clone()).ok_or(
            ClewdrError::UnexpectedNone {
                msg: "Missing billing context database",
            },
        )?;
        upsert_account_oauth(&db, account_id, Some(&refreshed.token), None).await?;
        update_account_metadata_unchecked(
            &db,
            account_id,
            refreshed.snapshot.email.as_deref(),
            refreshed.snapshot.account_type.as_deref(),
            Some(refreshed.snapshot.organization_uuid.as_str()),
        )
        .await?;
        batch_upsert_runtime_states(&db, &[(account_id, refreshed.snapshot.runtime.clone())])
            .await?;
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
        if self.oauth_token.is_some() && self.cookie.is_none() && self.account_id.is_some() {
            let account_id = self.account_id;
            let access_token = match self.check_token() {
                TokenStatus::Valid => self.oauth_token.as_ref().map(|t| t.access_token.clone()),
                TokenStatus::Expired => {
                    if let Err(err) = self
                        .persist_oauth_refresh(account_id.expect("checked above"))
                        .await
                    {
                        if Self::is_oauth_disabled_failure(&err) {
                            self.mark_oauth_account_disabled(account_id.expect("checked above"))
                                .await;
                        } else if Self::is_oauth_auth_failure(&err) {
                            self.mark_oauth_account_auth_error(
                                account_id.expect("checked above"),
                                err.to_string(),
                            )
                            .await;
                        }
                        self.release_selected_slot(account_id).await;
                        return Err(err);
                    }
                    self.oauth_token.as_ref().map(|t| t.access_token.clone())
                }
                TokenStatus::None => None,
            }
            .ok_or(ClewdrError::UnexpectedNone {
                msg: "No OAuth access token available",
            })?;

            match self.send_chat(access_token, p).await {
                Ok(response) => {
                    if !self.stream {
                        self.release_selected_slot(account_id).await;
                    }
                    return Ok(response);
                }
                Err(err) => {
                    if Self::is_oauth_disabled_failure(&err) {
                        self.mark_oauth_account_disabled(account_id.expect("checked above"))
                            .await;
                    } else if Self::is_oauth_auth_failure(&err) {
                        self.mark_oauth_account_auth_error(
                            account_id.expect("checked above"),
                            err.to_string(),
                        )
                        .await;
                    }
                    self.release_selected_slot(account_id).await;
                    return Err(err);
                }
            }
        }

        for i in 0..MAX_RETRIES + 1 {
            if i > 0 {
                info!("[RETRY] attempt: {}", i.to_string().green());
            }
            let mut state = self.to_owned();
            let p = p.to_owned();

            let cookie = state.request_cookie().await?;
            let account_id = cookie.account_id;
            // Propagate account_id to billing context
            if let Some(ref mut ctx) = state.billing_ctx {
                ctx.account_id = cookie.account_id;
            }
            let retry = async {
                match state.check_token() {
                    TokenStatus::None => {
                        info!("No token found, requesting new token");
                        let org = state.get_organization().await?;
                        let code_res = state.exchange_code(&org).await?;
                        state.exchange_token(code_res).await?;
                        state.return_cookie(None).await;
                    }
                    TokenStatus::Expired => {
                        info!("Token expired, refreshing token");
                        state.refresh_token().await?;
                        state.return_cookie(None).await;
                    }
                    TokenStatus::Valid => {
                        info!("Token is valid, proceeding with request");
                    }
                }
                let Some(access_token) = state.cookie.as_ref().and_then(|c| c.token.to_owned())
                else {
                    return Err(ClewdrError::UnexpectedNone {
                        msg: "No access token found in cookie",
                    });
                };
                state
                    .send_chat(access_token.access_token.to_owned(), p)
                    .await
            }
            .instrument(tracing::info_span!(
                "claude_code",
                "cookie" = cookie.cookie.ellipse()
            ));
            match retry.await {
                Ok(res) => {
                    // For streaming, the slot is released in MessageStop handler
                    if !self.stream {
                        state.release_selected_slot(account_id).await;
                    }
                    return Ok(res);
                }
                Err(e) => {
                    state.release_selected_slot(account_id).await;
                    error!(
                        "[{}] {}",
                        state.cookie.as_ref().unwrap().cookie.ellipse().green(),
                        e
                    );
                    // 429 error
                    if let ClewdrError::InvalidCookie { reason } = e {
                        state.return_cookie(Some(reason.to_owned())).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(ClewdrError::TooManyRetries)
    }

    pub async fn send_chat(
        &mut self,
        access_token: String,
        p: CreateMessageParams,
    ) -> Result<axum::response::Response, ClewdrError> {
        let model_family = Self::classify_model(&p.model);
        let response = self.execute_claude_request(&access_token, &p).await?;
        self.handle_success_response(response, model_family).await
    }

    async fn execute_claude_request(
        &mut self,
        access_token: &str,
        body: &CreateMessageParams,
    ) -> Result<wreq::Response, ClewdrError> {
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
        if let Some(cookie) = self.cookie.as_mut() {
            if cookie.count_tokens_allowed == Some(value) {
                return;
            }
            cookie.set_count_tokens_allowed(Some(value));
            let cloned = cookie.clone();
            if let Err(err) = self.cookie_actor_handle.return_cookie(cloned, None).await {
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
        if self.oauth_token.is_some() && self.cookie.is_none() && self.account_id.is_some() {
            let account_id = self.account_id;
            if let Some(ref mut ctx) = self.billing_ctx {
                ctx.account_id = account_id;
            }
            let access_token = match self.check_token() {
                TokenStatus::Valid => self.oauth_token.as_ref().map(|t| t.access_token.clone()),
                TokenStatus::Expired => {
                    if let Err(err) = self
                        .persist_oauth_refresh(account_id.expect("checked above"))
                        .await
                    {
                        if Self::is_oauth_disabled_failure(&err) {
                            self.mark_oauth_account_disabled(account_id.expect("checked above"))
                                .await;
                        } else if Self::is_oauth_auth_failure(&err) {
                            self.mark_oauth_account_auth_error(
                                account_id.expect("checked above"),
                                err.to_string(),
                            )
                            .await;
                        }
                        self.release_selected_slot(account_id).await;
                        return Err(err);
                    }
                    self.oauth_token.as_ref().map(|t| t.access_token.clone())
                }
                TokenStatus::None => None,
            }
            .ok_or(ClewdrError::UnexpectedNone {
                msg: "No OAuth access token available",
            })?;

            match self.perform_count_tokens(access_token, p).await {
                Ok((response, _)) => {
                    self.release_selected_slot(account_id).await;
                    return Ok(response);
                }
                Err(err) => {
                    if Self::is_oauth_disabled_failure(&err) {
                        self.mark_oauth_account_disabled(account_id.expect("checked above"))
                            .await;
                    } else if Self::is_oauth_auth_failure(&err) {
                        self.mark_oauth_account_auth_error(
                            account_id.expect("checked above"),
                            err.to_string(),
                        )
                        .await;
                    }
                    self.release_selected_slot(account_id).await;
                    return Err(err);
                }
            }
        }

        for i in 0..MAX_RETRIES + 1 {
            if i > 0 {
                info!("[TOKENS][RETRY] attempt: {}", i.to_string().green());
            }
            let mut state = self.to_owned();
            let p = p.to_owned();

            let cookie = state.request_cookie().await?;
            let account_id = cookie.account_id;
            if let Some(ref mut ctx) = state.billing_ctx {
                ctx.account_id = cookie.account_id;
            }
            let cookie_disallows = matches!(cookie.count_tokens_allowed, Some(false));
            if cookie_disallows {
                state.release_selected_slot(account_id).await;
                state.persist_count_tokens_allowed(false).await;
                let (response, _) = Self::local_count_tokens_response(&p);
                return Ok(response);
            }
            let retry = async {
                match state.check_token() {
                    TokenStatus::None => {
                        info!("No token found, requesting new token");
                        let org = state.get_organization().await?;
                        let code_res = state.exchange_code(&org).await?;
                        state.exchange_token(code_res).await?;
                        state.return_cookie(None).await;
                    }
                    TokenStatus::Expired => {
                        info!("Token expired, refreshing token");
                        state.refresh_token().await?;
                        state.return_cookie(None).await;
                    }
                    TokenStatus::Valid => {
                        info!("Token is valid, proceeding with count_tokens");
                    }
                }
                let Some(access_token) = state.cookie.as_ref().and_then(|c| c.token.to_owned())
                else {
                    return Err(ClewdrError::UnexpectedNone {
                        msg: "No access token found in cookie",
                    });
                };
                state
                    .perform_count_tokens(access_token.access_token.to_owned(), p)
                    .await
            }
            .instrument(tracing::info_span!(
                "claude_code_tokens",
                "cookie" = cookie.cookie.ellipse()
            ));
            match retry.await {
                Ok((res, _)) => {
                    state.release_selected_slot(account_id).await;
                    return Ok(res);
                }
                Err(e) => {
                    state.release_selected_slot(account_id).await;
                    error!(
                        "[{}][TOKENS] {}",
                        state.cookie.as_ref().unwrap().cookie.ellipse().green(),
                        e
                    );
                    if let ClewdrError::InvalidCookie { reason } = e {
                        state.return_cookie(Some(reason.to_owned())).await;
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
    ) -> Result<axum::response::Response, ClewdrError> {
        if !self.stream {
            let (resp, billing_usage) = Self::materialize_non_stream_response(response).await?;
            let bu = billing_usage.unwrap_or_else(|| crate::billing::BillingUsage {
                input_tokens: self.usage.input_tokens as u64,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                ttft_ms: None,
            });
            self.persist_usage_totals(bu.input_tokens, bu.output_tokens, model_family)
                .await;
            // Billing DB write (awaited for non-streaming)
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
        if let Some(cookie) = self.cookie.as_mut() {
            // Lazy boundary refresh if due, then reset period counters and start fresh
            Self::update_cookie_boundaries_if_due(cookie, &self.cookie_actor_handle).await;
            cookie.add_and_bucket_usage(input, output, family);
            let cloned = cookie.clone();
            if let Err(err) = self.cookie_actor_handle.return_cookie(cloned, None).await {
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
        let stream_start = std::time::Instant::now();
        let handle = self.cookie_actor_handle.clone();
        let oauth_pool = self.oauth_pool.clone();
        let cookie = self.cookie.clone();
        let billing_ctx = self.billing_ctx.clone();
        let billing_ctx_for_stream = billing_ctx.clone();
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
                            let _ = ttft.compare_exchange(
                                -1,
                                stream_start.elapsed().as_millis() as i64,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            );
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

                            // Cookie persistence + slot release
                            if let (Some(cookie), handle) = (cookie.clone(), handle.clone()) {
                                let mut c = cookie.clone();
                                let aid = stream_account_id;
                                let released = slot_released_inner.clone();
                                tokio::spawn(async move {
                                    ClaudeCodeState::update_cookie_boundaries_if_due(
                                        &mut c, &handle,
                                    )
                                    .await;
                                    c.add_and_bucket_usage(total_input, total_out, family);
                                    let _ = handle.return_cookie(c, None).await;
                                    if let Some(aid) = aid {
                                        if !released.swap(true, Ordering::Relaxed) {
                                            handle.release_slot(aid).await;
                                        }
                                    }
                                });
                            } else if let Some(aid) = stream_account_id {
                                let released = slot_released_inner.clone();
                                let oauth_pool = oauth_pool.clone();
                                tokio::spawn(async move {
                                    if !released.swap(true, Ordering::Relaxed)
                                        && let Some(pool) = oauth_pool
                                    {
                                        pool.release(aid).await;
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
            handle: CookieActorHandle,
            oauth_pool: Option<std::sync::Arc<crate::providers::claude::OAuthAccountPool>>,
            oauth_only: bool,
            cookie: Option<crate::config::CookieStatus>,
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
                        let oauth_pool = self.oauth_pool.clone();
                        let oauth_only = self.oauth_only;
                        let cookie = self.cookie.clone();
                        let family = self.family;
                        let billing_ctx = self.billing_ctx.clone();
                        tokio::spawn(async move {
                            if !completed {
                                if let Some(mut cookie) = cookie {
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
                                    let _ = h.return_cookie(cookie, None).await;
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
                            if oauth_only {
                                if let Some(pool) = oauth_pool {
                                    pool.release(aid).await;
                                }
                            } else {
                                h.release_slot(aid).await;
                            }
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
            handle: self.cookie_actor_handle.clone(),
            oauth_pool: self.oauth_pool.clone(),
            oauth_only: self.oauth_token.is_some() && self.cookie.is_none(),
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
        cookie: &mut crate::config::CookieStatus,
        handle: &crate::services::cookie_actor::CookieActorHandle,
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
        if let Some((sess, week, opus, sonnet)) = Self::fetch_usage_resets(cookie, handle).await {
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
        cookie: &mut crate::config::CookieStatus,
        handle: &CookieActorHandle,
    ) -> Option<(Option<i64>, Option<i64>, Option<i64>, Option<i64>)> {
        let profile = crate::stealth::global_profile().clone();
        let mut state =
            ClaudeCodeState::from_cookie(handle.clone(), cookie.clone(), profile).ok()?;
        let usage = state.fetch_usage_metrics().await.ok()?;
        state.return_cookie(None).await;
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
