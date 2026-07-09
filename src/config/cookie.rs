use std::{
    collections::BTreeMap,
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
    config::{MimicryMode, PLACEHOLDER_COOKIE, ThirdPartyMimicryConfig, TokenInfo},
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
/// Step 4 introduced this as the canonical kind discriminator for an
/// `AccountSlot`. Step 5 adds `ApiKey` for pay-as-you-go Anthropic-compatible
/// endpoints (official `api.anthropic.com` or custom endpoints such as AWS).
/// Loader fills this from the DB column `accounts.auth_source`.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    #[default]
    Cookie,
    #[serde(rename = "oauth")]
    OAuth,
    ApiKey,
}

impl AuthMethod {
    /// Map the persisted `accounts.auth_source` string to a typed kind.
    /// Unknown values fall back to `Cookie` (defensive — the column CHECK
    /// constrains it to `cookie | oauth | api_key`).
    pub fn from_auth_source(s: &str) -> Self {
        match s {
            "oauth" => AuthMethod::OAuth,
            "api_key" => AuthMethod::ApiKey,
            _ => AuthMethod::Cookie,
        }
    }
}

/// Secret string with a masked `Debug` impl. Used for the API key value
/// inside `AccountSlot` so accidental log / tracing output cannot leak
/// the credential. Deserialize is `transparent` so DB-loaded TEXT rows
/// land here directly; the type intentionally does **not** derive
/// `Serialize` — the only `AccountSlot` field that holds one carries
/// `#[serde(skip_serializing)]`, and other call sites should not be
/// emitting secret bytes either.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct ApiKeySecret(String);

impl Debug for ApiKeySecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            f.write_str("ApiKeySecret(<empty>)")
        } else {
            f.write_str("ApiKeySecret(***)")
        }
    }
}

impl ApiKeySecret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Per-account extra HTTP headers for API-key accounts (e.g.
/// `anthropic-workspace-id`). Header **keys** are debug-visible (they
/// are useful for diagnostics and not sensitive on their own); header
/// **values** are masked in `Debug` output and skipped by the
/// `AccountSlot` `Serialize` path because they may contain secrets per
/// PRD §Security. Deserialize is transparent so the DB JSON column
/// loads directly into this map.
#[derive(Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct ApiKeyExtraHeaders(BTreeMap<String, String>);

impl Debug for ApiKeyExtraHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(self.0.keys().map(|k| (k, "***")))
            .finish()
    }
}

impl ApiKeyExtraHeaders {
    pub fn new(m: BTreeMap<String, String>) -> Self {
        Self(m)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.0
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

    /// Generalized replacement for the fixed opus/sonnet weekly buckets
    /// above: Anthropic now reports per-model weekly restrictions as
    /// `kind: "weekly_scoped"` entries in `usage.limits[]`, scoped to an
    /// arbitrary model name (not just Opus/Sonnet). Populated from the
    /// same probe response; entries matching "opus"/"sonnet" also
    /// backfill the fixed fields above for backward compatibility.
    #[serde(default)]
    pub weekly_scoped_limits: Vec<ScopedWeeklyLimit>,

    // ---------- ApiKey credential fields (Step 5) ----------
    // Populated iff `auth_method == ApiKey`. `base_url` is the upstream
    // origin (e.g. `https://api.anthropic.com/`, already normalized by
    // `normalize_api_key_base_url` at insert / acquire time); `secret`
    // is the literal API key; `extra_headers` is a per-account KV map
    // merged into outbound requests (values are secrets).
    //
    // Both secret-bearing fields carry `#[serde(skip_serializing)]` so
    // any future caller that serializes an `AccountSlot` (status dumps,
    // snapshots, etc.) cannot accidentally leak the credential. The
    // wrappers' `Debug` impls mask values, so derived `Debug` on
    // `AccountSlot` is safe for tracing.
    //
    // All three carry `#[serde(default)]` so deserializing pre-Step-5
    // snapshots (no api_key columns) does not fail.
    #[serde(default)]
    pub api_key_base_url: Option<String>,
    #[serde(default, skip_serializing)]
    pub api_key_secret: Option<ApiKeySecret>,
    #[serde(default, skip_serializing)]
    pub api_key_extra_headers: Option<ApiKeyExtraHeaders>,
    // Optional JSON object shallow-merged over the outbound request body just
    // before send (api_key channels, `/v1/messages` only). Not secret-bearing
    // (values are routing params like `models: [...]`), so it is serialized
    // normally; `#[serde(default)]` keeps pre-existing snapshots deserializing.
    #[serde(default)]
    pub api_key_extra_body: Option<serde_json::Value>,
    // Two-tier mimicry. `mimicry_mode` is always `None` for cookie/oauth slots
    // (CHECK-enforced); on API-key channels it selects the clean passthrough
    // (`None`) or the third-party relay cloak (`ThirdParty`). `mimicry_config`
    // carries the per-channel cloak knobs and is `Some` only for a `ThirdParty`
    // slot. Both `#[serde(default)]` so pre-existing snapshots deserialize.
    #[serde(default)]
    pub mimicry_mode: MimicryMode,
    #[serde(default)]
    pub mimicry_config: Option<ThirdPartyMimicryConfig>,
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
            weekly_scoped_limits: Vec::new(),
            api_key_base_url: None,
            api_key_secret: None,
            api_key_extra_headers: None,
            api_key_extra_body: None,
            mimicry_mode: MimicryMode::None,
            mimicry_config: None,
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

    /// Construct an ApiKey slot directly from `(account_id, base_url,
    /// secret, extra_headers)`. Used by the loader (`do_reload`) for
    /// `auth_source = 'api_key'` rows. `base_url` is expected to be
    /// already normalized via `normalize_api_key_base_url`; this
    /// constructor does not re-validate.
    pub fn api_key(
        account_id: i64,
        base_url: String,
        secret: ApiKeySecret,
        extra_headers: Option<ApiKeyExtraHeaders>,
        extra_body: Option<serde_json::Value>,
        mimicry_mode: MimicryMode,
        mimicry_config: Option<ThirdPartyMimicryConfig>,
    ) -> Self {
        Self {
            cookie: None,
            auth_method: AuthMethod::ApiKey,
            account_id: Some(account_id),
            api_key_base_url: Some(base_url),
            api_key_secret: Some(secret),
            api_key_extra_headers: extra_headers,
            api_key_extra_body: extra_body,
            mimicry_mode,
            mimicry_config,
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
            AuthMethod::ApiKey => match self.account_id {
                Some(id) => format!("apikey#{id}"),
                None => "apikey#?".to_string(),
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

/// One entry from Anthropic's `usage.limits[]` array where `kind ==
/// "weekly_scoped"`. Generalizes what used to be the fixed
/// `seven_day_opus`/`seven_day_sonnet` top-level fields (now always null) —
/// `model_display_name` can be any model Anthropic reports (e.g. "Opus",
/// "Sonnet", "Fable"), and the array may hold 0, 1, or more entries.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ScopedWeeklyLimit {
    pub model_display_name: String,
    pub resets_at: Option<i64>,
    pub utilization: Option<f64>,
}

/// Extract every `kind == "weekly_scoped"` entry from a raw usage-probe JSON
/// body's `limits[]` array (same shape for the cookie-session usage endpoint
/// and the OAuth `/api/oauth/usage` endpoint). Entries missing
/// `scope.model.display_name`, with a blank display name, or carrying
/// neither `resets_at` nor `percent` (nothing meaningful to show) are
/// skipped rather than causing a failure. Duplicate display names (after
/// trimming) are deduped, last-entry-wins — matching `scoped_legacy_backfill`'s
/// last-match-wins semantics.
pub fn parse_weekly_scoped_limits(usage_raw: &serde_json::Value) -> Vec<ScopedWeeklyLimit> {
    let Some(limits) = usage_raw.get("limits").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut result: Vec<ScopedWeeklyLimit> = Vec::new();
    for entry in limits {
        if entry.get("kind").and_then(|v| v.as_str()) != Some("weekly_scoped") {
            continue;
        }
        let Some(model_display_name) = entry
            .get("scope")
            .and_then(|v| v.get("model"))
            .and_then(|v| v.get("display_name"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
        else {
            continue;
        };
        let resets_at = entry
            .get("resets_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp());
        let utilization = entry.get("percent").and_then(|v| v.as_f64());
        if resets_at.is_none() && utilization.is_none() {
            continue;
        }
        let scoped = ScopedWeeklyLimit {
            model_display_name,
            resets_at,
            utilization,
        };
        if let Some(existing) = result.iter_mut().find(|e| {
            e.model_display_name
                .eq_ignore_ascii_case(&scoped.model_display_name)
        }) {
            *existing = scoped;
        } else {
            result.push(scoped);
        }
    }
    result
}

/// Find a scoped entry whose display name case-insensitively equals `want`
/// (e.g. "opus"/"sonnet"), used to backfill the legacy fixed fields for
/// backward compatibility. Last match wins if duplicates exist.
pub fn scoped_legacy_backfill(
    entries: &[ScopedWeeklyLimit],
    want: &str,
) -> Option<(Option<i64>, Option<f64>)> {
    entries
        .iter()
        .rfind(|e| e.model_display_name.eq_ignore_ascii_case(want))
        .map(|e| (e.resets_at, e.utilization))
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
    pub weekly_scoped_limits: Vec<ScopedWeeklyLimit>,
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
            weekly_scoped_limits: self.weekly_scoped_limits.clone(),
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
        self.weekly_scoped_limits = p.weekly_scoped_limits.clone();
    }

    /// Merge runtime fields owned by the OAuth profile/usage snapshot.
    ///
    /// OAuth snapshots report upstream reset boundaries and utilization, but
    /// they do not carry local-only counters/capability probes. Keep those
    /// fields from the current slot so a refresh/probe cannot erase locally
    /// accumulated usage.
    pub fn apply_oauth_snapshot_runtime(&mut self, p: &RuntimeStateParams) {
        self.reset_time = p.reset_time;
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
        self.weekly_scoped_limits = p.weekly_scoped_limits.clone();
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

    /// Real sample captured from the OAuth usage-probe endpoint (2026-07-09):
    /// seven_day_opus/seven_day_sonnet are always null now; the real
    /// per-model weekly restriction lives in `limits[]` as a
    /// `kind: "weekly_scoped"` entry naming an arbitrary model ("Fable"
    /// here, not Opus/Sonnet).
    const SAMPLE_USAGE_JSON: &str = r#"{
        "limits": [
            {"group": "session", "is_active": false, "kind": "session", "percent": 2,
             "resets_at": "2026-07-09T15:50:00.048020+00:00", "scope": null, "severity": "normal"},
            {"group": "weekly", "is_active": true, "kind": "weekly_all", "percent": 100,
             "resets_at": "2026-07-10T02:00:00.048043+00:00", "scope": null, "severity": "critical"},
            {"group": "weekly", "is_active": false, "kind": "weekly_scoped", "percent": 98,
             "resets_at": "2026-07-10T02:00:00.048334+00:00",
             "scope": {"model": {"display_name": "Fable", "id": null}, "surface": null},
             "severity": "critical"}
        ]
    }"#;

    #[test]
    fn parse_weekly_scoped_limits_extracts_scoped_entries_only() {
        let usage: serde_json::Value = serde_json::from_str(SAMPLE_USAGE_JSON).unwrap();
        let entries = parse_weekly_scoped_limits(&usage);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model_display_name, "Fable");
        assert_eq!(entries[0].utilization, Some(98.0));
        let expected_ts = chrono::DateTime::parse_from_rfc3339("2026-07-10T02:00:00.048334+00:00")
            .unwrap()
            .timestamp();
        assert_eq!(entries[0].resets_at, Some(expected_ts));
    }

    #[test]
    fn parse_weekly_scoped_limits_skips_entries_missing_display_name() {
        let usage: serde_json::Value = serde_json::from_str(
            r#"{"limits": [{"kind": "weekly_scoped", "percent": 50, "resets_at": null, "scope": {"model": {"id": null}}}]}"#,
        )
        .unwrap();
        assert!(parse_weekly_scoped_limits(&usage).is_empty());
    }

    #[test]
    fn parse_weekly_scoped_limits_handles_missing_limits_array() {
        let usage: serde_json::Value = serde_json::from_str(r#"{}"#).unwrap();
        assert!(parse_weekly_scoped_limits(&usage).is_empty());
    }

    #[test]
    fn scoped_legacy_backfill_matches_case_insensitively() {
        let entries = vec![ScopedWeeklyLimit {
            model_display_name: "Opus".to_string(),
            resets_at: Some(123),
            utilization: Some(50.0),
        }];
        for want in ["opus", "OPUS", "Opus"] {
            assert_eq!(
                scoped_legacy_backfill(&entries, want),
                Some((Some(123), Some(50.0)))
            );
        }
        assert_eq!(scoped_legacy_backfill(&entries, "sonnet"), None);
    }

    #[test]
    fn scoped_legacy_backfill_last_match_wins_on_duplicates() {
        let entries = vec![
            ScopedWeeklyLimit {
                model_display_name: "Sonnet".to_string(),
                resets_at: Some(1),
                utilization: Some(10.0),
            },
            ScopedWeeklyLimit {
                model_display_name: "Sonnet".to_string(),
                resets_at: Some(2),
                utilization: Some(20.0),
            },
        ];
        assert_eq!(
            scoped_legacy_backfill(&entries, "sonnet"),
            Some((Some(2), Some(20.0)))
        );
    }

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
        assert_eq!(AuthMethod::from_auth_source("api_key"), AuthMethod::ApiKey);
        // Unknown / legacy "hybrid" / empty all fall back to Cookie so a
        // mis-typed DB value can't accidentally route a slot through the
        // OAuth or ApiKey send-path.
        assert_eq!(AuthMethod::from_auth_source(""), AuthMethod::Cookie);
        assert_eq!(AuthMethod::from_auth_source("hybrid"), AuthMethod::Cookie);
        assert_eq!(AuthMethod::from_auth_source("OAuth"), AuthMethod::Cookie);
        assert_eq!(AuthMethod::from_auth_source("apikey"), AuthMethod::Cookie);
    }

    #[test]
    fn auth_method_serde_lowercase() {
        // Wire format must match the persisted auth_source column to keep
        // any future cross-process snapshot exchange clean.
        let cookie_json = serde_json::to_string(&AuthMethod::Cookie).unwrap();
        let oauth_json = serde_json::to_string(&AuthMethod::OAuth).unwrap();
        let apikey_json = serde_json::to_string(&AuthMethod::ApiKey).unwrap();
        assert_eq!(cookie_json, "\"cookie\"");
        assert_eq!(oauth_json, "\"oauth\"");
        assert_eq!(apikey_json, "\"api_key\"");
        let parsed: AuthMethod = serde_json::from_str("\"oauth\"").unwrap();
        assert_eq!(parsed, AuthMethod::OAuth);
        let parsed: AuthMethod = serde_json::from_str("\"api_key\"").unwrap();
        assert_eq!(parsed, AuthMethod::ApiKey);
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

    /// Step 5: API-key slot built via `AccountSlot::api_key`. Label is
    /// `apikey#{id}`, no cookie/oauth fields populated.
    #[test]
    fn api_key_slot_constructor_and_label() {
        let slot = AccountSlot::api_key(
            7,
            "https://api.anthropic.com/".to_string(),
            ApiKeySecret::new("sk-ant-test-xyz"),
            None,
            None,
            MimicryMode::None,
            None,
        );
        assert_eq!(slot.auth_method, AuthMethod::ApiKey);
        assert_eq!(slot.account_id, Some(7));
        assert_eq!(
            slot.api_key_base_url.as_deref(),
            Some("https://api.anthropic.com/")
        );
        assert_eq!(
            slot.api_key_secret.as_ref().map(|s| s.as_str()),
            Some("sk-ant-test-xyz")
        );
        assert!(slot.api_key_extra_headers.is_none());
        assert!(slot.cookie.is_none());
        assert!(slot.token.is_none());
        assert_eq!(slot.credential_label(), "apikey#7");

        let no_id = AccountSlot {
            auth_method: AuthMethod::ApiKey,
            account_id: None,
            ..AccountSlot::default()
        };
        assert_eq!(no_id.credential_label(), "apikey#?");
    }

    /// `ApiKeySecret`'s `Debug` impl must NEVER leak the inner string.
    /// This is the last line of defense if a tracing span / error path
    /// drops a `?slot` or `{:?}` formatter onto an api-key slot.
    #[test]
    fn api_key_secret_debug_is_masked() {
        let s = ApiKeySecret::new("sk-ant-supersecret-1234567890");
        let dbg = format!("{:?}", s);
        assert_eq!(dbg, "ApiKeySecret(***)");
        assert!(!dbg.contains("sk-ant"));
        assert!(!dbg.contains("supersecret"));

        let empty = ApiKeySecret::new("");
        assert_eq!(format!("{:?}", empty), "ApiKeySecret(<empty>)");
    }

    /// Extra-header values are secrets per PRD §Security. Keys remain
    /// visible (diagnostically useful, not sensitive on their own).
    #[test]
    fn api_key_extra_headers_debug_masks_values() {
        let mut map = BTreeMap::new();
        map.insert(
            "anthropic-workspace-id".to_string(),
            "wrkspc_supersecret_xyz".to_string(),
        );
        map.insert(
            "x-custom-token".to_string(),
            "another_secret_value".to_string(),
        );
        let h = ApiKeyExtraHeaders::new(map);
        let dbg = format!("{:?}", h);
        // Keys present
        assert!(dbg.contains("anthropic-workspace-id"));
        assert!(dbg.contains("x-custom-token"));
        // Values masked
        assert!(!dbg.contains("wrkspc_supersecret_xyz"));
        assert!(!dbg.contains("another_secret_value"));
        assert!(dbg.contains("***"));
    }

    /// `AccountSlot::Serialize` must NEVER emit `api_key_secret` or
    /// `api_key_extra_headers`. Both are explicitly marked
    /// `#[serde(skip_serializing)]` and this test pins that behavior.
    #[test]
    fn account_slot_serialize_omits_api_key_secrets() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "anthropic-workspace-id".to_string(),
            "wrkspc_should_not_appear".to_string(),
        );
        let slot = AccountSlot::api_key(
            3,
            "https://api.anthropic.com/".to_string(),
            ApiKeySecret::new("sk-ant-should-not-appear"),
            Some(ApiKeyExtraHeaders::new(headers)),
            None,
            MimicryMode::None,
            None,
        );
        let json = serde_json::to_string(&slot).expect("AccountSlot serializes");
        // base_url is fine to expose (it's not secret) — make sure the
        // serialization still emits the non-secret api_key field so the
        // skip is targeted, not blanket.
        assert!(json.contains("api_key_base_url"));
        assert!(json.contains("https://api.anthropic.com/"));
        // Secrets must NOT appear.
        assert!(!json.contains("sk-ant-should-not-appear"));
        assert!(!json.contains("wrkspc_should_not_appear"));
        assert!(!json.contains("api_key_secret"));
        assert!(!json.contains("api_key_extra_headers"));
    }

    /// Round-trip a pre-Step-5 snapshot (no api_key_* fields in JSON):
    /// `#[serde(default)]` must yield `None` for all three new fields.
    #[test]
    fn account_slot_deserialize_tolerates_missing_api_key_fields() {
        let json = r#"{
            "cookie": null,
            "auth_method": "oauth",
            "account_id": 11
        }"#;
        let slot: AccountSlot = serde_json::from_str(json).expect("legacy snapshot");
        assert_eq!(slot.auth_method, AuthMethod::OAuth);
        assert!(slot.api_key_base_url.is_none());
        assert!(slot.api_key_secret.is_none());
        assert!(slot.api_key_extra_headers.is_none());
    }
}
