//! Google Gemini API provider implementation.
//!
//! This module implements the Provider trait for the Google Gemini API,
//! supporting streaming responses and function calling (tool use).

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
use std::pin::Pin;

// ============================================================================
// Constants
// ============================================================================

const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_MAX_TOKENS: u32 = 8192;

// ============================================================================
// Gemini Provider
// ============================================================================

/// Google Gemini API provider.
pub struct GeminiProvider {
    client: Client,
    model: String,
    base_url: String,
}

impl GeminiProvider {
    /// Create a new Gemini provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            base_url: GEMINI_API_BASE.to_string(),
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

    /// Build the streaming URL.
    fn streaming_url(&self, api_key: &str) -> String {
        format!(
            "{}/models/{}:streamGenerateContent?alt=sse&key={}",
            self.base_url, self.model, api_key
        )
    }

    /// Build the request body for the Gemini API.
    #[allow(clippy::unused_self)]
    fn build_request(&self, context: &Context, options: &StreamOptions) -> GeminiRequest {
        let contents = Self::build_contents(context);
        let system_instruction = context.system_prompt.as_ref().map(|s| GeminiContent {
            role: None,
            parts: vec![GeminiPart::Text { text: s.clone() }],
        });

        let tools: Option<Vec<GeminiTool>> = if context.tools.is_empty() {
            None
        } else {
            Some(vec![GeminiTool {
                function_declarations: context.tools.iter().map(convert_tool_to_gemini).collect(),
            }])
        };

        let tool_config = if tools.is_some() {
            Some(GeminiToolConfig {
                function_calling_config: GeminiFunctionCallingConfig {
                    mode: "AUTO".to_string(),
                },
            })
        } else {
            None
        };

        GeminiRequest {
            contents,
            system_instruction,
            tools,
            tool_config,
            generation_config: Some(GeminiGenerationConfig {
                max_output_tokens: options.max_tokens.or(Some(DEFAULT_MAX_TOKENS)),
                temperature: options.temperature,
                candidate_count: Some(1),
            }),
        }
    }

    /// Build the contents array from context messages.
    fn build_contents(context: &Context) -> Vec<GeminiContent> {
        let mut contents = Vec::new();

        for message in &context.messages {
            contents.extend(convert_message_to_gemini(message));
        }

        contents
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn name(&self) -> &'static str {
        "google"
    }

    fn api(&self) -> &'static str {
        "gemini"
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
            .or_else(|| std::env::var("GOOGLE_API_KEY").ok())
            .or_else(|| std::env::var("GEMINI_API_KEY").ok())
            .ok_or_else(|| Error::config("Missing Google/Gemini API key"))?;

        let request_body = self.build_request(context, options);
        let url = self.streaming_url(&auth_value);

        // Build request
        let mut request = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream");

        for (key, value) in &options.headers {
            request = request.header(key, value);
        }

        let request = request.json(&request_body)?;

        let response = Box::pin(request.send()).await?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::api(format!(
                "Gemini API error (HTTP {status}): {body}"
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
                loop {
                    match state.event_source.next().await {
                        Some(Ok(msg)) => {
                            if msg.event == "ping" {
                                continue;
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
                        None => {
                            // Stream ended naturally
                            if !state.finished {
                                state.finished = true;
                                state.finalize_content();
                                return Some((
                                    Ok(StreamEvent::Done {
                                        reason: state.partial.stop_reason,
                                        message: state.partial.clone(),
                                    }),
                                    state,
                                ));
                            }
                            return None;
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
    tool_calls: Vec<ToolCallState>,
    started: bool,
    finished: bool,
}

struct ToolCallState {
    id: String,
    name: String,
    arguments: serde_json::Value,
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
            finished: false,
        }
    }

    fn process_event(&mut self, data: &str) -> Result<Option<StreamEvent>> {
        let response: GeminiStreamResponse = serde_json::from_str(data)
            .map_err(|e| Error::api(format!("JSON parse error: {e}\nData: {data}")))?;

        // Handle usage metadata
        if let Some(metadata) = response.usage_metadata {
            self.partial.usage.input = metadata.prompt_token_count.unwrap_or(0);
            self.partial.usage.output = metadata.candidates_token_count.unwrap_or(0);
            self.partial.usage.total_tokens = metadata.total_token_count.unwrap_or(0);
        }

        // Process candidates
        if let Some(candidates) = response.candidates {
            if let Some(candidate) = candidates.into_iter().next() {
                return self.process_candidate(candidate);
            }
        }

        Ok(None)
    }

    #[allow(clippy::unnecessary_wraps)]
    fn process_candidate(&mut self, candidate: GeminiCandidate) -> Result<Option<StreamEvent>> {
        // Handle finish reason
        if let Some(reason) = candidate.finish_reason {
            self.partial.stop_reason = match reason.as_str() {
                "MAX_TOKENS" => StopReason::Length,
                "SAFETY" | "RECITATION" | "OTHER" => StopReason::Error,
                // STOP and any other reason treated as normal stop
                _ => StopReason::Stop,
            };
        }

        // Process content parts
        if let Some(content) = candidate.content {
            for part in content.parts {
                match part {
                    GeminiPart::Text { text } => {
                        // Always save text first (before started check) to avoid losing content
                        self.current_text.push_str(&text);

                        // Emit start event on first content
                        if !self.started {
                            self.started = true;
                            return Ok(Some(StreamEvent::Start {
                                partial: self.partial.clone(),
                            }));
                        }

                        return Ok(Some(StreamEvent::TextDelta {
                            content_index: 0,
                            delta: text,
                            partial: self.partial.clone(),
                        }));
                    }
                    GeminiPart::FunctionCall { function_call } => {
                        // Generate a unique ID for this tool call
                        let id = format!("call_{}", uuid::Uuid::new_v4().simple());

                        // Serialize args before moving
                        let args_str = serde_json::to_string(&function_call.args)
                            .unwrap_or_else(|_| "{}".to_string());

                        self.tool_calls.push(ToolCallState {
                            id,
                            name: function_call.name.clone(),
                            arguments: function_call.args,
                        });

                        // Update stop reason for tool use
                        self.partial.stop_reason = StopReason::ToolUse;

                        // Emit start if not started
                        if !self.started {
                            self.started = true;
                            return Ok(Some(StreamEvent::Start {
                                partial: self.partial.clone(),
                            }));
                        }

                        // Emit tool call delta
                        return Ok(Some(StreamEvent::ToolCallDelta {
                            content_index: self.tool_calls.len() - 1,
                            delta: args_str,
                            partial: self.partial.clone(),
                        }));
                    }
                    GeminiPart::InlineData { .. } | GeminiPart::FunctionResponse { .. } => {
                        // These are for input, not output
                    }
                }
            }
        }

        Ok(None)
    }

    fn finalize_content(&mut self) {
        // Add accumulated text
        if !self.current_text.is_empty() {
            self.partial
                .content
                .push(ContentBlock::Text(TextContent::new(&self.current_text)));
        }

        // Add tool calls
        for tc in &self.tool_calls {
            self.partial.content.push(ContentBlock::ToolCall(ToolCall {
                id: tc.id.clone(),
                name: tc.name.clone(),
                arguments: tc.arguments.clone(),
                thought_signature: None,
            }));
        }
    }
}

// ============================================================================
// Gemini API Types
// ============================================================================

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<GeminiToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum GeminiPart {
    Text {
        text: String,
    },
    InlineData {
        inline_data: GeminiBlob,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GeminiFunctionCall,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: GeminiFunctionResponse,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiBlob {
    mime_type: String,
    data: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiFunctionCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiFunctionResponse {
    name: String,
    response: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiTool {
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Debug, Serialize)]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiToolConfig {
    function_calling_config: GeminiFunctionCallingConfig,
}

#[derive(Debug, Serialize)]
struct GeminiFunctionCallingConfig {
    mode: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate_count: Option<u32>,
}

// ============================================================================
// Streaming Response Types
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiStreamResponse {
    #[serde(default)]
    candidates: Option<Vec<GeminiCandidate>>,
    #[serde(default)]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiContent>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_field_names)]
struct GeminiUsageMetadata {
    #[serde(default)]
    prompt_token_count: Option<u64>,
    #[serde(default)]
    candidates_token_count: Option<u64>,
    #[serde(default)]
    total_token_count: Option<u64>,
}

// ============================================================================
// Conversion Functions
// ============================================================================

fn convert_message_to_gemini(message: &Message) -> Vec<GeminiContent> {
    match message {
        Message::User(user) => vec![GeminiContent {
            role: Some("user".to_string()),
            parts: convert_user_content_to_parts(&user.content),
        }],
        Message::Assistant(assistant) => {
            let mut parts = Vec::new();

            for block in &assistant.content {
                match block {
                    ContentBlock::Text(t) => {
                        parts.push(GeminiPart::Text {
                            text: t.text.clone(),
                        });
                    }
                    ContentBlock::ToolCall(tc) => {
                        parts.push(GeminiPart::FunctionCall {
                            function_call: GeminiFunctionCall {
                                name: tc.name.clone(),
                                args: tc.arguments.clone(),
                            },
                        });
                    }
                    ContentBlock::Thinking(_) | ContentBlock::Image(_) => {
                        // Skip thinking blocks and images in assistant output
                    }
                }
            }

            if parts.is_empty() {
                return Vec::new();
            }

            vec![GeminiContent {
                role: Some("model".to_string()),
                parts,
            }]
        }
        Message::ToolResult(result) => {
            // Gemini expects function responses as model role with functionResponse part
            let content_text = result
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");

            let response_value = if result.is_error {
                serde_json::json!({ "error": content_text })
            } else {
                serde_json::json!({ "result": content_text })
            };

            vec![GeminiContent {
                role: Some("user".to_string()),
                parts: vec![GeminiPart::FunctionResponse {
                    function_response: GeminiFunctionResponse {
                        name: result.tool_name.clone(),
                        response: response_value,
                    },
                }],
            }]
        }
    }
}

fn convert_user_content_to_parts(content: &UserContent) -> Vec<GeminiPart> {
    match content {
        UserContent::Text(text) => vec![GeminiPart::Text { text: text.clone() }],
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(t) => Some(GeminiPart::Text {
                    text: t.text.clone(),
                }),
                ContentBlock::Image(img) => Some(GeminiPart::InlineData {
                    inline_data: GeminiBlob {
                        mime_type: img.mime_type.clone(),
                        data: img.data.clone(),
                    },
                }),
                _ => None,
            })
            .collect(),
    }
}

fn convert_tool_to_gemini(tool: &ToolDef) -> GeminiFunctionDeclaration {
    GeminiFunctionDeclaration {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters: tool.parameters.clone(),
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

        let converted = convert_message_to_gemini(&message);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, Some("user".to_string()));
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

        let converted = convert_tool_to_gemini(&tool);
        assert_eq!(converted.name, "test_tool");
        assert_eq!(converted.description, "A test tool");
    }

    #[test]
    fn test_provider_info() {
        let provider = GeminiProvider::new("gemini-2.0-flash");
        assert_eq!(provider.name(), "google");
        assert_eq!(provider.api(), "gemini");
    }

    #[test]
    fn test_streaming_url() {
        let provider = GeminiProvider::new("gemini-2.0-flash");
        let url = provider.streaming_url("test-key");
        assert!(url.contains("gemini-2.0-flash"));
        assert!(url.contains("streamGenerateContent"));
        assert!(url.contains("key=test-key"));
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
        let fixture = load_fixture("gemini_stream.json");
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
                "gemini-test".to_string(),
                "google-generative".to_string(),
                "google".to_string(),
            );
            let mut out = Vec::new();

            loop {
                let Some(item) = state.event_source.next().await else {
                    if !state.finished {
                        state.finished = true;
                        state.finalize_content();
                        out.push(StreamEvent::Done {
                            reason: state.partial.stop_reason,
                            message: state.partial.clone(),
                        });
                    }
                    break;
                };

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
