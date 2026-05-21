//! OpenAI Chat Completions API wire types.
//!
//! Mirrors the public schema for `POST /v1/chat/completions`, the streaming
//! `chat.completion.chunk` envelope, and the error body. These types only
//! handle serialization — translation to and from the internal Claude shape
//! lives in `crate::middleware::openai`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// Top-level `POST /v1/chat/completions` body.
///
/// Unknown fields are absorbed into [`Self::extra`] so that future OpenAI
/// additions do not break deserialization. Fields the translation layer
/// silently ignores (`frequency_penalty`, `presence_penalty`, `logit_bias`,
/// `seed`, `service_tier`, `store`, non-string `metadata` entries, …) are
/// still surfaced here so logging and observability can see them.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StringOrVec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ChatToolChoice>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,

    /// Free-form metadata. OpenAI accepts any JSON; we silently drop non-string
    /// values during translation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,

    #[serde(default, flatten, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, Value>,
}

/// `"stop"` is either a single sequence or an array. OpenAI imposes a max of
/// four entries; we enforce that during translation, not here.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum StringOrVec {
    String(String),
    Vec(Vec<String>),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct StreamOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_usage: Option<bool>,
}

/// One message in the conversation.
///
/// OpenAI uses the `developer` role as an alias for `system` in newer
/// reasoning models; we accept both and merge them in the translation layer.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ChatMessage {
    System {
        content: ChatContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Developer {
        content: ChatContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    User {
        content: ChatContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Assistant {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<ChatContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Tool {
        content: ChatContent,
        tool_call_id: String,
    },
}

/// Message content: either a plain string or a sequence of typed parts (text
/// blocks and image references).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ChatContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrlSpec },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImageUrlSpec {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FunctionDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    /// OpenAI structured-output strict flag. Forwarded to Anthropic
    /// `CustomTool.strict` so JSON-schema-validated function calls keep
    /// strict-mode semantics across the proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    /// Preserve future / vendor-specific function fields instead of
    /// silently dropping them at the wire boundary.
    #[serde(default, flatten, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ChatToolChoice {
    /// `"auto"`, `"none"`, or `"required"`.
    Mode(String),
    Specific(SpecificToolChoice),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpecificToolChoice {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: SpecificToolChoiceFunction,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpecificToolChoiceFunction {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema { json_schema: Value },
}

/// Both an incoming assistant `tool_calls[]` entry and the outgoing response
/// shape. Incoming requests may omit `id` (older clients); during translation
/// we synthesize a `toolu_*` fallback. Outgoing responses always populate
/// both `id` and `kind`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

// ---------------------------------------------------------------------------
// Non-streaming response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletion {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: UsageOut,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
}

impl ChatCompletion {
    pub const OBJECT: &'static str = "chat.completion";
}

#[derive(Debug, Clone, Serialize)]
pub struct Choice {
    pub index: usize,
    pub message: AssistantMessage,
    pub finish_reason: String,
    /// OpenAI clients expect this key to exist; we always emit `null`.
    pub logprobs: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssistantMessage {
    pub role: &'static str,
    /// Always serialized. When the assistant turn was a pure tool call
    /// (no text), this is `null` so strict OpenAI SDK / schema
    /// validators still deserialize the response.
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl AssistantMessage {
    pub const ROLE: &'static str = "assistant";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageOut {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptTokensDetails {
    pub cached_tokens: u32,
}

// ---------------------------------------------------------------------------
// Streaming response (chat.completion.chunk)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub model: String,
    pub choices: Vec<StreamChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageOut>,
}

impl ChatCompletionChunk {
    pub const OBJECT: &'static str = "chat.completion.chunk";
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamChoice {
    pub index: usize,
    pub delta: Delta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Delta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<DeltaToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeltaToolCall {
    pub index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub function: DeltaFunction,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DeltaFunction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIError {
    pub error: OpenAIErrorBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIErrorBody {
    pub message: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}

impl OpenAIErrorBody {
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: "invalid_request_error".to_string(),
            code: None,
            param: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserialize_minimal_request() {
        let raw = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let req: ChatCompletionRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.model, "claude-sonnet-4-6");
        assert_eq!(req.messages.len(), 1);
        match &req.messages[0] {
            ChatMessage::User { content, .. } => match content {
                ChatContent::Text(s) => assert_eq!(s, "hi"),
                ChatContent::Parts(_) => panic!("expected text"),
            },
            _ => panic!("expected user role"),
        }
    }

    #[test]
    fn deserialize_developer_role() {
        let raw = json!({"role": "developer", "content": "rules"});
        let msg: ChatMessage = serde_json::from_value(raw).unwrap();
        assert!(matches!(msg, ChatMessage::Developer { .. }));
    }

    #[test]
    fn deserialize_content_parts_with_image_url() {
        let raw = json!([
            {"type": "text", "text": "what's in the image?"},
            {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
        ]);
        let parts: ChatContent = serde_json::from_value(raw).unwrap();
        match parts {
            ChatContent::Parts(items) => {
                assert_eq!(items.len(), 2);
                assert!(matches!(items[0], ContentPart::Text { .. }));
                assert!(matches!(items[1], ContentPart::ImageUrl { .. }));
            }
            ChatContent::Text(_) => panic!("expected parts"),
        }
    }

    #[test]
    fn deserialize_tool_call_request_without_id() {
        let raw = json!({
            "role": "assistant",
            "tool_calls": [{"function": {"name": "calc", "arguments": "{}"}}]
        });
        let msg: ChatMessage = serde_json::from_value(raw).unwrap();
        match msg {
            ChatMessage::Assistant { tool_calls, .. } => {
                let calls = tool_calls.unwrap();
                assert_eq!(calls.len(), 1);
                assert!(calls[0].id.is_none());
                assert!(calls[0].kind.is_none());
                assert_eq!(calls[0].function.name, "calc");
            }
            _ => panic!("expected assistant"),
        }
    }

    #[test]
    fn deserialize_string_or_vec_stop() {
        let single: StringOrVec = serde_json::from_value(json!("END")).unwrap();
        assert_eq!(single, StringOrVec::String("END".to_string()));
        let multi: StringOrVec = serde_json::from_value(json!(["A", "B"])).unwrap();
        assert_eq!(
            multi,
            StringOrVec::Vec(vec!["A".to_string(), "B".to_string()])
        );
    }

    #[test]
    fn deserialize_tool_choice_modes() {
        let auto: ChatToolChoice = serde_json::from_value(json!("auto")).unwrap();
        assert!(matches!(auto, ChatToolChoice::Mode(ref s) if s == "auto"));
        let specific: ChatToolChoice = serde_json::from_value(json!({
            "type": "function",
            "function": {"name": "search"}
        }))
        .unwrap();
        match specific {
            ChatToolChoice::Specific(s) => assert_eq!(s.function.name, "search"),
            _ => panic!("expected specific"),
        }
    }

    #[test]
    fn deserialize_response_format_variants() {
        let text: ResponseFormat = serde_json::from_value(json!({"type": "text"})).unwrap();
        assert!(matches!(text, ResponseFormat::Text));
        let obj: ResponseFormat = serde_json::from_value(json!({"type": "json_object"})).unwrap();
        assert!(matches!(obj, ResponseFormat::JsonObject));
        let schema: ResponseFormat = serde_json::from_value(json!({
            "type": "json_schema",
            "json_schema": {"name": "x", "schema": {"type": "object"}}
        }))
        .unwrap();
        assert!(matches!(schema, ResponseFormat::JsonSchema { .. }));
    }

    #[test]
    fn unknown_fields_absorbed_into_extra() {
        let raw = json!({
            "model": "x",
            "messages": [],
            "future_field": {"foo": 1}
        });
        let req: ChatCompletionRequest = serde_json::from_value(raw).unwrap();
        assert!(req.extra.contains_key("future_field"));
    }

    #[test]
    fn serialize_chat_completion_emits_constants() {
        let cc = ChatCompletion {
            id: "chatcmpl-abc".to_string(),
            object: ChatCompletion::OBJECT,
            created: 1700000000,
            model: "claude-sonnet-4-6".to_string(),
            choices: vec![Choice {
                index: 0,
                message: AssistantMessage {
                    role: AssistantMessage::ROLE,
                    content: Some("hi".to_string()),
                    tool_calls: None,
                    reasoning_content: None,
                },
                finish_reason: "stop".to_string(),
                logprobs: Value::Null,
            }],
            usage: UsageOut {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                prompt_tokens_details: None,
            },
            system_fingerprint: None,
        };
        let body = serde_json::to_value(&cc).unwrap();
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["choices"][0]["message"]["role"], "assistant");
        assert_eq!(body["choices"][0]["logprobs"], Value::Null);
    }

    #[test]
    fn serialize_chat_completion_chunk_keeps_object_chunk() {
        let chunk = ChatCompletionChunk {
            id: "chatcmpl-abc".to_string(),
            object: ChatCompletionChunk::OBJECT,
            created: 1,
            model: "claude-sonnet-4-6".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: Some("assistant".to_string()),
                    ..Delta::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let body = serde_json::to_value(&chunk).unwrap();
        assert_eq!(body["object"], "chat.completion.chunk");
        assert_eq!(body["choices"][0]["delta"]["role"], "assistant");
        assert!(body["choices"][0]["delta"].get("content").is_none());
    }
}
