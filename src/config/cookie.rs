use std::{
    fmt::{Debug, Display},
    ops::Deref,
    str::FromStr,
    sync::LazyLock,
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use snafu::{GenerateImplicitData, Location};
use tracing::info;

use crate::{
    config::{PLACEHOLDER_COOKIE, TokenInfo},
    error::ClewdrError,
};

/// Model family for usage bucketing
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModelFamily {
    Sonnet,
    Opus,
    Other,
}

/// Authentication method for an account.
///
/// Step 4 introduces this as the canonical kind discriminator for an
/// `AccountSlot`. Pre-Step-4 code derived "is this OAuth?" from the
/// presence of a placeholder cookie (`is_oauth_placeholder_slot`); that
/// implicit shape is being retired across PR #6/#7/#8. Loader fills this
/// from the DB column `accounts.auth_source`.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    #[default]
    Cookie,
    OAuth,
}

impl AuthMethod {
    /// Map the persisted `accounts.auth_source` string to a typed kind.
    /// Unknown values fall back to `Cookie` (defensive — Step 1 already
    /// constrained the column to `cookie | oauth`).
    pub fn from_auth_source(s: &str) -> Self {
        match s {
            "oauth" => AuthMethod::OAuth,
            _ => AuthMethod::Cookie,
        }
    }
}

/// Per-period usage breakdown by family
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct UsageBreakdown {
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,

    #[serde(default)]
    pub sonnet_input_tokens: u64,
    #[serde(default)]
    pub sonnet_output_tokens: u64,

    #[serde(default)]
    pub opus_input_tokens: u64,
    #[serde(default)]
    pub opus_output_tokens: u64,
}

/// A struct representing a cookie
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClewdrCookie {
    inner: String,
}

impl Serialize for ClewdrCookie {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.inner)
    }
}

impl<'de> Deserialize<'de> for ClewdrCookie {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        ClewdrCookie::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// A struct representing a cookie with its information
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct AccountSlot {
    /// The session cookie blob, present iff `auth_method == Cookie`.
    /// Step 4 / C8 flipped this to `Option<ClewdrCookie>`: pre-C8 OAuth
    /// rows were padded with a synthetic placeholder cookie
    /// (`oauth_placeholder_cookie(id)`) just so this field could be
    /// non-Option, which leaked the cookie shape into log lines, slot
    /// identity, and serialization. The loader (`do_reload`) now
    /// constructs OAuth slots with `cookie = None` directly.
    ///
    /// Hot-path access points (`exchange_token`, `probe_cookie`,
    /// `from_credential` Cookie arm) gate on `auth_method == Cookie`
    /// before reading this field, so the `expect("Cookie kind invariant")`
    /// at those sites is sound.
    pub cookie: Option<ClewdrCookie>,
    /// Authentication kind (Cookie or OAuth). Loader populates this from
    /// `accounts.auth_source`. `#[serde(default)]` keeps deserialization
    /// of pre-Step-4 snapshots compatible (defaults to Cookie).
    #[serde(default)]
    pub auth_method: AuthMethod,
    #[serde(default)]
    pub account_id: Option<i64>,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub token: Option<TokenInfo>,
    #[serde(default)]
    pub reset_time: Option<i64>,
    #[serde(default)]
    pub supports_claude_1m_sonnet: Option<bool>,
    #[serde(default)]
    pub supports_claude_1m_opus: Option<bool>,
    #[serde(default)]
    pub count_tokens_allowed: Option<bool>,

    // New: Per-period usage breakdown
    #[serde(default)]
    pub session_usage: UsageBreakdown,
    #[serde(default)]
    pub weekly_usage: UsageBreakdown,
    #[serde(default)]
    pub weekly_sonnet_usage: UsageBreakdown,
    #[serde(default)]
    pub weekly_opus_usage: UsageBreakdown,
    #[serde(default)]
    pub lifetime_usage: UsageBreakdown,

    // Reset boundaries for each period (epoch seconds, UTC)
    #[serde(default)]
    pub session_resets_at: Option<i64>,
    #[serde(default)]
    pub weekly_resets_at: Option<i64>,
    #[serde(default)]
    pub weekly_sonnet_resets_at: Option<i64>,
    #[serde(default)]
    pub weekly_opus_resets_at: Option<i64>,

    /// Last time we probed Anthropic console for resets_at
    #[serde(default)]
    pub resets_last_checked_at: Option<i64>,

    /// Whether the subscription exposes a reset boundary for each window
    /// None = unknown (not probed yet), Some(true) = track this window, Some(false) = no limit, never probe again
    #[serde(default)]
    pub session_has_reset: Option<bool>,
    #[serde(default)]
    pub weekly_has_reset: Option<bool>,
    #[serde(default)]
    pub weekly_sonnet_has_reset: Option<bool>,
    #[serde(default)]
    pub weekly_opus_has_reset: Option<bool>,

    // Account metadata from bootstrap probe
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub account_type: Option<String>,

    #[serde(default)]
    pub session_utilization: Option<f64>,
    #[serde(default)]
    pub weekly_utilization: Option<f64>,
    #[serde(default)]
    pub weekly_sonnet_utilization: Option<f64>,
    #[serde(default)]
    pub weekly_opus_utilization: Option<f64>,
}

// `AccountSlot` deliberately does not implement `PartialEq` / `Eq` /
// `Hash` / `Ord` / `PartialOrd`. Step 4 / C9 retired the cookie-keyed
// impls — pre-Step-4 they hashed/sorted by `self.cookie`, which (a) was
// the wrong identity once OAuth slots existed (C8 made cookie Optional;
// two OAuth slots would compare equal under those impls), and (b) had
// no remaining production caller after Step 2 keyed every pool bucket
// by `account_id` (HashMap<i64, _> / VecDeque<_>).
//
// Code that needs an account identity must use `slot.account_id`
// explicitly (`Option<i64>`). Code that needs to dedupe / sort must
// build its own keying strategy. The compiler now enforces that no
// caller silently leans on cookie-keyed identity.

impl AccountSlot {
    /// Creates a new AccountSlot instance
    ///
    /// # Arguments
    /// * `cookie` - Cookie string
    /// * `reset_time` - Optional timestamp when the cookie can be reused
    ///
    /// # Returns
    /// A new AccountSlot instance
    pub fn new(cookie: &str, reset_time: Option<i64>) -> Result<Self, ClewdrError> {
        let cookie = ClewdrCookie::from_str(cookie)?;
        Ok(Self {
            cookie: Some(cookie),
            auth_method: AuthMethod::Cookie,
            account_id: None,
            proxy_url: None,
            token: None,
            reset_time,
            supports_claude_1m_sonnet: None,
            supports_claude_1m_opus: None,
            count_tokens_allowed: None,

            session_usage: UsageBreakdown::default(),
            weekly_usage: UsageBreakdown::default(),
            weekly_sonnet_usage: UsageBreakdown::default(),
            weekly_opus_usage: UsageBreakdown::default(),
            lifetime_usage: UsageBreakdown::default(),
            session_resets_at: None,
            weekly_resets_at: None,
            weekly_sonnet_resets_at: None,
            weekly_opus_resets_at: None,
            resets_last_checked_at: None,
            session_has_reset: None,
            weekly_has_reset: None,
            weekly_sonnet_has_reset: None,
            weekly_opus_has_reset: None,
            email: None,
            account_type: None,
            session_utilization: None,
            weekly_utilization: None,
            weekly_sonnet_utilization: None,
            weekly_opus_utilization: None,
        })
    }

    /// Checks if the cookie's reset time has expired
    /// If the reset time has passed, sets it to None so the cookie becomes valid again
    ///
    /// # Returns
    /// The same AccountSlot with potentially updated reset_time
    pub fn reset(self) -> Self {
        if let Some(t) = self.reset_time
            && t <= chrono::Utc::now().timestamp()
        {
            info!("Cookie reset time expired");
            return Self {
                reset_time: None,
                session_usage: UsageBreakdown::default(),
                weekly_usage: UsageBreakdown::default(),
                weekly_sonnet_usage: UsageBreakdown::default(),
                weekly_opus_usage: UsageBreakdown::default(),
                ..self
            };
        }
        self
    }

    /// Construct an OAuth slot directly from `(account_id, token)`. Used
    /// by the loader (`do_reload`) for oauth-only DB rows and by tests
    /// (post-C10) replacing the historical `AccountSlot::new(&oauth_placeholder_cookie(id), None)`
    /// idiom. Step 4 / C8 onward — this is the canonical OAuth slot
    /// constructor.
    pub fn oauth(account_id: i64, token: TokenInfo) -> Self {
        Self {
            cookie: None,
            auth_method: AuthMethod::OAuth,
            account_id: Some(account_id),
            token: Some(token),
            ..Self::default()
        }
    }

    pub fn add_token(&mut self, token: TokenInfo) {
        self.token = Some(token);
    }

    /// Short, log-safe label identifying the credential. Pre-Step-4 / C7
    /// every call site reached for `slot.cookie.ellipse()` directly,
    /// which (a) leaks the cookie shape into log messages even for OAuth
    /// accounts and (b) panics in C8 once `slot.cookie` flips to
    /// `Option<ClewdrCookie>`. This helper centralizes the label so the
    /// flip is a one-line change here.
    pub fn credential_label(&self) -> String {
        match self.auth_method {
            AuthMethod::Cookie => self
                .cookie
                .as_ref()
                .map(|c| c.ellipse())
                .unwrap_or_else(|| "cookie#?".to_string()),
            AuthMethod::OAuth => match self.account_id {
                Some(id) => format!("oauth#{id}"),
                None => "oauth#?".to_string(),
            },
        }
    }

    pub fn set_count_tokens_allowed(&mut self, value: Option<bool>) {
        self.count_tokens_allowed = value;
    }

    pub fn reset_window_usage(&mut self) {
        // Legacy window counters removed; reset session buckets conservatively
        self.session_usage = UsageBreakdown::default();
        self.weekly_usage = UsageBreakdown::default();
        self.weekly_sonnet_usage = UsageBreakdown::default();
        self.weekly_opus_usage = UsageBreakdown::default();
    }

    // ------------------------
    // New usage aggregation
    // ------------------------

    pub fn set_session_resets_at(&mut self, ts: Option<i64>) {
        self.session_resets_at = ts;
    }

    pub fn set_weekly_resets_at(&mut self, ts: Option<i64>) {
        self.weekly_resets_at = ts;
    }

    pub fn set_weekly_sonnet_resets_at(&mut self, ts: Option<i64>) {
        self.weekly_sonnet_resets_at = ts;
    }

    pub fn set_weekly_opus_resets_at(&mut self, ts: Option<i64>) {
        self.weekly_opus_resets_at = ts;
    }

    pub fn add_and_bucket_usage(&mut self, input: u64, output: u64, family: ModelFamily) {
        if input == 0 && output == 0 {
            return;
        }
        // Legacy totals/windows removed; only bucketed aggregation remains

        // session bucket (total + per family)
        self.session_usage.total_input_tokens =
            self.session_usage.total_input_tokens.saturating_add(input);
        self.session_usage.total_output_tokens = self
            .session_usage
            .total_output_tokens
            .saturating_add(output);
        match family {
            ModelFamily::Sonnet => {
                self.session_usage.sonnet_input_tokens =
                    self.session_usage.sonnet_input_tokens.saturating_add(input);
                self.session_usage.sonnet_output_tokens = self
                    .session_usage
                    .sonnet_output_tokens
                    .saturating_add(output);
            }
            ModelFamily::Opus => {
                self.session_usage.opus_input_tokens =
                    self.session_usage.opus_input_tokens.saturating_add(input);
                self.session_usage.opus_output_tokens =
                    self.session_usage.opus_output_tokens.saturating_add(output);
            }
            ModelFamily::Other => {}
        }

        // weekly bucket (total + per family)
        self.weekly_usage.total_input_tokens =
            self.weekly_usage.total_input_tokens.saturating_add(input);
        self.weekly_usage.total_output_tokens =
            self.weekly_usage.total_output_tokens.saturating_add(output);
        match family {
            ModelFamily::Sonnet => {
                self.weekly_usage.sonnet_input_tokens =
                    self.weekly_usage.sonnet_input_tokens.saturating_add(input);
                self.weekly_usage.sonnet_output_tokens = self
                    .weekly_usage
                    .sonnet_output_tokens
                    .saturating_add(output);

                // weekly_sonnet bucket (only sonnet contributes)
                self.weekly_sonnet_usage.total_input_tokens = self
                    .weekly_sonnet_usage
                    .total_input_tokens
                    .saturating_add(input);
                self.weekly_sonnet_usage.total_output_tokens = self
                    .weekly_sonnet_usage
                    .total_output_tokens
                    .saturating_add(output);
                self.weekly_sonnet_usage.sonnet_input_tokens = self
                    .weekly_sonnet_usage
                    .sonnet_input_tokens
                    .saturating_add(input);
                self.weekly_sonnet_usage.sonnet_output_tokens = self
                    .weekly_sonnet_usage
                    .sonnet_output_tokens
                    .saturating_add(output);
            }
            ModelFamily::Opus => {
                self.weekly_usage.opus_input_tokens =
                    self.weekly_usage.opus_input_tokens.saturating_add(input);
                self.weekly_usage.opus_output_tokens =
                    self.weekly_usage.opus_output_tokens.saturating_add(output);
            }
            ModelFamily::Other => {}
        }

        // weekly_opus bucket (only opus contributes)
        if matches!(family, ModelFamily::Opus) {
            self.weekly_opus_usage.total_input_tokens = self
                .weekly_opus_usage
                .total_input_tokens
                .saturating_add(input);
            self.weekly_opus_usage.total_output_tokens = self
                .weekly_opus_usage
                .total_output_tokens
                .saturating_add(output);
            self.weekly_opus_usage.opus_input_tokens = self
                .weekly_opus_usage
                .opus_input_tokens
                .saturating_add(input);
            self.weekly_opus_usage.opus_output_tokens = self
                .weekly_opus_usage
                .opus_output_tokens
                .saturating_add(output);
        }

        // lifetime bucket (total + per family)
        self.lifetime_usage.total_input_tokens =
            self.lifetime_usage.total_input_tokens.saturating_add(input);
        self.lifetime_usage.total_output_tokens = self
            .lifetime_usage
            .total_output_tokens
            .saturating_add(output);
        match family {
            ModelFamily::Sonnet => {
                self.lifetime_usage.sonnet_input_tokens = self
                    .lifetime_usage
                    .sonnet_input_tokens
                    .saturating_add(input);
                self.lifetime_usage.sonnet_output_tokens = self
                    .lifetime_usage
                    .sonnet_output_tokens
                    .saturating_add(output);
            }
            ModelFamily::Opus => {
                self.lifetime_usage.opus_input_tokens =
                    self.lifetime_usage.opus_input_tokens.saturating_add(input);
                self.lifetime_usage.opus_output_tokens = self
                    .lifetime_usage
                    .opus_output_tokens
                    .saturating_add(output);
            }
            ModelFamily::Other => {}
        }
    }
}

/// Parameters for upserting account_runtime_state to DB.
#[derive(Debug, Clone)]
pub struct RuntimeStateParams {
    pub reset_time: Option<i64>,
    pub supports_claude_1m_sonnet: Option<bool>,
    pub supports_claude_1m_opus: Option<bool>,
    pub count_tokens_allowed: Option<bool>,
    pub session_resets_at: Option<i64>,
    pub weekly_resets_at: Option<i64>,
    pub weekly_sonnet_resets_at: Option<i64>,
    pub weekly_opus_resets_at: Option<i64>,
    pub resets_last_checked_at: Option<i64>,
    pub session_has_reset: Option<bool>,
    pub weekly_has_reset: Option<bool>,
    pub weekly_sonnet_has_reset: Option<bool>,
    pub weekly_opus_has_reset: Option<bool>,
    pub session_utilization: Option<f64>,
    pub weekly_utilization: Option<f64>,
    pub weekly_sonnet_utilization: Option<f64>,
    pub weekly_opus_utilization: Option<f64>,
    pub buckets: [UsageBreakdown; 5], // session, weekly, weekly_sonnet, weekly_opus, lifetime
}

impl AccountSlot {
    /// Extract runtime state parameters for DB persistence.
    pub fn to_runtime_params(&self) -> RuntimeStateParams {
        RuntimeStateParams {
            reset_time: self.reset_time,
            supports_claude_1m_sonnet: self.supports_claude_1m_sonnet,
            supports_claude_1m_opus: self.supports_claude_1m_opus,
            count_tokens_allowed: self.count_tokens_allowed,
            session_resets_at: self.session_resets_at,
            weekly_resets_at: self.weekly_resets_at,
            weekly_sonnet_resets_at: self.weekly_sonnet_resets_at,
            weekly_opus_resets_at: self.weekly_opus_resets_at,
            resets_last_checked_at: self.resets_last_checked_at,
            session_has_reset: self.session_has_reset,
            weekly_has_reset: self.weekly_has_reset,
            weekly_sonnet_has_reset: self.weekly_sonnet_has_reset,
            weekly_opus_has_reset: self.weekly_opus_has_reset,
            session_utilization: self.session_utilization,
            weekly_utilization: self.weekly_utilization,
            weekly_sonnet_utilization: self.weekly_sonnet_utilization,
            weekly_opus_utilization: self.weekly_opus_utilization,
            buckets: [
                self.session_usage.clone(),
                self.weekly_usage.clone(),
                self.weekly_sonnet_usage.clone(),
                self.weekly_opus_usage.clone(),
                self.lifetime_usage.clone(),
            ],
        }
    }

    /// Apply runtime state from a DB row onto this AccountSlot.
    pub fn apply_runtime_state(&mut self, p: &RuntimeStateParams) {
        self.reset_time = p.reset_time;
        self.supports_claude_1m_sonnet = p.supports_claude_1m_sonnet;
        self.supports_claude_1m_opus = p.supports_claude_1m_opus;
        self.count_tokens_allowed = p.count_tokens_allowed;
        self.session_resets_at = p.session_resets_at;
        self.weekly_resets_at = p.weekly_resets_at;
        self.weekly_sonnet_resets_at = p.weekly_sonnet_resets_at;
        self.weekly_opus_resets_at = p.weekly_opus_resets_at;
        self.resets_last_checked_at = p.resets_last_checked_at;
        self.session_has_reset = p.session_has_reset;
        self.weekly_has_reset = p.weekly_has_reset;
        self.weekly_sonnet_has_reset = p.weekly_sonnet_has_reset;
        self.weekly_opus_has_reset = p.weekly_opus_has_reset;
        self.session_utilization = p.session_utilization;
        self.weekly_utilization = p.weekly_utilization;
        self.weekly_sonnet_utilization = p.weekly_sonnet_utilization;
        self.weekly_opus_utilization = p.weekly_opus_utilization;
        self.session_usage = p.buckets[0].clone();
        self.weekly_usage = p.buckets[1].clone();
        self.weekly_sonnet_usage = p.buckets[2].clone();
        self.weekly_opus_usage = p.buckets[3].clone();
        self.lifetime_usage = p.buckets[4].clone();
    }
}

impl Deref for ClewdrCookie {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Default for ClewdrCookie {
    fn default() -> Self {
        Self {
            inner: PLACEHOLDER_COOKIE.to_string(),
        }
    }
}

impl ClewdrCookie {
    pub fn ellipse(&self) -> String {
        let len = self.inner.len();
        if len > 20 {
            format!("{}...", &self.inner[..20])
        } else {
            self.inner.to_owned()
        }
    }
}

impl FromStr for ClewdrCookie {
    type Err = ClewdrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        static RE_FULL: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"sk-ant-sid\d{2}-[0-9A-Za-z_-]{86,120}-[0-9A-Za-z_-]{6}AA").unwrap()
        });
        static RE_BASE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^[0-9A-Za-z_-]{86,120}-[0-9A-Za-z_-]{6}AA$").unwrap());

        let cleaned = s
            .trim()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect::<String>();

        if let Some(found) = RE_FULL.find(&cleaned) {
            return Ok(Self {
                inner: found.as_str().to_string(),
            });
        }

        if RE_BASE.is_match(&cleaned) {
            return Ok(Self { inner: cleaned });
        }

        Err(ClewdrError::ParseCookieError {
            loc: Location::generate(),
            msg: "Invalid cookie format",
        })
    }
}

impl Display for ClewdrCookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sessionKey={}", self.inner)
    }
}

impl Debug for ClewdrCookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_base_cookie_with_len(prefix_len: usize) -> String {
        format!("{}-{}AA", "a".repeat(prefix_len), "b".repeat(6))
    }

    #[test]
    fn test_sk_cookie_from_str() {
        let base = make_base_cookie_with_len(86);
        let full = format!("sk-ant-sid01-{base}");
        let cookie = ClewdrCookie::from_str(&full).unwrap();
        assert_eq!(cookie.inner, full);
    }

    #[test]
    fn test_cookie_from_str() {
        let base = make_base_cookie_with_len(86);
        let cookie = ClewdrCookie::from_str(&base).unwrap();
        assert_eq!(cookie.inner, base);
    }

    #[test]
    fn test_long_cookie_from_str() {
        let base = make_base_cookie_with_len(109);
        let full = format!("sk-ant-sid02-{base}");
        let cookie = ClewdrCookie::from_str(&full).unwrap();
        assert_eq!(cookie.inner, full);
    }

    #[test]
    fn test_invalid_cookie() {
        let result = ClewdrCookie::from_str("invalid-cookie");
        assert!(result.is_err());
    }

    #[test]
    fn auth_method_default_is_cookie() {
        // Pre-Step-4 snapshots and any code path that constructs an
        // AccountSlot without explicitly setting auth_method must land on
        // Cookie — flipping this default would silently re-classify
        // existing data as OAuth on first reload.
        assert_eq!(AuthMethod::default(), AuthMethod::Cookie);
        let slot = AccountSlot::default();
        assert_eq!(slot.auth_method, AuthMethod::Cookie);
    }

    #[test]
    fn auth_method_from_auth_source_strings() {
        assert_eq!(AuthMethod::from_auth_source("cookie"), AuthMethod::Cookie);
        assert_eq!(AuthMethod::from_auth_source("oauth"), AuthMethod::OAuth);
        // Unknown / legacy "hybrid" / empty all fall back to Cookie so a
        // mis-typed DB value can't accidentally route a slot through the
        // OAuth send-path.
        assert_eq!(AuthMethod::from_auth_source(""), AuthMethod::Cookie);
        assert_eq!(AuthMethod::from_auth_source("hybrid"), AuthMethod::Cookie);
        assert_eq!(AuthMethod::from_auth_source("OAuth"), AuthMethod::Cookie);
    }

    #[test]
    fn auth_method_serde_lowercase() {
        // Wire format must match the persisted auth_source column to keep
        // any future cross-process snapshot exchange clean.
        let cookie_json = serde_json::to_string(&AuthMethod::Cookie).unwrap();
        let oauth_json = serde_json::to_string(&AuthMethod::OAuth).unwrap();
        assert_eq!(cookie_json, "\"cookie\"");
        assert_eq!(oauth_json, "\"oauth\"");
        let parsed: AuthMethod = serde_json::from_str("\"oauth\"").unwrap();
        assert_eq!(parsed, AuthMethod::OAuth);
    }

    #[test]
    fn account_slot_new_defaults_auth_method_to_cookie() {
        // Cookie accounts that go through `exchange_token` later hold a
        // bearer token (slot.token = Some(_)). auth_method must NOT be
        // derived from token presence — once Cookie, always Cookie until
        // a reload from a row with auth_source="oauth" overwrites it.
        let base = make_base_cookie_with_len(86);
        let full = format!("sk-ant-sid01-{base}");
        let slot = AccountSlot::new(&full, None).unwrap();
        assert_eq!(slot.auth_method, AuthMethod::Cookie);
    }

    /// Step 4 / C7 introduces `credential_label()` as the log/tracing
    /// substitute for `slot.cookie.ellipse()`. Cookie accounts get the
    /// same ellipsed cookie blob as before (call sites are wire-compat).
    /// OAuth accounts get an `oauth#{account_id}` tag instead of the
    /// placeholder cookie blob, so logs no longer pretend they have a
    /// session cookie. Slots without an account_id (test fixtures, edge
    /// case) fall back to `oauth#?`.
    #[test]
    fn credential_label_dispatches_by_auth_method() {
        let base = make_base_cookie_with_len(86);
        let cookie_blob = format!("sk-ant-sid01-{base}");

        // Cookie account: label is the ellipsed cookie blob.
        let cookie_slot = AccountSlot::new(&cookie_blob, None).unwrap();
        let cookie_label = cookie_slot.credential_label();
        assert!(
            cookie_label.starts_with("sk-ant-sid01-"),
            "cookie label should preserve the ellipsed cookie shape, got: {cookie_label}"
        );

        // OAuth account with id: label is the per-account tag.
        let oauth_slot = AccountSlot {
            auth_method: AuthMethod::OAuth,
            account_id: Some(42),
            ..AccountSlot::default()
        };
        assert_eq!(oauth_slot.credential_label(), "oauth#42");

        // OAuth account without id (test fixture / loader race): falls
        // back to a clear sentinel rather than panicking on the unwrap
        // future C8 callers might be tempted to do.
        let oauth_no_id = AccountSlot {
            auth_method: AuthMethod::OAuth,
            account_id: None,
            ..AccountSlot::default()
        };
        assert_eq!(oauth_no_id.credential_label(), "oauth#?");
    }
}
