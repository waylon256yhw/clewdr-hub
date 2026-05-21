//! OpenAI Chat Completions response translation.
//!
//! Consumes an axum response whose body is a Claude
//! [`CreateMessageResponse`] (success) or a Claude error envelope
//! (failure) and emits an axum response shaped like an OpenAI
//! [`ChatCompletion`] or [`crate::types::openai::OpenAIErrorBody`].

use axum::{Json, response::IntoResponse};
use http::StatusCode;
use serde_json::{Value, json};

use crate::types::{
    claude::{ContentBlock, CreateMessageResponse, StopReason, Usage},
    openai::{
        AssistantMessage, ChatCompletion, Choice, FunctionCall, OpenAIErrorBody,
        PromptTokensDetails, ToolCall, UsageOut,
    },
};

const RESPONSE_BODY_LIMIT: usize = 32 * 1024 * 1024;

/// Translate a non-stream Anthropic response into the OpenAI shape.
///
/// Always returns a complete `axum::Response`; failures during body
/// translation surface as OpenAI-shaped error envelopes with the appropriate
/// status code.
pub async fn to_openai_non_stream(
    resp: axum::response::Response,
    include_reasoning: bool,
) -> axum::response::Response {
    let (parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, RESPONSE_BODY_LIMIT).await {
        Ok(b) => b,
        Err(e) => {
            return openai_error_response(
                StatusCode::BAD_GATEWAY,
                OpenAIErrorBody {
                    message: format!("failed to read upstream body: {e}"),
                    kind: "api_error".to_string(),
                    code: None,
                    param: None,
                },
            );
        }
    };

    if !parts.status.is_success() {
        let body = translate_upstream_error_body(&bytes);
        return openai_error_response(parts.status, body);
    }

    let upstream: CreateMessageResponse = match serde_json::from_slice(&bytes) {
        Ok(u) => u,
        Err(e) => {
            return openai_error_response(
                StatusCode::BAD_GATEWAY,
                OpenAIErrorBody {
                    message: format!("failed to parse upstream response: {e}"),
                    kind: "api_error".to_string(),
                    code: None,
                    param: None,
                },
            );
        }
    };

    let chat = build_chat_completion(upstream, include_reasoning);
    Json(chat).into_response()
}

/// Build an [`OpenAIErrorBody`] from raw Anthropic error JSON bytes. Falls
/// back to a generic `api_error` envelope when the body is not parseable.
pub fn translate_upstream_error_body(bytes: &[u8]) -> OpenAIErrorBody {
    let parsed: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let inner = parsed.get("error").cloned().unwrap_or(parsed);
    let message = inner
        .get("message")
        .map(stringify_value)
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned());
    let upstream_type = inner
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("api_error");
    OpenAIErrorBody {
        message,
        kind: anthropic_type_to_oai(upstream_type).to_string(),
        code: None,
        param: None,
    }
}

/// Map an Anthropic error `type` field to its closest OpenAI `error.type`
/// equivalent. Unknown values fall back to `api_error`.
pub fn anthropic_type_to_oai(t: &str) -> &'static str {
    match t {
        "rate_limited" | "overloaded_error" | "rate_limit_error" => "rate_limit_exceeded",
        "authentication_error" | "permission_error" => "invalid_request_error",
        "invalid_request_error" => "invalid_request_error",
        "not_found_error" => "invalid_request_error",
        _ => "api_error",
    }
}

fn stringify_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn openai_error_response(status: StatusCode, body: OpenAIErrorBody) -> axum::response::Response {
    (status, Json(json!({ "error": body }))).into_response()
}

fn build_chat_completion(
    upstream: CreateMessageResponse,
    include_reasoning: bool,
) -> ChatCompletion {
    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let created = chrono::Utc::now().timestamp();
    let model = upstream.model.clone();

    let mut content_str = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut reasoning_str = String::new();

    for block in &upstream.content {
        match block {
            ContentBlock::Text { text, .. } => content_str.push_str(text),
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                let arguments = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                tool_calls.push(ToolCall {
                    id: Some(id.clone()),
                    kind: Some("function".to_string()),
                    function: FunctionCall {
                        name: name.clone(),
                        arguments,
                    },
                });
            }
            ContentBlock::Thinking { thinking, .. } if include_reasoning => {
                reasoning_str.push_str(thinking);
            }
            // All other block types (image, document, search_result,
            // server_tool_use, tool_reference, web_*_tool_result,
            // redacted_thinking, mcp_*, container_upload) have no OpenAI
            // chat.completion equivalent. Drop them rather than leak the
            // Anthropic-specific shape into the wire response.
            _ => {}
        }
    }

    let finish_reason =
        map_stop_reason(upstream.stop_reason.as_ref(), !tool_calls.is_empty()).to_string();

    let usage = map_usage(upstream.usage.as_ref());

    let message = AssistantMessage {
        role: AssistantMessage::ROLE,
        content: (!content_str.is_empty()).then_some(content_str),
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        reasoning_content: (!reasoning_str.is_empty()).then_some(reasoning_str),
    };

    ChatCompletion {
        id,
        object: ChatCompletion::OBJECT,
        created,
        model,
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason,
            logprobs: Value::Null,
        }],
        usage,
        system_fingerprint: None,
    }
}

pub(crate) fn map_stop_reason(
    stop_reason: Option<&StopReason>,
    has_tool_calls: bool,
) -> &'static str {
    if has_tool_calls {
        return "tool_calls";
    }
    match stop_reason {
        Some(StopReason::ToolUse) => "tool_calls",
        Some(StopReason::MaxTokens) | Some(StopReason::ModelContextWindowExceeded) => "length",
        Some(StopReason::Refusal) => "content_filter",
        _ => "stop",
    }
}

pub(crate) fn map_usage(usage: Option<&Usage>) -> UsageOut {
    let (input_tokens, output_tokens, cache_create, cache_read) = match usage {
        Some(u) => (
            u.input_tokens,
            u.output_tokens,
            u.cache_creation_input_tokens.unwrap_or(0),
            u.cache_read_input_tokens.unwrap_or(0),
        ),
        None => (0, 0, 0, 0),
    };
    let prompt_tokens = input_tokens + cache_create + cache_read;
    UsageOut {
        prompt_tokens,
        completion_tokens: output_tokens,
        total_tokens: prompt_tokens + output_tokens,
        prompt_tokens_details: Some(PromptTokensDetails {
            cached_tokens: cache_read,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::claude::Role;
    use serde_json::json;

    fn make_response(
        content: Vec<ContentBlock>,
        stop_reason: Option<StopReason>,
        usage: Option<Usage>,
    ) -> CreateMessageResponse {
        CreateMessageResponse {
            content,
            id: "msg_abc".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            role: Role::Assistant,
            stop_reason,
            stop_sequence: None,
            type_: "message".to_string(),
            usage,
        }
    }

    #[test]
    fn text_block_becomes_message_content() {
        let resp = make_response(
            vec![ContentBlock::text("hello world")],
            Some(StopReason::EndTurn),
            None,
        );
        let chat = build_chat_completion(resp, true);
        assert_eq!(
            chat.choices[0].message.content.as_deref(),
            Some("hello world")
        );
        assert!(chat.choices[0].message.tool_calls.is_none());
        assert!(chat.choices[0].message.reasoning_content.is_none());
        assert_eq!(chat.choices[0].finish_reason, "stop");
    }

    #[test]
    fn multiple_text_blocks_concatenate() {
        let resp = make_response(
            vec![ContentBlock::text("foo"), ContentBlock::text("bar")],
            Some(StopReason::EndTurn),
            None,
        );
        let chat = build_chat_completion(resp, true);
        assert_eq!(chat.choices[0].message.content.as_deref(), Some("foobar"));
    }

    #[test]
    fn tool_use_block_becomes_tool_call_with_function_kind() {
        let resp = make_response(
            vec![ContentBlock::ToolUse {
                id: "toolu_1".to_string(),
                name: "calc".to_string(),
                input: json!({"x": 1}),
                cache_control: None,
                caller: None,
            }],
            Some(StopReason::ToolUse),
            None,
        );
        let chat = build_chat_completion(resp, true);
        assert!(chat.choices[0].message.content.is_none());
        let calls = chat.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_deref(), Some("toolu_1"));
        assert_eq!(calls[0].kind.as_deref(), Some("function"));
        assert_eq!(calls[0].function.name, "calc");
        assert_eq!(calls[0].function.arguments, "{\"x\":1}");
        assert_eq!(chat.choices[0].finish_reason, "tool_calls");
    }

    #[test]
    fn tool_calls_override_endturn_finish_reason() {
        let resp = make_response(
            vec![ContentBlock::ToolUse {
                id: "toolu_1".to_string(),
                name: "calc".to_string(),
                input: json!({}),
                cache_control: None,
                caller: None,
            }],
            Some(StopReason::EndTurn),
            None,
        );
        let chat = build_chat_completion(resp, true);
        assert_eq!(chat.choices[0].finish_reason, "tool_calls");
    }

    #[test]
    fn thinking_block_becomes_reasoning_content() {
        let resp = make_response(
            vec![
                ContentBlock::Thinking {
                    signature: "sig".to_string(),
                    thinking: "let me think".to_string(),
                },
                ContentBlock::text("answer"),
            ],
            Some(StopReason::EndTurn),
            None,
        );
        let chat = build_chat_completion(resp, true);
        assert_eq!(
            chat.choices[0].message.reasoning_content.as_deref(),
            Some("let me think")
        );
        assert_eq!(chat.choices[0].message.content.as_deref(), Some("answer"));
    }

    #[test]
    fn include_reasoning_false_drops_thinking() {
        let resp = make_response(
            vec![
                ContentBlock::Thinking {
                    signature: "sig".to_string(),
                    thinking: "let me think".to_string(),
                },
                ContentBlock::text("answer"),
            ],
            Some(StopReason::EndTurn),
            None,
        );
        let chat = build_chat_completion(resp, false);
        assert!(chat.choices[0].message.reasoning_content.is_none());
    }

    #[test]
    fn finish_reason_maps_max_tokens_to_length() {
        let resp = make_response(
            vec![ContentBlock::text("partial")],
            Some(StopReason::MaxTokens),
            None,
        );
        let chat = build_chat_completion(resp, true);
        assert_eq!(chat.choices[0].finish_reason, "length");
    }

    #[test]
    fn finish_reason_maps_refusal_to_content_filter() {
        let resp = make_response(
            vec![ContentBlock::text("nope")],
            Some(StopReason::Refusal),
            None,
        );
        let chat = build_chat_completion(resp, true);
        assert_eq!(chat.choices[0].finish_reason, "content_filter");
    }

    #[test]
    fn finish_reason_defaults_to_stop_when_missing() {
        let resp = make_response(vec![ContentBlock::text("hi")], None, None);
        let chat = build_chat_completion(resp, true);
        assert_eq!(chat.choices[0].finish_reason, "stop");
    }

    #[test]
    fn usage_merges_cache_creation_and_read_into_prompt_tokens() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: Some(20),
            cache_read_input_tokens: Some(30),
        };
        let resp = make_response(
            vec![ContentBlock::text("hi")],
            Some(StopReason::EndTurn),
            Some(usage),
        );
        let chat = build_chat_completion(resp, true);
        assert_eq!(chat.usage.prompt_tokens, 150);
        assert_eq!(chat.usage.completion_tokens, 50);
        assert_eq!(chat.usage.total_tokens, 200);
        assert_eq!(chat.usage.prompt_tokens_details.unwrap().cached_tokens, 30);
    }

    #[test]
    fn usage_zero_when_upstream_missing() {
        let resp = make_response(vec![ContentBlock::text("hi")], None, None);
        let chat = build_chat_completion(resp, true);
        assert_eq!(chat.usage.prompt_tokens, 0);
        assert_eq!(chat.usage.completion_tokens, 0);
        assert_eq!(chat.usage.total_tokens, 0);
    }

    #[test]
    fn image_and_document_blocks_are_dropped() {
        let resp = make_response(
            vec![
                ContentBlock::text("intro"),
                ContentBlock::Document {
                    source: json!({"type": "url", "url": "https://x"}),
                    cache_control: None,
                    citations: None,
                    context: None,
                    title: None,
                },
                ContentBlock::text("outro"),
            ],
            Some(StopReason::EndTurn),
            None,
        );
        let chat = build_chat_completion(resp, true);
        assert_eq!(
            chat.choices[0].message.content.as_deref(),
            Some("introoutro")
        );
    }

    #[test]
    fn upstream_error_body_translated_to_oai_shape() {
        let bytes = serde_json::to_vec(&json!({
            "type": "error",
            "error": {"type": "rate_limited", "message": "slow down"}
        }))
        .unwrap();
        let body = translate_upstream_error_body(&bytes);
        assert_eq!(body.kind, "rate_limit_exceeded");
        assert_eq!(body.message, "slow down");
    }

    #[test]
    fn upstream_error_with_unknown_type_maps_to_api_error() {
        let bytes = serde_json::to_vec(&json!({
            "type": "error",
            "error": {"type": "unknown_thing", "message": "weird"}
        }))
        .unwrap();
        let body = translate_upstream_error_body(&bytes);
        assert_eq!(body.kind, "api_error");
    }

    #[test]
    fn upstream_error_with_object_message_serializes() {
        let bytes = serde_json::to_vec(&json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": {"detail": "bad"}}
        }))
        .unwrap();
        let body = translate_upstream_error_body(&bytes);
        assert!(body.message.contains("detail"));
    }

    #[test]
    fn anthropic_type_to_oai_known_mappings() {
        assert_eq!(anthropic_type_to_oai("rate_limited"), "rate_limit_exceeded");
        assert_eq!(
            anthropic_type_to_oai("overloaded_error"),
            "rate_limit_exceeded"
        );
        assert_eq!(
            anthropic_type_to_oai("authentication_error"),
            "invalid_request_error"
        );
        assert_eq!(
            anthropic_type_to_oai("permission_error"),
            "invalid_request_error"
        );
        assert_eq!(
            anthropic_type_to_oai("invalid_request_error"),
            "invalid_request_error"
        );
        assert_eq!(anthropic_type_to_oai("something_else"), "api_error");
    }
}
