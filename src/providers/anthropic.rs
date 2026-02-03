//! Anthropic Messages API provider implementation.
//!
//! This module implements the Provider trait for the Anthropic Messages API,
//! supporting streaming responses, tool use, and extended thinking.

use crate::error::{Error, Result};
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ThinkingContent,
    ThinkingLevel, ToolCall, Usage, UserContent,
};
use crate::provider::{CacheRetention, Context, Provider, StreamOptions, ToolDef};
use crate::sse::SseStream;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, Stream};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

// ============================================================================
// Constants
// ============================================================================

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 8192;

// ============================================================================
// Anthropic Provider
// ============================================================================

/// Anthropic Messages API provider.
pub struct AnthropicProvider {
    client: Client,
    model: String,
    base_url: String,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            base_url: ANTHROPIC_API_URL.to_string(),
        }
    }

    /// Create with a custom base URL.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Build the request body for the Anthropic API.
    fn build_request(&self, context: &Context, options: &StreamOptions) -> AnthropicRequest {
        let messages = context
            .messages
            .iter()
            .map(convert_message_to_anthropic)
            .collect();

        let tools: Option<Vec<AnthropicTool>> = if context.tools.is_empty() {
            None
        } else {
            Some(
                context
                    .tools
                    .iter()
                    .map(convert_tool_to_anthropic)
                    .collect(),
            )
        };

        // Build thinking configuration if enabled
        let thinking = options.thinking_level.and_then(|level| {
            if level == ThinkingLevel::Off {
                None
            } else {
                let budget = options.thinking_budgets.as_ref().map_or_else(
                    || level.default_budget(),
                    |b| match level {
                        ThinkingLevel::Off => 0,
                        ThinkingLevel::Minimal => b.minimal,
                        ThinkingLevel::Low => b.low,
                        ThinkingLevel::Medium => b.medium,
                        ThinkingLevel::High => b.high,
                        ThinkingLevel::XHigh => b.xhigh,
                    },
                );
                Some(AnthropicThinking {
                    r#type: "enabled".to_string(),
                    budget_tokens: budget,
                })
            }
        });

        AnthropicRequest {
            model: self.model.clone(),
            messages,
            system: context.system_prompt.clone(),
            max_tokens: options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            temperature: options.temperature,
            tools,
            stream: true,
            thinking,
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn api(&self) -> &'static str {
        "anthropic-messages"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    async fn stream(
        &self,
        context: &Context,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let api_key = options
            .api_key
            .clone()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .ok_or_else(|| Error::config("Missing Anthropic API key"))?;

        let request_body = self.build_request(context, options);

        // Build request with headers
        let mut request = self
            .client
            .post(&self.base_url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("X-API-Key", &api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION);

        // Add cache control header if needed
        if options.cache_retention != CacheRetention::None {
            request = request.header("anthropic-beta", "prompt-caching-2024-07-31");
        }

        // Add custom headers
        for (key, value) in &options.headers {
            request = request.header(key, value);
        }

        let request = request.json(&request_body);

        let response = request
            .send()
            .await
            .map_err(|e| Error::api(format!("HTTP request failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::api(format!(
                "Anthropic API error (HTTP {status}): {body}"
            )));
        }

        let byte_stream = response.bytes_stream().map(|chunk| {
            chunk
                .map(|bytes| bytes.to_vec())
                .map_err(std::io::Error::other)
        });

        // Create SSE stream for streaming responses.
        let event_source = SseStream::new(Box::pin(byte_stream));

        // Create stream state
        let model = self.model.clone();
        let api = self.api().to_string();
        let provider = self.name().to_string();

        let stream = stream::unfold(
            StreamState::new(event_source, model, api, provider),
            |mut state| async move {
                loop {
                    match state.event_source.next().await {
                        Some(Ok(msg)) => {
                            if msg.event == "ping" {
                                // Skip ping events
                            } else {
                                match state.process_event(&msg.data) {
                                    Ok(Some(event)) => return Some((Ok(event), state)),
                                    Ok(None) => {}
                                    Err(e) => return Some((Err(e), state)),
                                }
                            }
                        }
                        Some(Err(e)) => {
                            let err = Error::api(format!("SSE error: {e}"));
                            return Some((Err(err), state));
                        }
                        None => return None,
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

struct StreamState<S>
where
    S: Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin,
{
    event_source: SseStream<S>,
    partial: AssistantMessage,
    current_text: String,
    current_thinking: String,
    current_tool_json: String,
    current_tool_id: Option<String>,
    current_tool_name: Option<String>,
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
            current_text: String::new(),
            current_thinking: String::new(),
            current_tool_json: String::new(),
            current_tool_id: None,
            current_tool_name: None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn process_event(&mut self, data: &str) -> Result<Option<StreamEvent>> {
        let event: AnthropicStreamEvent =
            serde_json::from_str(data).map_err(|e| Error::api(format!("JSON parse error: {e}")))?;

        match event {
            AnthropicStreamEvent::MessageStart { message } => {
                Ok(Some(self.handle_message_start(message)))
            }
            AnthropicStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => Ok(self.handle_content_block_start(index, content_block)),
            AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
                Ok(self.handle_content_block_delta(index, delta))
            }
            AnthropicStreamEvent::ContentBlockStop { index } => {
                Ok(self.handle_content_block_stop(index))
            }
            AnthropicStreamEvent::MessageDelta { delta, usage } => {
                self.handle_message_delta(delta, usage);
                Ok(None)
            }
            AnthropicStreamEvent::MessageStop => {
                let reason = self.partial.stop_reason;
                Ok(Some(StreamEvent::Done {
                    reason,
                    message: self.partial.clone(),
                }))
            }
            AnthropicStreamEvent::Error { error } => {
                self.partial.stop_reason = StopReason::Error;
                self.partial.error_message = Some(error.message);
                Ok(Some(StreamEvent::Error {
                    reason: StopReason::Error,
                    error: self.partial.clone(),
                }))
            }
            AnthropicStreamEvent::Ping => Ok(None),
        }
    }

    fn handle_message_start(&mut self, message: AnthropicMessageStart) -> StreamEvent {
        if let Some(usage) = message.usage {
            self.partial.usage.input = usage.input;
            self.partial.usage.cache_read = usage.cache_read.unwrap_or(0);
            self.partial.usage.cache_write = usage.cache_write.unwrap_or(0);
        }
        StreamEvent::Start {
            partial: self.partial.clone(),
        }
    }

    fn handle_content_block_start(
        &mut self,
        index: u32,
        content_block: AnthropicContentBlock,
    ) -> Option<StreamEvent> {
        let content_index = index as usize;

        match content_block.r#type.as_str() {
            "text" => {
                self.current_text.clear();
                self.partial
                    .content
                    .push(ContentBlock::Text(TextContent::new("")));
                Some(StreamEvent::TextStart {
                    content_index,
                    partial: self.partial.clone(),
                })
            }
            "thinking" => {
                self.current_thinking.clear();
                self.partial
                    .content
                    .push(ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                    }));
                Some(StreamEvent::ThinkingStart {
                    content_index,
                    partial: self.partial.clone(),
                })
            }
            "tool_use" => {
                self.current_tool_json.clear();
                self.current_tool_id = content_block.id;
                self.current_tool_name = content_block.name;
                self.partial.content.push(ContentBlock::ToolCall(ToolCall {
                    id: self.current_tool_id.clone().unwrap_or_default(),
                    name: self.current_tool_name.clone().unwrap_or_default(),
                    arguments: serde_json::Value::Null,
                    thought_signature: None,
                }));
                Some(StreamEvent::ToolCallStart {
                    content_index,
                    partial: self.partial.clone(),
                })
            }
            _ => None,
        }
    }

    fn handle_content_block_delta(
        &mut self,
        index: u32,
        delta: AnthropicDelta,
    ) -> Option<StreamEvent> {
        let idx = index as usize;

        match delta.r#type.as_str() {
            "text_delta" => {
                if let Some(text) = delta.text {
                    self.current_text.push_str(&text);
                    if let Some(ContentBlock::Text(t)) = self.partial.content.get_mut(idx) {
                        t.text.push_str(&text);
                    }
                    Some(StreamEvent::TextDelta {
                        content_index: idx,
                        delta: text,
                        partial: self.partial.clone(),
                    })
                } else {
                    None
                }
            }
            "thinking_delta" => {
                if let Some(thinking) = delta.thinking {
                    self.current_thinking.push_str(&thinking);
                    if let Some(ContentBlock::Thinking(t)) = self.partial.content.get_mut(idx) {
                        t.thinking.push_str(&thinking);
                    }
                    Some(StreamEvent::ThinkingDelta {
                        content_index: idx,
                        delta: thinking,
                        partial: self.partial.clone(),
                    })
                } else {
                    None
                }
            }
            "input_json_delta" => {
                if let Some(partial_json) = delta.partial_json {
                    self.current_tool_json.push_str(&partial_json);
                    Some(StreamEvent::ToolCallDelta {
                        content_index: idx,
                        delta: partial_json,
                        partial: self.partial.clone(),
                    })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn handle_content_block_stop(&mut self, index: u32) -> Option<StreamEvent> {
        let idx = index as usize;

        match self.partial.content.get_mut(idx) {
            Some(ContentBlock::Text(t)) => {
                let content = std::mem::take(&mut self.current_text);
                t.text.clone_from(&content);
                Some(StreamEvent::TextEnd {
                    content_index: idx,
                    content,
                    partial: self.partial.clone(),
                })
            }
            Some(ContentBlock::Thinking(t)) => {
                let content = std::mem::take(&mut self.current_thinking);
                t.thinking.clone_from(&content);
                Some(StreamEvent::ThinkingEnd {
                    content_index: idx,
                    content,
                    partial: self.partial.clone(),
                })
            }
            Some(ContentBlock::ToolCall(tc)) => {
                let arguments: serde_json::Value =
                    match serde_json::from_str(&self.current_tool_json) {
                        Ok(args) => args,
                        Err(e) => {
                            eprintln!(
                                "Warning: Failed to parse tool arguments as JSON: {e}. Raw: {}",
                                &self.current_tool_json
                            );
                            serde_json::Value::Null
                        }
                    };
                tc.arguments = arguments.clone();

                let tool_call = ToolCall {
                    id: self.current_tool_id.take().unwrap_or_default(),
                    name: self.current_tool_name.take().unwrap_or_default(),
                    arguments,
                    thought_signature: None,
                };
                self.current_tool_json.clear();

                Some(StreamEvent::ToolCallEnd {
                    content_index: idx,
                    tool_call,
                    partial: self.partial.clone(),
                })
            }
            _ => None,
        }
    }

    fn handle_message_delta(
        &mut self,
        delta: AnthropicMessageDelta,
        usage: Option<AnthropicDeltaUsage>,
    ) {
        if let Some(stop_reason) = delta.stop_reason {
            self.partial.stop_reason = match stop_reason.as_str() {
                "max_tokens" => StopReason::Length,
                "tool_use" => StopReason::ToolUse,
                // "end_turn" and any other value map to Stop
                _ => StopReason::Stop,
            };
        }

        if let Some(u) = usage {
            self.partial.usage.output = u.output_tokens;
            self.partial.usage.total_tokens = self.partial.usage.input + self.partial.usage.output;
        }
    }
}

// ============================================================================
// Anthropic API Types
// ============================================================================

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
}

#[derive(Debug, Serialize)]
struct AnthropicThinking {
    r#type: String,
    budget_tokens: u32,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<AnthropicContent>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContent {
    Text {
        text: String,
    },
    Image {
        source: AnthropicImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<AnthropicToolResultContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

#[derive(Debug, Serialize)]
struct AnthropicImageSource {
    r#type: String,
    media_type: String,
    data: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicToolResultContent {
    Text { text: String },
    Image { source: AnthropicImageSource },
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

// ============================================================================
// Streaming Response Types
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicStreamEvent {
    MessageStart {
        message: AnthropicMessageStart,
    },
    ContentBlockStart {
        index: u32,
        content_block: AnthropicContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: AnthropicDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: AnthropicMessageDelta,
        #[serde(default)]
        usage: Option<AnthropicDeltaUsage>,
    },
    MessageStop,
    Error {
        error: AnthropicError,
    },
    Ping,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessageStart {
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

/// Usage statistics from Anthropic API.
/// Field names match the API response format.
#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
struct AnthropicUsage {
    #[serde(rename = "input_tokens")]
    input: u64,
    #[serde(default, rename = "cache_read_input_tokens")]
    cache_read: Option<u64>,
    #[serde(default, rename = "cache_creation_input_tokens")]
    cache_write: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AnthropicDeltaUsage {
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct AnthropicContentBlock {
    r#type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicDelta {
    r#type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessageDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicError {
    message: String,
}

// ============================================================================
// Conversion Functions
// ============================================================================

fn convert_message_to_anthropic(message: &Message) -> AnthropicMessage {
    match message {
        Message::User(user) => AnthropicMessage {
            role: "user".to_string(),
            content: convert_user_content(&user.content),
        },
        Message::Assistant(assistant) => AnthropicMessage {
            role: "assistant".to_string(),
            content: assistant
                .content
                .iter()
                .filter_map(convert_content_block_to_anthropic)
                .collect(),
        },
        Message::ToolResult(result) => AnthropicMessage {
            role: "user".to_string(),
            content: vec![AnthropicContent::ToolResult {
                tool_use_id: result.tool_call_id.clone(),
                content: result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(t) => Some(AnthropicToolResultContent::Text {
                            text: t.text.clone(),
                        }),
                        ContentBlock::Image(img) => Some(AnthropicToolResultContent::Image {
                            source: AnthropicImageSource {
                                r#type: "base64".to_string(),
                                media_type: img.mime_type.clone(),
                                data: img.data.clone(),
                            },
                        }),
                        _ => None,
                    })
                    .collect(),
                is_error: if result.is_error { Some(true) } else { None },
            }],
        },
    }
}

fn convert_user_content(content: &UserContent) -> Vec<AnthropicContent> {
    match content {
        UserContent::Text(text) => vec![AnthropicContent::Text { text: text.clone() }],
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(t) => Some(AnthropicContent::Text {
                    text: t.text.clone(),
                }),
                ContentBlock::Image(img) => Some(AnthropicContent::Image {
                    source: AnthropicImageSource {
                        r#type: "base64".to_string(),
                        media_type: img.mime_type.clone(),
                        data: img.data.clone(),
                    },
                }),
                _ => None,
            })
            .collect(),
    }
}

fn convert_content_block_to_anthropic(block: &ContentBlock) -> Option<AnthropicContent> {
    match block {
        ContentBlock::Text(t) => Some(AnthropicContent::Text {
            text: t.text.clone(),
        }),
        ContentBlock::ToolCall(tc) => Some(AnthropicContent::ToolUse {
            id: tc.id.clone(),
            name: tc.name.clone(),
            input: tc.arguments.clone(),
        }),
        // Thinking blocks are not sent back to the API
        ContentBlock::Thinking(_) | ContentBlock::Image(_) => None,
    }
}

fn convert_tool_to_anthropic(tool: &ToolDef) -> AnthropicTool {
    AnthropicTool {
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema: tool.parameters.clone(),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use futures::{StreamExt, stream};
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use std::path::PathBuf;

    #[test]
    fn test_convert_user_text_message() {
        let message = Message::User(crate::model::UserMessage {
            content: UserContent::Text("Hello".to_string()),
            timestamp: 0,
        });

        let converted = convert_message_to_anthropic(&message);
        assert_eq!(converted.role, "user");
        assert_eq!(converted.content.len(), 1);
    }

    #[test]
    fn test_thinking_budget() {
        assert_eq!(ThinkingLevel::Minimal.default_budget(), 1024);
        assert_eq!(ThinkingLevel::Low.default_budget(), 2048);
        assert_eq!(ThinkingLevel::Medium.default_budget(), 8192);
        assert_eq!(ThinkingLevel::High.default_budget(), 16384);
    }

    #[derive(Debug, Deserialize)]
    struct ProviderFixture {
        cases: Vec<ProviderCase>,
    }

    #[derive(Debug, Deserialize)]
    struct ProviderCase {
        name: String,
        events: Vec<Value>,
        expected: Vec<EventSummary>,
    }

    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct EventSummary {
        kind: String,
        #[serde(default)]
        content_index: Option<usize>,
        #[serde(default)]
        delta: Option<String>,
        #[serde(default)]
        content: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    }

    #[test]
    fn test_stream_fixtures() {
        let fixture = load_fixture("anthropic_stream.json");
        for case in fixture.cases {
            let events = collect_events(&case.events);
            let summaries: Vec<EventSummary> = events.iter().map(summarize_event).collect();
            assert_eq!(summaries, case.expected, "case {}", case.name);
        }
    }

    fn load_fixture(file_name: &str) -> ProviderFixture {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/provider_responses")
            .join(file_name);
        let raw = std::fs::read_to_string(path).expect("fixture read");
        serde_json::from_str(&raw).expect("fixture parse")
    }

    fn collect_events(events: &[Value]) -> Vec<StreamEvent> {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let bytes = events
                .iter()
                .map(|event| {
                    let data = match event {
                        Value::String(text) => text.clone(),
                        _ => serde_json::to_string(event).expect("serialize event"),
                    };
                    format!("data: {data}\n\n").into_bytes()
                })
                .collect::<Vec<_>>();

            let byte_stream = stream::iter(bytes.into_iter().map(Ok));
            let event_source = crate::sse::SseStream::new(Box::pin(byte_stream));
            let mut state = StreamState::new(
                event_source,
                "claude-test".to_string(),
                "anthropic-messages".to_string(),
                "anthropic".to_string(),
            );
            let mut out = Vec::new();

            while let Some(item) = state.event_source.next().await {
                let msg = match item {
                    Ok(msg) => msg,
                    Err(err) => panic!("SSE error: {err}"),
                };
                if msg.event == "ping" {
                    continue;
                }
                match state.process_event(&msg.data) {
                    Ok(Some(event)) => out.push(event),
                    Ok(None) => {}
                    Err(err) => panic!("process_event error: {err}"),
                }
            }

            out
        })
    }

    fn summarize_event(event: &StreamEvent) -> EventSummary {
        match event {
            StreamEvent::Start { .. } => EventSummary {
                kind: "start".to_string(),
                content_index: None,
                delta: None,
                content: None,
                reason: None,
            },
            StreamEvent::TextStart { content_index, .. } => EventSummary {
                kind: "text_start".to_string(),
                content_index: Some(*content_index),
                delta: None,
                content: None,
                reason: None,
            },
            StreamEvent::TextDelta {
                content_index,
                delta,
                ..
            } => EventSummary {
                kind: "text_delta".to_string(),
                content_index: Some(*content_index),
                delta: Some(delta.clone()),
                content: None,
                reason: None,
            },
            StreamEvent::TextEnd {
                content_index,
                content,
                ..
            } => EventSummary {
                kind: "text_end".to_string(),
                content_index: Some(*content_index),
                delta: None,
                content: Some(content.clone()),
                reason: None,
            },
            StreamEvent::Done { reason, .. } => EventSummary {
                kind: "done".to_string(),
                content_index: None,
                delta: None,
                content: None,
                reason: Some(reason_to_string(reason)),
            },
            StreamEvent::Error { reason, .. } => EventSummary {
                kind: "error".to_string(),
                content_index: None,
                delta: None,
                content: None,
                reason: Some(reason_to_string(reason)),
            },
            _ => EventSummary {
                kind: "other".to_string(),
                content_index: None,
                delta: None,
                content: None,
                reason: None,
            },
        }
    }

    fn reason_to_string(reason: &StopReason) -> String {
        match reason {
            StopReason::Stop => "stop",
            StopReason::Length => "length",
            StopReason::ToolUse => "tool_use",
            StopReason::Error => "error",
            StopReason::Aborted => "aborted",
        }
        .to_string()
    }
}
