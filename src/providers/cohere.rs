//! Cohere Chat API provider implementation.
//!
//! This module implements the Provider trait for Cohere's `v2/chat` endpoint,
//! supporting streaming output text/thinking and function tool calls.

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ThinkingContent,
    ToolCall, Usage, UserContent,
};
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use crate::sse::SseStream;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;

// ============================================================================
// Constants
// ============================================================================

const COHERE_CHAT_API_URL: &str = "https://api.cohere.com/v2/chat";
const DEFAULT_MAX_TOKENS: u32 = 4096;

// ============================================================================
// Cohere Provider
// ============================================================================

/// Cohere `v2/chat` streaming provider.
pub struct CohereProvider {
    client: Client,
    model: String,
    base_url: String,
    provider: String,
}

impl CohereProvider {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            base_url: COHERE_CHAT_API_URL.to_string(),
            provider: "cohere".to_string(),
        }
    }

    #[must_use]
    pub fn with_provider_name(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    #[must_use]
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    fn build_request(&self, context: &Context, options: &StreamOptions) -> CohereRequest {
        let messages = build_cohere_messages(context);

        let tools: Option<Vec<CohereTool>> = if context.tools.is_empty() {
            None
        } else {
            Some(context.tools.iter().map(convert_tool_to_cohere).collect())
        };

        CohereRequest {
            model: self.model.clone(),
            messages,
            max_tokens: options.max_tokens.or(Some(DEFAULT_MAX_TOKENS)),
            temperature: options.temperature,
            tools,
            stream: true,
        }
    }
}

#[async_trait]
impl Provider for CohereProvider {
    fn name(&self) -> &str {
        &self.provider
    }

    fn api(&self) -> &'static str {
        "cohere-chat"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    async fn stream(
        &self,
        context: &Context,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let has_authorization_header = options
            .headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case("authorization"));

        let auth_value = if has_authorization_header {
            None
        } else {
            Some(
                options
                    .api_key
                    .clone()
                    .or_else(|| std::env::var("COHERE_API_KEY").ok())
                    .ok_or_else(|| Error::config("Missing Cohere API key"))?,
            )
        };

        let request_body = self.build_request(context, options);

        let mut request = self
            .client
            .post(&self.base_url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream");

        if let Some(auth_value) = auth_value {
            request = request.header("Authorization", format!("Bearer {auth_value}"));
        }

        for (key, value) in &options.headers {
            request = request.header(key, value);
        }

        let request = request.json(&request_body)?;

        let response = Box::pin(request.send()).await?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::api(format!(
                "Cohere API error (HTTP {status}): {body}"
            )));
        }

        let content_type = response
            .headers()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.to_ascii_lowercase());
        if !content_type
            .as_deref()
            .is_some_and(|value| value.contains("text/event-stream"))
        {
            let message = content_type.map_or_else(
                || {
                    format!(
                        "Cohere API protocol error (HTTP {status}): missing Content-Type (expected text/event-stream)"
                    )
                },
                |value| {
                    format!(
                        "Cohere API protocol error (HTTP {status}): unexpected Content-Type {value} (expected text/event-stream)"
                    )
                },
            );
            return Err(Error::api(message));
        }

        let event_source = SseStream::new(response.bytes_stream());

        let model = self.model.clone();
        let api = self.api().to_string();
        let provider = self.name().to_string();

        let stream = stream::unfold(
            StreamState::new(event_source, model, api, provider),
            |mut state| async move {
                loop {
                    if let Some(event) = state.pending_events.pop_front() {
                        return Some((Ok(event), state));
                    }

                    if state.finished {
                        return None;
                    }

                    match state.event_source.next().await {
                        Some(Ok(msg)) => {
                            if msg.data == "[DONE]" {
                                state.finish();
                                continue;
                            }

                            if let Err(e) = state.process_event(&msg.data) {
                                return Some((Err(e), state));
                            }
                        }
                        Some(Err(e)) => {
                            let err = Error::api(format!("SSE error: {e}"));
                            return Some((Err(err), state));
                        }
                        None => {
                            // Stream ended without message-end; surface a consistent error.
                            return Some((
                                Err(Error::api("Stream ended without Done event")),
                                state,
                            ));
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }
}

// ============================================================================
// Stream State
// ============================================================================

struct ToolCallAccum {
    content_index: usize,
    id: String,
    name: String,
    arguments: String,
}

struct StreamState<S>
where
    S: Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin,
{
    event_source: SseStream<S>,
    partial: AssistantMessage,
    pending_events: VecDeque<StreamEvent>,
    started: bool,
    finished: bool,
    content_index_map: HashMap<u32, usize>,
    active_tool_call: Option<ToolCallAccum>,
}

impl<S> StreamState<S>
where
    S: Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin,
{
    fn new(event_source: SseStream<S>, model: String, api: String, provider: String) -> Self {
        Self {
            event_source,
            partial: AssistantMessage {
                content: Vec::new(),
                api,
                provider,
                model,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: chrono::Utc::now().timestamp_millis(),
            },
            pending_events: VecDeque::new(),
            started: false,
            finished: false,
            content_index_map: HashMap::new(),
            active_tool_call: None,
        }
    }

    fn ensure_started(&mut self) {
        if !self.started {
            self.started = true;
            self.pending_events.push_back(StreamEvent::Start {
                partial: self.partial.clone(),
            });
        }
    }

    fn content_block_for(&mut self, idx: u32, kind: CohereContentKind) -> usize {
        if let Some(existing) = self.content_index_map.get(&idx) {
            return *existing;
        }

        let content_index = self.partial.content.len();
        match kind {
            CohereContentKind::Text => {
                self.partial
                    .content
                    .push(ContentBlock::Text(TextContent::new("")));
                self.pending_events.push_back(StreamEvent::TextStart {
                    content_index,
                    partial: self.partial.clone(),
                });
            }
            CohereContentKind::Thinking => {
                self.partial
                    .content
                    .push(ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                    }));
                self.pending_events.push_back(StreamEvent::ThinkingStart {
                    content_index,
                    partial: self.partial.clone(),
                });
            }
        }

        self.content_index_map.insert(idx, content_index);
        content_index
    }

    #[allow(clippy::too_many_lines)]
    fn process_event(&mut self, data: &str) -> Result<()> {
        let chunk: CohereStreamChunk =
            serde_json::from_str(data).map_err(|e| Error::api(format!("JSON parse error: {e}")))?;

        match chunk {
            CohereStreamChunk::MessageStart { .. } => {
                self.ensure_started();
            }
            CohereStreamChunk::ContentStart { index, delta } => {
                self.ensure_started();
                let (kind, initial) = delta.message.content.kind_and_text();
                let content_index = self.content_block_for(index, kind);

                if !initial.is_empty() {
                    match kind {
                        CohereContentKind::Text => {
                            if let Some(ContentBlock::Text(t)) =
                                self.partial.content.get_mut(content_index)
                            {
                                t.text.push_str(&initial);
                            }
                            self.pending_events.push_back(StreamEvent::TextDelta {
                                content_index,
                                delta: initial,
                                partial: self.partial.clone(),
                            });
                        }
                        CohereContentKind::Thinking => {
                            if let Some(ContentBlock::Thinking(t)) =
                                self.partial.content.get_mut(content_index)
                            {
                                t.thinking.push_str(&initial);
                            }
                            self.pending_events.push_back(StreamEvent::ThinkingDelta {
                                content_index,
                                delta: initial,
                                partial: self.partial.clone(),
                            });
                        }
                    }
                }
            }
            CohereStreamChunk::ContentDelta { index, delta } => {
                self.ensure_started();
                let (kind, delta_text) = delta.message.content.kind_and_text();
                let content_index = self.content_block_for(index, kind);

                match kind {
                    CohereContentKind::Text => {
                        if let Some(ContentBlock::Text(t)) =
                            self.partial.content.get_mut(content_index)
                        {
                            t.text.push_str(&delta_text);
                        }
                        self.pending_events.push_back(StreamEvent::TextDelta {
                            content_index,
                            delta: delta_text,
                            partial: self.partial.clone(),
                        });
                    }
                    CohereContentKind::Thinking => {
                        if let Some(ContentBlock::Thinking(t)) =
                            self.partial.content.get_mut(content_index)
                        {
                            t.thinking.push_str(&delta_text);
                        }
                        self.pending_events.push_back(StreamEvent::ThinkingDelta {
                            content_index,
                            delta: delta_text,
                            partial: self.partial.clone(),
                        });
                    }
                }
            }
            CohereStreamChunk::ContentEnd { index } => {
                if let Some(content_index) = self.content_index_map.get(&index).copied() {
                    match self.partial.content.get(content_index) {
                        Some(ContentBlock::Text(t)) => {
                            self.pending_events.push_back(StreamEvent::TextEnd {
                                content_index,
                                content: t.text.clone(),
                                partial: self.partial.clone(),
                            });
                        }
                        Some(ContentBlock::Thinking(t)) => {
                            self.pending_events.push_back(StreamEvent::ThinkingEnd {
                                content_index,
                                content: t.thinking.clone(),
                                partial: self.partial.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            CohereStreamChunk::ToolCallStart { delta } => {
                self.ensure_started();
                let tc = delta.message.tool_calls;
                let content_index = self.partial.content.len();
                self.partial.content.push(ContentBlock::ToolCall(ToolCall {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    arguments: serde_json::Value::Null,
                    thought_signature: None,
                }));

                self.active_tool_call = Some(ToolCallAccum {
                    content_index,
                    id: tc.id,
                    name: tc.function.name,
                    arguments: tc.function.arguments.clone(),
                });

                self.pending_events.push_back(StreamEvent::ToolCallStart {
                    content_index,
                    partial: self.partial.clone(),
                });
                if !tc.function.arguments.is_empty() {
                    self.pending_events.push_back(StreamEvent::ToolCallDelta {
                        content_index,
                        delta: tc.function.arguments,
                        partial: self.partial.clone(),
                    });
                }
            }
            CohereStreamChunk::ToolCallDelta { delta } => {
                self.ensure_started();
                if let Some(active) = self.active_tool_call.as_mut() {
                    active
                        .arguments
                        .push_str(&delta.message.tool_calls.function.arguments);
                    self.pending_events.push_back(StreamEvent::ToolCallDelta {
                        content_index: active.content_index,
                        delta: delta.message.tool_calls.function.arguments,
                        partial: self.partial.clone(),
                    });
                }
            }
            CohereStreamChunk::ToolCallEnd => {
                if let Some(active) = self.active_tool_call.take() {
                    self.ensure_started();
                    let parsed_args: serde_json::Value = serde_json::from_str(&active.arguments)
                        .unwrap_or_else(|e| {
                            tracing::warn!(
                                error = %e,
                                raw = %active.arguments,
                                "Failed to parse tool arguments as JSON"
                            );
                            serde_json::Value::Null
                        });

                    if let Some(ContentBlock::ToolCall(block)) =
                        self.partial.content.get_mut(active.content_index)
                    {
                        block.arguments = parsed_args.clone();
                    }

                    self.partial.stop_reason = StopReason::ToolUse;
                    self.pending_events.push_back(StreamEvent::ToolCallEnd {
                        content_index: active.content_index,
                        tool_call: ToolCall {
                            id: active.id,
                            name: active.name,
                            arguments: parsed_args,
                            thought_signature: None,
                        },
                        partial: self.partial.clone(),
                    });
                }
            }
            CohereStreamChunk::MessageEnd { delta } => {
                self.ensure_started();
                self.partial.usage.input = delta.usage.tokens.input_tokens;
                self.partial.usage.output = delta.usage.tokens.output_tokens;
                self.partial.usage.total_tokens =
                    delta.usage.tokens.input_tokens + delta.usage.tokens.output_tokens;

                self.partial.stop_reason = match delta.finish_reason.as_str() {
                    "MAX_TOKENS" => StopReason::Length,
                    "TOOL_CALL" => StopReason::ToolUse,
                    "ERROR" => StopReason::Error,
                    _ => StopReason::Stop,
                };

                self.finish();
            }
            CohereStreamChunk::Unknown => {}
        }

        Ok(())
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        let reason = self.partial.stop_reason;
        self.pending_events.push_back(StreamEvent::Done {
            reason,
            message: self.partial.clone(),
        });
        self.finished = true;
    }
}

// ============================================================================
// Cohere API Types (minimal)
// ============================================================================

#[derive(Debug, Serialize)]
struct CohereRequest {
    model: String,
    messages: Vec<CohereMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<CohereTool>>,
    stream: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum CohereMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<CohereToolCallRef>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_plan: Option<String>,
    },
    Tool {
        content: String,
        tool_call_id: String,
    },
}

#[derive(Debug, Serialize)]
struct CohereToolCallRef {
    id: String,
    #[serde(rename = "type")]
    r#type: &'static str,
    function: CohereFunctionRef,
}

#[derive(Debug, Serialize)]
struct CohereFunctionRef {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct CohereTool {
    #[serde(rename = "type")]
    r#type: &'static str,
    function: CohereFunction,
}

#[derive(Debug, Serialize)]
struct CohereFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: serde_json::Value,
}

fn convert_tool_to_cohere(tool: &ToolDef) -> CohereTool {
    CohereTool {
        r#type: "function",
        function: CohereFunction {
            name: tool.name.clone(),
            description: if tool.description.trim().is_empty() {
                None
            } else {
                Some(tool.description.clone())
            },
            parameters: tool.parameters.clone(),
        },
    }
}

fn build_cohere_messages(context: &Context) -> Vec<CohereMessage> {
    let mut out = Vec::new();

    if let Some(system) = &context.system_prompt {
        out.push(CohereMessage::System {
            content: system.clone(),
        });
    }

    for message in &context.messages {
        match message {
            Message::User(user) => out.push(CohereMessage::User {
                content: extract_text_user_content(&user.content),
            }),
            Message::Custom(custom) => out.push(CohereMessage::User {
                content: custom.content.clone(),
            }),
            Message::Assistant(assistant) => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();

                for block in &assistant.content {
                    match block {
                        ContentBlock::Text(t) => text.push_str(&t.text),
                        ContentBlock::ToolCall(tc) => tool_calls.push(CohereToolCallRef {
                            id: tc.id.clone(),
                            r#type: "function",
                            function: CohereFunctionRef {
                                name: tc.name.clone(),
                                arguments: tc.arguments.to_string(),
                            },
                        }),
                        _ => {}
                    }
                }

                out.push(CohereMessage::Assistant {
                    content: if tool_calls.is_empty() && !text.is_empty() {
                        Some(text)
                    } else {
                        None
                    },
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_plan: None,
                });
            }
            Message::ToolResult(result) => {
                let mut content = String::new();
                for (i, block) in result.content.iter().enumerate() {
                    if i > 0 {
                        content.push('\n');
                    }
                    if let ContentBlock::Text(t) = block {
                        content.push_str(&t.text);
                    }
                }
                out.push(CohereMessage::Tool {
                    content,
                    tool_call_id: result.tool_call_id.clone(),
                });
            }
        }
    }

    out
}

fn extract_text_user_content(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => {
            let mut out = String::new();
            for block in blocks {
                if let ContentBlock::Text(t) = block {
                    out.push_str(&t.text);
                }
            }
            out
        }
    }
}

// ============================================================================
// Cohere streaming chunk types (minimal, forward-compatible)
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CohereStreamChunk {
    #[serde(rename = "message-start")]
    MessageStart { id: Option<String> },
    #[serde(rename = "content-start")]
    ContentStart {
        index: u32,
        delta: CohereContentStartDelta,
    },
    #[serde(rename = "content-delta")]
    ContentDelta {
        index: u32,
        delta: CohereContentDelta,
    },
    #[serde(rename = "content-end")]
    ContentEnd { index: u32 },
    #[serde(rename = "tool-call-start")]
    ToolCallStart { delta: CohereToolCallStartDelta },
    #[serde(rename = "tool-call-delta")]
    ToolCallDelta { delta: CohereToolCallDelta },
    #[serde(rename = "tool-call-end")]
    ToolCallEnd,
    #[serde(rename = "message-end")]
    MessageEnd { delta: CohereMessageEndDelta },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct CohereContentStartDelta {
    message: CohereDeltaMessage<CohereContentStart>,
}

#[derive(Debug, Deserialize)]
struct CohereContentDelta {
    message: CohereDeltaMessage<CohereContentDeltaPart>,
}

#[derive(Debug, Deserialize)]
struct CohereDeltaMessage<T> {
    content: T,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CohereContentStart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CohereContentDeltaPart {
    Text { text: String },
    Thinking { thinking: String },
}

#[derive(Debug, Clone, Copy)]
enum CohereContentKind {
    Text,
    Thinking,
}

impl CohereContentStart {
    fn kind_and_text(self) -> (CohereContentKind, String) {
        match self {
            Self::Text { text } => (CohereContentKind::Text, text),
            Self::Thinking { thinking } => (CohereContentKind::Thinking, thinking),
        }
    }
}

impl CohereContentDeltaPart {
    fn kind_and_text(self) -> (CohereContentKind, String) {
        match self {
            Self::Text { text } => (CohereContentKind::Text, text),
            Self::Thinking { thinking } => (CohereContentKind::Thinking, thinking),
        }
    }
}

#[derive(Debug, Deserialize)]
struct CohereToolCallStartDelta {
    message: CohereToolCallMessage<CohereToolCallStartBody>,
}

#[derive(Debug, Deserialize)]
struct CohereToolCallDelta {
    message: CohereToolCallMessage<CohereToolCallDeltaBody>,
}

#[derive(Debug, Deserialize)]
struct CohereToolCallMessage<T> {
    tool_calls: T,
}

#[derive(Debug, Deserialize)]
struct CohereToolCallStartBody {
    id: String,
    function: CohereToolCallFunctionStart,
}

#[derive(Debug, Deserialize)]
struct CohereToolCallFunctionStart {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct CohereToolCallDeltaBody {
    function: CohereToolCallFunctionDelta,
}

#[derive(Debug, Deserialize)]
struct CohereToolCallFunctionDelta {
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct CohereMessageEndDelta {
    finish_reason: String,
    usage: CohereUsage,
}

#[derive(Debug, Deserialize)]
struct CohereUsage {
    tokens: CohereUsageTokens,
}

#[derive(Debug, Deserialize)]
struct CohereUsageTokens {
    input_tokens: u64,
    output_tokens: u64,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use futures::stream;

    #[test]
    fn test_provider_info() {
        let provider = CohereProvider::new("command-r");
        assert_eq!(provider.name(), "cohere");
        assert_eq!(provider.api(), "cohere-chat");
    }

    #[test]
    fn test_stream_parses_text_and_tool_call() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let events = vec![
                serde_json::json!({ "type": "message-start", "id": "msg_1" }),
                serde_json::json!({
                    "type": "content-start",
                    "index": 0,
                    "delta": { "message": { "content": { "type": "text", "text": "Hello" } } }
                }),
                serde_json::json!({
                    "type": "content-delta",
                    "index": 0,
                    "delta": { "message": { "content": { "text": " world" } } }
                }),
                serde_json::json!({ "type": "content-end", "index": 0 }),
                serde_json::json!({
                    "type": "tool-call-start",
                    "delta": { "message": { "tool_calls": { "id": "call_1", "type": "function", "function": { "name": "echo", "arguments": "{\"text\":\"hi\"}" } } } }
                }),
                serde_json::json!({ "type": "tool-call-end" }),
                serde_json::json!({
                    "type": "message-end",
                    "delta": { "finish_reason": "TOOL_CALL", "usage": { "tokens": { "input_tokens": 1, "output_tokens": 2 } } }
                }),
            ];

            let byte_stream = stream::iter(
                events
                    .iter()
                    .map(|event| format!("data: {}\n\n", serde_json::to_string(event).unwrap()))
                    .map(|s| Ok(s.into_bytes())),
            );

            let event_source = crate::sse::SseStream::new(Box::pin(byte_stream));
            let mut state = StreamState::new(
                event_source,
                "command-r".to_string(),
                "cohere-chat".to_string(),
                "cohere".to_string(),
            );

            let mut out = Vec::new();
            while let Some(item) = state.event_source.next().await {
                let msg = item.expect("SSE event");
                state.process_event(&msg.data).expect("process_event");
                out.extend(state.pending_events.drain(..));
                if state.finished {
                    break;
                }
            }

            assert!(matches!(out.first(), Some(StreamEvent::Start { .. })));
            assert!(out.iter().any(|e| matches!(e, StreamEvent::TextDelta { delta, .. } if delta.contains("Hello"))));
            assert!(out.iter().any(|e| matches!(e, StreamEvent::ToolCallEnd { tool_call, .. } if tool_call.name == "echo")));
            assert!(out.iter().any(|e| matches!(e, StreamEvent::Done { reason: StopReason::ToolUse, .. })));
        });
    }
}
