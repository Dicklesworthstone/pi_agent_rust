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

        // Content-Type set by .json() below
        let mut request = self
            .client
            .post(&self.base_url)
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
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn test_provider_info() {
        let provider = CohereProvider::new("command-r");
        assert_eq!(provider.name(), "cohere");
        assert_eq!(provider.api(), "cohere-chat");
    }

    #[test]
    fn test_build_request_includes_system_tools_and_v2_shape() {
        let provider = CohereProvider::new("command-r");
        let context = Context {
            system_prompt: Some("You are concise.".to_string()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })],
            tools: vec![ToolDef {
                name: "search".to_string(),
                description: "Search docs".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "q": { "type": "string" }
                    },
                    "required": ["q"]
                }),
            }],
        };
        let options = StreamOptions {
            temperature: Some(0.2),
            max_tokens: Some(123),
            ..Default::default()
        };

        let request = provider.build_request(&context, &options);
        let value = serde_json::to_value(&request).expect("serialize request");

        assert_eq!(value["model"], "command-r");
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["messages"][0]["content"], "You are concise.");
        assert_eq!(value["messages"][1]["role"], "user");
        assert_eq!(value["messages"][1]["content"], "Ping");
        assert_eq!(value["stream"], true);
        assert_eq!(value["max_tokens"], 123);
        let temperature = value["temperature"]
            .as_f64()
            .expect("temperature should be numeric");
        assert!((temperature - 0.2).abs() < 1e-6);
        assert_eq!(value["tools"][0]["type"], "function");
        assert_eq!(value["tools"][0]["function"]["name"], "search");
        assert_eq!(value["tools"][0]["function"]["description"], "Search docs");
        assert_eq!(
            value["tools"][0]["function"]["parameters"],
            json!({
                "type": "object",
                "properties": {
                    "q": { "type": "string" }
                },
                "required": ["q"]
            })
        );
    }

    #[test]
    fn test_convert_tool_to_cohere_omits_empty_description() {
        let tool = ToolDef {
            name: "echo".to_string(),
            description: "   ".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                }
            }),
        };

        let converted = convert_tool_to_cohere(&tool);
        let value = serde_json::to_value(converted).expect("serialize converted tool");
        assert_eq!(value["type"], "function");
        assert_eq!(value["function"]["name"], "echo");
        assert!(value["function"].get("description").is_none());
    }

    #[test]
    fn test_stream_parses_text_and_tool_call() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let events = [
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

    #[test]
    fn test_stream_parses_thinking_and_max_tokens_stop_reason() {
        let events = vec![
            json!({ "type": "message-start", "id": "msg_1" }),
            json!({
                "type": "content-start",
                "index": 0,
                "delta": { "message": { "content": { "type": "thinking", "thinking": "Plan" } } }
            }),
            json!({
                "type": "content-delta",
                "index": 0,
                "delta": { "message": { "content": { "thinking": " more" } } }
            }),
            json!({ "type": "content-end", "index": 0 }),
            json!({
                "type": "message-end",
                "delta": { "finish_reason": "MAX_TOKENS", "usage": { "tokens": { "input_tokens": 2, "output_tokens": 3 } } }
            }),
        ];

        let out = collect_events(&events);
        assert!(
            out.iter()
                .any(|e| matches!(e, StreamEvent::ThinkingStart { .. }))
        );
        assert!(out.iter().any(
            |e| matches!(e, StreamEvent::ThinkingDelta { delta, .. } if delta.contains("Plan"))
        ));
        assert!(
            out.iter()
                .any(|e| matches!(e, StreamEvent::ThinkingEnd { content, .. } if content.contains("Plan more")))
        );
        assert!(out.iter().any(|e| matches!(
            e,
            StreamEvent::Done {
                reason: StopReason::Length,
                ..
            }
        )));
    }

    #[test]
    fn test_stream_sets_bearer_auth_header() {
        let captured = run_stream_and_capture_headers(Some("test-cohere-key"), HashMap::new())
            .expect("captured request");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer test-cohere-key")
        );
        assert_eq!(
            captured.headers.get("accept").map(String::as_str),
            Some("text/event-stream")
        );

        let body: Value = serde_json::from_str(&captured.body).expect("body json");
        assert_eq!(body["model"], "command-r");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn test_stream_uses_existing_authorization_header_without_api_key() {
        let mut headers = HashMap::new();
        headers.insert(
            "Authorization".to_string(),
            "Bearer from-custom-header".to_string(),
        );
        headers.insert("X-Test".to_string(), "1".to_string());

        let captured = run_stream_and_capture_headers(None, headers).expect("captured request");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer from-custom-header")
        );
        assert_eq!(
            captured.headers.get("x-test").map(String::as_str),
            Some("1")
        );
    }

    fn collect_events(events: &[Value]) -> Vec<StreamEvent> {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let byte_stream = stream::iter(
                events
                    .iter()
                    .map(|event| {
                        format!(
                            "data: {}\n\n",
                            serde_json::to_string(event).expect("serialize event")
                        )
                    })
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
            out
        })
    }

    #[derive(Debug)]
    struct CapturedRequest {
        headers: HashMap<String, String>,
        body: String,
    }

    fn run_stream_and_capture_headers(
        api_key: Option<&str>,
        extra_headers: HashMap<String, String>,
    ) -> Option<CapturedRequest> {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = CohereProvider::new("command-r").with_base_url(base_url);
        let context = Context {
            system_prompt: Some("system".to_string()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            tools: Vec::new(),
        };
        let options = StreamOptions {
            api_key: api_key.map(str::to_string),
            headers: extra_headers,
            ..Default::default()
        };

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider.stream(&context, &options).await.expect("stream");
            while let Some(event) = stream.next().await {
                if matches!(event.expect("stream event"), StreamEvent::Done { .. }) {
                    break;
                }
            }
        });

        rx.recv_timeout(Duration::from_secs(2)).ok()
    }

    fn success_sse_body() -> String {
        [
            r#"data: {"type":"message-start","id":"msg_1"}"#,
            "",
            r#"data: {"type":"message-end","delta":{"finish_reason":"COMPLETE","usage":{"tokens":{"input_tokens":1,"output_tokens":1}}}}"#,
            "",
        ]
        .join("\n")
    }

    fn spawn_test_server(
        status_code: u16,
        content_type: &str,
        body: &str,
    ) -> (String, mpsc::Receiver<CapturedRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = mpsc::channel();
        let body = body.to_string();
        let content_type = content_type.to_string();

        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set read timeout");

            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                match socket.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        bytes.extend_from_slice(&chunk[..n]);
                        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            || err.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(err) => panic!("read request failed: {err}"),
                }
            }

            let header_end = bytes
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .expect("request header boundary");
            let header_text = String::from_utf8_lossy(&bytes[..header_end]).to_string();
            let headers = parse_headers(&header_text);
            let mut request_body = bytes[header_end + 4..].to_vec();

            let content_length = headers
                .get("content-length")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            while request_body.len() < content_length {
                match socket.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => request_body.extend_from_slice(&chunk[..n]),
                    Err(err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            || err.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(err) => panic!("read request body failed: {err}"),
                }
            }

            let captured = CapturedRequest {
                headers,
                body: String::from_utf8_lossy(&request_body).to_string(),
            };
            tx.send(captured).expect("send captured request");

            let reason = match status_code {
                401 => "Unauthorized",
                500 => "Internal Server Error",
                _ => "OK",
            };
            let response = format!(
                "HTTP/1.1 {status_code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .expect("write response");
            socket.flush().expect("flush response");
        });

        (format!("http://{addr}"), rx)
    }

    fn parse_headers(header_text: &str) -> HashMap<String, String> {
        let mut headers = HashMap::new();
        for line in header_text.lines().skip(1) {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        headers
    }

    // ─── Request body format tests ──────────────────────────────────────

    #[test]
    fn test_build_request_no_system_prompt() {
        let provider = CohereProvider::new("command-r-plus");
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Hi".to_string()),
                timestamp: 0,
            })],
            tools: vec![],
        };
        let options = StreamOptions::default();

        let req = provider.build_request(&context, &options);
        let value = serde_json::to_value(&req).expect("serialize");

        // First message should be user, no system message.
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(value["messages"][0]["content"], "Hi");
        // No system role message at all.
        let msgs = value["messages"].as_array().unwrap();
        assert!(
            !msgs.iter().any(|m| m["role"] == "system"),
            "No system message should be present"
        );
    }

    #[test]
    fn test_build_request_default_max_tokens() {
        let provider = CohereProvider::new("command-r");
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("test".to_string()),
                timestamp: 0,
            })],
            tools: vec![],
        };
        let options = StreamOptions::default();

        let req = provider.build_request(&context, &options);
        let value = serde_json::to_value(&req).expect("serialize");

        assert_eq!(value["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn test_build_request_no_tools_omits_tools_field() {
        let provider = CohereProvider::new("command-r");
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("test".to_string()),
                timestamp: 0,
            })],
            tools: vec![],
        };
        let options = StreamOptions::default();

        let req = provider.build_request(&context, &options);
        let value = serde_json::to_value(&req).expect("serialize");

        assert!(
            value.get("tools").is_none() || value["tools"].is_null(),
            "tools field should be omitted when empty"
        );
    }

    #[test]
    fn test_build_request_full_conversation_with_tool_call_and_result() {
        let provider = CohereProvider::new("command-r");
        let context = Context {
            system_prompt: Some("Be concise.".to_string()),
            messages: vec![
                Message::User(crate::model::UserMessage {
                    content: UserContent::Text("Read /tmp/a.txt".to_string()),
                    timestamp: 0,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "call_1".to_string(),
                        name: "read".to_string(),
                        arguments: serde_json::json!({"path": "/tmp/a.txt"}),
                        thought_signature: None,
                    })],
                    api: "cohere-chat".to_string(),
                    provider: "cohere".to_string(),
                    model: "command-r".to_string(),
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    timestamp: 1,
                }),
                Message::ToolResult(crate::model::ToolResultMessage {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "read".to_string(),
                    content: vec![ContentBlock::Text(TextContent::new("file contents"))],
                    details: None,
                    is_error: false,
                    timestamp: 2,
                }),
            ],
            tools: vec![ToolDef {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({"type": "object"}),
            }],
        };
        let options = StreamOptions::default();

        let req = provider.build_request(&context, &options);
        let value = serde_json::to_value(&req).expect("serialize");

        let msgs = value["messages"].as_array().unwrap();
        // system, user, assistant, tool
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[3]["role"], "tool");

        // Assistant message should have tool_calls, not content text.
        assert!(msgs[2].get("content").is_none() || msgs[2]["content"].is_null());
        let tool_calls = msgs[2]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_1");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "read");

        // Tool result should reference the tool_call_id.
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[3]["content"], "file contents");
    }

    #[test]
    fn test_convert_custom_message_to_cohere() {
        let context = Context {
            system_prompt: None,
            messages: vec![Message::Custom(crate::model::CustomMessage {
                custom_type: "extension_note".to_string(),
                content: "Important context.".to_string(),
                display: false,
                details: None,
                timestamp: 0,
            })],
            tools: vec![],
        };

        let msgs = build_cohere_messages(&context);
        assert_eq!(msgs.len(), 1);
        let value = serde_json::to_value(&msgs[0]).expect("serialize");
        assert_eq!(value["role"], "user");
        assert_eq!(value["content"], "Important context.");
    }

    #[test]
    fn test_convert_user_blocks_extracts_text_only() {
        let content = UserContent::Blocks(vec![
            ContentBlock::Text(TextContent::new("part 1")),
            ContentBlock::Image(crate::model::ImageContent {
                data: "aGVsbG8=".to_string(),
                mime_type: "image/png".to_string(),
            }),
            ContentBlock::Text(TextContent::new("part 2")),
        ]);

        let text = extract_text_user_content(&content);
        assert_eq!(text, "part 1part 2");
    }

    // ─── Provider builder tests ─────────────────────────────────────────

    #[test]
    fn test_custom_provider_name() {
        let provider = CohereProvider::new("command-r").with_provider_name("my-proxy");
        assert_eq!(provider.name(), "my-proxy");
        assert_eq!(provider.api(), "cohere-chat");
    }

    #[test]
    fn test_custom_base_url() {
        let provider =
            CohereProvider::new("command-r").with_base_url("https://proxy.example.com/v2/chat");
        assert_eq!(provider.base_url, "https://proxy.example.com/v2/chat");
    }

    // ─── Stream event parsing tests ─────────────────────────────────────

    #[test]
    fn test_stream_complete_finish_reason_maps_to_stop() {
        let events = vec![
            json!({ "type": "message-start", "id": "msg_1" }),
            json!({
                "type": "message-end",
                "delta": {
                    "finish_reason": "COMPLETE",
                    "usage": { "tokens": { "input_tokens": 5, "output_tokens": 10 } }
                }
            }),
        ];

        let out = collect_events(&events);
        assert!(out.iter().any(|e| matches!(
            e,
            StreamEvent::Done {
                reason: StopReason::Stop,
                message,
                ..
            } if message.usage.input == 5 && message.usage.output == 10
        )));
    }

    #[test]
    fn test_stream_error_finish_reason_maps_to_error() {
        let events = vec![
            json!({ "type": "message-start", "id": "msg_1" }),
            json!({
                "type": "message-end",
                "delta": {
                    "finish_reason": "ERROR",
                    "usage": { "tokens": { "input_tokens": 1, "output_tokens": 0 } }
                }
            }),
        ];

        let out = collect_events(&events);
        assert!(out.iter().any(|e| matches!(
            e,
            StreamEvent::Done {
                reason: StopReason::Error,
                ..
            }
        )));
    }

    #[test]
    fn test_stream_tool_call_with_streamed_arguments() {
        let events = vec![
            json!({ "type": "message-start", "id": "msg_1" }),
            json!({
                "type": "tool-call-start",
                "delta": {
                    "message": {
                        "tool_calls": {
                            "id": "call_42",
                            "type": "function",
                            "function": { "name": "bash", "arguments": "{\"co" }
                        }
                    }
                }
            }),
            json!({
                "type": "tool-call-delta",
                "delta": {
                    "message": {
                        "tool_calls": {
                            "function": { "arguments": "mmand\"" }
                        }
                    }
                }
            }),
            json!({
                "type": "tool-call-delta",
                "delta": {
                    "message": {
                        "tool_calls": {
                            "function": { "arguments": ": \"ls -la\"}" }
                        }
                    }
                }
            }),
            json!({ "type": "tool-call-end" }),
            json!({
                "type": "message-end",
                "delta": {
                    "finish_reason": "TOOL_CALL",
                    "usage": { "tokens": { "input_tokens": 10, "output_tokens": 20 } }
                }
            }),
        ];

        let out = collect_events(&events);

        // Should have ToolCallEnd with properly assembled arguments.
        let tool_end = out
            .iter()
            .find(|e| matches!(e, StreamEvent::ToolCallEnd { .. }));
        assert!(tool_end.is_some(), "Expected ToolCallEnd event");
        if let Some(StreamEvent::ToolCallEnd { tool_call, .. }) = tool_end {
            assert_eq!(tool_call.name, "bash");
            assert_eq!(tool_call.id, "call_42");
            assert_eq!(tool_call.arguments["command"], "ls -la");
        }
    }

    #[test]
    fn test_stream_unknown_event_type_ignored() {
        let events = vec![
            json!({ "type": "message-start", "id": "msg_1" }),
            json!({ "type": "some-future-event", "data": "ignored" }),
            json!({
                "type": "content-start",
                "index": 0,
                "delta": { "message": { "content": { "type": "text", "text": "OK" } } }
            }),
            json!({ "type": "content-end", "index": 0 }),
            json!({
                "type": "message-end",
                "delta": {
                    "finish_reason": "COMPLETE",
                    "usage": { "tokens": { "input_tokens": 1, "output_tokens": 1 } }
                }
            }),
        ];

        let out = collect_events(&events);
        // Should complete successfully despite unknown event.
        assert!(out.iter().any(|e| matches!(e, StreamEvent::Done { .. })));
        assert!(out.iter().any(|e| matches!(
            e,
            StreamEvent::TextStart {
                content_index: 0,
                ..
            }
        )));
    }
}
