//! Streaming Anthropic SSE → OpenAI Chat Completions chunk transformer.
//!
//! The state machine is kept independent from axum so unit tests can feed
//! a [`Vec<StreamEvent>`] and assert on the resulting payload sequence.
//! [`to_openai_stream`] is the SSE consumer that drives the state machine
//! on top of the inner axum response.

use std::{collections::HashMap, convert::Infallible, time::Duration};

use axum::response::{
    IntoResponse,
    sse::{Event as SseEvent, KeepAlive, Sse},
};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Serialize;
use serde_json::json;

use crate::{
    middleware::openai::response::translate_upstream_error_body,
    types::{
        claude::{ContentBlock, ContentBlockDelta, StopReason, StreamEvent},
        openai::{
            ChatCompletionChunk, Delta, DeltaFunction, DeltaToolCall, OpenAIErrorBody,
            PromptTokensDetails, StreamChoice, UsageOut,
        },
    },
};

use super::response::map_stop_reason;

const STREAM_BODY_LIMIT: usize = 32 * 1024 * 1024;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy)]
enum BlockKind {
    Text,
    Tool,
    Thinking,
    Other,
}

#[derive(Debug, Default, Clone, Copy)]
struct AccumulatedUsage {
    input_tokens: u32,
    output_tokens: u32,
    cache_creation: u32,
    cache_read: u32,
}

impl AccumulatedUsage {
    fn merge_initial(&mut self, u: &crate::types::claude::Usage) {
        if u.input_tokens > 0 {
            self.input_tokens = u.input_tokens;
        }
        if u.output_tokens > 0 {
            self.output_tokens = u.output_tokens;
        }
        if let Some(cc) = u.cache_creation_input_tokens {
            self.cache_creation = cc;
        }
        if let Some(cr) = u.cache_read_input_tokens {
            self.cache_read = cr;
        }
    }

    fn merge_delta(&mut self, u: &crate::types::claude::StreamUsage) {
        // message_delta usage fields are cumulative — store, don't add.
        self.output_tokens = u.output_tokens;
        if u.input_tokens > 0 {
            self.input_tokens = u.input_tokens;
        }
        if let Some(cc) = u.cache_creation_input_tokens {
            self.cache_creation = cc;
        }
        if let Some(cr) = u.cache_read_input_tokens {
            self.cache_read = cr;
        }
    }

    fn to_out(self) -> UsageOut {
        let prompt_tokens = self.input_tokens + self.cache_creation + self.cache_read;
        UsageOut {
            prompt_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: prompt_tokens + self.output_tokens,
            prompt_tokens_details: Some(PromptTokensDetails {
                cached_tokens: self.cache_read,
            }),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum StreamPayload {
    Chunk(ChatCompletionChunk),
    Error { error: OpenAIErrorBody },
}

pub(crate) struct StreamState {
    completion_id: String,
    created: i64,
    model: String,
    block_kind: HashMap<usize, BlockKind>,
    tool_sub_idx: HashMap<usize, usize>,
    next_tool_idx: usize,
    usage: AccumulatedUsage,
    pending_finish_reason: Option<String>,
    role_chunk_sent: bool,
    finished: bool,
    seen_usage: bool,
}

impl StreamState {
    pub(crate) fn new(request_model: String) -> Self {
        Self {
            completion_id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            created: chrono::Utc::now().timestamp(),
            model: request_model,
            block_kind: HashMap::new(),
            tool_sub_idx: HashMap::new(),
            next_tool_idx: 0,
            usage: AccumulatedUsage::default(),
            pending_finish_reason: None,
            role_chunk_sent: false,
            finished: false,
            seen_usage: false,
        }
    }

    fn chunk_with_choice(
        &self,
        delta: Delta,
        finish_reason: Option<String>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: self.completion_id.clone(),
            object: ChatCompletionChunk::OBJECT,
            created: self.created,
            model: self.model.clone(),
            choices: vec![StreamChoice {
                index: 0,
                delta,
                finish_reason,
            }],
            usage: None,
        }
    }

    fn chunk_usage_only(&self) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: self.completion_id.clone(),
            object: ChatCompletionChunk::OBJECT,
            created: self.created,
            model: self.model.clone(),
            choices: Vec::new(),
            usage: Some(self.usage.to_out()),
        }
    }

    fn map_terminal_finish(&self) -> String {
        if let Some(reason) = &self.pending_finish_reason {
            return reason.clone();
        }
        let has_tools = self.next_tool_idx > 0;
        map_stop_reason(None, has_tools).to_string()
    }

    pub(crate) fn process(
        &mut self,
        event: StreamEvent,
        include_reasoning: bool,
        include_usage: bool,
    ) -> Vec<StreamPayload> {
        if self.finished {
            return Vec::new();
        }
        let mut out = Vec::new();
        match event {
            StreamEvent::MessageStart { message } => {
                if !message.model.is_empty() {
                    self.model = message.model;
                }
                if let Some(usage) = message.usage.as_ref() {
                    self.usage.merge_initial(usage);
                    self.seen_usage = true;
                }
                if !self.role_chunk_sent {
                    self.role_chunk_sent = true;
                    out.push(StreamPayload::Chunk(self.chunk_with_choice(
                        Delta {
                            role: Some("assistant".to_string()),
                            ..Delta::default()
                        },
                        None,
                    )));
                }
            }
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => match content_block {
                ContentBlock::Text { .. } => {
                    self.block_kind.insert(index, BlockKind::Text);
                }
                ContentBlock::ToolUse { id, name, .. } => {
                    self.block_kind.insert(index, BlockKind::Tool);
                    let sub_idx = self.next_tool_idx;
                    self.next_tool_idx += 1;
                    self.tool_sub_idx.insert(index, sub_idx);
                    out.push(StreamPayload::Chunk(self.chunk_with_choice(
                        Delta {
                            tool_calls: Some(vec![DeltaToolCall {
                                index: sub_idx,
                                id: Some(id),
                                kind: Some("function".to_string()),
                                function: DeltaFunction {
                                    name: Some(name),
                                    arguments: Some(String::new()),
                                },
                            }]),
                            ..Delta::default()
                        },
                        None,
                    )));
                }
                ContentBlock::Thinking { .. } => {
                    self.block_kind.insert(index, BlockKind::Thinking);
                }
                _ => {
                    self.block_kind.insert(index, BlockKind::Other);
                }
            },
            StreamEvent::ContentBlockDelta { index, delta } => {
                let kind = self
                    .block_kind
                    .get(&index)
                    .copied()
                    .unwrap_or(BlockKind::Other);
                match (kind, delta) {
                    (BlockKind::Text, ContentBlockDelta::TextDelta { text }) => {
                        out.push(StreamPayload::Chunk(self.chunk_with_choice(
                            Delta {
                                content: Some(text),
                                ..Delta::default()
                            },
                            None,
                        )));
                    }
                    (BlockKind::Tool, ContentBlockDelta::InputJsonDelta { partial_json }) => {
                        if let Some(&sub_idx) = self.tool_sub_idx.get(&index) {
                            out.push(StreamPayload::Chunk(self.chunk_with_choice(
                                Delta {
                                    tool_calls: Some(vec![DeltaToolCall {
                                        index: sub_idx,
                                        id: None,
                                        kind: None,
                                        function: DeltaFunction {
                                            name: None,
                                            arguments: Some(partial_json),
                                        },
                                    }]),
                                    ..Delta::default()
                                },
                                None,
                            )));
                        }
                    }
                    (BlockKind::Thinking, ContentBlockDelta::ThinkingDelta { thinking })
                        if include_reasoning =>
                    {
                        out.push(StreamPayload::Chunk(self.chunk_with_choice(
                            Delta {
                                reasoning_content: Some(thinking),
                                ..Delta::default()
                            },
                            None,
                        )));
                    }
                    _ => {}
                }
            }
            StreamEvent::ContentBlockStop { .. } => {}
            StreamEvent::MessageDelta { delta, usage } => {
                if let Some(stop) = delta.stop_reason {
                    let has_tools = self.next_tool_idx > 0;
                    self.pending_finish_reason =
                        Some(map_stop_reason(Some(&stop), has_tools).to_string());
                }
                if let Some(u) = usage.as_ref() {
                    self.usage.merge_delta(u);
                    self.seen_usage = true;
                }
            }
            StreamEvent::MessageStop => {
                let finish_reason = self.map_terminal_finish();
                out.push(StreamPayload::Chunk(
                    self.chunk_with_choice(Delta::default(), Some(finish_reason)),
                ));
                if include_usage && self.seen_usage {
                    out.push(StreamPayload::Chunk(self.chunk_usage_only()));
                }
                self.finished = true;
            }
            StreamEvent::Ping => {}
            StreamEvent::Error { error } => {
                out.push(StreamPayload::Error {
                    error: OpenAIErrorBody {
                        message: error.message,
                        kind: "upstream_error".to_string(),
                        code: None,
                        param: None,
                    },
                });
                self.finished = true;
            }
        }
        out
    }

    /// Called when the upstream stream ends without a `message_stop`; emits a
    /// terminal chunk so OAI clients still see a `finish_reason`.
    pub(crate) fn finalize(&mut self, include_usage: bool) -> Vec<StreamPayload> {
        if self.finished {
            return Vec::new();
        }
        let finish_reason = self.map_terminal_finish();
        let mut out = vec![StreamPayload::Chunk(
            self.chunk_with_choice(Delta::default(), Some(finish_reason)),
        )];
        if include_usage && self.seen_usage {
            out.push(StreamPayload::Chunk(self.chunk_usage_only()));
        }
        self.finished = true;
        out
    }
}

fn payload_to_data(payload: &StreamPayload) -> Option<String> {
    match payload {
        StreamPayload::Chunk(c) => serde_json::to_string(c).ok(),
        StreamPayload::Error { error } => serde_json::to_string(&json!({ "error": error })).ok(),
    }
}

/// Wrap an upstream Anthropic SSE response into an OpenAI-compatible
/// `text/event-stream` body. Driven from `code_provider.invoke()`'s SSE
/// output; this function does not touch billing or slot state — those run
/// inside the inner stream's existing `tap` pipeline.
pub fn to_openai_stream(
    resp: axum::response::Response,
    include_reasoning: bool,
    include_usage: bool,
    request_model: String,
) -> axum::response::Response {
    let (parts, body) = resp.into_parts();

    let stream = async_stream::stream! {
        if !parts.status.is_success() {
            let bytes = axum::body::to_bytes(body, STREAM_BODY_LIMIT)
                .await
                .map(|b| b.to_vec())
                .unwrap_or_default();
            let err = translate_upstream_error_body(&bytes);
            if let Ok(json) = serde_json::to_string(&json!({ "error": err })) {
                yield Ok::<_, Infallible>(SseEvent::default().data(json));
            }
            yield Ok::<_, Infallible>(SseEvent::default().data("[DONE]"));
            return;
        }

        let mut state = StreamState::new(request_model);
        let mut events = body.into_data_stream().eventsource();
        let mut upstream_errored = false;

        while let Some(item) = events.next().await {
            match item {
                Ok(event) => {
                    if event.data == "[DONE]" {
                        continue;
                    }
                    let Ok(parsed) = serde_json::from_str::<StreamEvent>(&event.data) else {
                        continue;
                    };
                    for payload in state.process(parsed, include_reasoning, include_usage) {
                        if let Some(data) = payload_to_data(&payload) {
                            yield Ok::<_, Infallible>(SseEvent::default().data(data));
                        }
                    }
                    if state.finished {
                        break;
                    }
                }
                Err(e) => {
                    upstream_errored = true;
                    let body = OpenAIErrorBody {
                        message: format!("upstream stream error: {e}"),
                        kind: "upstream_error".to_string(),
                        code: None,
                        param: None,
                    };
                    let payload = StreamPayload::Error { error: body };
                    if let Some(data) = payload_to_data(&payload) {
                        yield Ok::<_, Infallible>(SseEvent::default().data(data));
                    }
                    break;
                }
            }
        }

        if !upstream_errored {
            for payload in state.finalize(include_usage) {
                if let Some(data) = payload_to_data(&payload) {
                    yield Ok::<_, Infallible>(SseEvent::default().data(data));
                }
            }
        }

        yield Ok::<_, Infallible>(SseEvent::default().data("[DONE]"));
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEPALIVE_INTERVAL))
        .into_response()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::claude::{
        MessageDeltaContent, MessageStartContent, Role, StreamError, StreamUsage, Usage,
    };

    fn collect_chunks(payloads: &[StreamPayload]) -> Vec<&ChatCompletionChunk> {
        payloads
            .iter()
            .filter_map(|p| match p {
                StreamPayload::Chunk(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    fn drive(
        events: Vec<StreamEvent>,
        include_reasoning: bool,
        include_usage: bool,
    ) -> Vec<StreamPayload> {
        let mut state = StreamState::new("claude-sonnet-4-6".to_string());
        let mut out = Vec::new();
        for e in events {
            out.extend(state.process(e, include_reasoning, include_usage));
        }
        out.extend(state.finalize(include_usage));
        out
    }

    fn message_start(model: &str, usage: Option<Usage>) -> StreamEvent {
        StreamEvent::MessageStart {
            message: MessageStartContent {
                id: "msg_x".to_string(),
                type_: "message".to_string(),
                role: Role::Assistant,
                content: Vec::new(),
                model: model.to_string(),
                stop_reason: None,
                stop_sequence: None,
                usage,
            },
        }
    }

    #[test]
    fn text_stream_emits_role_chunk_then_content_chunks_then_finish() {
        let events = vec![
            message_start("claude-sonnet-4-6", None),
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::text(""),
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::TextDelta {
                    text: "hello ".to_string(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::TextDelta {
                    text: "world".to_string(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaContent {
                    stop_reason: Some(StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: None,
            },
            StreamEvent::MessageStop,
        ];
        let payloads = drive(events, true, false);
        let chunks = collect_chunks(&payloads);
        assert_eq!(chunks.len(), 4);
        assert_eq!(
            chunks[0].choices[0].delta.role.as_deref(),
            Some("assistant")
        );
        assert_eq!(
            chunks[1].choices[0].delta.content.as_deref(),
            Some("hello ")
        );
        assert_eq!(chunks[2].choices[0].delta.content.as_deref(), Some("world"));
        assert_eq!(chunks[3].choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn tool_call_stream_emits_initial_chunk_then_argument_fragments() {
        let events = vec![
            message_start("claude-sonnet-4-6", None),
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::ToolUse {
                    id: "toolu_1".to_string(),
                    name: "calc".to_string(),
                    input: serde_json::json!({}),
                    cache_control: None,
                    caller: None,
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::InputJsonDelta {
                    partial_json: "{\"x\":".to_string(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::InputJsonDelta {
                    partial_json: "1}".to_string(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaContent {
                    stop_reason: Some(StopReason::ToolUse),
                    stop_sequence: None,
                },
                usage: None,
            },
            StreamEvent::MessageStop,
        ];
        let payloads = drive(events, true, false);
        let chunks = collect_chunks(&payloads);
        // role + tool_start + 2 arg fragments + finish
        assert_eq!(chunks.len(), 5);
        let tool_start = &chunks[1].choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tool_start.index, 0);
        assert_eq!(tool_start.id.as_deref(), Some("toolu_1"));
        assert_eq!(tool_start.kind.as_deref(), Some("function"));
        assert_eq!(tool_start.function.name.as_deref(), Some("calc"));
        assert_eq!(tool_start.function.arguments.as_deref(), Some(""));
        let frag1 = &chunks[2].choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(frag1.index, 0);
        assert!(frag1.id.is_none());
        assert!(frag1.kind.is_none());
        assert_eq!(frag1.function.arguments.as_deref(), Some("{\"x\":"));
        let frag2 = &chunks[3].choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(frag2.function.arguments.as_deref(), Some("1}"));
        assert_eq!(
            chunks[4].choices[0].finish_reason.as_deref(),
            Some("tool_calls")
        );
    }

    #[test]
    fn thinking_block_emits_reasoning_content_delta() {
        let events = vec![
            message_start("claude-sonnet-4-6", None),
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Thinking {
                    signature: "sig".to_string(),
                    thinking: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ThinkingDelta {
                    thinking: "let me think".to_string(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaContent {
                    stop_reason: Some(StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: None,
            },
            StreamEvent::MessageStop,
        ];
        let payloads = drive(events, true, false);
        let chunks = collect_chunks(&payloads);
        let thinking = chunks
            .iter()
            .find(|c| c.choices[0].delta.reasoning_content.is_some())
            .unwrap();
        assert_eq!(
            thinking.choices[0].delta.reasoning_content.as_deref(),
            Some("let me think")
        );
    }

    #[test]
    fn thinking_dropped_when_include_reasoning_disabled() {
        let events = vec![
            message_start("claude-sonnet-4-6", None),
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Thinking {
                    signature: "sig".to_string(),
                    thinking: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ThinkingDelta {
                    thinking: "let me think".to_string(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageStop,
        ];
        let payloads = drive(events, false, false);
        let chunks = collect_chunks(&payloads);
        assert!(
            chunks
                .iter()
                .all(|c| c.choices[0].delta.reasoning_content.is_none())
        );
    }

    #[test]
    fn include_usage_emits_usage_chunk_with_empty_choices() {
        let events = vec![
            message_start(
                "claude-sonnet-4-6",
                Some(Usage {
                    input_tokens: 80,
                    output_tokens: 0,
                    cache_creation_input_tokens: Some(10),
                    cache_read_input_tokens: Some(20),
                }),
            ),
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::text(""),
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::TextDelta {
                    text: "hi".to_string(),
                },
            },
            StreamEvent::MessageDelta {
                delta: MessageDeltaContent {
                    stop_reason: Some(StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: Some(StreamUsage {
                    input_tokens: 80,
                    output_tokens: 25,
                    cache_creation_input_tokens: Some(10),
                    cache_read_input_tokens: Some(20),
                }),
            },
            StreamEvent::MessageStop,
        ];
        let payloads = drive(events, true, true);
        let chunks = collect_chunks(&payloads);
        let usage_chunk = chunks
            .iter()
            .find(|c| c.choices.is_empty() && c.usage.is_some())
            .expect("usage chunk should be emitted");
        let usage = usage_chunk.usage.as_ref().unwrap();
        assert_eq!(usage.prompt_tokens, 110);
        assert_eq!(usage.completion_tokens, 25);
        assert_eq!(usage.total_tokens, 135);
        assert_eq!(
            usage.prompt_tokens_details.as_ref().unwrap().cached_tokens,
            20
        );
    }

    #[test]
    fn include_usage_skipped_when_no_usage_seen() {
        let events = vec![
            message_start("claude-sonnet-4-6", None),
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::text(""),
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::TextDelta {
                    text: "hi".to_string(),
                },
            },
            StreamEvent::MessageStop,
        ];
        let payloads = drive(events, true, true);
        let chunks = collect_chunks(&payloads);
        assert!(chunks.iter().all(|c| c.usage.is_none()));
    }

    #[test]
    fn upstream_error_event_emits_error_payload_and_finishes() {
        let events = vec![
            message_start("claude-sonnet-4-6", None),
            StreamEvent::Error {
                error: StreamError {
                    type_: "overloaded_error".to_string(),
                    message: "upstream overloaded".to_string(),
                },
            },
        ];
        let payloads = drive(events, true, false);
        let err = payloads
            .iter()
            .find(|p| matches!(p, StreamPayload::Error { .. }))
            .expect("error payload should be present");
        match err {
            StreamPayload::Error { error } => {
                assert_eq!(error.kind, "upstream_error");
                assert_eq!(error.message, "upstream overloaded");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn stream_ending_without_message_stop_emits_fallback_finish() {
        let events = vec![
            message_start("claude-sonnet-4-6", None),
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::text(""),
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::TextDelta {
                    text: "hi".to_string(),
                },
            },
            // No message_stop — caller drops the stream.
        ];
        let payloads = drive(events, true, false);
        let chunks = collect_chunks(&payloads);
        let last = chunks.last().unwrap();
        assert!(last.choices[0].finish_reason.is_some());
        assert_eq!(last.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn ping_and_content_block_stop_emit_nothing() {
        let events = vec![
            StreamEvent::Ping,
            StreamEvent::ContentBlockStop { index: 0 },
        ];
        let mut state = StreamState::new("m".to_string());
        for e in events {
            assert!(state.process(e, true, false).is_empty());
        }
    }

    #[test]
    fn signature_delta_is_dropped() {
        let events = vec![
            message_start("m", None),
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Thinking {
                    signature: String::new(),
                    thinking: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::SignatureDelta {
                    signature: "abc".to_string(),
                },
            },
            StreamEvent::MessageStop,
        ];
        let payloads = drive(events, true, false);
        let chunks = collect_chunks(&payloads);
        // Only role chunk and finish chunk; signature drops.
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].choices[0].delta.role.is_some());
        assert!(chunks[1].choices[0].finish_reason.is_some());
    }

    #[test]
    fn max_tokens_stop_reason_maps_to_length() {
        let events = vec![
            message_start("m", None),
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::text(""),
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::TextDelta {
                    text: "x".to_string(),
                },
            },
            StreamEvent::MessageDelta {
                delta: MessageDeltaContent {
                    stop_reason: Some(StopReason::MaxTokens),
                    stop_sequence: None,
                },
                usage: None,
            },
            StreamEvent::MessageStop,
        ];
        let payloads = drive(events, true, false);
        let chunks = collect_chunks(&payloads);
        assert_eq!(
            chunks.last().unwrap().choices[0].finish_reason.as_deref(),
            Some("length")
        );
    }

    #[test]
    fn role_chunk_only_emitted_once_across_multiple_message_starts() {
        // Defensive: shouldn't normally happen but the state machine must not
        // double-emit the assistant role chunk.
        let events = vec![message_start("m", None), message_start("m", None)];
        let payloads = drive(events, true, false);
        let chunks = collect_chunks(&payloads);
        let role_chunks: Vec<_> = chunks
            .iter()
            .filter(|c| c.choices.first().is_some_and(|s| s.delta.role.is_some()))
            .collect();
        assert_eq!(role_chunks.len(), 1);
    }

    #[test]
    fn tool_use_first_then_endturn_still_reports_tool_calls() {
        let events = vec![
            message_start("m", None),
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "n".to_string(),
                    input: serde_json::json!({}),
                    cache_control: None,
                    caller: None,
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaContent {
                    stop_reason: Some(StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: None,
            },
            StreamEvent::MessageStop,
        ];
        let payloads = drive(events, true, false);
        let chunks = collect_chunks(&payloads);
        assert_eq!(
            chunks.last().unwrap().choices[0].finish_reason.as_deref(),
            Some("tool_calls")
        );
    }
}
