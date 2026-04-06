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
    config::{CLAUDE_ENDPOINT, CookieStatus, Reason, TokenInfo},
    error::{ClewdrError, WreqSnafu},
    providers::claude::OAuthAccountPool,
    services::cookie_actor::CookieActorHandle,
    stealth::SharedStealthProfile,
    types::claude::Usage,
};

static SUPER_CLIENT: LazyLock<wreq::Client> = LazyLock::new(wreq::Client::new);

fn proxy_from_profile(profile: &SharedStealthProfile) -> Option<wreq::Proxy> {
    profile
        .load()
        .proxy
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|p| {
            wreq::Proxy::all(p)
                .inspect_err(|e| error!("Failed to parse proxy from settings: {e}"))
                .ok()
        })
}

fn build_api_client(proxy: Option<&wreq::Proxy>) -> wreq::Client {
    let mut builder = wreq::Client::builder();
    if let Some(proxy) = proxy {
        builder = builder.proxy(proxy.to_owned());
    }
    builder.build().unwrap_or_else(|e| {
        error!("Failed to build API client: {e}");
        SUPER_CLIENT.to_owned()
    })
}

#[derive(Clone)]
pub struct ClaudeCodeState {
    pub cookie_actor_handle: CookieActorHandle,
    pub cookie: Option<CookieStatus>,
    pub cookie_header_value: HeaderValue,
    pub proxy: Option<wreq::Proxy>,
    pub endpoint: url::Url,
    pub client: wreq::Client,
    pub stream: bool,
    pub system_prompt_hash: Option<u64>,
    pub anthropic_beta_header: Option<String>,
    pub oauth_token: Option<TokenInfo>,
    pub account_id: Option<i64>,
    pub organization_uuid: Option<String>,
    pub(crate) oauth_pool: Option<std::sync::Arc<OAuthAccountPool>>,
    pub usage: Usage,
    pub billing_ctx: Option<BillingContext>,
    pub stealth_profile: SharedStealthProfile,
    pub bound_account_ids: Vec<i64>,
}

impl ClaudeCodeState {
    /// Create a new ClaudeCodeState instance
    pub fn new(
        cookie_actor_handle: CookieActorHandle,
        stealth_profile: SharedStealthProfile,
    ) -> Self {
        let proxy = proxy_from_profile(&stealth_profile);
        ClaudeCodeState {
            cookie_actor_handle,
            cookie: None,
            cookie_header_value: HeaderValue::from_static(""),
            client: build_api_client(proxy.as_ref()),
            proxy,
            endpoint: crate::config::ENDPOINT_URL.to_owned(),
            stream: false,
            system_prompt_hash: None,
            anthropic_beta_header: None,
            oauth_token: None,
            account_id: None,
            organization_uuid: None,
            oauth_pool: None,
            usage: Usage::default(),
            billing_ctx: None,
            stealth_profile,
            bound_account_ids: Vec::new(),
        }
    }

    /// Build a ClaudeCodeState initialized with an existing cookie snapshot
    pub fn from_cookie(
        cookie_actor_handle: CookieActorHandle,
        cookie: CookieStatus,
        stealth_profile: SharedStealthProfile,
    ) -> Result<Self, ClewdrError> {
        let mut state = Self::new(cookie_actor_handle, stealth_profile);
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

    /// Returns the current cookie to the cookie manager
    /// Optionally provides a reason for returning the cookie (e.g., invalid, banned)
    pub async fn return_cookie(&self, reason: Option<Reason>) {
        // return the cookie to the cookie manager
        if let Some(ref cookie) = self.cookie {
            self.cookie_actor_handle
                .return_cookie(cookie.to_owned(), reason)
                .await
                .unwrap_or_else(|e| {
                    error!("Failed to send cookie: {}", e);
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

    /// Requests a new cookie from the cookie manager
    /// Updates the internal state with the new cookie and proxy configuration
    pub async fn request_cookie(&mut self) -> Result<CookieStatus, ClewdrError> {
        let res = self
            .cookie_actor_handle
            .request(self.system_prompt_hash, &self.bound_account_ids)
            .await?;
        self.cookie = Some(res.to_owned());
        self.cookie_header_value = HeaderValue::from_str(res.cookie.to_string().as_str())?;
        // Always pull latest proxy from stealth profile
        self.proxy = proxy_from_profile(&self.stealth_profile);
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
        Ok(res)
    }

    pub fn check_token(&self) -> TokenStatus {
        if let Some(token_info) = &self.oauth_token {
            if token_info.is_expired() {
                return TokenStatus::Expired;
            }
            return TokenStatus::Valid;
        }
        let Some(CookieStatus {
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

pub enum TokenStatus {
    None,
    Expired,
    Valid,
}
