//! Claude Code mimicry — shared building blocks and the third-party relay cloak.
//!
//! clewdr-hub emits two distinct "look like Claude Code" profiles that share
//! generic Anthropic request hygiene but diverge on wire identity:
//!
//! * **Official** (Cookie/OAuth subscription): conservative, pinned, applied by
//!   [`crate::claude_code_state::ClaudeCodeState::execute_claude_request`]. The
//!   adversary is Anthropic, so the fingerprint must be a verified-consistent
//!   set — no admin knobs.
//! * **Third-party** ([`third_party`], opt-in on API-key channels): the adversary
//!   is the relay's up-front validator, so it mirrors the proven Claude-Cloak
//!   wire shape (Bearer auth, `cch=00000` literal, synthesized `anthropic-beta`
//!   without the OAuth token, Claude-Cloak-style `metadata.user_id`).
//!
//! This module owns the values that are common to both (the Stainless header
//! block, the reserved extra-header guard); profile-specific logic lives in the
//! send path ([`crate::claude_code_state`]) and in [`third_party`].

pub mod third_party;

/// Anthropic JS SDK (Stainless-generated) default header values the real
/// Claude Code CLI sends on every `/v1/messages` request (observed in 2.1.185
/// capture; package-version tracks `@anthropic-ai/sdk`). `runtime-version` is
/// environment-derived on a real install; we pin a plausible recent Node value.
/// These are fixed fingerprint headers, unrelated to body content, so both the
/// messages and count_tokens paths send them.
pub(crate) const STAINLESS_HEADERS: &[(&str, &str)] = &[
    ("x-stainless-retry-count", "0"),
    ("x-stainless-timeout", "600"),
    ("x-stainless-lang", "js"),
    ("x-stainless-package-version", "0.94.0"),
    ("x-stainless-os", "Linux"),
    ("x-stainless-arch", "x64"),
    ("x-stainless-runtime", "node"),
    ("x-stainless-runtime-version", "v24.3.0"),
];

/// Header names that MUST NOT come from per-account extra_headers on an ApiKey
/// send: either we set them ourselves (`x-api-key`, `anthropic-version`,
/// `anthropic-beta`, `content-type`) or they reintroduce subscription-shaped
/// behavior the ApiKey dispatch controls (`user-agent`) or they belong to the
/// transport layer (`host`, `content-length`, `accept-encoding`) and overriding
/// them breaks the request.
///
/// Admin write-time validation is the primary guardrail; the send-time filter is
/// defense in depth for a manual DB edit that bypasses validation.
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
