mod chat;
mod exchange;
mod organization;
pub mod probe;
use std::sync::LazyLock;

use http::{
    HeaderValue, Method,
    header::{COOKIE, ORIGIN, REFERER, USER_AGENT},
};
use snafu::ResultExt;
use tracing::error;
use wreq::RequestBuilder;
use wreq_util::Emulation;

use crate::{
    billing::BillingContext,
    config::{AccountSlot, CLAUDE_ENDPOINT, Reason, TokenInfo},
    error::{ClewdrError, WreqSnafu},
    services::account_pool::{AccountPoolHandle, CredentialFingerprint},
    stealth::SharedStealthProfile,
    types::claude::Usage,
};

static SUPER_CLIENT: LazyLock<wreq::Client> = LazyLock::new(wreq::Client::new);

pub(crate) fn proxy_from_url(proxy_url: Option<&str>) -> Option<wreq::Proxy> {
    proxy_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|p| {
            wreq::Proxy::all(p)
                .inspect_err(|e| error!("Failed to parse proxy URL: {e}"))
                .ok()
        })
}

pub(crate) fn build_api_client(proxy_url: Option<&str>) -> wreq::Client {
    let mut builder = wreq::Client::builder();
    if let Some(proxy) = proxy_from_url(proxy_url) {
        builder = builder.proxy(proxy);
    }
    builder.build().unwrap_or_else(|e| {
        error!("Failed to build API client: {e}");
        SUPER_CLIENT.to_owned()
    })
}

#[derive(Clone)]
pub struct ClaudeCodeState {
    pub account_pool_handle: AccountPoolHandle,
    pub cookie: Option<AccountSlot>,
    pub cookie_header_value: HeaderValue,
    pub proxy_url: Option<String>,
    pub proxy: Option<wreq::Proxy>,
    pub endpoint: url::Url,
    pub client: wreq::Client,
    pub stream: bool,
    pub system_prompt_hash: Option<u64>,
    pub anthropic_beta_header: Option<String>,
    pub oauth_token: Option<TokenInfo>,
    pub account_id: Option<i64>,
    pub organization_uuid: Option<String>,
    pub usage: Usage,
    pub billing_ctx: Option<BillingContext>,
    pub stealth_profile: SharedStealthProfile,
    pub bound_account_ids: Vec<i64>,
    pub selected_account_id: Option<std::sync::Arc<std::sync::Mutex<Option<i64>>>>,
}

impl ClaudeCodeState {
    /// Create a new ClaudeCodeState instance
    pub fn new(
        account_pool_handle: AccountPoolHandle,
        stealth_profile: SharedStealthProfile,
    ) -> Self {
        ClaudeCodeState {
            account_pool_handle,
            cookie: None,
            cookie_header_value: HeaderValue::from_static(""),
            proxy_url: None,
            client: build_api_client(None),
            proxy: None,
            endpoint: crate::config::ENDPOINT_URL.to_owned(),
            stream: false,
            system_prompt_hash: None,
            anthropic_beta_header: None,
            oauth_token: None,
            account_id: None,
            organization_uuid: None,
            usage: Usage::default(),
            billing_ctx: None,
            stealth_profile,
            bound_account_ids: Vec::new(),
            selected_account_id: None,
        }
    }

    /// Build a ClaudeCodeState initialized with an existing cookie snapshot
    pub fn from_cookie(
        account_pool_handle: AccountPoolHandle,
        cookie: AccountSlot,
        stealth_profile: SharedStealthProfile,
    ) -> Result<Self, ClewdrError> {
        let mut state = Self::new(account_pool_handle, stealth_profile);
        state.cookie = Some(cookie);
        let cookie_value = state
            .cookie
            .as_ref()
            .ok_or(ClewdrError::UnexpectedNone {
                msg: "Cookie missing while initializing state",
            })?
            .cookie
            .to_string();
        let header_value = HeaderValue::from_str(cookie_value.as_str())?;
        state.cookie_header_value = header_value.clone();
        state.proxy_url = state
            .cookie
            .as_ref()
            .and_then(|slot| slot.proxy_url.clone());
        state.proxy = proxy_from_url(state.proxy_url.as_deref());
        let mut client = wreq::Client::builder()
            .cookie_store(true)
            .emulation(Emulation::Chrome136);
        if let Some(ref proxy) = state.proxy {
            client = client.proxy(proxy.to_owned());
        }
        state.client = client.build().context(WreqSnafu {
            msg: "Failed to build client for cookie",
        })?;
        Ok(state)
    }

    /// Returns the current account to the account pool
    /// Optionally provides a reason for returning the account (e.g., invalid, banned)
    pub async fn release_account(&self, reason: Option<Reason>) {
        // return the account to the account pool
        if let Some(ref cookie) = self.cookie {
            let Some(account_id) = cookie.account_id else {
                return;
            };
            let update = cookie.to_runtime_params();
            // Capture the request-time credential identity so the pool can
            // discard this release if the credential has been admin-rotated
            // since acquire (Step 4 / C5).
            let fingerprint = CredentialFingerprint::from_slot(cookie);
            self.account_pool_handle
                .release_runtime(account_id, update, reason, fingerprint)
                .await
                .unwrap_or_else(|e| {
                    error!("Failed to release account: {}", e);
                });
        }
    }

    /// Build a request for console/browser endpoints (with Origin/Referer/Cookie)
    pub fn build_request(&self, method: Method, url: impl ToString) -> RequestBuilder {
        let profile = self.stealth_profile.load();
        let ua = profile.user_agent();
        let mut req = self
            .client
            .request(method, url.to_string())
            .header(ORIGIN, CLAUDE_ENDPOINT)
            .header(REFERER, format!("{CLAUDE_ENDPOINT}new"))
            .header(USER_AGENT, ua);
        if !self.cookie_header_value.as_bytes().is_empty() {
            req = req.header(COOKIE, self.cookie_header_value.clone());
        }
        req
    }

    /// Set the cookie header value
    pub fn set_cookie_header_value(&mut self, value: HeaderValue) {
        self.cookie_header_value = value;
    }

    /// Requests a new account from the account pool
    /// Updates the internal state with the new cookie and proxy configuration
    pub async fn acquire_account(&mut self) -> Result<AccountSlot, ClewdrError> {
        if let Some(selected_account_id) = &self.selected_account_id
            && let Ok(mut slot) = selected_account_id.lock()
        {
            *slot = None;
        }
        let res = self
            .account_pool_handle
            .request(self.system_prompt_hash, &self.bound_account_ids)
            .await?;
        self.cookie = Some(res.to_owned());
        self.cookie_header_value = HeaderValue::from_str(res.cookie.to_string().as_str())?;
        self.proxy_url = res.proxy_url.clone();
        self.proxy = proxy_from_url(self.proxy_url.as_deref());
        self.endpoint = crate::config::ENDPOINT_URL.to_owned();
        let mut client = wreq::Client::builder()
            .cookie_store(true)
            .emulation(Emulation::Chrome136);
        if let Some(ref proxy) = self.proxy {
            client = client.proxy(proxy.to_owned());
        }
        self.client = client.build().context(WreqSnafu {
            msg: "Failed to build client with new cookie",
        })?;
        if let Some(selected_account_id) = &self.selected_account_id
            && let Ok(mut slot) = selected_account_id.lock()
        {
            *slot = res.account_id;
        }
        Ok(res)
    }

    pub fn set_proxy_url(&mut self, proxy_url: Option<&str>) {
        self.proxy_url = proxy_url.map(|s| s.to_string());
        self.proxy = proxy_from_url(proxy_url);
        self.client = build_api_client(proxy_url);
    }

    pub fn check_token(&self) -> TokenStatus {
        if let Some(token_info) = &self.oauth_token {
            if token_info.is_expired() {
                return TokenStatus::Expired;
            }
            return TokenStatus::Valid;
        }
        let Some(AccountSlot {
            token: Some(token_info),
            ..
        }) = &self.cookie
        else {
            return TokenStatus::None;
        };
        if token_info.is_expired() {
            TokenStatus::Expired
        } else {
            TokenStatus::Valid
        }
    }
}

pub(crate) fn is_oauth_auth_failure(err: &ClewdrError) -> bool {
    match err {
        ClewdrError::InvalidCookie { reason } => matches!(reason, Reason::Null | Reason::Banned),
        ClewdrError::ClaudeHttpError { code, .. } => matches!(code.as_u16(), 401 | 403),
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

pub enum TokenStatus {
    None,
    Expired,
    Valid,
}

#[cfg(test)]
mod tests {
    use super::is_oauth_auth_failure;
    use crate::{config::Reason, error::ClewdrError};

    #[test]
    fn oauth_auth_failure_detects_invalid_cookie_null_and_banned() {
        assert!(is_oauth_auth_failure(&ClewdrError::InvalidCookie {
            reason: Reason::Null,
        }));
        assert!(is_oauth_auth_failure(&ClewdrError::InvalidCookie {
            reason: Reason::Banned,
        }));
        assert!(!is_oauth_auth_failure(&ClewdrError::InvalidCookie {
            reason: Reason::Disabled,
        }));
    }
}
