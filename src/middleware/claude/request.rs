use std::{
    env,
    hash::{DefaultHasher, Hash, Hasher},
    sync::LazyLock,
    vec,
};

use axum::{
    Json,
    extract::{FromRequest, Request},
};
use http::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    error::ClewdrError,
    middleware::claude::ClaudeContext,
    stealth::{self, StealthProfile},
    types::claude::{
        CacheControlEphemeral, CacheControlType, ContentBlock, CreateMessageParams, FallbackTarget,
        Message, MessageContent, OutputConfig, OutputEffort, Role, Thinking, ThinkingDisplay,
        Usage,
    },
};

const CLAUDE_CODE_ENTRYPOINT_ENV: &str = "CLAUDE_CODE_ENTRYPOINT";
pub(crate) fn prepend_system_blocks(body: &mut CreateMessageParams, blocks: Vec<ContentBlock>) {
    if blocks.is_empty() {
        return;
    }

    let mut prefixed = blocks
        .into_iter()
        .map(|block| json!(block))
        .collect::<Vec<_>>();
    match body.system.take() {
        Some(Value::String(text)) if !text.trim().is_empty() => {
            prefixed.push(json!(ContentBlock::text(text)));
        }
        Some(Value::Array(mut systems)) => {
            prefixed.append(&mut systems);
        }
        Some(Value::Null) | None => {}
        Some(other) => prefixed.push(other),
    }
    body.system = Some(Value::Array(prefixed));
}

fn first_user_message_text(messages: &[Message]) -> &str {
    messages
        .iter()
        .find(|message| message.role == Role::User)
        .and_then(|message| match &message.content {
            MessageContent::Text { content } => Some(content.as_str()),
            MessageContent::Blocks { content } => content.iter().find_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            }),
        })
        .unwrap_or_default()
}

fn sample_js_code_unit(text: &str, idx: usize) -> String {
    text.encode_utf16()
        .nth(idx)
        .map(|unit| String::from_utf16_lossy(&[unit]))
        .unwrap_or_else(|| "0".to_string())
}

pub(crate) fn claude_code_billing_header(messages: &[Message], profile: &StealthProfile) -> String {
    let first_text = first_user_message_text(messages);
    let sampled = [4, 7, 20]
        .into_iter()
        .map(|idx| sample_js_code_unit(first_text, idx))
        .collect::<String>();
    let version_hash = format!(
        "{:x}",
        Sha256::digest(format!(
            "{}{}{}",
            profile.billing_salt, sampled, profile.cli_version
        ))
    );
    let entrypoint = env::var(CLAUDE_CODE_ENTRYPOINT_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "cli".to_string());

    // cch = SHA256(full first user message text)[..5]
    let cch = format!("{:x}", Sha256::digest(first_text));
    let cch = &cch[..5.min(cch.len())];

    format!(
        "x-anthropic-billing-header: cc_version={}.{}; cc_entrypoint={entrypoint}; cch={cch};",
        profile.cli_version,
        &version_hash[..3]
    )
}

fn is_billing_header_text(text: &str) -> bool {
    text.trim_start()
        .to_ascii_lowercase()
        .starts_with("x-anthropic-billing-header:")
}

fn strip_leading_billing_header(text: &str) -> Option<String> {
    if !is_billing_header_text(text) {
        return Some(text.to_string());
    }

    let stripped = text
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n");
    let stripped = stripped.trim_start_matches('\n').trim().to_string();

    if stripped.is_empty() {
        None
    } else {
        Some(stripped)
    }
}

pub(crate) fn strip_billing_headers_from_system(body: &mut CreateMessageParams) {
    let Some(system) = body.system.take() else {
        return;
    };

    let stripped = match system {
        Value::String(text) => strip_leading_billing_header(&text).map(Value::String),
        Value::Array(systems) => {
            let filtered = systems
                .into_iter()
                .filter(|entry| match entry {
                    Value::String(text) => !is_billing_header_text(text),
                    Value::Object(obj)
                        if matches!(obj.get("type"), Some(Value::String(t)) if t == "text") =>
                    {
                        obj.get("text")
                            .and_then(Value::as_str)
                            .is_none_or(|text| !is_billing_header_text(text))
                    }
                    _ => true,
                })
                .collect::<Vec<_>>();
            Some(Value::Array(filtered))
        }
        other => Some(other),
    };

    body.system = stripped;
}

pub(crate) fn drop_empty_system(body: &mut CreateMessageParams) {
    let Some(system) = body.system.take() else {
        return;
    };

    let is_empty = match &system {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Array(systems) => systems.is_empty()
            || systems.iter().all(|entry| match entry {
                Value::Null => true,
                Value::String(text) => text.trim().is_empty(),
                Value::Object(obj) if matches!(obj.get("type"), Some(Value::String(t)) if t == "text") => {
                    obj.get("text")
                        .and_then(Value::as_str)
                        .is_none_or(|text| text.trim().is_empty())
                }
                _ => false,
            }),
        _ => false,
    };

    body.system = (!is_empty).then_some(system);
}

fn strip_empty_text_blocks(blocks: &mut Vec<ContentBlock>) {
    for block in blocks.iter_mut() {
        if let ContentBlock::SearchResult { content, .. } = block {
            strip_empty_text_blocks(content);
        }
    }

    blocks.retain(|block| {
        !matches!(
            block,
            ContentBlock::Text { text, .. } if text.is_empty()
        )
    });
}

fn has_non_empty_system(system: &Option<Value>) -> bool {
    match system {
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(systems)) => systems.iter().any(|entry| match entry {
            Value::String(text) => !text.trim().is_empty(),
            Value::Object(obj) if matches!(obj.get("type"), Some(Value::String(t)) if t == "text") => {
                obj.get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty())
            }
            Value::Null => false,
            other => !other.is_null(),
        }),
        Some(Value::Null) | None => false,
        Some(_) => true,
    }
}

pub(crate) fn drop_empty_message_text_blocks(body: &mut CreateMessageParams) {
    let had_messages = !body.messages.is_empty();
    body.messages
        .retain_mut(|message| match &mut message.content {
            MessageContent::Text { content } => !content.is_empty(),
            MessageContent::Blocks { content } => {
                strip_empty_text_blocks(content);
                !content.is_empty()
            }
        });

    if body.messages.is_empty() && had_messages && has_non_empty_system(&body.system) {
        fill_system_only_user_placeholder(body);
    }
}

pub(crate) fn fill_system_only_user_placeholder(body: &mut CreateMessageParams) {
    if body.messages.is_empty() && has_non_empty_system(&body.system) {
        body.messages
            .push(Message::new_text(Role::User, "Continue."));
    }
}

pub(crate) fn strip_ephemeral_scope_from_system(system: &mut Value) {
    let Some(items) = system.as_array_mut() else {
        return;
    };

    for item in items {
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        let Some(cache_control) = obj.get_mut("cache_control") else {
            continue;
        };
        let Some(cache_obj) = cache_control.as_object_mut() else {
            continue;
        };

        if let Some(ephemeral) = cache_obj.get_mut("ephemeral")
            && let Some(ephemeral_obj) = ephemeral.as_object_mut()
        {
            ephemeral_obj.remove("scope");
        }

        if matches!(cache_obj.get("type"), Some(Value::String(t)) if t == "ephemeral") {
            cache_obj.remove("scope");
        }
    }
}

pub(crate) fn extract_anthropic_beta_header(headers: &HeaderMap) -> Option<String> {
    let mut parts = Vec::new();
    for value in headers.get_all("anthropic-beta") {
        if let Ok(raw) = value.to_str() {
            for token in raw.split(',') {
                let token = token.trim();
                if !token.is_empty() {
                    parts.push(token.to_string());
                }
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(","))
    }
}

pub(crate) const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-06-01";

pub(crate) fn ensure_anthropic_beta_token(
    current: Option<String>,
    required: &str,
) -> Option<String> {
    let mut tokens = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for token in current
        .as_deref()
        .unwrap_or("")
        .split(',')
        .chain(std::iter::once(required))
    {
        let token = token.trim();
        if !token.is_empty() && seen.insert(token.to_ascii_lowercase()) {
            tokens.push(token.to_string());
        }
    }
    (!tokens.is_empty()).then(|| tokens.join(","))
}

/// Fable's conservative safety classifiers can refuse benign requests. Keep
/// the public model usable by always enabling Anthropic's same-request
/// server-side fallback to the only permitted launch target, Opus 4.8.
pub(crate) fn apply_fable_fallback(body: &mut CreateMessageParams) -> bool {
    if !matches_model_with_optional_date_suffix(&body.model, "claude-fable-5") {
        return false;
    }
    body.fallbacks = Some(vec![FallbackTarget {
        model: "claude-opus-4-8".to_string(),
    }]);
    true
}

/// Returns true when the request path targets `/v1/messages/count_tokens`.
/// Counting tokens should not write to cache; the upstream may also reject the
/// top-level `cache_control` field on this endpoint.
pub(crate) fn is_count_tokens_path(path: &str) -> bool {
    path.ends_with("/count_tokens")
}

/// Set the top-level `cache_control` breakpoint when the API key has
/// `auto_cache_enabled = true`. With this set, the server auto-places the
/// breakpoint on the last cacheable block and advances it as the conversation
/// grows. No-op when the flag is off or when no authenticated user is present.
pub(crate) fn apply_auto_cache(
    body: &mut CreateMessageParams,
    auth_user: Option<&crate::db::models::AuthenticatedUser>,
) {
    if auth_user.is_some_and(|u| u.auto_cache_enabled) {
        body.cache_control = Some(CacheControlEphemeral {
            type_: CacheControlType::Ephemeral,
            ttl: None,
        });
    }
}

/// Inject `metadata.user_id` if missing (for non-CLI clients).
/// Format: `user_{64hex}_account_{org_uuid}_session_{random_uuid}`
pub(crate) fn inject_metadata_user_id(
    body: &mut CreateMessageParams,
    auth_user: Option<&crate::db::models::AuthenticatedUser>,
) {
    // Check if metadata.user_id already exists
    if let Some(ref metadata) = body.metadata
        && metadata
            .fields
            .get("user_id")
            .is_some_and(|v| !v.is_empty())
    {
        return;
    }

    let Some(auth) = auth_user else {
        return;
    };

    // Deterministic user hex: HMAC-SHA256(billing_salt, api_key_id)
    let profile = stealth::global_profile().load();
    let key_id = auth.api_key_id.unwrap_or(0);
    let user_hex = format!(
        "{:x}",
        Sha256::digest(format!("{}{}", profile.billing_salt, key_id))
    );
    let session_uuid = uuid::Uuid::new_v4();
    // account part left empty (like relay/中转 scenario)
    let user_id = format!("user_{user_hex}_account__session_{session_uuid}");

    let metadata = body.metadata.get_or_insert_with(Default::default);
    metadata.fields.insert("user_id".to_string(), user_id);
}

fn cache_control_system_hash(body: &CreateMessageParams) -> Option<u64> {
    let cache_systems = body
        .system
        .as_ref()
        .and_then(Value::as_array)
        .map(|systems| {
            systems
                .iter()
                .filter(|s| s["cache_control"].as_object().is_some())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    (!cache_systems.is_empty()).then(|| {
        let mut hasher = DefaultHasher::new();
        cache_systems.hash(&mut hasher);
        hasher.finish()
    })
}

fn claude_code_session_affinity_hash(
    body: &CreateMessageParams,
    auth_user: Option<&crate::db::models::AuthenticatedUser>,
) -> Option<u64> {
    let metadata_user_id = body.metadata.as_ref()?.fields.get("user_id")?.trim();
    if metadata_user_id.is_empty() || !metadata_user_id.contains("_session_") {
        return None;
    }

    let mut hasher = DefaultHasher::new();
    "claude-code-session-affinity-v1".hash(&mut hasher);
    metadata_user_id.hash(&mut hasher);
    if let Some(auth) = auth_user {
        auth.user_id.hash(&mut hasher);
        auth.api_key_id.hash(&mut hasher);
    }
    Some(hasher.finish())
}

pub(crate) fn request_affinity_hash(
    body: &CreateMessageParams,
    auth_user: Option<&crate::db::models::AuthenticatedUser>,
) -> Option<u64> {
    // Claude Code emits a stable session id in metadata.user_id. Prefer it over
    // system-cache blocks so helper-model requests (for example Haiku) stay on
    // the same account even when their system prompt differs from the main turn.
    claude_code_session_affinity_hash(body, auth_user).or_else(|| cache_control_system_hash(body))
}

/// Normalize sampling parameters to keep Anthropic-compatible behavior across clients.
///
/// We intentionally discard `top_p` and `top_k` for all requests. In practice they
/// add little value for the target deployment model, while some clients/models send
/// combinations that Anthropic rejects.
///
/// When thinking is active (enabled or adaptive):
///   - `temperature` must be 1 or unset
///
/// For model families that dropped extended thinking budgets, legacy
/// `thinking.type=enabled` + `budget_tokens` is removed upstream: the OAuth
/// surface silently ignores it (client asks for thinking, gets none), and the
/// public API will 400. Rewrite to `thinking.type=adaptive` so pre-4.7 clients
/// transparently keep a thinking chain. We pin `display="summarized"` on the
/// rewritten request so older callers see an explicit thinking summary instead
/// of depending on upstream defaults, and explicitly pin
/// `output_config.effort="high"` when the legacy request did not set one.
///
/// Fable 5 requires thinking, so missing or explicitly disabled thinking is
/// normalized to adaptive thinking with a summarized display.
///
/// Operators can also enable an effort override from the admin settings page;
/// when enabled it overwrites `output_config.effort` on supported reasoning
/// requests and leaves other models untouched. Older Opus versions receive a
/// compatible fallback when the configured effort level is no longer supported.
pub(crate) fn normalize_sampling_params(body: &mut CreateMessageParams, profile: &StealthProfile) {
    body.top_p = None;
    body.top_k = None;

    let family = ReasoningFamily::detect(&body.model);

    let mut rewrote_legacy_thinking = false;
    if family.is_some_and(ReasoningFamily::requires_adaptive_thinking_rewrite) {
        let (rewritten, rewrote_legacy) = match body.thinking.take() {
            Some(Thinking::Enabled {
                budget_tokens: _,
                display,
            }) => (
                Some(Thinking::Adaptive {
                    display: Some(display.unwrap_or(ThinkingDisplay::Summarized)),
                }),
                true,
            ),
            other => (other, false),
        };
        rewrote_legacy_thinking = rewrote_legacy;
        body.thinking = rewritten;
    }

    let forced_thinking = family.is_some_and(ReasoningFamily::requires_thinking)
        && matches!(body.thinking, None | Some(Thinking::Disabled));
    if forced_thinking {
        body.thinking = Some(Thinking::adaptive_with_display(ThinkingDisplay::Summarized));
    }

    let thinking_active = matches!(
        body.thinking,
        Some(Thinking::Adaptive { .. }) | Some(Thinking::Enabled { .. })
    );
    if thinking_active && body.temperature != Some(1.0) {
        body.temperature = None;
    }

    if rewrote_legacy_thinking || forced_thinking {
        body.output_config
            .get_or_insert_with(default_output_config)
            .effort
            .get_or_insert(OutputEffort::High);
    }

    if let Some(force_output_effort) = profile
        .force_output_effort
        .as_ref()
        .and_then(|effort| family.map(|f| f.clamp_forced_effort(effort)))
    {
        body.output_config
            .get_or_insert_with(default_output_config)
            .effort = Some(force_output_effort);
    }
}

fn default_output_config() -> OutputConfig {
    OutputConfig {
        effort: None,
        format: None,
    }
}

/// Capability matrix for Claude model families with special reasoning behavior.
///
/// Adding a new model means: extend the enum, add a row to
/// [`ReasoningFamily::detect`], and slot it into the capability methods below.
/// Other callers only see `Option<ReasoningFamily>`, so unrelated models keep
/// transparent pass-through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReasoningFamily {
    Fable5,
    V4_5,
    V4_6,
    V4_7,
    V4_8,
}

impl ReasoningFamily {
    fn detect(model: &str) -> Option<Self> {
        const TABLE: &[(&str, ReasoningFamily)] = &[
            ("claude-fable-5", ReasoningFamily::Fable5),
            ("claude-opus-4-8", ReasoningFamily::V4_8),
            ("claude-opus-4-7", ReasoningFamily::V4_7),
            ("claude-opus-4-6", ReasoningFamily::V4_6),
            ("claude-opus-4-5", ReasoningFamily::V4_5),
        ];
        TABLE.iter().find_map(|&(prefix, family)| {
            matches_model_with_optional_date_suffix(model, prefix).then_some(family)
        })
    }

    /// 4.7+ removed extended thinking budgets, so legacy `enabled` requests must
    /// be rewritten to `adaptive` before going upstream.
    fn requires_adaptive_thinking_rewrite(self) -> bool {
        matches!(self, Self::Fable5 | Self::V4_7 | Self::V4_8)
    }

    fn requires_thinking(self) -> bool {
        matches!(self, Self::Fable5)
    }

    /// Map an admin-forced effort level onto the highest level this family
    /// actually supports.
    fn clamp_forced_effort(self, effort: &OutputEffort) -> OutputEffort {
        match self {
            Self::V4_5 => match effort {
                OutputEffort::Low => OutputEffort::Low,
                OutputEffort::Medium => OutputEffort::Medium,
                OutputEffort::High | OutputEffort::XHigh | OutputEffort::Max => OutputEffort::High,
            },
            Self::V4_6 => match effort {
                OutputEffort::Low => OutputEffort::Low,
                OutputEffort::Medium => OutputEffort::Medium,
                OutputEffort::High => OutputEffort::High,
                OutputEffort::XHigh | OutputEffort::Max => OutputEffort::Max,
            },
            Self::Fable5 | Self::V4_7 | Self::V4_8 => effort.clone(),
        }
    }
}

fn matches_model_with_optional_date_suffix(model: &str, prefix: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m == prefix
        || m.strip_prefix(&format!("{prefix}-"))
            .is_some_and(|s| s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit()))
}

/// Predefined test message for connection testing
static TEST_MESSAGE_CLAUDE: LazyLock<Message> =
    LazyLock::new(|| Message::new_blocks(Role::User, vec![ContentBlock::text("Hi")]));

static TEST_MESSAGE_TEXT: LazyLock<Message> = LazyLock::new(|| Message::new_text(Role::User, "Hi"));

/// Build a [`ClaudeContext`] from a fully-normalized request body.
///
/// Callers must have already applied the request-shaping pipeline
/// (`drop_empty_system`, `normalize_sampling_params`,
/// `strip_billing_headers_from_system`, `prepend_system_blocks`,
/// `strip_ephemeral_scope_from_system`, `inject_metadata_user_id`) so that the
/// affinity hash and token count reflect the final upstream payload.
pub(crate) fn build_claude_context(
    body: &CreateMessageParams,
    auth_user: Option<&crate::db::models::AuthenticatedUser>,
    anthropic_beta: Option<String>,
) -> ClaudeContext {
    let stream = body.stream.unwrap_or_default();
    let system_prompt_hash = request_affinity_hash(body, auth_user);
    let input_tokens = body.count_tokens();

    ClaudeContext {
        stream,
        system_prompt_hash,
        anthropic_beta,
        usage: Usage {
            input_tokens,
            output_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        },
        user_id: auth_user.map(|u| u.user_id),
        api_key_id: auth_user.and_then(|u| u.api_key_id),
        max_concurrent: auth_user.map(|u| u.max_concurrent),
        rpm_limit: auth_user.map(|u| u.rpm_limit),
        model_raw: body.model.clone(),
        request_id: uuid::Uuid::new_v4().to_string(),
        started_at: chrono::Utc::now(),
        weekly_budget_nanousd: auth_user.map(|u| u.weekly_budget_nanousd),
        monthly_budget_nanousd: auth_user.map(|u| u.monthly_budget_nanousd),
        bound_account_ids: auth_user
            .map(|u| u.bound_account_ids.clone())
            .unwrap_or_default(),
        selected_account_id: Default::default(),
        // Filled by the surface preprocess (Claude/OpenAI from_request)
        // from the snapshot auth stored in extensions. None here means
        // the request was not audited, or hasn't reached preprocess yet.
        audit: None,
    }
}

pub struct ClaudeCodePreprocess(pub CreateMessageParams, pub ClaudeContext);

impl<S> FromRequest<S> for ClaudeCodePreprocess
where
    S: Send + Sync,
{
    type Rejection = ClewdrError;

    async fn from_request(req: Request, _: &S) -> Result<Self, Self::Rejection> {
        let auth_user = req
            .extensions()
            .get::<crate::db::models::AuthenticatedUser>()
            .cloned();
        // Snapshot the audit context here — `Json::from_request` below
        // consumes `req`, so we cannot read extensions afterwards.
        let audit_snapshot = req
            .extensions()
            .get::<crate::db::models::RequestAuditSnapshot>()
            .cloned();
        let mut anthropic_beta = extract_anthropic_beta_header(req.headers());
        let is_count_tokens = is_count_tokens_path(req.uri().path());
        let Json(mut body) = Json::<CreateMessageParams>::from_request(req, &()).await?;

        drop_empty_system(&mut body);
        drop_empty_message_text_blocks(&mut body);

        // Load runtime settings once so request normalization and billing-header
        // generation see the same profile snapshot.
        let profile = stealth::global_profile().load();
        normalize_sampling_params(&mut body, &profile);

        // Check for test messages
        if !body.stream.unwrap_or_default()
            && (body.messages == vec![TEST_MESSAGE_CLAUDE.to_owned()]
                || body.messages == vec![TEST_MESSAGE_TEXT.to_owned()])
        {
            return Err(ClewdrError::TestMessage);
        }

        strip_billing_headers_from_system(&mut body);

        let system_prefixes = vec![ContentBlock::text(claude_code_billing_header(
            &body.messages,
            &profile,
        ))];
        prepend_system_blocks(&mut body, system_prefixes);

        if let Some(system) = body.system.as_mut() {
            strip_ephemeral_scope_from_system(system);
        }

        // count_tokens path skips automatic cache: that endpoint has no value
        // from caching and the top-level cache_control may be rejected upstream.
        if !is_count_tokens {
            apply_auto_cache(&mut body, auth_user.as_ref());
            if apply_fable_fallback(&mut body) {
                anthropic_beta =
                    ensure_anthropic_beta_token(anthropic_beta, SERVER_SIDE_FALLBACK_BETA);
            }
        } else {
            body.fallbacks = None;
        }

        // Compute the affinity hash from the pre-injection request so two
        // requests from the same client share an affinity slot. Injecting a
        // generated `metadata.user_id` first would make every anonymous
        // request hash uniquely and defeat caching.
        let mut context = build_claude_context(&body, auth_user.as_ref(), anthropic_beta);

        // If the API key has enhanced audit enabled, tag api_surface
        // for this entry point and attach the snapshot. Non-audited
        // keys leave `context.audit` as None and the billing layer
        // skips the sidecar write.
        if let Some(mut snapshot) = audit_snapshot {
            snapshot.api_surface = Some("anthropic");
            context.audit = Some(snapshot);
        }

        // Inject metadata.user_id if missing (for non-CLI clients like 2API).
        // The context above intentionally observes the pre-injection state.
        inject_metadata_user_id(&mut body, auth_user.as_ref());

        Ok(Self(body, context))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_billing_header_format() {
        let profile = StealthProfile::default();
        let messages = vec![Message::new_text(Role::User, "hey")];
        let header = claude_code_billing_header(&messages, &profile);

        // Check format structure
        assert!(header.starts_with("x-anthropic-billing-header: cc_version="));
        assert!(header.contains(&profile.cli_version));
        assert!(header.contains("cc_entrypoint=cli"));
        // cch should NOT be 00000 anymore
        assert!(!header.contains("cch=00000"));
        // cch should be 5 hex chars
        let cch_start = header.find("cch=").unwrap() + 4;
        let cch_end = header[cch_start..].find(';').unwrap() + cch_start;
        let cch = &header[cch_start..cch_end];
        assert_eq!(cch.len(), 5);
        assert!(cch.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn claude_code_billing_header_cch_is_deterministic() {
        let profile = StealthProfile::default();
        let messages = vec![Message::new_text(Role::User, "hey")];
        let h1 = claude_code_billing_header(&messages, &profile);
        let h2 = claude_code_billing_header(&messages, &profile);
        assert_eq!(h1, h2);
    }

    #[test]
    fn claude_code_billing_header_cch_varies_with_content() {
        let profile = StealthProfile::default();
        let m1 = vec![Message::new_text(Role::User, "hello world")];
        let m2 = vec![Message::new_text(Role::User, "goodbye world")];
        let h1 = claude_code_billing_header(&m1, &profile);
        let h2 = claude_code_billing_header(&m2, &profile);
        // cch values should differ
        let extract_cch = |h: &str| {
            let start = h.find("cch=").unwrap() + 4;
            let end = h[start..].find(';').unwrap() + start;
            h[start..end].to_string()
        };
        assert_ne!(extract_cch(&h1), extract_cch(&h2));
    }

    #[test]
    fn prepend_system_blocks_keeps_billing_before_original() {
        let mut body = CreateMessageParams {
            messages: vec![Message::new_text(Role::User, "hey")],
            model: "claude-sonnet-4-5".to_string(),
            system: Some(json!("original system")),
            ..Default::default()
        };

        prepend_system_blocks(&mut body, vec![ContentBlock::text("billing")]);

        let systems = body.system.unwrap().as_array().cloned().unwrap();
        let texts = systems
            .iter()
            .map(|value| value["text"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["billing", "original system"]);
    }

    #[test]
    fn strip_billing_headers_from_string_system() {
        let mut body = CreateMessageParams {
            messages: vec![Message::new_text(Role::User, "hey")],
            model: "claude-sonnet-4-5".to_string(),
            system: Some(json!(
                "x-anthropic-billing-header: cc_version=2.1.117.320; cch=abcde;"
            )),
            ..Default::default()
        };

        strip_billing_headers_from_system(&mut body);

        assert!(body.system.is_none());
    }

    #[test]
    fn strip_billing_header_preserves_following_string_system_text() {
        let mut body = CreateMessageParams {
            messages: vec![Message::new_text(Role::User, "hey")],
            model: "claude-sonnet-4-5".to_string(),
            system: Some(json!(
                "x-anthropic-billing-header: cc_version=2.1.117.320; cch=abcde;\nYou are Claude.\nKeep this instruction."
            )),
            ..Default::default()
        };

        strip_billing_headers_from_system(&mut body);

        assert_eq!(
            body.system,
            Some(json!("You are Claude.\nKeep this instruction."))
        );
    }

    #[test]
    fn strip_billing_headers_from_array_system() {
        let mut body = CreateMessageParams {
            messages: vec![Message::new_text(Role::User, "hey")],
            model: "claude-sonnet-4-5".to_string(),
            system: Some(json!([
                {
                    "type": "text",
                    "text": "x-anthropic-billing-header: cc_version=2.1.117.320; cch=abcde;"
                },
                {
                    "type": "text",
                    "text": "keep me",
                    "cache_control": { "type": "ephemeral" }
                }
            ])),
            ..Default::default()
        };

        strip_billing_headers_from_system(&mut body);

        let systems = body.system.unwrap().as_array().cloned().unwrap();
        assert_eq!(systems.len(), 1);
        assert_eq!(systems[0]["text"].as_str(), Some("keep me"));
        assert_eq!(
            systems[0]["cache_control"]["type"].as_str(),
            Some("ephemeral")
        );
    }

    #[test]
    fn drop_empty_message_text_blocks_removes_empty_text_and_empty_messages() {
        let mut body = CreateMessageParams {
            messages: vec![
                Message::new_text(Role::User, ""),
                Message::new_blocks(
                    Role::User,
                    vec![ContentBlock::text(""), ContentBlock::text("keep")],
                ),
                Message::new_blocks(Role::Assistant, vec![ContentBlock::text("")]),
            ],
            model: "claude-sonnet-4-5".to_string(),
            ..Default::default()
        };

        drop_empty_message_text_blocks(&mut body);

        assert_eq!(body.messages.len(), 1);
        let MessageContent::Blocks { content } = &body.messages[0].content else {
            panic!("expected block message");
        };
        assert_eq!(content.len(), 1);
        assert_eq!(content[0], ContentBlock::text("keep"));
    }

    #[test]
    fn drop_empty_message_text_blocks_keeps_system_only_placeholder() {
        let mut body = CreateMessageParams {
            messages: vec![Message::new_blocks(
                Role::User,
                vec![ContentBlock::text("")],
            )],
            model: "claude-sonnet-4-5".to_string(),
            system: Some(json!([{
                "type": "text",
                "text": "Answer from these instructions."
            }])),
            ..Default::default()
        };

        drop_empty_message_text_blocks(&mut body);

        assert_eq!(
            body.messages,
            vec![Message::new_text(Role::User, "Continue.")]
        );
    }

    #[test]
    fn drop_empty_message_text_blocks_cleans_nested_search_results() {
        let mut body = CreateMessageParams {
            messages: vec![Message::new_blocks(
                Role::User,
                vec![ContentBlock::SearchResult {
                    content: vec![ContentBlock::text(""), ContentBlock::text("nested")],
                    source: "https://example.com".to_string(),
                    title: "result".to_string(),
                    cache_control: None,
                    citations: None,
                }],
            )],
            model: "claude-sonnet-4-5".to_string(),
            ..Default::default()
        };

        drop_empty_message_text_blocks(&mut body);

        let MessageContent::Blocks { content } = &body.messages[0].content else {
            panic!("expected block message");
        };
        let ContentBlock::SearchResult { content, .. } = &content[0] else {
            panic!("expected search result");
        };
        assert_eq!(content, &vec![ContentBlock::text("nested")]);
    }

    fn body_with_session_and_system(session: &str, system_text: &str) -> CreateMessageParams {
        CreateMessageParams {
            model: "claude-sonnet-4-5".to_string(),
            messages: vec![Message::new_text(Role::User, "hey")],
            system: Some(json!([
                {
                    "type": "text",
                    "text": system_text,
                    "cache_control": { "type": "ephemeral" }
                }
            ])),
            metadata: Some(crate::types::claude::Metadata {
                fields: std::collections::HashMap::from([(
                    "user_id".to_string(),
                    session.to_string(),
                )]),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn session_affinity_ignores_system_prompt_differences() {
        let session = "user_abc_account__session_9c37db4e-f0c3-44fd-9054-f182c7103381";
        let haiku = body_with_session_and_system(session, "haiku helper prompt");
        let opus = body_with_session_and_system(session, "main opus prompt");

        assert_eq!(
            request_affinity_hash(&haiku, None),
            request_affinity_hash(&opus, None)
        );
    }

    #[test]
    fn build_context_runs_before_inject_metadata_user_id() {
        // Regression: build_claude_context must observe the pre-injection
        // body so request_affinity_hash falls back to the stable
        // cache-control system hash. If a generated metadata.user_id
        // (which contains a random `_session_<uuid>`) leaks into the
        // affinity hash, every anonymous request would hash uniquely
        // and the per-account affinity cache would be defeated.
        //
        // Simulate the injection by setting metadata.user_id directly so
        // the test does not depend on the global stealth profile (which
        // is only initialized in the live binary).
        fn fresh_body() -> CreateMessageParams {
            CreateMessageParams {
                model: "claude-sonnet-4-6".to_string(),
                messages: vec![Message::new_text(Role::User, "hi")],
                system: Some(json!([
                    {
                        "type": "text",
                        "text": "stable prompt",
                        "cache_control": { "type": "ephemeral" }
                    }
                ])),
                ..Default::default()
            }
        }
        fn inject_synthetic_session(body: &mut CreateMessageParams, uuid: &str) {
            let metadata = body.metadata.get_or_insert_with(Default::default);
            metadata.fields.insert(
                "user_id".to_string(),
                format!("user_hex_account__session_{uuid}"),
            );
        }

        // Pre-injection: two identical bodies must agree on affinity.
        let pre1 = fresh_body();
        let pre2 = fresh_body();
        let pre_hash1 = build_claude_context(&pre1, None, None).system_prompt_hash;
        let pre_hash2 = build_claude_context(&pre2, None, None).system_prompt_hash;
        assert!(pre_hash1.is_some());
        assert_eq!(pre_hash1, pre_hash2);

        // Post-injection: two bodies with *different* synthetic session
        // uuids hash to different affinity slots. This is the exact
        // regression direction we are guarding against — building the
        // context after inject_metadata_user_id would land here.
        let mut post1 = fresh_body();
        inject_synthetic_session(&mut post1, "11111111-1111-1111-1111-111111111111");
        let mut post2 = fresh_body();
        inject_synthetic_session(&mut post2, "22222222-2222-2222-2222-222222222222");
        let post_hash1 = build_claude_context(&post1, None, None).system_prompt_hash;
        let post_hash2 = build_claude_context(&post2, None, None).system_prompt_hash;
        assert!(post_hash1.is_some());
        assert_ne!(post_hash1, post_hash2);
    }

    #[test]
    fn non_session_metadata_falls_back_to_system_hash() {
        let first = body_with_session_and_system("user-without-session", "first prompt");
        let second = body_with_session_and_system("user-without-session", "second prompt");

        assert_ne!(
            request_affinity_hash(&first, None),
            request_affinity_hash(&second, None)
        );
    }

    fn make_body(
        thinking: Option<Thinking>,
        temp: Option<f32>,
        top_p: Option<f32>,
        top_k: Option<u32>,
    ) -> CreateMessageParams {
        CreateMessageParams {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![Message::new_text(Role::User, "hi")],
            thinking,
            temperature: temp,
            top_p,
            top_k,
            ..Default::default()
        }
    }

    #[test]
    fn normalize_thinking_adaptive_strips_invalid_params() {
        let mut body = make_body(Some(Thinking::adaptive()), Some(0.7), Some(0.9), Some(40));
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert_eq!(body.temperature, None);
        assert_eq!(body.top_p, None);
        assert_eq!(body.top_k, None);
    }

    #[test]
    fn normalize_thinking_adaptive_keeps_valid_params() {
        let mut body = make_body(Some(Thinking::adaptive()), Some(1.0), Some(0.95), None);
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert_eq!(body.temperature, Some(1.0));
        assert_eq!(body.top_p, None);
        assert_eq!(body.top_k, None);
    }

    #[test]
    fn normalize_thinking_enabled_strips_invalid_params() {
        let mut body = make_body(Some(Thinking::new(4096)), Some(0.5), Some(0.8), Some(10));
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert_eq!(body.temperature, None);
        assert_eq!(body.top_p, None);
        assert_eq!(body.top_k, None);
    }

    #[test]
    fn normalize_thinking_strips_top_p_above_one() {
        let mut body = make_body(Some(Thinking::adaptive()), None, Some(1.5), None);
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert_eq!(body.top_p, None);
    }

    #[test]
    fn normalize_thinking_keeps_top_p_one() {
        let mut body = make_body(Some(Thinking::adaptive()), None, Some(1.0), None);
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert_eq!(body.top_p, None);
    }

    #[test]
    fn normalize_no_thinking_strips_top_p_and_top_k() {
        let mut body = make_body(None, Some(0.7), Some(0.9), Some(40));
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert_eq!(body.temperature, Some(0.7));
        assert_eq!(body.top_p, None);
        assert_eq!(body.top_k, None);
    }

    #[test]
    fn normalize_thinking_disabled_strips_top_p_and_top_k() {
        let mut body = make_body(Some(Thinking::Disabled), Some(0.7), Some(0.9), Some(40));
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert_eq!(body.temperature, Some(0.7));
        assert_eq!(body.top_p, None);
        assert_eq!(body.top_k, None);
    }

    #[test]
    fn normalize_opus_4_7_rewrites_enabled_thinking_to_adaptive() {
        let mut body = make_body(Some(Thinking::new(8000)), Some(0.7), None, None);
        body.model = "claude-opus-4-7".to_string();
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert!(matches!(
            body.thinking,
            Some(Thinking::Adaptive {
                display: Some(ThinkingDisplay::Summarized)
            })
        ));
        assert!(matches!(
            body.output_config,
            Some(OutputConfig {
                effort: Some(OutputEffort::High),
                ..
            })
        ));
        assert_eq!(body.temperature, None);
    }

    #[test]
    fn normalize_opus_4_7_with_date_suffix_rewrites_thinking() {
        let mut body = make_body(Some(Thinking::new(32000)), None, None, None);
        body.model = "claude-opus-4-7-20260416".to_string();
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert!(matches!(
            body.thinking,
            Some(Thinking::Adaptive {
                display: Some(ThinkingDisplay::Summarized)
            })
        ));
        assert!(matches!(
            body.output_config,
            Some(OutputConfig {
                effort: Some(OutputEffort::High),
                ..
            })
        ));
    }

    #[test]
    fn normalize_opus_4_7_leaves_adaptive_untouched() {
        let mut body = make_body(Some(Thinking::adaptive()), Some(1.0), None, None);
        body.model = "claude-opus-4-7".to_string();
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert!(matches!(body.thinking, Some(Thinking::Adaptive { .. })));
        assert_eq!(body.temperature, Some(1.0));
    }

    #[test]
    fn normalize_opus_4_7_preserves_explicit_enabled_display() {
        let mut body = make_body(
            Some(Thinking::Enabled {
                budget_tokens: 8000,
                display: Some(ThinkingDisplay::Omitted),
            }),
            None,
            None,
            None,
        );
        body.model = "claude-opus-4-7".to_string();
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert!(matches!(
            body.thinking,
            Some(Thinking::Adaptive {
                display: Some(ThinkingDisplay::Omitted)
            })
        ));
    }

    #[test]
    fn normalize_opus_4_6_keeps_enabled_thinking() {
        let mut body = make_body(Some(Thinking::new(8000)), None, None, None);
        body.model = "claude-opus-4-6".to_string();
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert!(matches!(body.thinking, Some(Thinking::Enabled { .. })));
    }

    #[test]
    fn normalize_opus_4_7_with_invalid_suffix_skips_rewrite() {
        let mut body = make_body(Some(Thinking::new(8000)), None, None, None);
        body.model = "claude-opus-4-7-preview1".to_string();
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert!(matches!(body.thinking, Some(Thinking::Enabled { .. })));
    }

    #[test]
    fn normalize_keeps_explicit_effort_when_rewriting_opus_4_7() {
        let mut body = make_body(Some(Thinking::new(8000)), None, None, None);
        body.model = "claude-opus-4-7".to_string();
        body.output_config = Some(OutputConfig {
            effort: Some(OutputEffort::Max),
            format: None,
        });
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert!(matches!(
            body.output_config,
            Some(OutputConfig {
                effort: Some(OutputEffort::Max),
                ..
            })
        ));
    }

    #[test]
    fn normalize_forced_effort_overrides_supported_opus_requests() {
        let mut body = make_body(None, Some(0.7), None, None);
        body.model = "claude-opus-4-6".to_string();
        let profile = StealthProfile {
            force_output_effort: Some(OutputEffort::XHigh),
            ..StealthProfile::default()
        };
        normalize_sampling_params(&mut body, &profile);
        assert_eq!(body.temperature, Some(0.7));
        assert!(matches!(
            body.output_config,
            Some(OutputConfig {
                effort: Some(OutputEffort::Max),
                ..
            })
        ));
    }

    #[test]
    fn normalize_forced_effort_keeps_all_levels_for_opus_4_7() {
        let mut body = make_body(None, Some(0.7), None, None);
        body.model = "claude-opus-4-7-20260416".to_string();
        let profile = StealthProfile {
            force_output_effort: Some(OutputEffort::XHigh),
            ..StealthProfile::default()
        };
        normalize_sampling_params(&mut body, &profile);
        assert!(matches!(
            body.output_config,
            Some(OutputConfig {
                effort: Some(OutputEffort::XHigh),
                ..
            })
        ));
    }

    #[test]
    fn normalize_forced_effort_downgrades_unsupported_opus_4_5_levels() {
        let mut body = make_body(None, Some(0.7), None, None);
        body.model = "claude-opus-4-5".to_string();
        let profile = StealthProfile {
            force_output_effort: Some(OutputEffort::Max),
            ..StealthProfile::default()
        };
        normalize_sampling_params(&mut body, &profile);
        assert!(matches!(
            body.output_config,
            Some(OutputConfig {
                effort: Some(OutputEffort::High),
                ..
            })
        ));
    }

    #[test]
    fn normalize_forced_effort_leaves_non_opus_requests_untouched() {
        let mut body = make_body(None, Some(0.7), None, None);
        body.model = "claude-sonnet-4-6".to_string();
        let profile = StealthProfile {
            force_output_effort: Some(OutputEffort::XHigh),
            ..StealthProfile::default()
        };
        normalize_sampling_params(&mut body, &profile);
        assert_eq!(body.temperature, Some(0.7));
        assert!(body.output_config.is_none());
    }

    #[test]
    fn normalize_opus_4_8_rewrites_enabled_thinking_to_adaptive() {
        let mut body = make_body(Some(Thinking::new(8000)), Some(0.7), None, None);
        body.model = "claude-opus-4-8-20260528".to_string();
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert!(matches!(
            body.thinking,
            Some(Thinking::Adaptive {
                display: Some(ThinkingDisplay::Summarized)
            })
        ));
        assert!(matches!(
            body.output_config,
            Some(OutputConfig {
                effort: Some(OutputEffort::High),
                ..
            })
        ));
    }

    #[test]
    fn normalize_forced_effort_keeps_all_levels_for_opus_4_8() {
        let mut body = make_body(None, Some(0.7), None, None);
        body.model = "claude-opus-4-8".to_string();
        let profile = StealthProfile {
            force_output_effort: Some(OutputEffort::XHigh),
            ..StealthProfile::default()
        };
        normalize_sampling_params(&mut body, &profile);
        assert!(matches!(
            body.output_config,
            Some(OutputConfig {
                effort: Some(OutputEffort::XHigh),
                ..
            })
        ));
    }

    #[test]
    fn normalize_fable_5_forces_missing_thinking_on() {
        let mut body = make_body(None, Some(0.7), None, None);
        body.model = "claude-fable-5".to_string();
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert!(matches!(
            body.thinking,
            Some(Thinking::Adaptive {
                display: Some(ThinkingDisplay::Summarized)
            })
        ));
        assert!(matches!(
            body.output_config,
            Some(OutputConfig {
                effort: Some(OutputEffort::High),
                ..
            })
        ));
        assert_eq!(body.temperature, None);
    }

    #[test]
    fn normalize_fable_5_forces_disabled_thinking_on() {
        let mut body = make_body(Some(Thinking::Disabled), Some(1.0), None, None);
        body.model = "claude-fable-5-20260609".to_string();
        normalize_sampling_params(&mut body, &StealthProfile::default());
        assert!(matches!(
            body.thinking,
            Some(Thinking::Adaptive {
                display: Some(ThinkingDisplay::Summarized)
            })
        ));
        assert_eq!(body.temperature, Some(1.0));
    }

    #[test]
    fn normalize_fable_5_rewrites_legacy_thinking_and_honors_effort_override() {
        let mut body = make_body(Some(Thinking::new(8000)), Some(0.7), None, None);
        body.model = "claude-fable-5".to_string();
        let profile = StealthProfile {
            force_output_effort: Some(OutputEffort::XHigh),
            ..StealthProfile::default()
        };
        normalize_sampling_params(&mut body, &profile);
        assert!(matches!(
            body.thinking,
            Some(Thinking::Adaptive {
                display: Some(ThinkingDisplay::Summarized)
            })
        ));
        assert!(matches!(
            body.output_config,
            Some(OutputConfig {
                effort: Some(OutputEffort::XHigh),
                ..
            })
        ));
        assert_eq!(body.temperature, None);
    }

    #[test]
    fn reasoning_family_detect_covers_bare_and_dated_ids() {
        assert_eq!(
            ReasoningFamily::detect("claude-fable-5"),
            Some(ReasoningFamily::Fable5)
        );
        assert_eq!(
            ReasoningFamily::detect("claude-fable-5-20260609"),
            Some(ReasoningFamily::Fable5)
        );
        assert_eq!(
            ReasoningFamily::detect("claude-opus-4-8"),
            Some(ReasoningFamily::V4_8)
        );
        assert_eq!(
            ReasoningFamily::detect("claude-opus-4-7-20260416"),
            Some(ReasoningFamily::V4_7)
        );
        assert_eq!(
            ReasoningFamily::detect("claude-opus-4-6"),
            Some(ReasoningFamily::V4_6)
        );
        assert_eq!(
            ReasoningFamily::detect("claude-opus-4-5"),
            Some(ReasoningFamily::V4_5)
        );
        assert_eq!(ReasoningFamily::detect("claude-sonnet-4-6"), None);
        assert_eq!(ReasoningFamily::detect("claude-fable-5-preview1"), None);
        assert_eq!(ReasoningFamily::detect("claude-opus-4-7-preview1"), None);
    }

    fn auth_user_with_auto_cache(enabled: bool) -> crate::db::models::AuthenticatedUser {
        crate::db::models::AuthenticatedUser {
            user_id: 1,
            username: "u".to_string(),
            role: "user".to_string(),
            api_key_id: Some(42),
            policy_id: 1,
            max_concurrent: 10,
            rpm_limit: 60,
            weekly_budget_nanousd: 0,
            monthly_budget_nanousd: 0,
            bound_account_ids: Vec::new(),
            auto_cache_enabled: enabled,
            enhanced_audit_enabled: false,
        }
    }

    fn simple_body() -> CreateMessageParams {
        CreateMessageParams {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![Message::new_text(Role::User, "hi")],
            ..Default::default()
        }
    }

    async fn ensure_test_stealth_profile() {
        let pool = crate::db::init_pool(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        crate::stealth::init_stealth_profile(&pool).await;
    }

    #[test]
    fn is_count_tokens_path_recognizes_suffix() {
        assert!(is_count_tokens_path("/v1/messages/count_tokens"));
        assert!(is_count_tokens_path("/proxy/v1/messages/count_tokens"));
        assert!(!is_count_tokens_path("/v1/messages"));
        assert!(!is_count_tokens_path("/v1/messages/count_tokens/extra"));
    }

    #[test]
    fn apply_auto_cache_off_omits_field() {
        let user = auth_user_with_auto_cache(false);
        let mut body = simple_body();
        apply_auto_cache(&mut body, Some(&user));
        assert!(body.cache_control.is_none());
        let value = serde_json::to_value(&body).unwrap();
        assert!(value.get("cache_control").is_none());
    }

    #[test]
    fn apply_auto_cache_on_sets_top_level_ephemeral() {
        let user = auth_user_with_auto_cache(true);
        let mut body = simple_body();
        apply_auto_cache(&mut body, Some(&user));
        let cc = body.cache_control.as_ref().expect("cache_control set");
        assert!(matches!(cc.type_, CacheControlType::Ephemeral));
        assert!(cc.ttl.is_none());
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(
            value["cache_control"],
            serde_json::json!({ "type": "ephemeral" })
        );
    }

    #[test]
    fn apply_auto_cache_no_auth_user_is_noop() {
        let mut body = simple_body();
        apply_auto_cache(&mut body, None);
        assert!(body.cache_control.is_none());
    }

    #[test]
    fn apply_fable_fallback_forces_opus_4_8() {
        let mut body = simple_body();
        body.model = "claude-fable-5".to_string();
        body.fallbacks = Some(vec![FallbackTarget {
            model: "some-other-model".to_string(),
        }]);
        assert!(apply_fable_fallback(&mut body));
        assert_eq!(body.fallbacks.as_ref().unwrap()[0].model, "claude-opus-4-8");
    }

    #[test]
    fn apply_fable_fallback_ignores_other_models() {
        let mut body = simple_body();
        assert!(!apply_fable_fallback(&mut body));
        assert!(body.fallbacks.is_none());
    }

    #[test]
    fn ensure_anthropic_beta_token_appends_once() {
        assert_eq!(
            ensure_anthropic_beta_token(
                Some("context-management-2025-06-27".to_string()),
                SERVER_SIDE_FALLBACK_BETA,
            )
            .as_deref(),
            Some("context-management-2025-06-27,server-side-fallback-2026-06-01")
        );
        assert_eq!(
            ensure_anthropic_beta_token(
                Some("SERVER-SIDE-FALLBACK-2026-06-01".to_string()),
                SERVER_SIDE_FALLBACK_BETA,
            )
            .as_deref(),
            Some("SERVER-SIDE-FALLBACK-2026-06-01")
        );
    }

    #[tokio::test]
    async fn claude_messages_preprocess_enables_fable_fallback() {
        ensure_test_stealth_profile().await;
        let request = Request::builder()
            .uri("/v1/messages")
            .header("content-type", "application/json")
            .header("anthropic-beta", "context-management-2025-06-27")
            .body(axum::body::Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "model": "claude-fable-5",
                    "max_tokens": 64,
                    "messages": [{ "role": "user", "content": "hello fallback" }]
                }))
                .unwrap(),
            ))
            .unwrap();
        let ClaudeCodePreprocess(body, context) = ClaudeCodePreprocess::from_request(request, &())
            .await
            .unwrap();
        assert_eq!(body.fallbacks.as_ref().unwrap()[0].model, "claude-opus-4-8");
        assert_eq!(
            context.anthropic_beta.as_deref(),
            Some("context-management-2025-06-27,server-side-fallback-2026-06-01")
        );
    }

    #[tokio::test]
    async fn claude_count_tokens_preprocess_strips_fallbacks() {
        ensure_test_stealth_profile().await;
        let request = Request::builder()
            .uri("/v1/messages/count_tokens")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "model": "claude-fable-5",
                    "max_tokens": 64,
                    "fallbacks": [{ "model": "claude-opus-4-8" }],
                    "messages": [{ "role": "user", "content": "hello fallback" }]
                }))
                .unwrap(),
            ))
            .unwrap();
        let ClaudeCodePreprocess(body, context) = ClaudeCodePreprocess::from_request(request, &())
            .await
            .unwrap();
        assert!(body.fallbacks.is_none());
        assert!(context.anthropic_beta.is_none());
    }
}
