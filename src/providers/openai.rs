//! OpenAI Chat Completions API provider implementation.
//!
//! This module implements the Provider trait for the OpenAI Chat Completions API,
//! supporting streaming responses and tool use. Compatible with:
//! - OpenAI direct API (api.openai.com)
//! - Azure OpenAI
//! - Any OpenAI-compatible API (Groq, Together, etc.)

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ToolCall, Usage,
    UserContent,
};
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use crate::sse::SseStream;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::pin::Pin;

// ============================================================================
// Constants
// ============================================================================

const OPENAI_API_URL: &str = "https://api.openai.com/v1/chat/completions";
const DEFAULT_MAX_TOKENS: u32 = 4096;

// ============================================================================
// OpenAI Provider
// ============================================================================

/// OpenAI Chat Completions API provider.
pub struct OpenAIProvider {
    client: Client,
    model: String,
    base_url: String,
    provider: String,
}

impl OpenAIProvider {
    /// Create a new OpenAI provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            base_url: OPENAI_API_URL.to_string(),
            provider: "openai".to_string(),
        }
    }

    /// Override the provider name reported in streamed events.
    ///
    /// This is useful for OpenAI-compatible backends (Groq, Together, etc.) that use this
    /// implementation but should still surface their own provider identifier in session logs.
    #[must_use]
    pub fn with_provider_name(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    /// Create with a custom base URL (for Azure, Groq, etc.).
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

    /// Build the request body for the OpenAI API.
    fn build_request(&self, context: &Context, options: &StreamOptions) -> OpenAIRequest {
        let messages = Self::build_messages(context);

        let tools: Option<Vec<OpenAITool>> = if context.tools.is_empty() {
            None
        } else {
            Some(context.tools.iter().map(convert_tool_to_openai).collect())
        };

        OpenAIRequest {
            model: self.model.clone(),
            messages,
            max_tokens: options.max_tokens.or(Some(DEFAULT_MAX_TOKENS)),
            temperature: options.temperature,
            tools,
            stream: true,
            stream_options: Some(OpenAIStreamOptions {
                include_usage: true,
            }),
        }
    }

    /// Build the messages array with system prompt prepended.
    fn build_messages(context: &Context) -> Vec<OpenAIMessage> {
        let mut messages = Vec::new();

        // Add system prompt as first message
        if let Some(system) = &context.system_prompt {
            messages.push(OpenAIMessage {
                role: "system".to_string(),
                content: Some(OpenAIContent::Text(system.clone())),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        // Convert conversation messages
        for message in &context.messages {
            messages.extend(convert_message_to_openai(message));
        }

        messages
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn name(&self) -> &str {
        &self.provider
    }

    fn api(&self) -> &'static str {
        "openai-completions"
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
                    .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                    .ok_or_else(|| Error::config("Missing OpenAI API key"))?,
            )
        };

        let request_body = self.build_request(context, options);

        // Note: Content-Type is set by .json() below; setting it here too
        // produces a duplicate header that OpenAI's server rejects.
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
                "OpenAI API error (HTTP {status}): {body}"
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
                        "OpenAI API protocol error (HTTP {status}): missing Content-Type (expected text/event-stream)"
                    )
                },
                |value| {
                    format!(
                        "OpenAI API protocol error (HTTP {status}): unexpected Content-Type {value} (expected text/event-stream)"
                    )
                },
            );
            return Err(Error::api(message));
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
                loop {
                    if let Some(event) = state.pending_events.pop_front() {
                        return Some((Ok(event), state));
                    }

                    match state.event_source.next().await {
                        Some(Ok(msg)) => {
                            // OpenAI sends "[DONE]" as final message
                            if msg.data == "[DONE]" {
                                let reason = state.partial.stop_reason;
                                return Some((
                                    Ok(StreamEvent::Done {
                                        reason,
                                        message: state.partial.clone(),
                                    }),
                                    state,
                                ));
                            }

                            if let Err(e) = state.process_event(&msg.data) {
                                return Some((Err(e), state));
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
    tool_calls: Vec<ToolCallState>,
    pending_events: VecDeque<StreamEvent>,
    started: bool,
}

struct ToolCallState {
    index: usize,
    content_index: usize,
    id: String,
    name: String,
    arguments: String,
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
            tool_calls: Vec::new(),
            pending_events: VecDeque::new(),
            started: false,
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

    fn process_event(&mut self, data: &str) -> Result<()> {
        let chunk: OpenAIStreamChunk =
            serde_json::from_str(data).map_err(|e| Error::api(format!("JSON parse error: {e}")))?;

        // Handle usage in final chunk
        if let Some(usage) = chunk.usage {
            self.partial.usage.input = usage.prompt_tokens;
            self.partial.usage.output = usage.completion_tokens.unwrap_or(0);
            self.partial.usage.total_tokens = usage.total_tokens;
        }

        // Process choices
        if let Some(choice) = chunk.choices.into_iter().next() {
            if !self.started
                && choice.finish_reason.is_none()
                && choice.delta.content.is_none()
                && choice.delta.tool_calls.is_none()
            {
                self.ensure_started();
                return Ok(());
            }

            self.process_choice(choice);
        }

        Ok(())
    }

    fn finalize_tool_call_arguments(&mut self) {
        for tc in &self.tool_calls {
            let arguments: serde_json::Value = match serde_json::from_str(&tc.arguments) {
                Ok(args) => args,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        raw = %tc.arguments,
                        "Failed to parse tool arguments as JSON"
                    );
                    serde_json::Value::Null
                }
            };

            if let Some(ContentBlock::ToolCall(block)) =
                self.partial.content.get_mut(tc.content_index)
            {
                block.arguments = arguments;
            }
        }
    }

    fn process_choice(&mut self, choice: OpenAIChoice) {
        // Handle finish reason
        if let Some(reason) = choice.finish_reason {
            self.partial.stop_reason = match reason.as_str() {
                "length" => StopReason::Length,
                "tool_calls" => StopReason::ToolUse,
                "content_filter" => StopReason::Error,
                _ => StopReason::Stop,
            };

            // Finalize tool call arguments
            self.finalize_tool_call_arguments();

            // Emit ToolCallEnd for each accumulated tool call
            for tc in &self.tool_calls {
                if let Some(ContentBlock::ToolCall(tool_call)) =
                    self.partial.content.get(tc.content_index)
                {
                    self.pending_events.push_back(StreamEvent::ToolCallEnd {
                        content_index: tc.content_index,
                        tool_call: tool_call.clone(),
                        partial: self.partial.clone(),
                    });
                }
            }

            return; // Done event handled by [DONE] message
        }

        let delta = choice.delta;
        if delta.content.is_some() || delta.tool_calls.is_some() {
            self.ensure_started();
        }

        // Handle text content
        if let Some(content) = delta.content {
            // Update partial content
            let last_is_text = matches!(self.partial.content.last(), Some(ContentBlock::Text(_)));
            if !last_is_text {
                self.partial
                    .content
                    .push(ContentBlock::Text(TextContent::new("")));
            }
            let content_index = self.partial.content.len() - 1;

            if let Some(ContentBlock::Text(t)) = self.partial.content.get_mut(content_index) {
                t.text.push_str(&content);
            }

            self.pending_events.push_back(StreamEvent::TextDelta {
                content_index,
                delta: content,
                partial: self.partial.clone(),
            });
        }

        // Handle tool calls
        if let Some(tool_calls) = delta.tool_calls {
            for tc_delta in tool_calls {
                let index = tc_delta.index as usize;

                // Ensure we have a slot for this tool call
                if self.tool_calls.len() <= index {
                    let content_index = self.partial.content.len();
                    self.tool_calls.push(ToolCallState {
                        index,
                        content_index,
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });

                    // Initialize the tool call block in partial content
                    self.partial.content.push(ContentBlock::ToolCall(ToolCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: serde_json::Value::Null,
                        thought_signature: None,
                    }));

                    self.pending_events.push_back(StreamEvent::ToolCallStart {
                        content_index,
                        partial: self.partial.clone(),
                    });
                }

                let tc = &mut self.tool_calls[index];
                let content_index = tc.content_index;

                // Update ID if present
                if let Some(id) = tc_delta.id {
                    tc.id = id;
                    if let Some(ContentBlock::ToolCall(block)) =
                        self.partial.content.get_mut(content_index)
                    {
                        block.id.clone_from(&tc.id);
                    }
                }

                // Update function name if present
                if let Some(function) = tc_delta.function {
                    if let Some(name) = function.name {
                        tc.name = name;
                        if let Some(ContentBlock::ToolCall(block)) =
                            self.partial.content.get_mut(content_index)
                        {
                            block.name.clone_from(&tc.name);
                        }
                    }
                    if let Some(args) = function.arguments {
                        tc.arguments.push_str(&args);
                        // Update arguments in partial (best effort parse, or just raw string if we supported it)
                        // Note: We don't update partial.arguments here because it requires valid JSON.
                        // We only update it at the end or if we switched to storing raw string args.
                        // But we MUST emit the delta.
                        self.pending_events.push_back(StreamEvent::ToolCallDelta {
                            content_index,
                            delta: args,
                            partial: self.partial.clone(),
                        });
                    }
                }
            }
        }
    }
}

// ============================================================================
// OpenAI API Types
// ============================================================================

#[derive(Debug, Serialize)]
struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<OpenAIStreamOptions>,
}

#[derive(Debug, Serialize)]
struct OpenAIStreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
struct OpenAIMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<OpenAIContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIToolCallRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum OpenAIContent {
    Text(String),
    Parts(Vec<OpenAIContentPart>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAIContentPart {
    Text { text: String },
    ImageUrl { image_url: OpenAIImageUrl },
}

#[derive(Debug, Serialize)]
struct OpenAIImageUrl {
    url: String,
}

#[derive(Debug, Serialize)]
struct OpenAIToolCallRef {
    id: String,
    r#type: String,
    function: OpenAIFunctionRef,
}

#[derive(Debug, Serialize)]
struct OpenAIFunctionRef {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct OpenAITool {
    r#type: String,
    function: OpenAIFunction,
}

#[derive(Debug, Serialize)]
struct OpenAIFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

// ============================================================================
// Streaming Response Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct OpenAIStreamChunk {
    #[serde(default)]
    choices: Vec<OpenAIChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    delta: OpenAIDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct OpenAIToolCallDelta {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OpenAIFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct OpenAIFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
struct OpenAIUsage {
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: Option<u64>,
    total_tokens: u64,
}

// ============================================================================
// Conversion Functions
// ============================================================================

fn convert_message_to_openai(message: &Message) -> Vec<OpenAIMessage> {
    match message {
        Message::User(user) => vec![OpenAIMessage {
            role: "user".to_string(),
            content: Some(convert_user_content(&user.content)),
            tool_calls: None,
            tool_call_id: None,
        }],
        Message::Custom(custom) => vec![OpenAIMessage {
            role: "user".to_string(),
            content: Some(OpenAIContent::Text(custom.content.clone())),
            tool_calls: None,
            tool_call_id: None,
        }],
        Message::Assistant(assistant) => {
            let mut messages = Vec::new();

            // Collect text content
            let text: String = assistant
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");

            // Collect tool calls
            let tool_calls: Vec<OpenAIToolCallRef> = assistant
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall(tc) => Some(OpenAIToolCallRef {
                        id: tc.id.clone(),
                        r#type: "function".to_string(),
                        function: OpenAIFunctionRef {
                            name: tc.name.clone(),
                            arguments: tc.arguments.to_string(),
                        },
                    }),
                    _ => None,
                })
                .collect();

            let content = if text.is_empty() {
                None
            } else {
                Some(OpenAIContent::Text(text))
            };

            let tool_calls = if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            };

            messages.push(OpenAIMessage {
                role: "assistant".to_string(),
                content,
                tool_calls,
                tool_call_id: None,
            });

            messages
        }
        Message::ToolResult(result) => {
            // OpenAI expects tool results as separate messages with role "tool"
            let content = result
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");

            vec![OpenAIMessage {
                role: "tool".to_string(),
                content: Some(OpenAIContent::Text(content)),
                tool_calls: None,
                tool_call_id: Some(result.tool_call_id.clone()),
            }]
        }
    }
}

fn convert_user_content(content: &UserContent) -> OpenAIContent {
    match content {
        UserContent::Text(text) => OpenAIContent::Text(text.clone()),
        UserContent::Blocks(blocks) => {
            let parts: Vec<OpenAIContentPart> = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(t) => Some(OpenAIContentPart::Text {
                        text: t.text.clone(),
                    }),
                    ContentBlock::Image(img) => {
                        // Convert to data URL for OpenAI
                        let url = format!("data:{};base64,{}", img.mime_type, img.data);
                        Some(OpenAIContentPart::ImageUrl {
                            image_url: OpenAIImageUrl { url },
                        })
                    }
                    _ => None,
                })
                .collect();
            OpenAIContent::Parts(parts)
        }
    }
}

fn convert_tool_to_openai(tool: &ToolDef) -> OpenAITool {
    OpenAITool {
        r#type: "function".to_string(),
        function: OpenAIFunction {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
        },
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
    use serde_json::{Value, json};
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

        let converted = convert_message_to_openai(&message);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "user");
    }

    #[test]
    fn test_tool_conversion() {
        let tool = ToolDef {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "arg": {"type": "string"}
                }
            }),
        };

        let converted = convert_tool_to_openai(&tool);
        assert_eq!(converted.r#type, "function");
        assert_eq!(converted.function.name, "test_tool");
        assert_eq!(converted.function.description, "A test tool");
        assert_eq!(
            converted.function.parameters,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "arg": {"type": "string"}
                }
            })
        );
    }

    #[test]
    fn test_provider_info() {
        let provider = OpenAIProvider::new("gpt-4o");
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.api(), "openai-completions");
    }

    #[test]
    fn test_build_request_includes_system_tools_and_stream_options() {
        let provider = OpenAIProvider::new("gpt-4o");
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
        assert_eq!(value["model"], "gpt-4o");
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["messages"][0]["content"], "You are concise.");
        assert_eq!(value["messages"][1]["role"], "user");
        assert_eq!(value["messages"][1]["content"], "Ping");
        let temperature = value["temperature"]
            .as_f64()
            .expect("temperature should serialize as number");
        assert!((temperature - 0.2).abs() < 1e-6);
        assert_eq!(value["max_tokens"], 123);
        assert_eq!(value["stream"], true);
        assert_eq!(value["stream_options"]["include_usage"], true);
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
    fn test_stream_accumulates_tool_call_argument_deltas() {
        let events = vec![
            json!({ "choices": [{ "delta": {} }] }),
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_1",
                            "function": {
                                "name": "search",
                                "arguments": "{\"q\":\"ru"
                            }
                        }]
                    }
                }]
            }),
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {
                                "arguments": "st\"}"
                            }
                        }]
                    }
                }]
            }),
            json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
            Value::String("[DONE]".to_string()),
        ];

        let out = collect_events(&events);
        assert!(
            out.iter()
                .any(|e| matches!(e, StreamEvent::ToolCallStart { .. }))
        );
        assert!(out.iter().any(
            |e| matches!(e, StreamEvent::ToolCallDelta { delta, .. } if delta == "{\"q\":\"ru")
        ));
        assert!(
            out.iter()
                .any(|e| matches!(e, StreamEvent::ToolCallDelta { delta, .. } if delta == "st\"}"))
        );
        let done = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::Done { message, .. } => Some(message),
                _ => None,
            })
            .expect("done event");
        let tool_call = done
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .expect("assembled tool call content");
        assert_eq!(tool_call.id, "call_1");
        assert_eq!(tool_call.name, "search");
        assert_eq!(tool_call.arguments, json!({ "q": "rust" }));
        assert!(out.iter().any(|e| matches!(
            e,
            StreamEvent::Done {
                reason: StopReason::ToolUse,
                ..
            }
        )));
    }

    #[test]
    fn test_stream_sets_bearer_auth_header() {
        let captured = run_stream_and_capture_headers().expect("captured request");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer test-openai-key")
        );
        assert_eq!(
            captured.headers.get("accept").map(String::as_str),
            Some("text/event-stream")
        );

        let body: Value = serde_json::from_str(&captured.body).expect("request body json");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
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
        let fixture = load_fixture("openai_stream.json");
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

    #[derive(Debug)]
    struct CapturedRequest {
        headers: HashMap<String, String>,
        body: String,
    }

    fn run_stream_and_capture_headers() -> Option<CapturedRequest> {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = OpenAIProvider::new("gpt-4o").with_base_url(base_url);
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            tools: Vec::new(),
        };
        let options = StreamOptions {
            api_key: Some("test-openai-key".to_string()),
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
            r#"data: {"choices":[{"delta":{}}]}"#,
            "",
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
            "",
            "data: [DONE]",
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

        (format!("http://{addr}/chat/completions"), rx)
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
                "gpt-test".to_string(),
                "openai".to_string(),
                "openai".to_string(),
            );
            let mut out = Vec::new();

            while let Some(item) = state.event_source.next().await {
                let msg = item.expect("SSE event");
                if msg.data == "[DONE]" {
                    out.extend(state.pending_events.drain(..));
                    let reason = state.partial.stop_reason;
                    out.push(StreamEvent::Done {
                        reason,
                        message: state.partial.clone(),
                    });
                    break;
                }
                state.process_event(&msg.data).expect("process_event");
                out.extend(state.pending_events.drain(..));
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
            StreamEvent::TextStart { content_index, .. } => EventSummary {
                kind: "text_start".to_string(),
                content_index: Some(*content_index),
                delta: None,
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
