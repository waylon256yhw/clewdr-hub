//! OpenAI Chat Completions → Anthropic Messages translation.
//!
//! Consumes a [`ChatCompletionRequest`] (the wire shape OpenAI clients send)
//! and produces a [`CreateMessageParams`] suitable for the existing Claude
//! upstream pipeline, along with a `stream` flag and any pre-derived state
//! the handler needs.
//!
//! Hard rejects (`n > 1`, `logprobs: true`, unsupported image media type)
//! surface as [`OpenAIRequestError`] with HTTP 400. Soft-ignored fields
//! (`frequency_penalty`, `presence_penalty`, `logit_bias`, `seed`,
//! `service_tier`, `store`, `top_logprobs`, non-string `metadata` entries,
//! unknown fields in `extra`) are silently dropped to keep client behavior
//! consistent with mainstream OpenAI-compatible proxies.

use axum::{
    Json,
    extract::{FromRequest, Request, rejection::JsonRejection},
};
use http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

use crate::{
    db::models::AuthenticatedUser,
    middleware::claude::{
        ClaudeContext, build_claude_context, claude_code_billing_header, drop_empty_system,
        inject_metadata_user_id, normalize_sampling_params, prepend_system_blocks,
        strip_billing_headers_from_system, strip_ephemeral_scope_from_system,
    },
    stealth,
    types::{
        claude::{
            ContentBlock, CreateMessageParams, CustomTool, ImageSource, ImageUrl, Message,
            MessageContent, Metadata, OutputConfig, OutputEffort, OutputFormat, Role, Tool,
            ToolChoice,
        },
        openai::{
            ChatCompletionRequest, ChatContent, ChatMessage, ChatToolChoice, ContentPart,
            OpenAIErrorBody, ResponseFormat, StringOrVec, ToolCall, ToolDef,
        },
    },
};

const MAX_STOP_SEQUENCES: usize = 4;
const DEFAULT_MAX_TOKENS: u32 = 16_384;
const SUPPORTED_IMAGE_MEDIA_TYPES: &[&str] =
    &["image/jpeg", "image/png", "image/gif", "image/webp"];

/// Error returned by the OAI request pipeline (extractor + translator). The
/// `IntoResponse` impl lives in the parent module so the wire shape is
/// emitted consistently across the OpenAI surface.
#[derive(Debug, Clone)]
pub struct OpenAIRequestError {
    pub status: StatusCode,
    pub body: OpenAIErrorBody,
}

impl From<JsonRejection> for OpenAIRequestError {
    fn from(rejection: JsonRejection) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            body: OpenAIErrorBody::invalid_request(rejection.body_text()),
        }
    }
}

/// Translate an OpenAI request into an Anthropic Messages request.
///
/// Returns `(claude_body, stream_flag)`. The body is *not* yet normalized —
/// callers must run it through the shared request-shaping pipeline
/// (drop_empty_system → normalize_sampling_params →
/// strip_billing_headers_from_system → prepend_system_blocks →
/// strip_ephemeral_scope_from_system → inject_metadata_user_id).
pub fn translate_request(
    oai: ChatCompletionRequest,
) -> Result<(CreateMessageParams, bool), OpenAIRequestError> {
    if oai.n.is_some_and(|n| n > 1) {
        return Err(OpenAIRequestError {
            status: StatusCode::BAD_REQUEST,
            body: OpenAIErrorBody::invalid_request(
                "n > 1 is not supported by the OpenAI-compatible endpoint",
            ),
        });
    }
    if oai.logprobs == Some(true) {
        return Err(OpenAIRequestError {
            status: StatusCode::BAD_REQUEST,
            body: OpenAIErrorBody::invalid_request(
                "logprobs is not supported by the OpenAI-compatible endpoint",
            ),
        });
    }

    let mut system_entries: Vec<Value> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();
    for msg in oai.messages {
        match msg {
            ChatMessage::System { content, .. } | ChatMessage::Developer { content, .. } => {
                push_system_entries(&mut system_entries, content);
            }
            ChatMessage::User { content, .. } => {
                let blocks = user_content_to_blocks(content)?;
                if blocks.is_empty() {
                    continue;
                }
                if blocks.len() == 1
                    && let ContentBlock::Text { text, .. } = &blocks[0]
                {
                    messages.push(Message::new_text(Role::User, text.clone()));
                } else {
                    messages.push(Message::new_blocks(Role::User, blocks));
                }
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
                ..
            } => {
                let mut blocks: Vec<ContentBlock> = Vec::new();
                if let Some(content) = content {
                    assistant_text_to_blocks(content, &mut blocks);
                }
                if let Some(calls) = tool_calls {
                    for call in calls {
                        blocks.push(tool_call_to_block(call));
                    }
                }
                if blocks.is_empty() {
                    continue;
                }
                messages.push(Message::new_blocks(Role::Assistant, blocks));
            }
            ChatMessage::Tool {
                content,
                tool_call_id,
            } => {
                let block = ContentBlock::ToolResult {
                    tool_use_id: tool_call_id,
                    content: Value::String(content),
                    cache_control: None,
                    is_error: None,
                };
                append_tool_result(&mut messages, block);
            }
        }
    }

    let system = if system_entries.is_empty() {
        None
    } else {
        Some(Value::Array(system_entries))
    };

    let tools = oai.tools.map(translate_tools);
    let tool_choice = oai
        .tool_choice
        .map(translate_tool_choice)
        .transpose()?
        .flatten();

    // tool_choice="none" must strip tools entirely.
    let (tools, tool_choice) = match tool_choice.as_ref() {
        Some(ToolChoice::None) => (None, None),
        _ => (tools, tool_choice),
    };

    let max_tokens = oai
        .max_completion_tokens
        .or(oai.max_tokens)
        .unwrap_or(DEFAULT_MAX_TOKENS);

    let stop_sequences = oai.stop.map(|s| match s {
        StringOrVec::String(s) => vec![s],
        StringOrVec::Vec(mut v) => {
            v.truncate(MAX_STOP_SEQUENCES);
            v
        }
    });

    let mut output_config = None;
    if let Some(effort) = oai.reasoning_effort.as_deref().and_then(parse_effort) {
        output_config = Some(OutputConfig {
            effort: Some(effort),
            format: None,
        });
    }

    let output_format = match oai.response_format {
        Some(ResponseFormat::JsonSchema { json_schema }) => json_schema
            .get("schema")
            .cloned()
            .map(|schema| OutputFormat::JsonSchema { schema }),
        // Text and JsonObject are silently accepted as no-ops.
        _ => None,
    };

    let mut metadata = Metadata::default();
    if let Some(map) = oai.metadata {
        for (k, v) in map {
            if let Value::String(s) = v {
                metadata.fields.insert(k, s);
            }
        }
    }
    if let Some(user) = oai.user.as_ref().filter(|u| !u.is_empty())
        && !metadata.fields.contains_key("user_id")
    {
        metadata.fields.insert("user_id".to_string(), user.clone());
    }
    let metadata = (!metadata.fields.is_empty()).then_some(metadata);

    let stream = oai.stream.unwrap_or(false);

    let body = CreateMessageParams {
        max_tokens,
        messages,
        model: oai.model,
        system,
        temperature: oai.temperature,
        top_p: oai.top_p,
        top_k: oai.top_k,
        stop_sequences,
        stream: Some(stream),
        tools,
        tool_choice,
        metadata,
        output_config,
        output_format,
        ..CreateMessageParams::default()
    };

    Ok((body, stream))
}

fn push_system_entries(systems: &mut Vec<Value>, content: ChatContent) {
    match content {
        ChatContent::Text(text) if !text.is_empty() => {
            systems.push(json!({ "type": "text", "text": text }));
        }
        ChatContent::Text(_) => {}
        ChatContent::Parts(parts) => {
            for part in parts {
                if let ContentPart::Text { text } = part
                    && !text.is_empty()
                {
                    systems.push(json!({ "type": "text", "text": text }));
                }
            }
        }
    }
}

fn user_content_to_blocks(content: ChatContent) -> Result<Vec<ContentBlock>, OpenAIRequestError> {
    Ok(match content {
        ChatContent::Text(text) => vec![ContentBlock::text(text)],
        ChatContent::Parts(parts) => {
            let mut out = Vec::with_capacity(parts.len());
            for part in parts {
                match part {
                    ContentPart::Text { text } => out.push(ContentBlock::text(text)),
                    ContentPart::ImageUrl { image_url } => {
                        out.push(image_url_to_block(&image_url.url)?);
                    }
                }
            }
            out
        }
    })
}

fn assistant_text_to_blocks(content: ChatContent, out: &mut Vec<ContentBlock>) {
    match content {
        ChatContent::Text(text) if !text.is_empty() => {
            out.push(ContentBlock::text(text));
        }
        ChatContent::Text(_) => {}
        ChatContent::Parts(parts) => {
            for part in parts {
                if let ContentPart::Text { text } = part
                    && !text.is_empty()
                {
                    out.push(ContentBlock::text(text));
                }
                // Assistant image parts are not part of the public OAI spec.
                // Silently ignore.
            }
        }
    }
}

fn image_url_to_block(url: &str) -> Result<ContentBlock, OpenAIRequestError> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (header, data) = rest.split_once(',').ok_or_else(|| OpenAIRequestError {
            status: StatusCode::BAD_REQUEST,
            body: OpenAIErrorBody::invalid_request("malformed image data URL"),
        })?;
        let mut parts = header.split(';');
        let media_type = parts.next().unwrap_or_default().to_ascii_lowercase();
        let is_base64 = parts.any(|p| p.trim().eq_ignore_ascii_case("base64"));
        if !is_base64 {
            return Err(OpenAIRequestError {
                status: StatusCode::BAD_REQUEST,
                body: OpenAIErrorBody::invalid_request(
                    "only base64-encoded image data URLs are supported",
                ),
            });
        }
        if !SUPPORTED_IMAGE_MEDIA_TYPES.contains(&media_type.as_str()) {
            return Err(OpenAIRequestError {
                status: StatusCode::BAD_REQUEST,
                body: OpenAIErrorBody::invalid_request(format!(
                    "unsupported image media type: {media_type}"
                )),
            });
        }
        return Ok(ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type,
                data: data.to_string(),
            },
            cache_control: None,
        });
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(ContentBlock::ImageUrl {
            image_url: ImageUrl {
                url: url.to_string(),
            },
        });
    }
    Err(OpenAIRequestError {
        status: StatusCode::BAD_REQUEST,
        body: OpenAIErrorBody::invalid_request(format!(
            "unsupported image_url scheme: {}",
            url.chars().take(40).collect::<String>()
        )),
    })
}

fn tool_call_to_block(call: ToolCall) -> ContentBlock {
    let id = call.id.unwrap_or_else(generate_tool_use_id);
    let input = parse_tool_call_arguments(&call.function.arguments);
    ContentBlock::ToolUse {
        id,
        name: call.function.name,
        input,
        cache_control: None,
        caller: None,
    }
}

fn parse_tool_call_arguments(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw)
        .ok()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}))
}

fn generate_tool_use_id() -> String {
    format!("toolu_{}", uuid::Uuid::new_v4().simple())
}

fn append_tool_result(messages: &mut Vec<Message>, block: ContentBlock) {
    if let Some(last) = messages.last_mut()
        && last.role == Role::User
        && let MessageContent::Blocks { content: blocks } = &mut last.content
        && blocks
            .iter()
            .all(|b| matches!(b, ContentBlock::ToolResult { .. }))
    {
        blocks.push(block);
        return;
    }
    messages.push(Message::new_blocks(Role::User, vec![block]));
}

fn translate_tools(tools: Vec<ToolDef>) -> Vec<Tool> {
    tools
        .into_iter()
        .map(|def| {
            Tool::Custom(CustomTool {
                name: def.function.name,
                description: def.function.description,
                input_schema: def
                    .function
                    .parameters
                    .unwrap_or_else(|| json!({ "type": "object" })),
                allowed_callers: None,
                cache_control: None,
                defer_loading: None,
                input_examples: None,
                strict: None,
                type_: None,
                extra: Default::default(),
            })
        })
        .collect()
}

fn translate_tool_choice(choice: ChatToolChoice) -> Result<Option<ToolChoice>, OpenAIRequestError> {
    match choice {
        ChatToolChoice::Mode(mode) => match mode.as_str() {
            "auto" => Ok(Some(ToolChoice::Auto {
                disable_parallel_tool_use: None,
            })),
            "required" => Ok(Some(ToolChoice::Any {
                disable_parallel_tool_use: None,
            })),
            "none" => Ok(Some(ToolChoice::None)),
            other => Err(OpenAIRequestError {
                status: StatusCode::BAD_REQUEST,
                body: OpenAIErrorBody::invalid_request(format!(
                    "unknown tool_choice mode: {other}"
                )),
            }),
        },
        ChatToolChoice::Specific(spec) => Ok(Some(ToolChoice::Tool {
            name: spec.function.name,
            disable_parallel_tool_use: None,
        })),
    }
}

fn parse_effort(raw: &str) -> Option<OutputEffort> {
    match raw.to_ascii_lowercase().as_str() {
        "low" => Some(OutputEffort::Low),
        "medium" => Some(OutputEffort::Medium),
        "high" => Some(OutputEffort::High),
        _ => None,
    }
}

fn parse_include_reasoning(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get("x-include-reasoning") else {
        return true;
    };
    let Ok(raw) = value.to_str() else {
        return true;
    };
    !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "false" | "0" | "no"
    )
}

// ---------------------------------------------------------------------------
// Extractor
// ---------------------------------------------------------------------------

pub struct OpenAIChatPreprocess {
    pub params: CreateMessageParams,
    pub context: ClaudeContext,
    pub include_reasoning: bool,
    pub include_usage: bool,
}

impl<S> FromRequest<S> for OpenAIChatPreprocess
where
    S: Send + Sync,
{
    type Rejection = OpenAIRequestError;

    async fn from_request(req: Request, _: &S) -> Result<Self, Self::Rejection> {
        let auth_user = req.extensions().get::<AuthenticatedUser>().cloned();
        let include_reasoning = parse_include_reasoning(req.headers());
        let Json(oai) = Json::<ChatCompletionRequest>::from_request(req, &()).await?;
        let include_usage = oai
            .stream_options
            .as_ref()
            .and_then(|s| s.include_usage)
            .unwrap_or(false);
        let (mut body, _stream) = translate_request(oai)?;

        drop_empty_system(&mut body);

        let profile = stealth::global_profile().load();
        normalize_sampling_params(&mut body, &profile);

        strip_billing_headers_from_system(&mut body);

        let system_prefixes = vec![ContentBlock::text(claude_code_billing_header(
            &body.messages,
            &profile,
        ))];
        prepend_system_blocks(&mut body, system_prefixes);

        if let Some(system) = body.system.as_mut() {
            strip_ephemeral_scope_from_system(system);
        }

        // Build context before inject_metadata_user_id so the affinity hash
        // observes the pre-injection state — see ClaudeCodePreprocess for
        // the matching ordering on the Anthropic path.
        let context = build_claude_context(&body, auth_user.as_ref(), None);

        inject_metadata_user_id(&mut body, auth_user.as_ref());

        Ok(Self {
            params: body,
            context,
            include_reasoning,
            include_usage,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req_from_json(value: Value) -> ChatCompletionRequest {
        serde_json::from_value(value).expect("valid request fixture")
    }

    #[test]
    fn merges_system_messages_in_order() {
        let req = req_from_json(json!({
            "model": "claude-sonnet-4-6",
            "messages": [
                {"role": "system", "content": "first"},
                {"role": "system", "content": "second"},
                {"role": "user", "content": "hi"}
            ]
        }));
        let (body, _) = translate_request(req).unwrap();
        let system = body.system.unwrap();
        let arr = system.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["text"], "first");
        assert_eq!(arr[1]["text"], "second");
    }

    #[test]
    fn merges_developer_role_into_system() {
        let req = req_from_json(json!({
            "model": "claude-sonnet-4-6",
            "messages": [
                {"role": "system", "content": "rules-a"},
                {"role": "developer", "content": "rules-b"},
                {"role": "user", "content": "hi"}
            ]
        }));
        let (body, _) = translate_request(req).unwrap();
        let arr = body.system.unwrap();
        let arr = arr.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["text"], "rules-a");
        assert_eq!(arr[1]["text"], "rules-b");
    }

    #[test]
    fn translates_image_url_data_url() {
        let req = req_from_json(json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]}]
        }));
        let (body, _) = translate_request(req).unwrap();
        let msg = &body.messages[0];
        let MessageContent::Blocks { content } = &msg.content else {
            panic!("expected blocks")
        };
        match &content[1] {
            ContentBlock::Image { source, .. } => match source {
                ImageSource::Base64 { media_type, data } => {
                    assert_eq!(media_type, "image/png");
                    assert_eq!(data, "AAAA");
                }
                _ => panic!("expected base64"),
            },
            _ => panic!("expected image block"),
        }
    }

    #[test]
    fn translates_image_url_https_passthrough() {
        let req = req_from_json(json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
            ]}]
        }));
        let (body, _) = translate_request(req).unwrap();
        let msg = &body.messages[0];
        let MessageContent::Blocks { content } = &msg.content else {
            panic!("expected blocks")
        };
        match &content[0] {
            ContentBlock::ImageUrl { image_url } => {
                assert_eq!(image_url.url, "https://example.com/a.png");
            }
            _ => panic!("expected image_url"),
        }
    }

    #[test]
    fn rejects_unsupported_image_media_type() {
        let req = req_from_json(json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/tiff;base64,AAAA"}}
            ]}]
        }));
        let err = translate_request(req).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn translates_assistant_tool_calls_and_tool_results() {
        let req = req_from_json(json!({
            "model": "claude-sonnet-4-6",
            "messages": [
                {"role": "user", "content": "what's 1+1"},
                {"role": "assistant", "content": "computing", "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "calc", "arguments": "{\"x\":1}"}}
                ]},
                {"role": "tool", "content": "2", "tool_call_id": "call_1"}
            ]
        }));
        let (body, _) = translate_request(req).unwrap();
        assert_eq!(body.messages.len(), 3);
        let MessageContent::Blocks { content } = &body.messages[1].content else {
            panic!("expected blocks")
        };
        assert_eq!(content.len(), 2);
        assert!(matches!(content[0], ContentBlock::Text { .. }));
        match &content[1] {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "calc");
                assert_eq!(input["x"], 1);
            }
            _ => panic!("expected tool_use"),
        }
        // Tool message becomes a user message with a tool_result block.
        assert_eq!(body.messages[2].role, Role::User);
        let MessageContent::Blocks { content } = &body.messages[2].content else {
            panic!("expected blocks")
        };
        match &content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(content, &Value::String("2".to_string()));
            }
            _ => panic!("expected tool_result"),
        }
    }

    #[test]
    fn tool_call_invalid_arguments_falls_back_to_empty_object() {
        let req = req_from_json(json!({
            "model": "claude-sonnet-4-6",
            "messages": [
                {"role": "assistant", "tool_calls": [
                    {"id": "call_1", "function": {"name": "calc", "arguments": "{not json"}}
                ]}
            ]
        }));
        let (body, _) = translate_request(req).unwrap();
        let MessageContent::Blocks { content } = &body.messages[0].content else {
            panic!("expected blocks")
        };
        match &content[0] {
            ContentBlock::ToolUse { input, .. } => {
                assert!(input.is_object());
                assert_eq!(input.as_object().unwrap().len(), 0);
            }
            _ => panic!("expected tool_use"),
        }
    }

    #[test]
    fn tool_call_missing_id_generates_toolu_prefix() {
        let req = req_from_json(json!({
            "model": "claude-sonnet-4-6",
            "messages": [
                {"role": "assistant", "tool_calls": [
                    {"function": {"name": "calc", "arguments": "{}"}}
                ]}
            ]
        }));
        let (body, _) = translate_request(req).unwrap();
        let MessageContent::Blocks { content } = &body.messages[0].content else {
            panic!("expected blocks")
        };
        match &content[0] {
            ContentBlock::ToolUse { id, .. } => assert!(id.starts_with("toolu_")),
            _ => panic!("expected tool_use"),
        }
    }

    #[test]
    fn multiple_tool_results_merge_into_one_user_message() {
        let req = req_from_json(json!({
            "model": "claude-sonnet-4-6",
            "messages": [
                {"role": "tool", "content": "A", "tool_call_id": "call_1"},
                {"role": "tool", "content": "B", "tool_call_id": "call_2"}
            ]
        }));
        let (body, _) = translate_request(req).unwrap();
        assert_eq!(body.messages.len(), 1);
        let MessageContent::Blocks { content } = &body.messages[0].content else {
            panic!("expected blocks")
        };
        assert_eq!(content.len(), 2);
        assert!(matches!(content[0], ContentBlock::ToolResult { .. }));
        assert!(matches!(content[1], ContentBlock::ToolResult { .. }));
    }

    #[test]
    fn tool_choice_mode_required_maps_to_any() {
        let req = req_from_json(json!({
            "model": "x",
            "messages": [],
            "tools": [{"type": "function", "function": {"name": "calc"}}],
            "tool_choice": "required"
        }));
        let (body, _) = translate_request(req).unwrap();
        assert!(matches!(body.tool_choice, Some(ToolChoice::Any { .. })));
    }

    #[test]
    fn tool_choice_mode_none_strips_tools() {
        let req = req_from_json(json!({
            "model": "x",
            "messages": [],
            "tools": [{"type": "function", "function": {"name": "calc"}}],
            "tool_choice": "none"
        }));
        let (body, _) = translate_request(req).unwrap();
        assert!(body.tools.is_none());
        assert!(body.tool_choice.is_none());
    }

    #[test]
    fn tool_choice_specific_function() {
        let req = req_from_json(json!({
            "model": "x",
            "messages": [],
            "tools": [{"type": "function", "function": {"name": "search"}}],
            "tool_choice": {"type": "function", "function": {"name": "search"}}
        }));
        let (body, _) = translate_request(req).unwrap();
        match body.tool_choice.unwrap() {
            ToolChoice::Tool { name, .. } => assert_eq!(name, "search"),
            _ => panic!("expected tool_choice::Tool"),
        }
    }

    #[test]
    fn reasoning_effort_maps_to_output_effort() {
        let req = req_from_json(json!({
            "model": "x",
            "messages": [],
            "reasoning_effort": "HIGH"
        }));
        let (body, _) = translate_request(req).unwrap();
        match body.output_config.unwrap().effort.unwrap() {
            OutputEffort::High => {}
            _ => panic!("expected high effort"),
        }
    }

    #[test]
    fn response_format_json_schema_extracts_schema() {
        let req = req_from_json(json!({
            "model": "x",
            "messages": [],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": "out", "schema": {"type": "object", "properties": {}}}
            }
        }));
        let (body, _) = translate_request(req).unwrap();
        match body.output_format.unwrap() {
            OutputFormat::JsonSchema { schema } => {
                assert_eq!(schema["type"], "object");
            }
        }
    }

    #[test]
    fn response_format_json_object_is_silent_noop() {
        let req = req_from_json(json!({
            "model": "x",
            "messages": [],
            "response_format": {"type": "json_object"}
        }));
        let (body, _) = translate_request(req).unwrap();
        assert!(body.output_format.is_none());
    }

    #[test]
    fn user_field_writes_metadata_user_id() {
        let req = req_from_json(json!({
            "model": "x",
            "messages": [],
            "user": "u-42"
        }));
        let (body, _) = translate_request(req).unwrap();
        assert_eq!(
            body.metadata.unwrap().fields.get("user_id").unwrap(),
            "u-42"
        );
    }

    #[test]
    fn user_field_does_not_overwrite_existing_metadata_user_id() {
        let req = req_from_json(json!({
            "model": "x",
            "messages": [],
            "user": "u-42",
            "metadata": {"user_id": "from-metadata"}
        }));
        let (body, _) = translate_request(req).unwrap();
        assert_eq!(
            body.metadata.unwrap().fields.get("user_id").unwrap(),
            "from-metadata"
        );
    }

    #[test]
    fn metadata_drops_non_string_fields() {
        let req = req_from_json(json!({
            "model": "x",
            "messages": [],
            "metadata": {"keep": "yes", "drop": [1, 2], "also_drop": 3}
        }));
        let (body, _) = translate_request(req).unwrap();
        let meta = body.metadata.unwrap();
        assert_eq!(meta.fields.get("keep").unwrap(), "yes");
        assert!(!meta.fields.contains_key("drop"));
        assert!(!meta.fields.contains_key("also_drop"));
    }

    #[test]
    fn stop_string_normalized_to_vec() {
        let req = req_from_json(json!({
            "model": "x",
            "messages": [],
            "stop": "END"
        }));
        let (body, _) = translate_request(req).unwrap();
        assert_eq!(body.stop_sequences.unwrap(), vec!["END".to_string()]);
    }

    #[test]
    fn stop_array_truncated_to_four() {
        let req = req_from_json(json!({
            "model": "x",
            "messages": [],
            "stop": ["A", "B", "C", "D", "E", "F"]
        }));
        let (body, _) = translate_request(req).unwrap();
        let stops = body.stop_sequences.unwrap();
        assert_eq!(stops.len(), 4);
        assert_eq!(stops, vec!["A", "B", "C", "D"]);
    }

    #[test]
    fn max_completion_tokens_takes_precedence_over_max_tokens() {
        let req = req_from_json(json!({
            "model": "x",
            "messages": [],
            "max_tokens": 100,
            "max_completion_tokens": 250
        }));
        let (body, _) = translate_request(req).unwrap();
        assert_eq!(body.max_tokens, 250);
    }

    #[test]
    fn defaults_max_tokens_when_omitted() {
        let req = req_from_json(json!({"model": "x", "messages": []}));
        let (body, _) = translate_request(req).unwrap();
        assert_eq!(body.max_tokens, DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn rejects_n_gt_1() {
        let req = req_from_json(json!({"model": "x", "messages": [], "n": 2}));
        let err = translate_request(req).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rejects_logprobs_true() {
        let req = req_from_json(json!({"model": "x", "messages": [], "logprobs": true}));
        let err = translate_request(req).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn silently_ignores_penalty_and_seed_fields() {
        let req = req_from_json(json!({
            "model": "x",
            "messages": [{"role": "user", "content": "hi"}],
            "frequency_penalty": 1.0,
            "presence_penalty": 0.5,
            "logit_bias": {"42": -1},
            "seed": 7,
            "service_tier": "auto",
            "store": true,
            "top_logprobs": 3
        }));
        let result = translate_request(req);
        assert!(result.is_ok());
    }

    #[test]
    fn stream_flag_propagated() {
        let req = req_from_json(json!({
            "model": "x",
            "messages": [],
            "stream": true
        }));
        let (body, stream) = translate_request(req).unwrap();
        assert!(stream);
        assert_eq!(body.stream, Some(true));
    }

    #[test]
    fn include_reasoning_header_default_true() {
        let headers = HeaderMap::new();
        assert!(parse_include_reasoning(&headers));
    }

    #[test]
    fn include_reasoning_header_false_disables() {
        let mut headers = HeaderMap::new();
        headers.insert("x-include-reasoning", "false".parse().unwrap());
        assert!(!parse_include_reasoning(&headers));
        headers.insert("x-include-reasoning", "0".parse().unwrap());
        assert!(!parse_include_reasoning(&headers));
        headers.insert("x-include-reasoning", "NO".parse().unwrap());
        assert!(!parse_include_reasoning(&headers));
    }
}
