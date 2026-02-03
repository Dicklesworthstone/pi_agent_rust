//! OpenAI Chat Completions API provider implementation.
//!
//! This module implements the Provider trait for the OpenAI Chat Completions API,
//! supporting streaming responses and tool use. Compatible with:
//! - OpenAI direct API (api.openai.com)
//! - Azure OpenAI
//! - Any OpenAI-compatible API (Groq, Together, etc.)

use crate::error::{Error, Result};
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ToolCall, Usage,
    UserContent,
};
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
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
}

impl OpenAIProvider {
    /// Create a new OpenAI provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            base_url: OPENAI_API_URL.to_string(),
        }
    }

    /// Create with a custom base URL (for Azure, Groq, etc.).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
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
    fn name(&self) -> &'static str {
        "openai"
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
        let api_key = options
            .api_key
            .clone()
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .ok_or_else(|| Error::config("Missing OpenAI API key"))?;

        let request_body = self.build_request(context, options);

        // Build request with headers
        let mut request = self
            .client
            .post(&self.base_url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("Authorization", format!("Bearer {api_key}"));

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
                "OpenAI API error (HTTP {status}): {body}"
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

                            match state.process_event(&msg.data) {
                                Ok(Some(event)) => return Some((Ok(event), state)),
                                Ok(None) => {}
                                Err(e) => return Some((Err(e), state)),
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
    started: bool,
}

struct ToolCallState {
    index: usize,
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
            started: false,
        }
    }

    fn process_event(&mut self, data: &str) -> Result<Option<StreamEvent>> {
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
            return Ok(self.process_choice(choice));
        }

        Ok(None)
    }

    fn process_choice(&mut self, choice: OpenAIChoice) -> Option<StreamEvent> {
        // Handle finish reason
        if let Some(reason) = choice.finish_reason {
            self.partial.stop_reason = match reason.as_str() {
                "length" => StopReason::Length,
                "tool_calls" => StopReason::ToolUse,
                "content_filter" => StopReason::Error,
                _ => StopReason::Stop,
            };

            // Finalize any pending text
            if !self.current_text.is_empty() {
                self.partial
                    .content
                    .push(ContentBlock::Text(TextContent::new(&self.current_text)));
            }

            // Finalize any pending tool calls
            for tc in &self.tool_calls {
                let arguments: serde_json::Value = match serde_json::from_str(&tc.arguments) {
                    Ok(args) => args,
                    Err(e) => {
                        eprintln!(
                            "Warning: Failed to parse tool arguments as JSON: {e}. Raw: {}",
                            &tc.arguments
                        );
                        serde_json::Value::Null
                    }
                };
                self.partial.content.push(ContentBlock::ToolCall(ToolCall {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments,
                    thought_signature: None,
                }));
            }

            return None; // Done event handled by [DONE] message
        }

        let delta = choice.delta;

        // Handle text content
        if let Some(content) = delta.content {
            // Always save text first (before started check) to avoid losing content
            self.current_text.push_str(&content);

            if !self.started {
                self.started = true;
                return Some(StreamEvent::Start {
                    partial: self.partial.clone(),
                });
            }

            return Some(StreamEvent::TextDelta {
                content_index: 0,
                delta: content,
                partial: self.partial.clone(),
            });
        }

        // Emit start event if we haven't yet (e.g., role-only first chunk)
        if !self.started {
            self.started = true;
            return Some(StreamEvent::Start {
                partial: self.partial.clone(),
            });
        }

        // Handle tool calls
        if let Some(tool_calls) = delta.tool_calls {
            for tc_delta in tool_calls {
                let index = tc_delta.index as usize;

                // Ensure we have a slot for this tool call
                while self.tool_calls.len() <= index {
                    self.tool_calls.push(ToolCallState {
                        index: self.tool_calls.len(),
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                }

                let tc = &mut self.tool_calls[index];

                // Update ID if present
                if let Some(id) = tc_delta.id {
                    tc.id = id;
                }

                // Update function name if present
                if let Some(function) = tc_delta.function {
                    if let Some(name) = function.name {
                        tc.name = name;
                    }
                    if let Some(args) = function.arguments {
                        tc.arguments.push_str(&args);
                        return Some(StreamEvent::ToolCallDelta {
                            content_index: index,
                            delta: args,
                            partial: self.partial.clone(),
                        });
                    }
                }
            }
        }

        None
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
    use serde_json::Value;
    use std::path::PathBuf;

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
    }

    #[test]
    fn test_provider_info() {
        let provider = OpenAIProvider::new("gpt-4o");
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.api(), "openai-completions");
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
                "gpt-test".to_string(),
                "openai".to_string(),
                "openai".to_string(),
            );
            let mut out = Vec::new();

            while let Some(item) = state.event_source.next().await {
                let msg = match item {
                    Ok(msg) => msg,
                    Err(err) => panic!("SSE error: {err}"),
                };
                if msg.data == "[DONE]" {
                    let reason = state.partial.stop_reason;
                    out.push(StreamEvent::Done {
                        reason,
                        message: state.partial.clone(),
                    });
                    break;
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
                reason: Some(reason_to_string(reason)),
            },
            StreamEvent::Error { reason, .. } => EventSummary {
                kind: "error".to_string(),
                content_index: None,
                delta: None,
                content: None,
                reason: Some(reason_to_string(reason)),
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
