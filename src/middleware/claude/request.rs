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
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    error::ClewdrError,
    middleware::claude::ClaudeContext,
    stealth::{self, StealthProfile},
    types::claude::{
        ContentBlock, CreateMessageParams, Message, MessageContent, Role, Thinking, Usage,
    },
};

const CLAUDE_CODE_ENTRYPOINT_ENV: &str = "CLAUDE_CODE_ENTRYPOINT";

fn prepend_system_blocks(body: &mut CreateMessageParams, blocks: Vec<ContentBlock>) {
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

fn claude_code_billing_header(messages: &[Message], profile: &StealthProfile) -> String {
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

fn drop_empty_system(body: &mut CreateMessageParams) {
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

fn strip_ephemeral_scope_from_system(system: &mut Value) {
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

/// Inject `metadata.user_id` if missing (for non-CLI clients).
/// Format: `user_{64hex}_account_{org_uuid}_session_{random_uuid}`
fn inject_metadata_user_id(
    body: &mut CreateMessageParams,
    auth_user: Option<&crate::db::models::AuthenticatedUser>,
) {
    // Check if metadata.user_id already exists
    if let Some(ref metadata) = body.metadata {
        if metadata.fields.get("user_id").is_some_and(|v| !v.is_empty()) {
            return;
        }
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

/// Predefined test message for connection testing
static TEST_MESSAGE_CLAUDE: LazyLock<Message> =
    LazyLock::new(|| Message::new_blocks(Role::User, vec![ContentBlock::text("Hi")]));

static TEST_MESSAGE_TEXT: LazyLock<Message> =
    LazyLock::new(|| Message::new_text(Role::User, "Hi"));

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
        let client_session_id = req
            .headers()
            .get("x-claude-code-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let session_id = client_session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let Json(mut body) = Json::<CreateMessageParams>::from_request(req, &()).await?;

        if body.model.ends_with("-thinking") {
            body.model = body.model.trim_end_matches("-thinking").to_string();
            body.thinking.get_or_insert(Thinking::new(4096));
        }
        drop_empty_system(&mut body);

        if body.temperature.is_some() {
            body.top_p = None;
        }

        // Check for test messages
        if !body.stream.unwrap_or_default()
            && (body.messages == vec![TEST_MESSAGE_CLAUDE.to_owned()]
                || body.messages == vec![TEST_MESSAGE_TEXT.to_owned()])
        {
            return Err(ClewdrError::TestMessage);
        }

        let stream = body.stream.unwrap_or_default();

        // Load stealth profile for billing header generation
        let profile = stealth::global_profile().load();

        let system_prefixes = vec![ContentBlock::text(claude_code_billing_header(
            &body.messages,
            &profile,
        ))];
        prepend_system_blocks(&mut body, system_prefixes);

        if let Some(system) = body.system.as_mut() {
            strip_ephemeral_scope_from_system(system);
        }

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
        let system_prompt_hash = (!cache_systems.is_empty()).then(|| {
            let mut hasher = DefaultHasher::new();
            cache_systems.hash(&mut hasher);
            hasher.finish()
        });

        let input_tokens = body.count_tokens();

        // Inject metadata.user_id if missing (for non-CLI clients like 2API)
        inject_metadata_user_id(&mut body, auth_user.as_ref());

        let context = ClaudeContext {
            stream,
            system_prompt_hash,
            usage: Usage {
                input_tokens,
                output_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
            user_id: auth_user.as_ref().map(|u| u.user_id),
            api_key_id: auth_user.as_ref().and_then(|u| u.api_key_id),
            max_concurrent: auth_user.as_ref().map(|u| u.max_concurrent),
            rpm_limit: auth_user.as_ref().map(|u| u.rpm_limit),
            model_raw: body.model.clone(),
            request_id: uuid::Uuid::new_v4().to_string(),
            started_at: chrono::Utc::now(),
            weekly_budget_nanousd: auth_user.as_ref().map(|u| u.weekly_budget_nanousd),
            monthly_budget_nanousd: auth_user.as_ref().map(|u| u.monthly_budget_nanousd),
            session_id,
        };

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

        prepend_system_blocks(
            &mut body,
            vec![ContentBlock::text("billing")],
        );

        let systems = body.system.unwrap().as_array().cloned().unwrap();
        let texts = systems
            .iter()
            .map(|value| value["text"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["billing", "original system"]);
    }
}
