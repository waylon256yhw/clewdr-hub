//! Account-failure classifier (Step 3.5).
//!
//! Splits scheduler decision (`AccountFailureAction`) from outward
//! display (`AccountNormalizedReason`) so every entry point —
//! `messages` / `count_tokens` / `test` / `probe_cookie` /
//! `probe_oauth` / OAuth `refresh` / cookie `bootstrap` — produces
//! the same scheduling action and the same set of stable
//! display-side reason names for the same upstream signal.
//!
//! The legacy [`Reason`] enum stays as the DB persistence + pool
//! identity carrier; [`AccountNormalizedReason::to_reason`] bridges
//! to it for the persistence path. The legacy
//! `ClewdrError::InvalidCookie { reason }` variant also stays — its
//! producers have not been retired in this round, only its consumers
//! route through the classifier.
//!
//! See `docs/account-normalization-2026-04-21.md` §Step 3.5 and
//! `docs/error-handling-notes-2026-04-20.md`.

use serde::{Deserialize, Serialize};

use crate::config::Reason;
use crate::error::ClewdrError;

/// Scheduler-side action for an account on a single failure event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AccountFailureAction {
    /// Authentication is rejected. Move out of rotation, mark `auth_error`.
    TerminalAuth,
    /// Account / organization is disabled. Move out of rotation, mark `disabled`.
    TerminalDisabled,
    /// Account is healthy but the window is exhausted. Park until `reset_time`.
    Cooldown { reset_time: i64 },
    /// Upstream / network glitch. Do not change the account's terminal state.
    TransientUpstream,
    /// Local logic error. Do not change the account's terminal state.
    InternalError,
}

/// Display-side stable name for the failure cause.
///
/// One-to-one with the `type` string emitted on the API response, and
/// one-to-one with the bridging [`Reason`] used for DB persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum AccountNormalizedReason {
    /// OAuth refresh exchange rejected the refresh token.
    OauthRefreshInvalid,
    /// Anthropic returned 401/403 with the
    /// "oauth authentication is currently not allowed for this organization"
    /// phrase.
    OauthOrgAuthNotAllowed,
    /// OAuth profile/bootstrap fetch returned an invalid identity.
    OauthProfileRejected,
    /// OAuth usage probe rejected the token.
    OauthUsageRejected,
    /// Cookie bootstrap returned an account with no memberships.
    CookieMembershipMissing,
    /// Cookie bootstrap returned an account whose memberships have no
    /// chat capability.
    CookieNoChatCapability,
    /// Anthropic flagged the organization as disabled (HTTP 400 +
    /// "organization has been disabled" phrase).
    OrganizationDisabled,
    /// Account is banned (legacy `Reason::Banned` carrier).
    AccountBanned,
    /// Account is on a free tier and not eligible for dispatch.
    FreeTier,
    /// HTTP 429 with a known reset timestamp.
    RateLimited { reset_time: i64 },
    /// Server-side restriction with a known reset timestamp.
    Restricted { reset_time: i64 },
    /// Generic upstream auth rejection (401/403 without a more specific
    /// phrase). Different from `OauthOrgAuthNotAllowed`, which is the
    /// phrase-matched subset.
    UpstreamAuthRejected,
    /// Other upstream HTTP error (5xx, unmatched 4xx). Treated as
    /// transient at the scheduler level.
    UpstreamHttp { status: u16 },
    /// Network / transport / EventSource glitch.
    UpstreamTransient,
    /// Local logic / parsing / DB error.
    InternalError,
}

/// Which entry point produced the failure event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureSource {
    Messages,
    CountTokens,
    Test,
    ProbeCookie,
    ProbeOauth,
    OauthRefresh,
    Bootstrap,
}

impl FailureSource {
    /// Stable snake_case name. Same shape as the serde-renamed form,
    /// but available without going through `serde_json` — used by
    /// `IntoResponse` to populate `ClaudeErrorBody.failure_source` and
    /// by Persisted DTO conversion.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::CountTokens => "count_tokens",
            Self::Test => "test",
            Self::ProbeCookie => "probe_cookie",
            Self::ProbeOauth => "probe_oauth",
            Self::OauthRefresh => "oauth_refresh",
            Self::Bootstrap => "bootstrap",
        }
    }
}

/// Full classification of a single failure event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountFailureContext {
    pub action: AccountFailureAction,
    pub normalized_reason: AccountNormalizedReason,
    pub source: FailureSource,
    pub stage: Option<&'static str>,
    pub upstream_http_status: Option<u16>,
    pub raw_message: String,
}

impl AccountFailureAction {
    /// Map to the `request_logs.status` enum value used by
    /// `persist_terminal_request_log` / `persist_probe_log`. C3a
    /// adopters call this so every entry point (messages /
    /// count_tokens / test / probe / refresh) produces the same
    /// `status` for the same scheduling action.
    pub fn to_log_status(self) -> &'static str {
        match self {
            Self::TerminalAuth | Self::TerminalDisabled => "auth_rejected",
            Self::Cooldown { .. } => "no_account_available",
            Self::TransientUpstream => "upstream_error",
            Self::InternalError => "internal_error",
        }
    }
}

impl AccountNormalizedReason {
    /// Stable snake_case name used as the API response `type` field.
    pub fn as_type_str(&self) -> &'static str {
        match self {
            Self::OauthRefreshInvalid => "oauth_refresh_invalid",
            Self::OauthOrgAuthNotAllowed => "oauth_org_auth_not_allowed",
            Self::OauthProfileRejected => "oauth_profile_rejected",
            Self::OauthUsageRejected => "oauth_usage_rejected",
            Self::CookieMembershipMissing => "cookie_membership_missing",
            Self::CookieNoChatCapability => "cookie_no_chat_capability",
            Self::OrganizationDisabled => "organization_disabled",
            Self::AccountBanned => "account_banned",
            Self::FreeTier => "free_tier",
            Self::RateLimited { .. } => "rate_limited",
            Self::Restricted { .. } => "restricted",
            Self::UpstreamAuthRejected => "upstream_auth_rejected",
            Self::UpstreamHttp { .. } => "upstream_http_error",
            Self::UpstreamTransient => "upstream_transient",
            Self::InternalError => "internal_error",
        }
    }

    /// Bridge to the legacy [`Reason`] enum for DB persistence.
    ///
    /// Returns `None` for transient / internal classes — the caller
    /// must not persist those as `invalid_reason`. Only call this
    /// after observing `action == TerminalAuth | TerminalDisabled |
    /// Cooldown`.
    pub fn to_reason(&self) -> Option<Reason> {
        Some(match self {
            Self::OauthRefreshInvalid
            | Self::OauthOrgAuthNotAllowed
            | Self::OauthProfileRejected
            | Self::OauthUsageRejected
            | Self::CookieMembershipMissing
            | Self::CookieNoChatCapability
            | Self::UpstreamAuthRejected => Reason::Null,
            Self::OrganizationDisabled => Reason::Disabled,
            Self::AccountBanned => Reason::Banned,
            Self::FreeTier => Reason::Free,
            Self::RateLimited { reset_time } => Reason::TooManyRequest(*reset_time),
            Self::Restricted { reset_time } => Reason::Restricted(*reset_time),
            Self::UpstreamHttp { .. } | Self::UpstreamTransient | Self::InternalError => {
                return None;
            }
        })
    }
}

/// Refine `Reason::Null` into a more specific normalized reason based
/// on the source / stage context the caller passed in.
fn refine_null_reason(
    source: FailureSource,
    stage: Option<&'static str>,
) -> AccountNormalizedReason {
    match (source, stage) {
        (FailureSource::OauthRefresh, _) => AccountNormalizedReason::OauthRefreshInvalid,
        (FailureSource::ProbeOauth, Some("refresh")) => {
            AccountNormalizedReason::OauthRefreshInvalid
        }
        (FailureSource::ProbeOauth, Some("profile" | "bootstrap")) => {
            AccountNormalizedReason::OauthProfileRejected
        }
        (FailureSource::ProbeOauth, Some("usage")) => AccountNormalizedReason::OauthUsageRejected,
        (FailureSource::Bootstrap | FailureSource::ProbeCookie, Some("memberships")) => {
            AccountNormalizedReason::CookieMembershipMissing
        }
        (FailureSource::Bootstrap | FailureSource::ProbeCookie, Some("chat_capability")) => {
            AccountNormalizedReason::CookieNoChatCapability
        }
        // Catch-all: 401/403 + "oauth not allowed" phrase has already
        // collapsed into `Reason::Null` upstream, and most other
        // `Reason::Null` sites are bootstrap-ish identity rejections.
        _ => AccountNormalizedReason::OauthOrgAuthNotAllowed,
    }
}

/// Extract the HTTP status code from a wrapped `Whatever` message of the
/// form "...status NNN...". Used to keep upstream HTTP status visible
/// when an OAuth token-endpoint failure is surfaced via
/// `ClewdrError::Whatever { message: "OAuth token request failed with
/// status NNN: ..." }` (see `src/oauth.rs::send_oauth_token_request`).
fn extract_status_code(lowercase_msg: &str) -> Option<u16> {
    let idx = lowercase_msg.find("status ")?;
    let after = &lowercase_msg[idx + "status ".len()..];
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    // Require exactly 3 digits in the [100, 599] range so we don't latch
    // onto unrelated numbers that happen to follow the word "status".
    if digits.len() != 3 {
        return None;
    }
    let status = digits.parse::<u16>().ok()?;
    (100..600).contains(&status).then_some(status)
}

/// Classify a `ClewdrError::Whatever` message body into the action +
/// normalized reason + upstream HTTP status the wrapped failure
/// represents.
///
/// Returns `None` when the message has no recognisable signal — the
/// caller treats that as `InternalError`.
fn classify_whatever_message(
    message: &str,
) -> Option<(AccountFailureAction, AccountNormalizedReason, Option<u16>)> {
    let msg = message.to_ascii_lowercase();
    let refresh_token_invalid = msg.contains("invalid_grant")
        || msg.contains("refresh token not found")
        || (msg.contains("refresh token") && (msg.contains("invalid") || msg.contains("expired")))
        || (msg.contains("access token") && (msg.contains("expired") || msg.contains("invalid")));
    if refresh_token_invalid {
        return Some((
            AccountFailureAction::TerminalAuth,
            AccountNormalizedReason::OauthRefreshInvalid,
            None,
        ));
    }
    // Preserve upstream HTTP status from "...status NNN..." messages —
    // OAuth token-endpoint outages (5xx, transient gateway errors)
    // would otherwise fall through to InternalError, hiding the
    // upstream from /test, refresh, and probe_oauth diagnostics.
    if let Some(status) = extract_status_code(&msg) {
        return Some(match status {
            401 | 403 => (
                AccountFailureAction::TerminalAuth,
                AccountNormalizedReason::UpstreamAuthRejected,
                Some(status),
            ),
            _ => (
                AccountFailureAction::TransientUpstream,
                AccountNormalizedReason::UpstreamHttp { status },
                Some(status),
            ),
        });
    }
    None
}

/// Classify a single account-related failure event into a
/// scheduler action + display-side normalized reason + diagnostic
/// context.
pub fn classify_account_failure(
    err: &ClewdrError,
    source: FailureSource,
    stage: Option<&'static str>,
) -> AccountFailureContext {
    let raw_message = err.to_string();

    let (action, normalized_reason, upstream_http_status) = match err {
        ClewdrError::InvalidCookie { reason } => match reason {
            Reason::Free => (
                AccountFailureAction::TerminalDisabled,
                AccountNormalizedReason::FreeTier,
                None,
            ),
            Reason::Disabled => (
                AccountFailureAction::TerminalDisabled,
                AccountNormalizedReason::OrganizationDisabled,
                Some(400),
            ),
            Reason::Banned => (
                AccountFailureAction::TerminalAuth,
                AccountNormalizedReason::AccountBanned,
                None,
            ),
            Reason::Null => (
                AccountFailureAction::TerminalAuth,
                refine_null_reason(source, stage),
                None,
            ),
            Reason::TooManyRequest(ts) => (
                AccountFailureAction::Cooldown { reset_time: *ts },
                AccountNormalizedReason::RateLimited { reset_time: *ts },
                Some(429),
            ),
            Reason::Restricted(ts) => (
                AccountFailureAction::Cooldown { reset_time: *ts },
                AccountNormalizedReason::Restricted { reset_time: *ts },
                None,
            ),
        },

        // Phrase-matched 400 "organization has been disabled" that bypassed
        // `check_claude` (defensive — `check_claude` already collapses this
        // into `InvalidCookie + Reason::Disabled`).
        ClewdrError::ClaudeHttpError { code, inner } if code.as_u16() == 400 => {
            let phrase = inner
                .message
                .as_str()
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_else(|| inner.message.to_string().to_ascii_lowercase());
            if phrase.contains("organization has been disabled") {
                (
                    AccountFailureAction::TerminalDisabled,
                    AccountNormalizedReason::OrganizationDisabled,
                    Some(400),
                )
            } else {
                (
                    AccountFailureAction::TransientUpstream,
                    AccountNormalizedReason::UpstreamHttp { status: 400 },
                    Some(400),
                )
            }
        }

        // Bare 401/403 not phrase-matched by `check_claude` — `chat.rs`
        // historically treats these as oauth auth failures.
        ClewdrError::ClaudeHttpError { code, .. } if matches!(code.as_u16(), 401 | 403) => {
            let status = code.as_u16();
            (
                AccountFailureAction::TerminalAuth,
                AccountNormalizedReason::UpstreamAuthRejected,
                Some(status),
            )
        }

        ClewdrError::ClaudeHttpError { code, .. } => {
            let status = code.as_u16();
            (
                AccountFailureAction::TransientUpstream,
                AccountNormalizedReason::UpstreamHttp { status },
                Some(status),
            )
        }

        // OAuth refresh / token rotation surfaces here. The token endpoint
        // wraps non-2xx responses as `Whatever { message: "OAuth token
        // request failed with status NNN: ..." }`, so 5xx / 4xx outages
        // must surface as TransientUpstream + UpstreamHttp{status} (not
        // InternalError) to keep diagnostics accurate.
        ClewdrError::Whatever { message, .. } => classify_whatever_message(message).unwrap_or((
            AccountFailureAction::InternalError,
            AccountNormalizedReason::InternalError,
            None,
        )),

        ClewdrError::RequestTokenError { .. } => (
            AccountFailureAction::TerminalAuth,
            AccountNormalizedReason::OauthRefreshInvalid,
            None,
        ),

        ClewdrError::WreqError { .. }
        | ClewdrError::EventSourceAxumError { .. }
        | ClewdrError::EventSourceRquestError { .. }
        | ClewdrError::HttpError { .. } => (
            AccountFailureAction::TransientUpstream,
            AccountNormalizedReason::UpstreamTransient,
            None,
        ),

        _ => (
            AccountFailureAction::InternalError,
            AccountNormalizedReason::InternalError,
            None,
        ),
    };

    AccountFailureContext {
        action,
        normalized_reason,
        source,
        stage,
        upstream_http_status,
        raw_message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ClaudeErrorBody;
    use serde_json::json;
    use wreq::StatusCode;

    fn classify(err: ClewdrError, source: FailureSource) -> AccountFailureContext {
        classify_account_failure(&err, source, None)
    }

    fn classify_stage(
        err: ClewdrError,
        source: FailureSource,
        stage: &'static str,
    ) -> AccountFailureContext {
        classify_account_failure(&err, source, Some(stage))
    }

    fn http_error(status: u16, message: &str) -> ClewdrError {
        ClewdrError::ClaudeHttpError {
            code: StatusCode::from_u16(status).unwrap(),
            inner: Box::new(ClaudeErrorBody {
                message: json!(message.to_string()),
                r#type: "error".to_string(),
                code: Some(status),
                ..Default::default()
            }),
        }
    }

    // ---- InvalidCookie paths ----

    #[test]
    fn invalid_cookie_free_is_terminal_disabled_free_tier() {
        let ctx = classify(
            ClewdrError::InvalidCookie {
                reason: Reason::Free,
            },
            FailureSource::Messages,
        );
        assert_eq!(ctx.action, AccountFailureAction::TerminalDisabled);
        assert_eq!(ctx.normalized_reason, AccountNormalizedReason::FreeTier);
        assert_eq!(ctx.normalized_reason.as_type_str(), "free_tier");
        assert_eq!(ctx.normalized_reason.to_reason(), Some(Reason::Free));
    }

    #[test]
    fn invalid_cookie_disabled_is_terminal_disabled_org_disabled() {
        let ctx = classify(
            ClewdrError::InvalidCookie {
                reason: Reason::Disabled,
            },
            FailureSource::Messages,
        );
        assert_eq!(ctx.action, AccountFailureAction::TerminalDisabled);
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::OrganizationDisabled
        );
        assert_eq!(ctx.upstream_http_status, Some(400));
        assert_eq!(ctx.normalized_reason.to_reason(), Some(Reason::Disabled));
    }

    #[test]
    fn invalid_cookie_banned_is_terminal_auth_account_banned() {
        let ctx = classify(
            ClewdrError::InvalidCookie {
                reason: Reason::Banned,
            },
            FailureSource::Messages,
        );
        assert_eq!(ctx.action, AccountFailureAction::TerminalAuth);
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::AccountBanned
        );
        assert_eq!(ctx.normalized_reason.to_reason(), Some(Reason::Banned));
    }

    #[test]
    fn invalid_cookie_too_many_request_is_cooldown_rate_limited() {
        let ts = 1_700_000_000_i64;
        let ctx = classify(
            ClewdrError::InvalidCookie {
                reason: Reason::TooManyRequest(ts),
            },
            FailureSource::Messages,
        );
        assert_eq!(
            ctx.action,
            AccountFailureAction::Cooldown { reset_time: ts }
        );
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::RateLimited { reset_time: ts }
        );
        assert_eq!(ctx.normalized_reason.as_type_str(), "rate_limited");
        assert_eq!(
            ctx.normalized_reason.to_reason(),
            Some(Reason::TooManyRequest(ts))
        );
        assert_eq!(ctx.upstream_http_status, Some(429));
    }

    #[test]
    fn invalid_cookie_restricted_is_cooldown_restricted() {
        let ts = 1_800_000_000_i64;
        let ctx = classify(
            ClewdrError::InvalidCookie {
                reason: Reason::Restricted(ts),
            },
            FailureSource::Messages,
        );
        assert_eq!(
            ctx.action,
            AccountFailureAction::Cooldown { reset_time: ts }
        );
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::Restricted { reset_time: ts }
        );
        assert_eq!(
            ctx.normalized_reason.to_reason(),
            Some(Reason::Restricted(ts))
        );
    }

    // ---- Reason::Null source/stage refinement ----

    #[test]
    fn null_with_oauth_refresh_source_refines_to_refresh_invalid() {
        let ctx = classify(
            ClewdrError::InvalidCookie {
                reason: Reason::Null,
            },
            FailureSource::OauthRefresh,
        );
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::OauthRefreshInvalid
        );
        assert_eq!(ctx.action, AccountFailureAction::TerminalAuth);
    }

    #[test]
    fn null_with_probe_oauth_refresh_stage_refines_to_refresh_invalid() {
        let ctx = classify_stage(
            ClewdrError::InvalidCookie {
                reason: Reason::Null,
            },
            FailureSource::ProbeOauth,
            "refresh",
        );
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::OauthRefreshInvalid
        );
    }

    #[test]
    fn null_with_probe_oauth_profile_stage_refines_to_profile_rejected() {
        let ctx = classify_stage(
            ClewdrError::InvalidCookie {
                reason: Reason::Null,
            },
            FailureSource::ProbeOauth,
            "profile",
        );
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::OauthProfileRejected
        );
    }

    #[test]
    fn null_with_probe_oauth_usage_stage_refines_to_usage_rejected() {
        let ctx = classify_stage(
            ClewdrError::InvalidCookie {
                reason: Reason::Null,
            },
            FailureSource::ProbeOauth,
            "usage",
        );
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::OauthUsageRejected
        );
    }

    #[test]
    fn null_with_bootstrap_memberships_stage_refines_to_membership_missing() {
        let ctx = classify_stage(
            ClewdrError::InvalidCookie {
                reason: Reason::Null,
            },
            FailureSource::Bootstrap,
            "memberships",
        );
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::CookieMembershipMissing
        );
    }

    #[test]
    fn null_with_bootstrap_chat_capability_stage_refines_to_no_chat_capability() {
        let ctx = classify_stage(
            ClewdrError::InvalidCookie {
                reason: Reason::Null,
            },
            FailureSource::Bootstrap,
            "chat_capability",
        );
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::CookieNoChatCapability
        );
    }

    #[test]
    fn null_with_messages_source_falls_back_to_oauth_org_not_allowed() {
        let ctx = classify(
            ClewdrError::InvalidCookie {
                reason: Reason::Null,
            },
            FailureSource::Messages,
        );
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::OauthOrgAuthNotAllowed
        );
    }

    // ---- ClaudeHttpError paths ----

    #[test]
    fn claude_http_401_is_terminal_auth_upstream_auth_rejected() {
        let ctx = classify(http_error(401, "unauthorized"), FailureSource::Messages);
        assert_eq!(ctx.action, AccountFailureAction::TerminalAuth);
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::UpstreamAuthRejected
        );
        assert_eq!(ctx.upstream_http_status, Some(401));
    }

    #[test]
    fn claude_http_403_is_terminal_auth_upstream_auth_rejected() {
        let ctx = classify(http_error(403, "forbidden"), FailureSource::Messages);
        assert_eq!(ctx.action, AccountFailureAction::TerminalAuth);
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::UpstreamAuthRejected
        );
    }

    #[test]
    fn claude_http_400_org_disabled_phrase_is_terminal_disabled() {
        let ctx = classify(
            http_error(400, "this organization has been disabled"),
            FailureSource::Messages,
        );
        assert_eq!(ctx.action, AccountFailureAction::TerminalDisabled);
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::OrganizationDisabled
        );
    }

    #[test]
    fn claude_http_400_other_is_transient() {
        let ctx = classify(http_error(400, "bad request"), FailureSource::Messages);
        assert_eq!(ctx.action, AccountFailureAction::TransientUpstream);
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::UpstreamHttp { status: 400 }
        );
    }

    #[test]
    fn claude_http_500_is_transient_upstream_http() {
        let ctx = classify(http_error(500, "internal error"), FailureSource::Messages);
        assert_eq!(ctx.action, AccountFailureAction::TransientUpstream);
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::UpstreamHttp { status: 500 }
        );
        // 5xx must not persist a Reason.
        assert_eq!(ctx.normalized_reason.to_reason(), None);
    }

    // ---- Whatever (OAuth refresh) paths ----

    #[test]
    fn whatever_invalid_grant_is_terminal_auth_refresh_invalid() {
        let err = ClewdrError::Whatever {
            message: "OAuth refresh failed: invalid_grant".to_string(),
            source: None,
        };
        let ctx = classify(err, FailureSource::OauthRefresh);
        assert_eq!(ctx.action, AccountFailureAction::TerminalAuth);
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::OauthRefreshInvalid
        );
    }

    #[test]
    fn whatever_refresh_token_expired_is_refresh_invalid() {
        let err = ClewdrError::Whatever {
            message: "the refresh token has expired".to_string(),
            source: None,
        };
        let ctx = classify(err, FailureSource::OauthRefresh);
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::OauthRefreshInvalid
        );
    }

    #[test]
    fn whatever_status_401_is_upstream_auth_rejected() {
        let err = ClewdrError::Whatever {
            message: "request returned status 401".to_string(),
            source: None,
        };
        let ctx = classify(err, FailureSource::Messages);
        assert_eq!(ctx.action, AccountFailureAction::TerminalAuth);
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::UpstreamAuthRejected
        );
        assert_eq!(ctx.upstream_http_status, Some(401));
    }

    /// P2 fix: `send_oauth_token_request` wraps non-2xx responses as
    /// `Whatever { message: "OAuth token request failed with status NNN: ..." }`.
    /// 5xx outages MUST surface as TransientUpstream + UpstreamHttp{status},
    /// not InternalError, so /test, refresh, and probe_oauth diagnostics
    /// stay accurate during transient Anthropic / proxy failures.
    #[test]
    fn whatever_oauth_token_endpoint_5xx_is_transient_upstream() {
        for status in [500_u16, 502, 503, 504] {
            let err = ClewdrError::Whatever {
                message: format!("OAuth token request failed with status {status}: gateway error"),
                source: None,
            };
            let ctx = classify(err, FailureSource::OauthRefresh);
            assert_eq!(
                ctx.action,
                AccountFailureAction::TransientUpstream,
                "status {status} should map to TransientUpstream",
            );
            assert_eq!(
                ctx.normalized_reason,
                AccountNormalizedReason::UpstreamHttp { status },
            );
            assert_eq!(ctx.upstream_http_status, Some(status));
            // 5xx must not persist a Reason — caller must not invalidate
            // the account on a transient gateway error.
            assert_eq!(ctx.normalized_reason.to_reason(), None);
        }
    }

    /// 4xx other than 401/403 (e.g. 400 without `invalid_grant`, 429 from
    /// the OAuth endpoint) also flow through as TransientUpstream — the
    /// account-side decision belongs to ClaudeHttpError 429 with reset
    /// timestamps, not Whatever 429.
    #[test]
    fn whatever_status_400_without_invalid_grant_is_transient_upstream() {
        let err = ClewdrError::Whatever {
            message: "OAuth token request failed with status 400: bad request".to_string(),
            source: None,
        };
        let ctx = classify(err, FailureSource::OauthRefresh);
        assert_eq!(ctx.action, AccountFailureAction::TransientUpstream);
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::UpstreamHttp { status: 400 }
        );
        assert_eq!(ctx.upstream_http_status, Some(400));
    }

    /// `invalid_grant` takes precedence over the status code — a 400
    /// response carrying invalid_grant is still terminal auth failure.
    #[test]
    fn whatever_invalid_grant_with_status_400_stays_refresh_invalid() {
        let err = ClewdrError::Whatever {
            message: "OAuth token request failed with status 400: invalid_grant".to_string(),
            source: None,
        };
        let ctx = classify(err, FailureSource::OauthRefresh);
        assert_eq!(ctx.action, AccountFailureAction::TerminalAuth);
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::OauthRefreshInvalid
        );
    }

    /// `extract_status_code` defends against false positives — e.g. an
    /// error message that contains the word "status" but isn't followed
    /// by a 3-digit number must NOT confuse the classifier.
    #[test]
    fn whatever_status_word_without_code_falls_through() {
        let err = ClewdrError::Whatever {
            message: "could not determine status of refresh".to_string(),
            source: None,
        };
        let ctx = classify(err, FailureSource::OauthRefresh);
        assert_eq!(ctx.action, AccountFailureAction::InternalError);
        assert_eq!(ctx.upstream_http_status, None);
    }

    #[test]
    fn whatever_unrelated_message_is_internal_error() {
        let err = ClewdrError::Whatever {
            message: "something went wrong locally".to_string(),
            source: None,
        };
        let ctx = classify(err, FailureSource::Messages);
        assert_eq!(ctx.action, AccountFailureAction::InternalError);
        assert_eq!(
            ctx.normalized_reason,
            AccountNormalizedReason::InternalError
        );
        // Internal must not persist a Reason.
        assert_eq!(ctx.normalized_reason.to_reason(), None);
    }

    // ---- Other variants ----

    #[test]
    fn empty_choices_is_internal_error() {
        let ctx = classify(ClewdrError::EmptyChoices, FailureSource::Messages);
        assert_eq!(ctx.action, AccountFailureAction::InternalError);
    }

    #[test]
    fn quota_exceeded_is_internal_error_not_account_failure() {
        // Quota / Rpm / UserConcurrency are not account-side failures —
        // classifier defends against accidental routing by mapping them to
        // InternalError so callers see "do not change account state".
        let ctx = classify(ClewdrError::QuotaExceeded, FailureSource::Messages);
        assert_eq!(ctx.action, AccountFailureAction::InternalError);
        assert_eq!(ctx.normalized_reason.to_reason(), None);
    }

    // ---- as_type_str round-trip + uniqueness ----

    #[test]
    fn action_to_log_status_covers_every_variant() {
        assert_eq!(
            AccountFailureAction::TerminalAuth.to_log_status(),
            "auth_rejected"
        );
        assert_eq!(
            AccountFailureAction::TerminalDisabled.to_log_status(),
            "auth_rejected"
        );
        assert_eq!(
            AccountFailureAction::Cooldown { reset_time: 0 }.to_log_status(),
            "no_account_available"
        );
        assert_eq!(
            AccountFailureAction::TransientUpstream.to_log_status(),
            "upstream_error"
        );
        assert_eq!(
            AccountFailureAction::InternalError.to_log_status(),
            "internal_error"
        );
    }

    #[test]
    fn as_type_str_is_unique_across_variants() {
        let names = [
            AccountNormalizedReason::OauthRefreshInvalid.as_type_str(),
            AccountNormalizedReason::OauthOrgAuthNotAllowed.as_type_str(),
            AccountNormalizedReason::OauthProfileRejected.as_type_str(),
            AccountNormalizedReason::OauthUsageRejected.as_type_str(),
            AccountNormalizedReason::CookieMembershipMissing.as_type_str(),
            AccountNormalizedReason::CookieNoChatCapability.as_type_str(),
            AccountNormalizedReason::OrganizationDisabled.as_type_str(),
            AccountNormalizedReason::AccountBanned.as_type_str(),
            AccountNormalizedReason::FreeTier.as_type_str(),
            AccountNormalizedReason::RateLimited { reset_time: 0 }.as_type_str(),
            AccountNormalizedReason::Restricted { reset_time: 0 }.as_type_str(),
            AccountNormalizedReason::UpstreamAuthRejected.as_type_str(),
            AccountNormalizedReason::UpstreamHttp { status: 500 }.as_type_str(),
            AccountNormalizedReason::UpstreamTransient.as_type_str(),
            AccountNormalizedReason::InternalError.as_type_str(),
        ];
        let unique: std::collections::HashSet<_> = names.iter().copied().collect();
        assert_eq!(unique.len(), names.len(), "as_type_str must be unique");
        // Critical regression: must NOT collapse to invalid_cookie any more.
        assert!(!names.contains(&"invalid_cookie"));
    }

    // ---- raw_message + source/stage carry-through ----

    #[test]
    fn context_preserves_source_stage_raw_message() {
        let ctx = classify_stage(
            ClewdrError::InvalidCookie {
                reason: Reason::Null,
            },
            FailureSource::ProbeOauth,
            "profile",
        );
        assert_eq!(ctx.source, FailureSource::ProbeOauth);
        assert_eq!(ctx.stage, Some("profile"));
        assert!(!ctx.raw_message.is_empty());
    }
}
