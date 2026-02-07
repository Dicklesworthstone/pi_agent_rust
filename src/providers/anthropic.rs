//! Anthropic Messages API provider implementation.
//!
//! This module implements the Provider trait for the Anthropic Messages API,
//! supporting streaming responses, tool use, and extended thinking.

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ThinkingContent,
    ThinkingLevel, ToolCall, Usage, UserContent,
};
use crate::provider::{CacheRetention, Context, Provider, StreamOptions, ToolDef};
use crate::sse::SseStream;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, Stream};
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

    /// Create with a custom HTTP client (VCR, test harness, etc.).
    #[must_use]
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
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

        let mut max_tokens = options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        if let Some(t) = &thinking {
            if max_tokens <= t.budget_tokens {
                max_tokens = t.budget_tokens + 4096;
            }
        }

        AnthropicRequest {
            model: self.model.clone(),
            messages,
            system: context.system_prompt.clone(),
            max_tokens,
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
        let auth_value = options
            .api_key
            .clone()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .ok_or_else(|| Error::config("Missing Anthropic API key"))?;

        let request_body = self.build_request(context, options);

        // Build request with headers (Content-Type set by .json() below)
        let mut request = self
            .client
            .post(&self.base_url)
            .header("Accept", "text/event-stream")
            .header("X-API-Key", &auth_value)
            .header("anthropic-version", ANTHROPIC_API_VERSION);

        // Add cache control header if needed
        if options.cache_retention != CacheRetention::None {
            request = request.header("anthropic-beta", "prompt-caching-2024-07-31");
        }

        // Add custom headers
        for (key, value) in &options.headers {
            request = request.header(key, value);
        }

        let request = request.json(&request_body)?;

        let response = Box::pin(request.send()).await?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::api(format!(
                "Anthropic API error (HTTP {status}): {body}"
            )));
        }

        // Create SSE stream for streaming responses.
        let event_source = SseStream::new(response.bytes_stream());

        // Create stream state
        let model = self.model.clone();
        let api = self.api().to_string();
        let provider = self.name().to_string();

        let stream = stream::unfold(
            StreamState::new(event_source, model, api, provider),
            |mut state| async move {
                if state.done {
                    return None;
                }
                loop {
                    match state.event_source.next().await {
                        Some(Ok(msg)) => {
                            if msg.event == "ping" {
                                // Skip ping events
                            } else {
                                match state.process_event(&msg.data) {
                                    Ok(Some(event)) => {
                                        if matches!(
                                            &event,
                                            StreamEvent::Done { .. } | StreamEvent::Error { .. }
                                        ) {
                                            state.done = true;
                                        }
                                        return Some((Ok(event), state));
                                    }
                                    Ok(None) => {}
                                    Err(e) => {
                                        state.done = true;
                                        return Some((Err(e), state));
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            state.done = true;
                            let err = Error::api(format!("SSE error: {e}"));
                            return Some((Err(err), state));
                        }
                        // Stream ended before message_stop (e.g.
                        // network disconnect).  Emit Done so the
                        // agent loop receives the partial message.
                        None => {
                            state.done = true;
                            let reason = state.partial.stop_reason;
                            return Some((
                                Ok(StreamEvent::Done {
                                    reason,
                                    message: state.partial.clone(),
                                }),
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
    done: bool,
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
            done: false,
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
            "signature_delta" => {
                // The Anthropic API sends signature_delta for thinking blocks
                // to deliver the thinking_signature required for multi-turn
                // extended thinking conversations.
                if let Some(sig) = delta.signature {
                    if let Some(ContentBlock::Thinking(t)) = self.partial.content.get_mut(idx) {
                        t.thinking_signature = Some(sig);
                    }
                }
                None
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
                            tracing::warn!(
                                error = %e,
                                raw = %self.current_tool_json,
                                "Failed to parse tool arguments as JSON"
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
            self.partial.usage.total_tokens = self.partial.usage.input
                + self.partial.usage.output
                + self.partial.usage.cache_read
                + self.partial.usage.cache_write;
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
    #[serde(default)]
    signature: Option<String>,
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
        Message::Custom(custom) => AnthropicMessage {
            role: "user".to_string(),
            content: vec![AnthropicContent::Text {
                text: custom.content.clone(),
            }],
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
    use serde_json::json;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

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

    #[test]
    fn test_build_request_includes_system_tools_and_thinking() {
        let provider = AnthropicProvider::new("claude-test");
        let context = Context {
            system_prompt: Some("System prompt".to_string()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })],
            tools: vec![ToolDef {
                name: "echo".to_string(),
                description: "Echo a string.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" }
                    },
                    "required": ["text"]
                }),
            }],
        };
        let options = StreamOptions {
            max_tokens: Some(128),
            temperature: Some(0.2),
            thinking_level: Some(ThinkingLevel::Medium),
            thinking_budgets: Some(crate::provider::ThinkingBudgets {
                minimal: 1024,
                low: 2048,
                medium: 9000,
                high: 16384,
                xhigh: 32768,
            }),
            ..Default::default()
        };

        let request = provider.build_request(&context, &options);
        assert_eq!(request.model, "claude-test");
        assert_eq!(request.system.as_deref(), Some("System prompt"));
        assert_eq!(request.temperature, Some(0.2));
        assert!(request.stream);
        assert_eq!(request.max_tokens, 13_096);

        let thinking = request.thinking.expect("thinking config");
        assert_eq!(thinking.r#type, "enabled");
        assert_eq!(thinking.budget_tokens, 9000);

        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].role, "user");
        assert_eq!(request.messages[0].content.len(), 1);
        match &request.messages[0].content[0] {
            AnthropicContent::Text { text } => assert_eq!(text, "Ping"),
            other => panic!("expected text content, got {other:?}"),
        }

        let tools = request.tools.expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "Echo a string.");
        assert_eq!(
            tools[0].input_schema,
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                },
                "required": ["text"]
            })
        );
    }

    #[test]
    fn test_build_request_omits_optional_fields_by_default() {
        let provider = AnthropicProvider::new("claude-test");
        let context = Context::default();
        let options = StreamOptions::default();

        let request = provider.build_request(&context, &options);
        assert_eq!(request.model, "claude-test");
        assert_eq!(request.system, None);
        assert!(request.tools.is_none());
        assert!(request.thinking.is_none());
        assert_eq!(request.max_tokens, DEFAULT_MAX_TOKENS);
        assert!(request.stream);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_stream_parses_thinking_and_tool_call_events() {
        let events = vec![
            json!({
                "type": "message_start",
                "message": { "usage": { "input_tokens": 3 } }
            }),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "thinking" }
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "thinking_delta", "thinking": "step 1" }
            }),
            json!({
                "type": "content_block_stop",
                "index": 0
            }),
            json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": { "type": "tool_use", "id": "tool_123", "name": "search" }
            }),
            json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": { "type": "input_json_delta", "partial_json": "{\"q\":\"ru" }
            }),
            json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": { "type": "input_json_delta", "partial_json": "st\"}" }
            }),
            json!({
                "type": "content_block_stop",
                "index": 1
            }),
            json!({
                "type": "content_block_start",
                "index": 2,
                "content_block": { "type": "text" }
            }),
            json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": { "type": "text_delta", "text": "done" }
            }),
            json!({
                "type": "content_block_stop",
                "index": 2
            }),
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "tool_use" },
                "usage": { "output_tokens": 5 }
            }),
            json!({
                "type": "message_stop"
            }),
        ];

        let out = collect_events(&events);
        assert_eq!(out.len(), 12, "expected full stream event sequence");

        assert!(matches!(&out[0], StreamEvent::Start { .. }));
        assert!(matches!(
            &out[1],
            StreamEvent::ThinkingStart {
                content_index: 0,
                ..
            }
        ));
        assert!(matches!(
            &out[2],
            StreamEvent::ThinkingDelta {
                content_index: 0,
                delta,
                ..
            } if delta == "step 1"
        ));
        assert!(matches!(
            &out[3],
            StreamEvent::ThinkingEnd {
                content_index: 0,
                content,
                ..
            } if content == "step 1"
        ));
        assert!(matches!(
            &out[4],
            StreamEvent::ToolCallStart {
                content_index: 1,
                ..
            }
        ));
        assert!(matches!(
            &out[5],
            StreamEvent::ToolCallDelta {
                content_index: 1,
                delta,
                ..
            } if delta == "{\"q\":\"ru"
        ));
        assert!(matches!(
            &out[6],
            StreamEvent::ToolCallDelta {
                content_index: 1,
                delta,
                ..
            } if delta == "st\"}"
        ));
        if let StreamEvent::ToolCallEnd {
            content_index,
            tool_call,
            ..
        } = &out[7]
        {
            assert_eq!(*content_index, 1);
            assert_eq!(tool_call.id, "tool_123");
            assert_eq!(tool_call.name, "search");
            assert_eq!(tool_call.arguments, json!({ "q": "rust" }));
        } else {
            panic!("expected ToolCallEnd event, got {:?}", out[7]);
        }
        assert!(matches!(
            &out[8],
            StreamEvent::TextStart {
                content_index: 2,
                ..
            }
        ));
        assert!(matches!(
            &out[9],
            StreamEvent::TextDelta {
                content_index: 2,
                delta,
                ..
            } if delta == "done"
        ));
        assert!(matches!(
            &out[10],
            StreamEvent::TextEnd {
                content_index: 2,
                content,
                ..
            } if content == "done"
        ));
        if let StreamEvent::Done { reason, message } = &out[11] {
            assert_eq!(*reason, StopReason::ToolUse);
            assert_eq!(message.stop_reason, StopReason::ToolUse);
        } else {
            panic!("expected Done event, got {:?}", out[11]);
        }
    }

    #[test]
    fn test_message_delta_sets_length_stop_reason_and_usage() {
        let events = vec![
            json!({
                "type": "message_start",
                "message": { "usage": { "input_tokens": 5 } }
            }),
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "max_tokens" },
                "usage": { "output_tokens": 7 }
            }),
            json!({
                "type": "message_stop"
            }),
        ];

        let out = collect_events(&events);
        assert_eq!(out.len(), 2);
        if let StreamEvent::Done { reason, message } = &out[1] {
            assert_eq!(*reason, StopReason::Length);
            assert_eq!(message.stop_reason, StopReason::Length);
            assert_eq!(message.usage.input, 5);
            assert_eq!(message.usage.output, 7);
            assert_eq!(message.usage.total_tokens, 12);
        } else {
            panic!("expected Done event, got {:?}", out[1]);
        }
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

    #[test]
    fn test_stream_error_event_maps_to_stop_reason_error() {
        let events = vec![json!({
            "type": "error",
            "error": { "message": "nope" }
        })];

        let out = collect_events(&events);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], StreamEvent::Error { .. }),
            "expected StreamEvent::Error, got {:?}",
            out[0]
        );
        if let StreamEvent::Error { reason, error } = &out[0] {
            assert_eq!(*reason, StopReason::Error);
            assert_eq!(error.stop_reason, StopReason::Error);
            assert_eq!(error.error_message.as_deref(), Some("nope"));
        }
    }

    #[test]
    fn test_stream_emits_single_done_when_transport_ends_after_message_stop() {
        let out = collect_stream_items_from_body(&success_sse_body());
        let done_count = out
            .iter()
            .filter(|item| matches!(item, Ok(StreamEvent::Done { .. })))
            .count();
        assert_eq!(done_count, 1, "expected exactly one terminal Done event");
    }

    #[test]
    fn test_stream_error_event_is_terminal() {
        let body = [
            r#"data: {"type":"error","error":{"message":"boom"}}"#,
            "",
            // If the stream keeps running after Error, this would produce Done.
            r#"data: {"type":"message_stop"}"#,
            "",
        ]
        .join("\n");

        let out = collect_stream_items_from_body(&body);
        assert_eq!(out.len(), 1, "Error should terminate the stream");
        assert!(matches!(out[0], Ok(StreamEvent::Error { .. })));
    }

    #[test]
    fn test_stream_parse_error_is_terminal() {
        let body = [
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
            "",
            r"data: {invalid-json}",
            "",
            // This should not be emitted after parse error.
            r#"data: {"type":"message_stop"}"#,
            "",
        ]
        .join("\n");

        let out = collect_stream_items_from_body(&body);
        assert_eq!(out.len(), 2, "parse error should stop further events");
        assert!(matches!(out[0], Ok(StreamEvent::Start { .. })));
        match &out[1] {
            Ok(event) => panic!("expected parse error item, got event: {event:?}"),
            Err(err) => assert!(err.to_string().contains("JSON parse error")),
        }
    }

    #[test]
    fn test_stream_sets_required_headers() {
        let captured = run_stream_and_capture_headers(CacheRetention::None)
            .expect("captured request for required headers");
        assert_eq!(
            captured.headers.get("x-api-key").map(String::as_str),
            Some("test-key")
        );
        assert_eq!(
            captured
                .headers
                .get("anthropic-version")
                .map(String::as_str),
            Some(ANTHROPIC_API_VERSION)
        );
        assert!(!captured.headers.contains_key("anthropic-beta"));
        assert!(captured.body.contains("\"stream\":true"));
    }

    #[test]
    fn test_stream_adds_prompt_caching_beta_header_when_enabled() {
        let captured = run_stream_and_capture_headers(CacheRetention::Short)
            .expect("captured request for beta header");
        assert_eq!(
            captured.headers.get("anthropic-beta").map(String::as_str),
            Some("prompt-caching-2024-07-31")
        );
    }

    #[test]
    fn test_stream_http_error_includes_status_and_body_message() {
        let (base_url, _rx) = spawn_test_server(
            401,
            "application/json",
            r#"{"type":"error","error":{"type":"authentication_error","message":"Invalid API key"}}"#,
        );
        let provider = AnthropicProvider::new("claude-test").with_base_url(base_url);
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            tools: Vec::new(),
        };
        let options = StreamOptions {
            api_key: Some("test-key".to_string()),
            ..Default::default()
        };

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let result = runtime.block_on(async { provider.stream(&context, &options).await });
        let Err(err) = result else {
            panic!("expected HTTP error");
        };
        let message = err.to_string();
        assert!(message.contains("Anthropic API error (HTTP 401)"));
        assert!(message.contains("Invalid API key"));
    }

    #[derive(Debug)]
    struct CapturedRequest {
        headers: HashMap<String, String>,
        body: String,
    }

    fn run_stream_and_capture_headers(cache_retention: CacheRetention) -> Option<CapturedRequest> {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = AnthropicProvider::new("claude-test").with_base_url(base_url);
        let context = Context {
            system_prompt: Some("test system".to_string()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            tools: Vec::new(),
        };
        let options = StreamOptions {
            api_key: Some("test-key".to_string()),
            cache_retention,
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

    fn collect_stream_items_from_body(body: &str) -> Vec<Result<StreamEvent>> {
        let (base_url, _rx) = spawn_test_server(200, "text/event-stream", body);
        let provider = AnthropicProvider::new("claude-test").with_base_url(base_url);
        let context = Context {
            system_prompt: Some("test system".to_string()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            tools: Vec::new(),
        };
        let options = StreamOptions {
            api_key: Some("test-key".to_string()),
            ..Default::default()
        };

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider.stream(&context, &options).await.expect("stream");
            let mut items = Vec::new();
            while let Some(item) = stream.next().await {
                items.push(item);
            }
            items
        })
    }

    fn success_sse_body() -> String {
        [
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
            "",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
            "",
            r#"data: {"type":"message_stop"}"#,
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

        (format!("http://{addr}/messages"), rx)
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
            let byte_stream = stream::iter(
                events
                    .iter()
                    .map(|event| {
                        let data = match event {
                            Value::String(text) => text.clone(),
                            _ => serde_json::to_string(event).expect("serialize event"),
                        };
                        format!("data: {data}\n\n").into_bytes()
                    })
                    .map(Ok),
            );
            let event_source = crate::sse::SseStream::new(Box::pin(byte_stream));
            let mut state = StreamState::new(
                event_source,
                "claude-test".to_string(),
                "anthropic-messages".to_string(),
                "anthropic".to_string(),
            );
            let mut out = Vec::new();

            while let Some(item) = state.event_source.next().await {
                let msg = item.expect("SSE event");
                if msg.event == "ping" {
                    continue;
                }
                if let Some(event) = state.process_event(&msg.data).expect("process_event") {
                    out.push(event);
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
                reason: Some(reason_to_string(*reason)),
            },
            StreamEvent::Error { reason, .. } => EventSummary {
                kind: "error".to_string(),
                content_index: None,
                delta: None,
                content: None,
                reason: Some(reason_to_string(*reason)),
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

    fn reason_to_string(reason: StopReason) -> String {
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
