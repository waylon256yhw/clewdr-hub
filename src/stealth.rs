use std::sync::Arc;

use arc_swap::ArcSwap;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tracing::warn;

use crate::db::billing::get_setting;
use crate::types::claude::OutputEffort;

/// Default values (compile-time fallbacks)
pub const DEFAULT_CLI_VERSION: &str = "2.1.181";
pub const DEFAULT_BILLING_SALT: &str = "59cf53e54c78";

/// xxh64 seed the real Claude Code CLI uses to checksum the outbound body into
/// the `cch` field of `x-anthropic-billing-header`. Mined from the bundle
/// (present verbatim in 2.1.178 through 2.1.185). Pinned: the value is telemetry
/// — Anthropic does not re-verify it against the received bytes (the real CLI's
/// own `cch` does not match a recompute over its wire bytes either, because it
/// hashes an internal pre-serialization form), so we only need a self-consistent
/// value of the right shape, not the CLI's exact value. See
/// `docs/cch-billing-header-findings.md` §5.1.
const CCH_SEED: u64 = 0x4d65_9218_e32a_3268;

/// The `cch=00000;` placeholder bytes the billing-header builder emits; the five
/// `0` digits sit at offset +4 and are overwritten in place by [`cch_rewrite`].
const CCH_PLACEHOLDER: &[u8] = b"cch=00000;";

/// Cached stealth configuration loaded from DB settings.
#[derive(Clone, Debug)]
pub struct StealthProfile {
    pub cli_version: String,
    pub billing_salt: String,
    pub force_output_effort: Option<OutputEffort>,
}

impl Default for StealthProfile {
    fn default() -> Self {
        Self {
            cli_version: DEFAULT_CLI_VERSION.into(),
            billing_salt: DEFAULT_BILLING_SALT.into(),
            force_output_effort: None,
        }
    }
}

impl StealthProfile {
    /// Load profile from DB settings, falling back to defaults for missing keys.
    pub async fn load_from_db(pool: &SqlitePool) -> Self {
        let mut profile = Self::default();

        fn non_empty(v: Result<Option<String>, sqlx::Error>) -> Option<String> {
            v.ok().flatten().filter(|s| !s.is_empty())
        }

        fn parse_bool(v: Option<&str>) -> bool {
            matches!(
                v.map(str::trim).filter(|s| !s.is_empty()),
                Some("1" | "true" | "yes" | "on")
            )
        }

        fn parse_effort(v: &str) -> Option<OutputEffort> {
            match v.trim().to_ascii_lowercase().as_str() {
                "low" => Some(OutputEffort::Low),
                "medium" => Some(OutputEffort::Medium),
                "high" => Some(OutputEffort::High),
                "xhigh" => Some(OutputEffort::XHigh),
                "max" => Some(OutputEffort::Max),
                _ => None,
            }
        }

        if let Some(v) = non_empty(get_setting(pool, "cc_cli_version").await) {
            profile.cli_version = v;
        }
        if let Some(v) = non_empty(get_setting(pool, "cc_billing_salt").await) {
            profile.billing_salt = v;
        }
        let effort_override_enabled =
            non_empty(get_setting(pool, "output_effort_override_enabled").await);
        let effort_override_level =
            non_empty(get_setting(pool, "output_effort_override_level").await);
        if parse_bool(effort_override_enabled.as_deref()) {
            profile.force_output_effort = effort_override_level.as_deref().and_then(parse_effort);
        }

        profile
    }

    /// User-Agent string: `claude-cli/{version} (external, cli)`
    pub fn user_agent(&self) -> String {
        format!("claude-cli/{} (external, cli)", self.cli_version)
    }
}

/// Global stealth profile, loaded once at startup and swappable at runtime.
pub type SharedStealthProfile = Arc<ArcSwap<StealthProfile>>;

/// Global singleton, initialized at startup via `init_stealth_profile()`.
static GLOBAL_PROFILE: std::sync::OnceLock<SharedStealthProfile> = std::sync::OnceLock::new();

/// Get the global stealth profile. Panics if not initialized.
pub fn global_profile() -> &'static SharedStealthProfile {
    GLOBAL_PROFILE
        .get()
        .expect("stealth profile not initialized")
}

/// Create and register the global shared profile from DB.
pub async fn init_stealth_profile(pool: &SqlitePool) -> SharedStealthProfile {
    let profile = StealthProfile::load_from_db(pool).await;
    warn!("Stealth profile loaded: cli={}", profile.cli_version);
    let shared = Arc::new(ArcSwap::from_pointee(profile));
    let _ = GLOBAL_PROFILE.set(shared.clone());
    shared
}

/// Reload stealth profile from DB and hot-swap into global singleton.
pub async fn reload_stealth_profile(pool: &SqlitePool) {
    let profile = StealthProfile::load_from_db(pool).await;
    warn!("Stealth profile reloaded: cli={}", profile.cli_version);
    global_profile().store(Arc::new(profile));
}

/// Overwrite the billing-header `cch=00000;` placeholder in a fully-serialized
/// request body with a self-consistent xxh64 checksum of those bytes, mirroring
/// the real CLI's billing checksum shape. Returns whether a rewrite happened.
///
/// The placeholder is located inside the top-level `system` value, NOT by
/// scanning the whole body: arbitrary user/tool content may contain the literal
/// `cch=00000;` or even a full billing-header-looking string. Restricting the
/// search to the serialized `system` value keeps those references untouched.
/// Returns false (no-op) when no system billing block carries a placeholder
/// (e.g. count_tokens, or already rewritten).
///
/// Self-consistency: the hash is taken over the body that still carries the
/// `00000` placeholder, then the five digits are replaced. A verifier that
/// re-zeros the field and recomputes gets the same value. Must run on the FINAL
/// bytes (after metadata/session injection) so the checksum covers them.
pub fn cch_rewrite(body: &mut [u8]) -> bool {
    const MARKER: &[u8] = b"x-anthropic-billing-header:";
    let Some((system_start, system_end)) = top_level_json_field_value(body, b"\"system\":") else {
        return false;
    };
    let system = &body[system_start..system_end];

    // Locate the placeholder inside the injected billing block. `system` is
    // serialized after `prepend_system_blocks`, so that block is first in the
    // top-level system array. We still scan all system markers defensively in
    // case older payload shapes are encountered.
    let mut search_from = 0;
    let mut placeholder_pos = None;
    while let Some(rel_marker) = system[search_from..]
        .windows(MARKER.len())
        .position(|w| w == MARKER)
    {
        let marker_pos = search_from + rel_marker;
        let after = &system[marker_pos..];
        let block_end = after.iter().position(|&b| b == b'"').unwrap_or(after.len());
        if let Some(rel) = after[..block_end]
            .windows(CCH_PLACEHOLDER.len())
            .position(|w| w == CCH_PLACEHOLDER)
        {
            placeholder_pos = Some(system_start + marker_pos + rel);
            break;
        }
        // This marker's value has no placeholder (e.g. a user message merely
        // mentioning the header); keep scanning past it.
        search_from = marker_pos + MARKER.len();
    }
    let Some(pos) = placeholder_pos else {
        return false; // no billing block carries a placeholder
    };
    let cch = xxhash_rust::xxh64::xxh64(body, CCH_SEED) & 0xf_ffff;
    let hex = format!("{cch:05x}");
    // placeholder is `cch=00000;` → the five `0` digits start at +4.
    body[pos + 4..pos + 9].copy_from_slice(hex.as_bytes());
    true
}

fn top_level_json_field_value(body: &[u8], field: &[u8]) -> Option<(usize, usize)> {
    let field_name = field
        .strip_prefix(b"\"")
        .and_then(|s| s.strip_suffix(b"\":"))?;
    let mut pos = body.iter().position(|b| !b.is_ascii_whitespace())?;
    if body.get(pos) != Some(&b'{') {
        return None;
    }
    pos += 1;

    loop {
        skip_json_ws(body, &mut pos);
        match body.get(pos)? {
            b'}' => return None,
            b'"' => {}
            _ => return None,
        }

        let key_start = pos;
        let key_end = json_string_end(body, key_start)?;
        pos = key_end;
        skip_json_ws(body, &mut pos);
        if body.get(pos) != Some(&b':') {
            return None;
        }
        pos += 1;
        skip_json_ws(body, &mut pos);

        let value_start = pos;
        let value_end = json_value_end(body, value_start)?;
        if body.get(key_start + 1..key_end - 1) == Some(field_name) {
            return Some((value_start, value_end));
        }

        pos = value_end;
        skip_json_ws(body, &mut pos);
        match body.get(pos)? {
            b',' => pos += 1,
            b'}' => return None,
            _ => return None,
        }
    }
}

fn json_value_end(body: &[u8], start: usize) -> Option<usize> {
    match *body.get(start)? {
        b'"' => json_string_end(body, start),
        b'[' | b'{' => json_container_end(body, start),
        _ => {
            let rel = body[start..]
                .iter()
                .position(|&b| b == b',' || b == b'}')
                .unwrap_or(body.len() - start);
            Some(start + rel)
        }
    }
}

fn skip_json_ws(body: &[u8], pos: &mut usize) {
    while *pos < body.len() && body[*pos].is_ascii_whitespace() {
        *pos += 1;
    }
}

fn json_string_end(body: &[u8], start: usize) -> Option<usize> {
    let mut escaped = false;
    for (idx, &b) in body.iter().enumerate().skip(start + 1) {
        if escaped {
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        } else if b == b'"' {
            return Some(idx + 1);
        }
    }
    None
}

fn json_container_end(body: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (idx, &b) in body.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(idx + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Derive a deterministic v4-shaped UUID from arbitrary seed material.
/// blake3(seed) → first 16 bytes → set the version (4) and RFC 4122 variant
/// bits. Stable for a given seed; indistinguishable from a random v4.
pub fn uuid_from_seed(seed: &[u8]) -> uuid::Uuid {
    let hash = blake3::hash(seed);
    let mut b = [0u8; 16];
    b.copy_from_slice(&hash.as_bytes()[..16]);
    b[6] = (b[6] & 0x0F) | 0x40; // version 4
    b[8] = (b[8] & 0x3F) | 0x80; // RFC 4122 variant
    uuid::Uuid::from_bytes(b)
}

/// Stable per-credential `device_id` (64 lowercase hex), mirroring the real
/// CLI's persistent device id. Deterministic from the billing salt + api-key id
/// so it stays constant for the credential's life. Same formula the previous
/// `metadata.user_id` user-hex used, keeping the value stable across this change.
pub fn derive_device_id(billing_salt: &str, api_key_id: i64) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{billing_salt}{api_key_id}"))
    )
}

/// The caller-conversation seed a session id is derived from, in priority order.
pub enum SessionSeed {
    /// ① An inbound `metadata.user_id.session_id` (a real Claude Code client).
    InboundSession(String),
    /// ② Hash of `system` + first user message (2api/OpenAI multi-turn stays
    /// stable while the first turn is unchanged).
    ContentHash(u64),
    /// ③ Last-resort: api-key + hour window (at least stable per key per hour,
    /// never per-request random).
    KeyTimeWindow,
}

/// Derive the outbound `session_id`, bound to the *selected* account so that an
/// account change (failover) naturally rotates the session — matching "another
/// official account/device" semantics and avoiding cross-account prompt-cache
/// confusion. Stable for a given (account, caller-conversation); only the
/// `KeyTimeWindow` fallback folds in the hour bucket.
pub fn derive_session_id(
    billing_salt: &str,
    selected_account_id: Option<i64>,
    api_key_id: i64,
    seed: &SessionSeed,
    now: chrono::DateTime<chrono::Utc>,
) -> uuid::Uuid {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"cc-session-v1");
    buf.extend_from_slice(billing_salt.as_bytes());
    buf.extend_from_slice(&selected_account_id.unwrap_or(0).to_le_bytes());
    buf.extend_from_slice(&api_key_id.to_le_bytes());
    match seed {
        SessionSeed::InboundSession(s) => {
            buf.push(1);
            buf.extend_from_slice(s.as_bytes());
        }
        SessionSeed::ContentHash(h) => {
            buf.push(2);
            buf.extend_from_slice(&h.to_le_bytes());
        }
        SessionSeed::KeyTimeWindow => {
            buf.push(3);
            let hour_bucket = now.timestamp().div_euclid(3600);
            buf.extend_from_slice(&hour_bucket.to_le_bytes());
        }
    }
    uuid_from_seed(&buf)
}

/// Build the `metadata.user_id` value: a JSON-stringified object
/// `{device_id, account_uuid, session_id}`, matching the real CLI exactly
/// (the field holds a stringified JSON object, not a flat string).
pub fn build_user_id_metadata(
    device_id: &str,
    account_uuid: &str,
    session_id: &uuid::Uuid,
) -> String {
    serde_json::json!({
        "device_id": device_id,
        "account_uuid": account_uuid,
        "session_id": session_id.to_string(),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cch_rewrite_is_self_consistent() {
        let mut body =
            br#"{"system":[{"text":"x-anthropic-billing-header: cc_version=2.1.181.abc; cc_entrypoint=cli; cch=00000;"}],"messages":[]}"#.to_vec();
        assert!(cch_rewrite(&mut body));
        // placeholder replaced with 5 hex digits.
        let s = String::from_utf8(body.clone()).unwrap();
        let start = s.find("cch=").unwrap() + 4;
        let cch = &s[start..start + 5];
        assert_ne!(cch, "00000");
        assert!(cch.chars().all(|c| c.is_ascii_hexdigit()));
        // A verifier that re-zeros and recomputes gets the same value.
        let mut zeroed = body.clone();
        let p = zeroed.windows(4).position(|w| w == b"cch=").unwrap();
        zeroed[p + 4..p + 9].copy_from_slice(b"00000");
        let expect = xxhash_rust::xxh64::xxh64(&zeroed, CCH_SEED) & 0xf_ffff;
        assert_eq!(format!("{expect:05x}"), cch);
    }

    #[test]
    fn cch_rewrite_noop_without_placeholder() {
        let mut body = br#"{"no":"placeholder"}"#.to_vec();
        let before = body.clone();
        assert!(!cch_rewrite(&mut body));
        assert_eq!(body, before);
    }

    #[test]
    fn cch_rewrite_ignores_placeholder_in_user_content() {
        // Arbitrary user/tool content may contain the literal cch=00000; — it
        // must NOT be mistaken for the billing placeholder. Only the one inside
        // the billing-header block is rewritten.
        let mut body = br#"{"system":[{"text":"x-anthropic-billing-header: cc_version=2.1.181.abc; cc_entrypoint=cli; cch=00000;"}],"messages":[{"role":"user","content":"here is a literal cch=00000; in my prompt"}]}"#.to_vec();
        assert!(cch_rewrite(&mut body));
        let s = String::from_utf8(body).unwrap();
        // billing block rewritten (the one right after the marker)...
        let marker = s.find("x-anthropic-billing-header:").unwrap();
        let billing_cch = s[marker..].find("cch=").unwrap() + marker + 4;
        assert_ne!(&s[billing_cch..billing_cch + 5], "00000");
        // ...but the user-content occurrence is untouched.
        assert!(s.contains("literal cch=00000; in my prompt"));
    }

    #[test]
    fn cch_rewrite_skips_user_marker_before_real_block() {
        // messages serialize before system, so a user message that merely
        // mentions the marker (no placeholder) precedes the real billing block.
        // The scan must skip it and rewrite the real block's placeholder.
        let mut body = br#"{"messages":[{"role":"user","content":"docs mention x-anthropic-billing-header: cc_version=...; here"}],"system":[{"text":"x-anthropic-billing-header: cc_version=2.1.181.abc; cc_entrypoint=cli; cch=00000;"}]}"#.to_vec();
        assert!(cch_rewrite(&mut body));
        let s = String::from_utf8(body).unwrap();
        // the real billing block (in system) is rewritten...
        let sys_marker = s.rfind("x-anthropic-billing-header:").unwrap();
        let real_cch = s[sys_marker..].find("cch=").unwrap() + sys_marker + 4;
        assert_ne!(&s[real_cch..real_cch + 5], "00000");
        // ...and the user-message mention is untouched.
        assert!(s.contains("docs mention x-anthropic-billing-header: cc_version=...; here"));
    }

    #[test]
    fn cch_rewrite_ignores_full_fake_billing_header_outside_system() {
        // User/tool content can mention a complete fake billing header before
        // or after the real system field. Only the top-level system block is
        // eligible for cch rewriting.
        let fake =
            "x-anthropic-billing-header: cc_version=9.9.999.abc; cc_entrypoint=cli; cch=00000;";
        let mut body = format!(
            r#"{{"messages":[{{"role":"user","content":"before {fake}"}}],"system":[{{"text":"x-anthropic-billing-header: cc_version=2.1.181.abc; cc_entrypoint=cli; cch=00000;"}}],"tools":[{{"name":"t","description":"after {fake}","input_schema":{{"type":"object"}}}}]}}"#
        )
        .into_bytes();
        assert!(cch_rewrite(&mut body));
        let s = String::from_utf8(body).unwrap();

        let sys_field = s.find(r#""system":"#).unwrap();
        let sys_marker = s[sys_field..].find("x-anthropic-billing-header:").unwrap() + sys_field;
        let real_cch = s[sys_marker..].find("cch=").unwrap() + sys_marker + 4;
        assert_ne!(&s[real_cch..real_cch + 5], "00000");

        assert!(s.contains(&format!("before {fake}")));
        assert!(s.contains(&format!("after {fake}")));
    }

    #[test]
    fn cch_rewrite_noop_without_billing_marker() {
        // A stray cch=00000; with no billing-header marker is not the billing
        // placeholder and must not be rewritten.
        let mut body = br#"{"messages":[{"role":"user","content":"cch=00000;"}]}"#.to_vec();
        let before = body.clone();
        assert!(!cch_rewrite(&mut body));
        assert_eq!(body, before);
    }

    #[test]
    fn uuid_from_seed_deterministic_and_v4_shaped() {
        let a = uuid_from_seed(b"abc");
        let b = uuid_from_seed(b"abc");
        let c = uuid_from_seed(b"abd");
        assert_eq!(a, b);
        assert_ne!(a, c);
        // v4 version nibble + RFC 4122 variant.
        let bytes = a.as_bytes();
        assert_eq!(bytes[6] & 0xF0, 0x40);
        assert_eq!(bytes[8] & 0xC0, 0x80);
    }

    #[test]
    fn session_id_binds_to_account_and_stable_per_conversation() {
        let now = chrono::DateTime::from_timestamp(1_000_000, 0).unwrap();
        let seed = SessionSeed::ContentHash(42);
        let s1 = derive_session_id("salt", Some(1), 7, &seed, now);
        let s1b = derive_session_id("salt", Some(1), 7, &SessionSeed::ContentHash(42), now);
        let s2 = derive_session_id("salt", Some(2), 7, &seed, now);
        assert_eq!(s1, s1b, "stable per (account, conversation)");
        assert_ne!(s1, s2, "account change rotates session");
    }

    #[test]
    fn session_id_content_seed_ignores_time_but_keytime_rotates() {
        let t0 = chrono::DateTime::from_timestamp(1_000_000, 0).unwrap();
        let t1 = chrono::DateTime::from_timestamp(1_000_000 + 7200, 0).unwrap();
        // ② content hash: time-independent.
        let c0 = derive_session_id("s", Some(1), 1, &SessionSeed::ContentHash(9), t0);
        let c1 = derive_session_id("s", Some(1), 1, &SessionSeed::ContentHash(9), t1);
        assert_eq!(c0, c1);
        // ③ key+time window: rotates across hour buckets.
        let k0 = derive_session_id("s", Some(1), 1, &SessionSeed::KeyTimeWindow, t0);
        let k1 = derive_session_id("s", Some(1), 1, &SessionSeed::KeyTimeWindow, t1);
        assert_ne!(k0, k1);
    }

    #[test]
    fn user_id_metadata_is_stringified_json_object() {
        let sid = uuid_from_seed(b"x");
        let raw = build_user_id_metadata("devhex", "org-uuid", &sid);
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["device_id"], "devhex");
        assert_eq!(v["account_uuid"], "org-uuid");
        assert_eq!(v["session_id"], sid.to_string());
    }

    #[test]
    fn user_id_metadata_empty_account_uuid() {
        let sid = uuid_from_seed(b"y");
        let raw = build_user_id_metadata("dev", "", &sid);
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["account_uuid"], "");
    }
}
