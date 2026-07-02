//! Third-party relay cloak — the Claude-Cloak-aligned wire profile.
//!
//! Applied to API-key channels whose `mimicry_mode == ThirdParty`. The threat
//! model is a 中转站 that validates request headers/body up front, so this makes
//! the request look like the real Claude Code CLI *to the relay*. It deliberately
//! DIFFERS from the official (Cookie/OAuth) profile in several places (see the
//! per-item notes below): `cch` stays the literal `00000` (relays whitelist /
//! recompute it), `anthropic-beta` is synthesized without the OAuth-only token,
//! and `metadata.user_id` follows Claude-Cloak (preserve valid inbound, else a
//! fresh fake) rather than the account-bound deterministic derivation.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwap;
use http::header::USER_AGENT;
use serde_json::Value;
use sqlx::SqlitePool;
use tracing::warn;

use crate::{
    config::{AuthHeaderForm, ThirdPartyMimicryConfig},
    db::billing::get_setting,
    error::ClewdrError,
    middleware::claude::{
        claude_code_billing_header_from_sample, fill_system_only_user_placeholder,
        prepend_system_blocks, strip_billing_headers_from_system,
    },
    mimicry::{STAINLESS_HEADERS, is_reserved_api_key_extra_header},
    stealth::{DEFAULT_BILLING_SALT, DEFAULT_CLI_VERSION, StealthProfile},
    types::claude::{ContentBlock, CreateMessageParams, Message, MessageContent, Role},
};

/// `anthropic-version` value sent on every cloak request.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The Claude Code identity system prompt the real CLI always sends. The
/// official path relies on the inbound client (a real CLI) to send this; the
/// third-party cloak must inject it itself, because the inbound client is
/// arbitrary.
pub(crate) const CC_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Global third-party cloak profile: the impersonated CLI version and the
/// billing salt. Version comes from the `tp_cloak_cli_version` setting (the
/// official path stays pinned to the compile-time `DEFAULT_CLI_VERSION`); salt
/// is shared with the official path (`cc_billing_salt`).
#[derive(Clone, Debug)]
pub struct ThirdPartyCloakProfile {
    pub cli_version: String,
    pub billing_salt: String,
}

impl Default for ThirdPartyCloakProfile {
    fn default() -> Self {
        Self {
            cli_version: DEFAULT_CLI_VERSION.into(),
            billing_salt: DEFAULT_BILLING_SALT.into(),
        }
    }
}

impl ThirdPartyCloakProfile {
    pub async fn load_from_db(pool: &SqlitePool) -> Self {
        let mut profile = Self::default();
        fn non_empty(v: Result<Option<String>, sqlx::Error>) -> Option<String> {
            v.ok().flatten().filter(|s| !s.trim().is_empty())
        }
        if let Some(v) = non_empty(get_setting(pool, "tp_cloak_cli_version").await) {
            profile.cli_version = v.trim().to_string();
        }
        if let Some(v) = non_empty(get_setting(pool, "cc_billing_salt").await) {
            profile.billing_salt = v;
        }
        profile
    }
}

pub type SharedThirdPartyProfile = Arc<ArcSwap<ThirdPartyCloakProfile>>;

static GLOBAL_PROFILE: OnceLock<SharedThirdPartyProfile> = OnceLock::new();

/// Get the global third-party cloak profile. Panics if not initialized.
pub fn global_profile() -> &'static SharedThirdPartyProfile {
    GLOBAL_PROFILE
        .get()
        .expect("third-party cloak profile not initialized")
}

/// Create and register the global profile from DB. Called once at startup,
/// right after `stealth::init_stealth_profile`.
pub async fn init_profile(pool: &SqlitePool) -> SharedThirdPartyProfile {
    let profile = ThirdPartyCloakProfile::load_from_db(pool).await;
    warn!(
        "Third-party cloak profile loaded: cli={}",
        profile.cli_version
    );
    let shared = Arc::new(ArcSwap::from_pointee(profile));
    let _ = GLOBAL_PROFILE.set(shared.clone());
    shared
}

/// Reload the profile from DB and hot-swap it (called when `tp_cloak_cli_version`
/// changes). No-op if the profile was never initialized (e.g. in unit tests).
pub async fn reload_profile(pool: &SqlitePool) {
    if let Some(shared) = GLOBAL_PROFILE.get() {
        let profile = ThirdPartyCloakProfile::load_from_db(pool).await;
        warn!(
            "Third-party cloak profile reloaded: cli={}",
            profile.cli_version
        );
        shared.store(Arc::new(profile));
    }
}

/// Synthesize the `anthropic-beta` header for a third-party send (ported from
/// Claude-Cloak `buildBetas`). Unlike the official path this NEVER adds
/// `oauth-2025-04-20` and does NOT inherit the inbound header — it derives the
/// token set purely from the model and the body features the client actually
/// used, then appends the per-channel `extra_beta`. Returns `None` (omit the
/// header) when empty.
pub(crate) fn synth_beta(
    model: &str,
    body: &CreateMessageParams,
    extra_beta: &[String],
) -> Option<String> {
    let mut betas: Vec<String> = Vec::new();
    if !model.to_ascii_lowercase().contains("haiku") {
        betas.push("claude-code-20250219".into());
    }
    betas.push("interleaved-thinking-2025-05-14".into());
    if body.context_management.is_some() {
        betas.push("context-management-2025-06-27".into());
    }
    if body
        .output_config
        .as_ref()
        .and_then(|c| c.effort.as_ref())
        .is_some()
    {
        betas.push("effort-2025-11-24".into());
    }
    let has_format = body
        .output_config
        .as_ref()
        .and_then(|c| c.format.as_ref())
        .is_some()
        || body.output_format.is_some();
    if has_format {
        betas.push("structured-outputs-2025-12-15".into());
    }
    for token in extra_beta {
        let token = token.trim();
        if !token.is_empty() && !betas.iter().any(|b| b.eq_ignore_ascii_case(token)) {
            betas.push(token.to_string());
        }
    }
    if betas.is_empty() {
        None
    } else {
        Some(betas.join(","))
    }
}

/// Build a fully-assembled third-party cloak request (headers + body) on the
/// given client and URL, WITHOUT sending it. This is the single source of truth
/// for the third-party wire shape, shared by the live send path
/// (`ClaudeCodeState::execute_third_party_request`) and the admin `/test` probe,
/// so a test exercises byte-for-byte what a real request would send.
///
/// `is_count_tokens` skips the body cloak (headers only), matching the real CLI's
/// count request. `extra_headers` are the per-account HTTP headers, applied after
/// the reserved-name filter.
pub(crate) fn build_cloak_request(
    client: &wreq::Client,
    url: &url::Url,
    secret: &str,
    cfg: &ThirdPartyMimicryConfig,
    extra_headers: &BTreeMap<String, String>,
    body: &CreateMessageParams,
    is_count_tokens: bool,
) -> Result<wreq::RequestBuilder, ClewdrError> {
    let profile = global_profile().load();
    let cli_version = cfg
        .cli_version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| profile.cli_version.clone());
    let user_agent = format!("claude-cli/{cli_version} (external, cli)");
    let stream = body.stream.unwrap_or(false);
    let beta = synth_beta(&body.model, body, &cfg.extra_beta);

    let mut send_body = body.clone();
    let session_id = if is_count_tokens {
        strip_billing_headers_from_system(&mut send_body);
        None
    } else {
        cloak_messages_body(
            &mut send_body,
            &cli_version,
            &profile.billing_salt,
            cfg.strict_system,
        )
    };
    let bytes = serde_json::to_vec(&send_body)?;

    let mut req = client.post(url.to_string());
    req = match cfg.auth_header {
        AuthHeaderForm::Bearer => req.bearer_auth(secret),
        AuthHeaderForm::XApiKey => req.header("x-api-key", secret),
    };
    req = req
        .header("content-type", "application/json")
        // Claude-Cloak sets an explicit Accept; the official path omits it, but
        // the third-party cloak mirrors Claude-Cloak's proven relay header set.
        .header("accept", "application/json")
        .header(USER_AGENT, user_agent)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("anthropic-dangerous-direct-browser-access", "true")
        .header("x-app", "cli");
    if let Some(beta) = beta {
        req = req.header("anthropic-beta", beta);
    }
    if let Some(session_id) = session_id {
        req = req.header("x-claude-code-session-id", session_id);
    }
    for (name, value) in STAINLESS_HEADERS {
        req = req.header(*name, *value);
    }
    if stream {
        req = req.header("x-stainless-helper-method", "stream");
    }
    for (k, v) in extra_headers {
        if is_reserved_api_key_extra_header(k) {
            continue;
        }
        req = req.header(k.as_str(), v.as_str());
    }

    Ok(req.body(bytes))
}

/// Cloak a `/v1/messages` body in place, Claude-Cloak style. Returns the session
/// id to emit as `X-Claude-Code-Session-Id` (`None` when there is no extractable
/// session — e.g. a preserved legacy-shaped inbound `user_id`).
///
/// Ordering matters: the first-user-text billing sample is captured BEFORE any
/// strict-mode system demotion (which would prepend a new leading user message
/// and shift the sample onto the demoted system text).
pub(crate) fn cloak_messages_body(
    body: &mut CreateMessageParams,
    cli_version: &str,
    billing_salt: &str,
    strict_system: bool,
) -> Option<String> {
    // Zero-input safety net (Claude-Cloak B4): a request that arrives with no
    // messages but a non-empty system would otherwise ship empty `messages`,
    // which relays (and Anthropic) reject. The middleware only injects this when
    // messages were emptied during cleanup, not when they arrive empty, so the
    // cloak re-applies it here before sampling the first user text.
    fill_system_only_user_placeholder(body);

    let first_text = first_user_text(&body.messages);

    // Drop the billing block the middleware injected with the official pinned
    // version; the third-party one is rebuilt below with the cloak version.
    strip_billing_headers_from_system(body);

    inject_identity(body, strict_system);
    let session_id = ensure_user_id(body);

    let profile = StealthProfile {
        cli_version: cli_version.to_string(),
        billing_salt: billing_salt.to_string(),
        force_output_effort: None,
    };
    let billing = claude_code_billing_header_from_sample(&first_text, &profile);
    // Prepend LAST so the billing block sits at system[0], mirroring both the
    // real CLI and Claude-Cloak. `cch` is left as the literal `00000` — no
    // xxh64 rewrite (the official-only telemetry hash) — because relays
    // recompute or whitelist that field.
    prepend_system_blocks(body, vec![ContentBlock::text(billing)]);

    session_id
}

/// Inject the Claude Code identity system block. Strict mode relocates the
/// client's own system into a leading user message and leaves only the identity
/// on the wire; non-strict prepends the identity ahead of the client system.
fn inject_identity(body: &mut CreateMessageParams, strict_system: bool) {
    if strict_system {
        let demoted = system_to_text(body.system.as_ref());
        if !demoted.trim().is_empty() {
            body.messages
                .insert(0, Message::new_text(Role::User, demoted));
        }
        body.system = Some(Value::Array(vec![serde_json::json!(ContentBlock::text(
            CC_IDENTITY
        ))]));
    } else {
        prepend_system_blocks(body, vec![ContentBlock::text(CC_IDENTITY)]);
    }
}

/// Ensure a structurally-valid `metadata.user_id` (Claude-Cloak style): keep a
/// valid inbound value, otherwise generate a fresh fake
/// `{device_id, account_uuid: "", session_id}`. Returns the session id for the
/// header (`None` when the preserved value carries no extractable session).
fn ensure_user_id(body: &mut CreateMessageParams) -> Option<String> {
    let meta = body.metadata.get_or_insert_with(Default::default);
    if let Some(existing) = meta.fields.get("user_id").cloned()
        && !existing.trim().is_empty()
        && let Some(session) = classify_user_id(&existing)
    {
        return session;
    }
    // 32 random bytes -> 64 hex chars, matching Claude-Cloak's device_id shape.
    let device_id = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let session_uuid = uuid::Uuid::new_v4();
    meta.fields.insert(
        "user_id".to_string(),
        crate::stealth::build_user_id_metadata(&device_id, "", &session_uuid),
    );
    Some(session_uuid.to_string())
}

/// Classify an inbound `user_id`, matching Claude-Cloak's `isValidUserId` /
/// `extractSessionId` exactly (user.ts): `Some(session)` = valid, keep it
/// (session is the non-empty `session_id`, if any); `None` = invalid, caller
/// regenerates. A JSON object is valid only when it carries STRING `device_id`
/// AND `session_id` fields — a bare `{}` or partial object is regenerated.
fn classify_user_id(existing: &str) -> Option<Option<String>> {
    if let Ok(value) = serde_json::from_str::<Value>(existing) {
        let Value::Object(map) = value else {
            return None;
        };
        let device_ok = map.get("device_id").and_then(Value::as_str).is_some();
        let session = map.get("session_id").and_then(Value::as_str);
        if device_ok && session.is_some() {
            return Some(session.filter(|s| !s.is_empty()).map(str::to_string));
        }
        return None;
    }
    // Legacy CLI shape: `^user_<64 hex>_account_`. Carries no JSON session id.
    if is_legacy_user_id(existing) {
        return Some(None);
    }
    None
}

/// Matches Claude-Cloak's legacy regex `^user_[a-fA-F0-9]{64}_account_`.
fn is_legacy_user_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("user_") else {
        return false;
    };
    if rest.len() < 64 + "_account_".len() {
        return false;
    }
    let (hex, tail) = rest.split_at(64);
    hex.bytes().all(|b| b.is_ascii_hexdigit()) && tail.starts_with("_account_")
}

/// The first user message's text (mirrors `request.rs::first_user_message_text`,
/// used here to sample the billing header before any system demotion).
fn first_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| match &m.content {
            MessageContent::Text { content } => content.clone(),
            MessageContent::Blocks { content } => content
                .iter()
                .find_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default(),
        })
        .unwrap_or_default()
}

/// Flatten a `system` value (String or array of text blocks) into plain text
/// for strict-mode demotion.
fn system_to_text(system: Option<&Value>) -> String {
    match system {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(v: Value) -> CreateMessageParams {
        serde_json::from_value(v).expect("valid CreateMessageParams")
    }

    fn system_texts(body: &CreateMessageParams) -> Vec<String> {
        match body.system.as_ref() {
            Some(Value::Array(blocks)) => blocks
                .iter()
                .map(|b| {
                    b.get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                })
                .collect(),
            Some(Value::String(s)) => vec![s.clone()],
            _ => vec![],
        }
    }

    #[test]
    fn synth_beta_never_emits_oauth_token() {
        let b = body(json!({"model": "claude-opus-4-8", "messages": []}));
        let beta = synth_beta("claude-opus-4-8", &b, &[]).unwrap();
        assert!(!beta.to_ascii_lowercase().contains("oauth-2025-04-20"));
        assert!(beta.contains("claude-code-20250219"));
        assert!(beta.contains("interleaved-thinking-2025-05-14"));
    }

    #[test]
    fn synth_beta_drops_claude_code_for_haiku() {
        let b = body(json!({"model": "claude-haiku-4-5", "messages": []}));
        let beta = synth_beta("claude-haiku-4-5", &b, &[]).unwrap();
        assert!(!beta.contains("claude-code-20250219"));
        assert!(beta.contains("interleaved-thinking-2025-05-14"));
    }

    #[test]
    fn synth_beta_feature_gates_and_extra() {
        let b = body(json!({
            "model": "claude-opus-4-8",
            "messages": [],
            "context_management": {"edits": []},
            "output_config": {"effort": "high"},
            "output_format": {"type": "json_schema", "schema": {}},
        }));
        let beta = synth_beta(
            "claude-opus-4-8",
            &b,
            &["custom-beta".into(), "custom-beta".into()],
        )
        .unwrap();
        assert!(beta.contains("context-management-2025-06-27"));
        assert!(beta.contains("effort-2025-11-24"));
        assert!(beta.contains("structured-outputs-2025-12-15"));
        // extra_beta appended once (dedup)
        assert_eq!(beta.matches("custom-beta").count(), 1);
    }

    #[test]
    fn cloak_strict_relocates_system_and_keeps_literal_cch() {
        let mut b = body(json!({
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": "hello there this is the user text"}],
            "system": "SECRET-CLIENT-SYSTEM",
        }));
        let session = cloak_messages_body(&mut b, "2.1.198", "testsalt", true);
        assert!(session.is_some(), "generated user_id yields a session id");

        let sys = system_texts(&b);
        // system[0] = billing block, system[1] = CC identity, nothing else.
        assert_eq!(sys.len(), 2);
        assert!(sys[0].starts_with("x-anthropic-billing-header:"));
        assert!(sys[0].contains("cc_version=2.1.198."));
        assert!(
            sys[0].contains("cch=00000;"),
            "cch stays literal, never rewritten"
        );
        assert!(!sys[0].contains("cch=00000;\";")); // sanity: it's the placeholder shape
        assert_eq!(sys[1], CC_IDENTITY);

        // Client system demoted into a leading user message.
        assert_eq!(b.messages.len(), 2);
        assert_eq!(b.messages[0].role, Role::User);
        match &b.messages[0].content {
            MessageContent::Text { content } => assert_eq!(content, "SECRET-CLIENT-SYSTEM"),
            _ => panic!("expected demoted text message"),
        }
    }

    #[test]
    fn cloak_billing_samples_original_first_user_text_not_demoted_system() {
        // The billing hash must be computed from the ORIGINAL first user text,
        // captured before strict demotion prepends the system as a user message.
        let user_text = "the genuine first user message with distinctive chars";
        let mut b = body(json!({
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": user_text}],
            "system": "a-very-different-system-string-that-would-change-the-hash",
        }));
        cloak_messages_body(&mut b, "2.1.198", "testsalt", true);

        let expected = claude_code_billing_header_from_sample(
            user_text,
            &StealthProfile {
                cli_version: "2.1.198".into(),
                billing_salt: "testsalt".into(),
                force_output_effort: None,
            },
        );
        assert_eq!(system_texts(&b)[0], expected);
    }

    #[test]
    fn cloak_non_strict_prepends_identity_before_client_system() {
        let mut b = body(json!({
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": "hi"}],
            "system": "CLIENT-SYS",
        }));
        cloak_messages_body(&mut b, "2.1.198", "salt", false);
        let sys = system_texts(&b);
        // [billing, CC_IDENTITY, client system]; no demotion into messages.
        assert_eq!(sys.len(), 3);
        assert!(sys[0].starts_with("x-anthropic-billing-header:"));
        assert_eq!(sys[1], CC_IDENTITY);
        assert_eq!(sys[2], "CLIENT-SYS");
        assert_eq!(b.messages.len(), 1);
    }

    #[test]
    fn ensure_user_id_preserves_valid_inbound_and_extracts_session() {
        let mut b = body(json!({
            "model": "claude-opus-4-8",
            "messages": [],
            "metadata": {"user_id": "{\"device_id\":\"abc\",\"account_uuid\":\"\",\"session_id\":\"sess-123\"}"},
        }));
        let session = ensure_user_id(&mut b);
        assert_eq!(session.as_deref(), Some("sess-123"));
        // Preserved verbatim.
        assert!(b.metadata.unwrap().fields["user_id"].contains("sess-123"));
    }

    #[test]
    fn ensure_user_id_generates_fake_when_missing() {
        let mut b = body(json!({"model": "claude-opus-4-8", "messages": []}));
        let session = ensure_user_id(&mut b).expect("fake yields session");
        let uid = b.metadata.unwrap().fields["user_id"].clone();
        let parsed: Value = serde_json::from_str(&uid).unwrap();
        assert_eq!(parsed["account_uuid"], "");
        assert_eq!(parsed["session_id"], session);
        assert_eq!(parsed["device_id"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn ensure_user_id_keeps_strict_legacy_shape_without_session() {
        // Exactly ^user_<64 hex>_account_...
        let legacy = format!("user_{}_account_x", "a".repeat(64));
        let mut b = body(json!({
            "model": "claude-opus-4-8",
            "messages": [],
            "metadata": {"user_id": legacy},
        }));
        let session = ensure_user_id(&mut b);
        assert_eq!(session, None, "legacy shape carries no session id");
        assert_eq!(b.metadata.unwrap().fields["user_id"], legacy);
    }

    #[test]
    fn ensure_user_id_regenerates_malformed_shapes() {
        // Cases Claude-Cloak treats as INVALID -> must regenerate a fake
        // {device_id, session_id} (session becomes Some).
        for bad in [
            "{}",                                  // empty object
            "{\"session_id\":\"s\"}",              // missing device_id
            "{\"device_id\":\"d\"}",               // missing session_id
            "not json at all",                     // random string
            "user_short_account_x",                // legacy but not 64 hex
            "prefix user_ and account_ scattered", // loose substring (old bug)
        ] {
            let mut b = body(json!({
                "model": "claude-opus-4-8",
                "messages": [],
                "metadata": {"user_id": bad},
            }));
            let session = ensure_user_id(&mut b).expect("regenerated -> has session");
            let uid = b.metadata.unwrap().fields["user_id"].clone();
            let parsed: Value = serde_json::from_str(&uid).unwrap();
            assert_eq!(parsed["session_id"], session, "input {bad:?}");
            assert_eq!(
                parsed["device_id"].as_str().unwrap().len(),
                64,
                "input {bad:?}"
            );
        }
    }

    #[test]
    fn cloak_injects_placeholder_for_system_only_request() {
        // Non-strict: a request that arrives with empty messages + a system must
        // get the "Continue." user turn (Claude-Cloak B4) so messages aren't
        // shipped empty and the billing sample is that placeholder.
        let mut b = body(json!({
            "model": "claude-opus-4-8",
            "messages": [],
            "system": "some system",
        }));
        cloak_messages_body(&mut b, "2.1.198", "salt", false);
        assert_eq!(b.messages.len(), 1);
        match &b.messages[0].content {
            MessageContent::Text { content } => assert_eq!(content, "Continue."),
            _ => panic!("expected placeholder user turn"),
        }
    }
}
