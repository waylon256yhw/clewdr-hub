use serde::{Deserialize, Serialize};

/// Strict Claude Code CLI version accepted by Claude-Cloak: `x.y.z`.
/// Empty values are handled by callers when they mean "inherit/fallback".
pub fn is_valid_claude_cli_version(value: &str) -> bool {
    let mut parts = value.split('.');
    let (Some(major), Some(minor), Some(patch), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    [major, minor, patch]
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// Which mimicry profile an API-key channel applies to its outbound requests.
///
/// * `None` — clean passthrough (the historical API-key behavior): only
///   `x-api-key` + `anthropic-version` + filtered extra headers, billing block
///   stripped. The default; unchanged for every existing api_key account.
/// * `ThirdParty` — the Claude-Cloak-style relay cloak (see
///   [`crate::mimicry::third_party`]): the full Claude Code wire shape so a
///   third-party relay's up-front validator accepts the request.
///
/// Cookie/OAuth accounts always carry `None` here — their (official) mimicry is
/// applied unconditionally on the subscription send path and is not configurable.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum MimicryMode {
    #[default]
    None,
    ThirdParty,
}

impl MimicryMode {
    /// Map the persisted `accounts.mimicry_mode` string to a typed kind.
    /// Unknown values fall back to `None` (defensive — the column CHECK
    /// constrains it to `none | third_party`).
    pub fn from_db(s: &str) -> Self {
        match s {
            "third_party" => MimicryMode::ThirdParty,
            _ => MimicryMode::None,
        }
    }

    /// The canonical DB / wire string for this mode.
    pub fn as_db(self) -> &'static str {
        match self {
            MimicryMode::None => "none",
            MimicryMode::ThirdParty => "third_party",
        }
    }

    pub fn is_third_party(self) -> bool {
        matches!(self, MimicryMode::ThirdParty)
    }
}

/// How the third-party cloak presents the upstream secret.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthHeaderForm {
    /// `Authorization: Bearer <secret>` — the default, matching Claude-Cloak and
    /// the real Claude Code CLI OAuth path; most relays key on this.
    #[default]
    Bearer,
    /// `x-api-key: <secret>` — for relays that expect the native Anthropic
    /// direct-API header instead.
    XApiKey,
}

/// Per-channel third-party cloak configuration. Persisted as JSON in
/// `accounts.mimicry_config`; only meaningful when `mimicry_mode = third_party`.
///
/// Deliberately narrow — the "尽善尽美" cloak defaults (billing block, Stainless
/// headers, parameter normalization, `cch=00000` placeholder, identity
/// injection) are固化 in the third-party profile and are NOT exposed as
/// per-channel toggles, to avoid an admin assembling a half-consistent
/// fingerprint.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThirdPartyMimicryConfig {
    /// Upstream auth header form. Default `Bearer`.
    #[serde(default)]
    pub auth_header: AuthHeaderForm,
    /// Impersonated CLI version. `None` inherits the global third-party profile
    /// (setting `tp_cloak_cli_version`).
    #[serde(default)]
    pub cli_version: Option<String>,
    /// Relocate the client's own `system` into a leading user message so the
    /// wire-visible system is just the Claude Code identity (Claude-Cloak strict
    /// mode). The UI defaults this on for new third-party channels.
    #[serde(default)]
    pub strict_system: bool,
    /// Extra `anthropic-beta` tokens to append for relays that require them.
    #[serde(default)]
    pub extra_beta: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::is_valid_claude_cli_version;

    #[test]
    fn claude_cli_version_requires_exact_x_y_z() {
        for valid in ["0.0.1", "2.1.198", "12.34.56"] {
            assert!(is_valid_claude_cli_version(valid), "{valid}");
        }
        for invalid in ["", "2.1", "2.1.198abc", "v2.1.198", "2.1.198-beta"] {
            assert!(!is_valid_claude_cli_version(invalid), "{invalid}");
        }
    }
}
