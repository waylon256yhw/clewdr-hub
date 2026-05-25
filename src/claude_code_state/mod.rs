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
    config::{AccountSlot, ApiKeyExtraHeaders, AuthMethod, CLAUDE_ENDPOINT, Reason, TokenInfo},
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

/// Normalize a user-supplied API-key base URL so `.join("v1/messages")`
/// (and `.join("v1/messages/count_tokens")`) reliably produces
/// `{origin}/v1/messages`. SQLite stores whatever the admin typed, so
/// this helper has to absorb four common shapes:
///
///   `https://api.anthropic.com`        → `https://api.anthropic.com/`
///   `https://api.anthropic.com/`       → `https://api.anthropic.com/`
///   `https://api.anthropic.com/v1`     → `https://api.anthropic.com/`
///   `https://api.anthropic.com/v1/`    → `https://api.anthropic.com/`
///
/// Only the literal trailing `v1` segment is stripped — a custom mount
/// path like `https://proxy.example/anthropic/` is preserved so the
/// final URL becomes `https://proxy.example/anthropic/v1/messages`.
///
/// The trailing-slash invariant is load-bearing for `url::Url::join`:
/// without it the join replaces the final path segment rather than
/// appending, which silently produces wrong upstream URLs.
pub fn normalize_api_key_base_url(raw: &str) -> Result<url::Url, ClewdrError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ClewdrError::BadRequestMessage {
            msg: "api_key base_url is empty".into(),
        });
    }
    let mut url = url::Url::parse(trimmed).map_err(|e| ClewdrError::BadRequestMessage {
        msg: format!("api_key base_url is not a valid URL: {e}"),
    })?;

    let path = url.path().to_string();
    let stripped = path
        .strip_suffix("/v1/")
        .or_else(|| path.strip_suffix("/v1"))
        .map(str::to_string)
        .unwrap_or(path);

    let final_path = if stripped.is_empty() {
        "/".to_string()
    } else if stripped.ends_with('/') {
        stripped
    } else {
        format!("{stripped}/")
    };

    url.set_path(&final_path);
    Ok(url)
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
    /// API-key credential, populated iff the dispatched slot has
    /// `auth_method == AuthMethod::ApiKey`. Travels via `x-api-key` on
    /// every send (see C7 `execute_claude_request`). `None` for
    /// Cookie/OAuth so accidental reuse on a subscription path produces
    /// a missing-header 4xx rather than leaking a stale key.
    pub api_key: Option<String>,
    /// Optional per-account headers (e.g. `anthropic-workspace-id`)
    /// attached on every ApiKey send after the reserved-name filter.
    /// `None` for non-ApiKey slots.
    pub api_key_extra_headers: Option<ApiKeyExtraHeaders>,
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
            api_key: None,
            api_key_extra_headers: None,
        }
    }

    /// Build a `ClaudeCodeState` initialized with an existing account
    /// snapshot, dispatching by `slot.auth_method`:
    ///
    /// - `Cookie`: assemble the `Cookie` HTTP header from `slot.cookie`
    ///   so the wreq client sends it on every request.
    /// - `OAuth`: leave `cookie_header_value` empty. OAuth requests carry
    ///   their token via `Authorization: Bearer …` (see `send_chat`),
    ///   not via Cookie. `build_request` (mod.rs:166) already filters
    ///   empty cookie headers, so the empty sentinel is a clean no-op.
    ///
    /// Step 4 / C7 renamed `from_cookie` → `from_credential` to match
    /// the post-Step-4 reality that an `AccountSlot` is a credential
    /// container, not a cookie wrapper. C8 will flip `slot.cookie` to
    /// `Option<ClewdrCookie>`; the Cookie branch already routes through
    /// `slot.cookie` (currently `ClewdrCookie`) so that flip becomes a
    /// local edit instead of a constructor rewrite.
    pub fn from_credential(
        account_pool_handle: AccountPoolHandle,
        slot: AccountSlot,
        stealth_profile: SharedStealthProfile,
    ) -> Result<Self, ClewdrError> {
        let mut state = Self::new(account_pool_handle, stealth_profile);
        let auth_method = slot.auth_method;
        state.proxy_url = slot.proxy_url.clone();
        state.proxy = proxy_from_url(state.proxy_url.as_deref());

        state.cookie_header_value = match auth_method {
            AuthMethod::Cookie => {
                // Slot is invariantly Cookie-kind here (just dispatched
                // on auth_method), so `slot.cookie` must be Some — it
                // was populated by `AccountSlot::new()` when the loader
                // built it. Surface the inconsistency as an error rather
                // than panicking if invariant breaks.
                let cookie_value = slot
                    .cookie
                    .as_ref()
                    .ok_or(ClewdrError::UnexpectedNone {
                        msg: "Cookie kind invariant: slot.cookie missing on Cookie account",
                    })?
                    .to_string();
                HeaderValue::from_str(cookie_value.as_str())?
            }
            // OAuth / ApiKey: same empty sentinel; `build_request` (L189)
            // skips the COOKIE attach when empty, and ApiKey auth flows
            // via `x-api-key` in C7's send arm instead.
            AuthMethod::OAuth | AuthMethod::ApiKey => HeaderValue::from_static(""),
        };

        // Per-auth_method endpoint + wreq client + api_key plumbing.
        // Cookie/OAuth keep the subscription-shaped client (Chrome TLS
        // emulation + cookie store) and the default Anthropic endpoint.
        // ApiKey overrides the endpoint from the slot's normalized
        // base_url and uses a plain client — Chrome emulation is
        // anti-detection for the subscription reverse-proxy path and is
        // both meaningless and potentially trip-wire for strict
        // corporate proxies on direct-API calls; the cookie store is
        // similarly redundant since ApiKey never attaches Cookie.
        let mut client_builder = wreq::Client::builder();
        match auth_method {
            AuthMethod::Cookie | AuthMethod::OAuth => {
                state.endpoint = crate::config::ENDPOINT_URL.to_owned();
                client_builder = client_builder
                    .cookie_store(true)
                    .emulation(Emulation::Chrome136);
            }
            AuthMethod::ApiKey => {
                let raw_base =
                    slot.api_key_base_url
                        .as_deref()
                        .ok_or(ClewdrError::UnexpectedNone {
                            msg: "ApiKey kind invariant: slot.api_key_base_url missing",
                        })?;
                // Re-normalize defensively: admin write-time validation
                // (C10) is the primary guard, but a manual DB edit could
                // skip it, and `.join("v1/messages")` silently produces
                // wrong URLs if the trailing-slash invariant breaks.
                state.endpoint = normalize_api_key_base_url(raw_base)?;
                state.api_key = slot.api_key_secret.as_ref().map(|s| s.as_str().to_string());
                state.api_key_extra_headers = slot.api_key_extra_headers.clone();
            }
        }
        if let Some(ref proxy) = state.proxy {
            client_builder = client_builder.proxy(proxy.to_owned());
        }
        state.client = client_builder.build().context(WreqSnafu {
            msg: "Failed to build client for credential",
        })?;

        state.cookie = Some(slot);
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

    /// Requests a new account from the account pool and rebuilds the
    /// per-request HTTP client. Per-auth-method dispatch (Step 4 / C7):
    ///
    /// - `Cookie`: `cookie_header_value` is set from the slot's cookie
    ///   blob so wreq sends `Cookie: …` on every request. The bearer
    ///   token (if any — set after `exchange_token`) is independent.
    /// - `OAuth`: `cookie_header_value` stays empty. The bearer token
    ///   travels via `Authorization: Bearer …` in `send_chat`.
    ///
    /// This is the chat / count_tokens hot path entry point — it runs on
    /// every retry inside `try_chat` / `try_count_tokens`. Pre-C7 it
    /// unconditionally called `HeaderValue::from_str(res.cookie.to_string())`,
    /// which (a) tagged OAuth slots with their placeholder cookie blob
    /// and (b) panics in C8 once `slot.cookie` flips to `Option<…>`.
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
        self.cookie_header_value = match res.auth_method {
            AuthMethod::Cookie => {
                // Cookie kind invariant: pool slot for cookie account
                // has `cookie = Some(_)`. Treat the missing case as an
                // error rather than panicking on `expect()`.
                let cookie_value = res
                    .cookie
                    .as_ref()
                    .ok_or(ClewdrError::UnexpectedNone {
                        msg: "Cookie kind invariant: dispatched cookie slot missing cookie blob",
                    })?
                    .to_string();
                HeaderValue::from_str(cookie_value.as_str())?
            }
            // OAuth / ApiKey: empty sentinel; see `from_credential` for
            // the rationale (mirror the constructor exactly so both
            // entry points produce identically-shaped state).
            AuthMethod::OAuth | AuthMethod::ApiKey => HeaderValue::from_static(""),
        };
        self.proxy_url = res.proxy_url.clone();
        self.proxy = proxy_from_url(self.proxy_url.as_deref());

        // Per-auth_method endpoint + wreq client + api_key plumbing.
        // Mirror of the dispatch in `from_credential` so the hot-path
        // re-acquire (every retry inside try_chat / try_count_tokens)
        // ends up with the exact same client shape as a fresh
        // constructor. Resetting `api_key` / `api_key_extra_headers` on
        // the non-ApiKey arm is load-bearing: this method runs on every
        // retry, and a previous ApiKey acquisition followed by a
        // Cookie/OAuth slot would otherwise leak the prior key onto
        // a subscription send.
        let mut client_builder = wreq::Client::builder();
        match res.auth_method {
            AuthMethod::Cookie | AuthMethod::OAuth => {
                self.endpoint = crate::config::ENDPOINT_URL.to_owned();
                self.api_key = None;
                self.api_key_extra_headers = None;
                client_builder = client_builder
                    .cookie_store(true)
                    .emulation(Emulation::Chrome136);
            }
            AuthMethod::ApiKey => {
                let raw_base =
                    res.api_key_base_url
                        .as_deref()
                        .ok_or(ClewdrError::UnexpectedNone {
                            msg: "ApiKey kind invariant: dispatched api_key slot missing base_url",
                        })?;
                self.endpoint = normalize_api_key_base_url(raw_base)?;
                self.api_key = res.api_key_secret.as_ref().map(|s| s.as_str().to_string());
                self.api_key_extra_headers = res.api_key_extra_headers.clone();
            }
        }
        if let Some(ref proxy) = self.proxy {
            client_builder = client_builder.proxy(proxy.to_owned());
        }
        self.client = client_builder.build().context(WreqSnafu {
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
        // ApiKey accounts authenticate via `x-api-key` on every send;
        // there is no bearer to refresh, so the OAuth/cookie token
        // ladder below does not apply. The retry loop in
        // `try_chat`/`try_count_tokens` short-circuits the entire
        // ladder (and the bearer extraction) before reaching here for
        // an ApiKey slot, so this branch is mostly defensive — but
        // any code path that ever calls `check_token` on an ApiKey
        // slot deserves a sensible answer rather than the falsey
        // `None` that the cookie/oauth ladder would produce.
        if self
            .cookie
            .as_ref()
            .is_some_and(|s| s.auth_method == AuthMethod::ApiKey)
        {
            return TokenStatus::Valid;
        }
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
    use crate::services::account_error::{
        AccountFailureAction, FailureSource, classify_account_failure,
    };
    // Step 3.5: route through the unified classifier so every entry point
    // (messages / count_tokens / probe / refresh / test) reaches the same
    // "this account's auth is rejected" verdict. The classifier already
    // covers `InvalidCookie + Reason::Null|Banned`, `ClaudeHttpError 401|403`
    // and `Whatever` messages with `invalid_grant` / refresh-token /
    // `status 401|403` phrases.
    matches!(
        classify_account_failure(err, FailureSource::Messages, None).action,
        AccountFailureAction::TerminalAuth
    )
}

pub enum TokenStatus {
    None,
    Expired,
    Valid,
}

#[cfg(test)]
mod tests {
    use super::{is_oauth_auth_failure, normalize_api_key_base_url};
    use crate::{
        config::Reason,
        error::{ClaudeErrorBody, ClewdrError},
    };
    use serde_json::json;
    use wreq::StatusCode;

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

    /// Step 3.5 C2: regression guard. The classifier must preserve the
    /// "auth failure" verdict for unphrased 401/403 responses (the chat
    /// path historically treats those as oauth auth failures), and must
    /// keep transient/internal classes out of the auth failure set.
    #[test]
    fn oauth_auth_failure_classifier_equivalence() {
        let http = |status: u16| ClewdrError::ClaudeHttpError {
            code: StatusCode::from_u16(status).unwrap(),
            inner: Box::new(ClaudeErrorBody {
                message: json!("upstream"),
                r#type: "error".to_string(),
                code: Some(status),
                ..Default::default()
            }),
        };
        assert!(is_oauth_auth_failure(&http(401)));
        assert!(is_oauth_auth_failure(&http(403)));
        assert!(!is_oauth_auth_failure(&http(500)));
        assert!(!is_oauth_auth_failure(&http(429)));
        assert!(!is_oauth_auth_failure(&ClewdrError::InvalidCookie {
            reason: Reason::TooManyRequest(123),
        }));
        assert!(!is_oauth_auth_failure(&ClewdrError::InvalidCookie {
            reason: Reason::Free,
        }));
        assert!(is_oauth_auth_failure(&ClewdrError::Whatever {
            message: "oauth refresh: invalid_grant".to_string(),
            source: None,
        }));
        assert!(!is_oauth_auth_failure(&ClewdrError::Whatever {
            message: "unrelated local failure".to_string(),
            source: None,
        }));
    }

    /// All four common admin-entered shapes of the anthropic base URL
    /// must, after `normalize_api_key_base_url` + `.join("v1/messages")`,
    /// land at the same upstream path. This is the single load-bearing
    /// invariant of the helper — break it and every ApiKey send 404s.
    #[test]
    fn normalize_api_key_base_url_canonicalizes_anthropic_shapes() {
        let expected = "https://api.anthropic.com/v1/messages";
        for raw in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/",
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com/v1/",
        ] {
            let normalized =
                normalize_api_key_base_url(raw).expect("anthropic base url should normalize");
            let joined = normalized.join("v1/messages").expect("join should succeed");
            assert_eq!(joined.as_str(), expected, "input: {raw}");
        }
    }

    /// Same invariant applied to the count_tokens sibling route — guards
    /// against a regression where the trailing-slash logic only worked
    /// against the one path the helper was written against.
    #[test]
    fn normalize_api_key_base_url_supports_count_tokens_join() {
        let normalized = normalize_api_key_base_url("https://api.anthropic.com/v1")
            .expect("v1-suffix base url should normalize");
        let joined = normalized
            .join("v1/messages/count_tokens")
            .expect("join should succeed");
        assert_eq!(
            joined.as_str(),
            "https://api.anthropic.com/v1/messages/count_tokens",
        );
    }

    /// A user pointing at an internal anthropic-compatible proxy mounted
    /// at `/anthropic/` must keep that prefix — only the literal trailing
    /// `/v1` segment is stripped, never a user-supplied mount path.
    #[test]
    fn normalize_api_key_base_url_preserves_custom_mount_path() {
        let normalized = normalize_api_key_base_url("https://proxy.example/anthropic/")
            .expect("custom mount should normalize");
        let joined = normalized.join("v1/messages").expect("join should succeed");
        assert_eq!(
            joined.as_str(),
            "https://proxy.example/anthropic/v1/messages",
        );
    }

    #[test]
    fn normalize_api_key_base_url_trims_whitespace() {
        let normalized = normalize_api_key_base_url("  https://api.anthropic.com/v1/  ")
            .expect("whitespace-padded base url should normalize");
        assert_eq!(
            normalized.join("v1/messages").unwrap().as_str(),
            "https://api.anthropic.com/v1/messages",
        );
    }

    #[test]
    fn normalize_api_key_base_url_rejects_empty() {
        for raw in ["", "   ", "\t\n"] {
            let err = normalize_api_key_base_url(raw)
                .expect_err("empty/whitespace base url should error");
            assert!(matches!(err, ClewdrError::BadRequestMessage { .. }));
        }
    }

    #[test]
    fn normalize_api_key_base_url_rejects_non_url() {
        let err =
            normalize_api_key_base_url("not a url at all").expect_err("invalid URL should error");
        assert!(matches!(err, ClewdrError::BadRequestMessage { .. }));
    }
}
