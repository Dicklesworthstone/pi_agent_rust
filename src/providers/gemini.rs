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
use std::collections::VecDeque;
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
        "google-generative-ai"
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

        // Build request (Content-Type set by .json() below)
        let mut request = self.client.post(&url).header("Accept", "text/event-stream");

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
    pending_events: VecDeque<StreamEvent>,
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
            pending_events: VecDeque::new(),
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
                        // Always accumulate text into partial first
                        let last_is_text =
                            matches!(self.partial.content.last(), Some(ContentBlock::Text(_)));
                        if !last_is_text {
                            self.partial
                                .content
                                .push(ContentBlock::Text(TextContent::new("")));
                        }
                        let content_index = self.partial.content.len() - 1;

                        if let Some(ContentBlock::Text(t)) =
                            self.partial.content.get_mut(content_index)
                        {
                            t.text.push_str(&text);
                        }

                        if !self.started {
                            self.started = true;
                            return Ok(Some(StreamEvent::Start {
                                partial: self.partial.clone(),
                            }));
                        }

                        return Ok(Some(StreamEvent::TextDelta {
                            content_index,
                            delta: text,
                            partial: self.partial.clone(),
                        }));
                    }
                    GeminiPart::FunctionCall { function_call } => {
                        // Generate a unique ID for this tool call
                        let id = format!("call_{}", uuid::Uuid::new_v4().simple());

                        // Serialize args
                        let args_str = serde_json::to_string(&function_call.args)
                            .unwrap_or_else(|_| "{}".to_string());
                        let GeminiFunctionCall { name, args } = function_call;

                        let tool_call = ToolCall {
                            id,
                            name,
                            arguments: args,
                            thought_signature: None,
                        };

                        self.partial.content.push(ContentBlock::ToolCall(tool_call));
                        let content_index = self.partial.content.len() - 1;

                        // Update stop reason for tool use
                        self.partial.stop_reason = StopReason::ToolUse;

                        // Emit start if not started
                        if !self.started {
                            self.started = true;
                            return Ok(Some(StreamEvent::Start {
                                partial: self.partial.clone(),
                            }));
                        }

                        // Emit tool call delta (Gemini sends full args, so delta is full args)
                        return Ok(Some(StreamEvent::ToolCallDelta {
                            content_index,
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
        Message::Custom(custom) => vec![GeminiContent {
            role: Some("user".to_string()),
            parts: vec![GeminiPart::Text {
                text: custom.content.clone(),
            }],
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
        assert_eq!(provider.api(), "google-generative-ai");
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

    // ─── Request body format tests ──────────────────────────────────────

    #[test]
    fn test_build_request_basic_text() {
        let provider = GeminiProvider::new("gemini-2.0-flash");
        let context = Context {
            system_prompt: Some("You are helpful.".to_string()),
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("What is Rust?".to_string()),
                timestamp: 0,
            })],
            tools: vec![],
        };
        let options = crate::provider::StreamOptions {
            max_tokens: Some(1024),
            temperature: Some(0.7),
            ..Default::default()
        };

        let req = provider.build_request(&context, &options);
        let json = serde_json::to_value(&req).expect("serialize");

        // Contents should have exactly one user message.
        let contents = json["contents"].as_array().expect("contents array");
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "What is Rust?");

        // System instruction should be present.
        assert_eq!(
            json["systemInstruction"]["parts"][0]["text"],
            "You are helpful."
        );

        // No tools should be present.
        assert!(json.get("tools").is_none() || json["tools"].is_null());

        // Generation config should match.
        assert_eq!(json["generationConfig"]["maxOutputTokens"], 1024);
        assert!((json["generationConfig"]["temperature"].as_f64().unwrap() - 0.7).abs() < 0.01);
        assert_eq!(json["generationConfig"]["candidateCount"], 1);
    }

    #[test]
    fn test_build_request_with_tools() {
        let provider = GeminiProvider::new("gemini-2.0-flash");
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Read a file".to_string()),
                timestamp: 0,
            })],
            tools: vec![
                ToolDef {
                    name: "read".to_string(),
                    description: "Read a file".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"}
                        },
                        "required": ["path"]
                    }),
                },
                ToolDef {
                    name: "write".to_string(),
                    description: "Write a file".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "content": {"type": "string"}
                        }
                    }),
                },
            ],
        };
        let options = crate::provider::StreamOptions::default();

        let req = provider.build_request(&context, &options);
        let json = serde_json::to_value(&req).expect("serialize");

        // System instruction should be absent.
        assert!(json.get("systemInstruction").is_none() || json["systemInstruction"].is_null());

        // Tools should be present as a single GeminiTool with function_declarations array.
        let tools = json["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        let declarations = tools[0]["functionDeclarations"]
            .as_array()
            .expect("declarations");
        assert_eq!(declarations.len(), 2);
        assert_eq!(declarations[0]["name"], "read");
        assert_eq!(declarations[1]["name"], "write");
        assert_eq!(declarations[0]["description"], "Read a file");

        // Tool config should be AUTO mode.
        assert_eq!(json["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
    }

    #[test]
    fn test_build_request_default_max_tokens() {
        let provider = GeminiProvider::new("gemini-2.0-flash");
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("hi".to_string()),
                timestamp: 0,
            })],
            tools: vec![],
        };
        let options = crate::provider::StreamOptions::default();

        let req = provider.build_request(&context, &options);
        let json = serde_json::to_value(&req).expect("serialize");

        // Default max tokens should be DEFAULT_MAX_TOKENS (8192).
        assert_eq!(
            json["generationConfig"]["maxOutputTokens"],
            DEFAULT_MAX_TOKENS
        );
    }

    // ─── API key as query parameter tests ───────────────────────────────

    #[test]
    fn test_streaming_url_includes_key_as_query_param() {
        let provider = GeminiProvider::new("gemini-2.0-flash");
        let url = provider.streaming_url("my-secret-key");

        // API key must be in the query string, not as a header.
        assert!(
            url.contains("key=my-secret-key"),
            "API key should be in query param"
        );
        assert!(url.contains("alt=sse"), "alt=sse should be present");
        assert!(
            url.contains("streamGenerateContent"),
            "should use streaming endpoint"
        );
    }

    #[test]
    fn test_streaming_url_custom_base() {
        let provider =
            GeminiProvider::new("gemini-pro").with_base_url("https://custom.example.com/v1");
        let url = provider.streaming_url("key123");

        assert!(url.starts_with("https://custom.example.com/v1/models/gemini-pro"));
        assert!(url.contains("key=key123"));
    }

    // ─── Content part mapping tests ─────────────────────────────────────

    #[test]
    fn test_convert_user_text_to_gemini_parts() {
        let parts = convert_user_content_to_parts(&UserContent::Text("hello world".to_string()));
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            GeminiPart::Text { text } => assert_eq!(text, "hello world"),
            _ => panic!("expected text part"),
        }
    }

    #[test]
    fn test_convert_user_blocks_with_image_to_gemini_parts() {
        let content = UserContent::Blocks(vec![
            ContentBlock::Text(TextContent::new("describe this")),
            ContentBlock::Image(crate::model::ImageContent {
                data: "aGVsbG8=".to_string(),
                mime_type: "image/png".to_string(),
            }),
        ]);

        let parts = convert_user_content_to_parts(&content);
        assert_eq!(parts.len(), 2);
        match &parts[0] {
            GeminiPart::Text { text } => assert_eq!(text, "describe this"),
            _ => panic!("expected text part"),
        }
        match &parts[1] {
            GeminiPart::InlineData { inline_data } => {
                assert_eq!(inline_data.mime_type, "image/png");
                assert_eq!(inline_data.data, "aGVsbG8=");
            }
            _ => panic!("expected inline_data part"),
        }
    }

    #[test]
    fn test_convert_assistant_message_with_tool_call() {
        let message = Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Text(TextContent::new("Let me read that file.")),
                ContentBlock::ToolCall(ToolCall {
                    id: "call_123".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({"path": "/tmp/test.txt"}),
                    thought_signature: None,
                }),
            ],
            api: "google".to_string(),
            provider: "google".to_string(),
            model: "gemini-2.0-flash".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 0,
        });

        let converted = convert_message_to_gemini(&message);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, Some("model".to_string()));
        assert_eq!(converted[0].parts.len(), 2);

        match &converted[0].parts[0] {
            GeminiPart::Text { text } => assert_eq!(text, "Let me read that file."),
            _ => panic!("expected text part"),
        }
        match &converted[0].parts[1] {
            GeminiPart::FunctionCall { function_call } => {
                assert_eq!(function_call.name, "read");
                assert_eq!(function_call.args["path"], "/tmp/test.txt");
            }
            _ => panic!("expected function_call part"),
        }
    }

    #[test]
    fn test_convert_assistant_empty_content_returns_empty() {
        let message = Message::Assistant(AssistantMessage {
            content: vec![],
            api: "google".to_string(),
            provider: "google".to_string(),
            model: "gemini-2.0-flash".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        });

        let converted = convert_message_to_gemini(&message);
        assert!(converted.is_empty());
    }

    #[test]
    fn test_convert_tool_result_success() {
        let message = Message::ToolResult(crate::model::ToolResultMessage {
            tool_call_id: "call_123".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("file contents here"))],
            details: None,
            is_error: false,
            timestamp: 0,
        });

        let converted = convert_message_to_gemini(&message);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, Some("user".to_string()));

        match &converted[0].parts[0] {
            GeminiPart::FunctionResponse { function_response } => {
                assert_eq!(function_response.name, "read");
                assert_eq!(function_response.response["result"], "file contents here");
                assert!(function_response.response.get("error").is_none());
            }
            _ => panic!("expected function_response part"),
        }
    }

    #[test]
    fn test_convert_tool_result_error() {
        let message = Message::ToolResult(crate::model::ToolResultMessage {
            tool_call_id: "call_456".to_string(),
            tool_name: "bash".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("command not found"))],
            details: None,
            is_error: true,
            timestamp: 0,
        });

        let converted = convert_message_to_gemini(&message);
        assert_eq!(converted.len(), 1);

        match &converted[0].parts[0] {
            GeminiPart::FunctionResponse { function_response } => {
                assert_eq!(function_response.name, "bash");
                assert_eq!(function_response.response["error"], "command not found");
                assert!(function_response.response.get("result").is_none());
            }
            _ => panic!("expected function_response part"),
        }
    }

    #[test]
    fn test_convert_custom_message() {
        let message = Message::Custom(crate::model::CustomMessage {
            custom_type: "system_note".to_string(),
            content: "Context window approaching limit.".to_string(),
            display: false,
            details: None,
            timestamp: 0,
        });

        let converted = convert_message_to_gemini(&message);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, Some("user".to_string()));
        match &converted[0].parts[0] {
            GeminiPart::Text { text } => {
                assert_eq!(text, "Context window approaching limit.");
            }
            _ => panic!("expected text part"),
        }
    }

    // ─── Response parsing / stop reason tests ───────────────────────────

    #[test]
    fn test_stop_reason_mapping() {
        // Test all finish reason strings map correctly.
        let test_cases = vec![
            ("STOP", StopReason::Stop),
            ("MAX_TOKENS", StopReason::Length),
            ("SAFETY", StopReason::Error),
            ("RECITATION", StopReason::Error),
            ("OTHER", StopReason::Error),
            ("UNKNOWN_REASON", StopReason::Stop), // unknown defaults to Stop
        ];

        for (reason_str, expected) in test_cases {
            let candidate = GeminiCandidate {
                content: None,
                finish_reason: Some(reason_str.to_string()),
            };

            let runtime = RuntimeBuilder::current_thread().build().unwrap();
            runtime.block_on(async {
                let byte_stream = stream::empty::<std::result::Result<Vec<u8>, std::io::Error>>();
                let event_source = crate::sse::SseStream::new(Box::pin(byte_stream));
                let mut state = StreamState::new(
                    event_source,
                    "test".to_string(),
                    "test".to_string(),
                    "test".to_string(),
                );
                state.process_candidate(candidate).unwrap();
                assert_eq!(
                    state.partial.stop_reason, expected,
                    "finish_reason '{reason_str}' should map to {expected:?}"
                );
            });
        }
    }

    #[test]
    fn test_usage_metadata_parsing() {
        let data = r#"{
            "usageMetadata": {
                "promptTokenCount": 42,
                "candidatesTokenCount": 100,
                "totalTokenCount": 142
            }
        }"#;

        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let byte_stream = stream::empty::<std::result::Result<Vec<u8>, std::io::Error>>();
            let event_source = crate::sse::SseStream::new(Box::pin(byte_stream));
            let mut state = StreamState::new(
                event_source,
                "test".to_string(),
                "test".to_string(),
                "test".to_string(),
            );
            state.process_event(data).unwrap();
            assert_eq!(state.partial.usage.input, 42);
            assert_eq!(state.partial.usage.output, 100);
            assert_eq!(state.partial.usage.total_tokens, 142);
        });
    }

    // ─── Full conversation round-trip tests ─────────────────────────────

    #[test]
    fn test_build_request_full_conversation() {
        let provider = GeminiProvider::new("gemini-2.0-flash");
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
                    api: "google".to_string(),
                    provider: "google".to_string(),
                    model: "gemini-2.0-flash".to_string(),
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
                parameters: serde_json::json!({"type": "object"}),
            }],
        };
        let options = crate::provider::StreamOptions::default();

        let req = provider.build_request(&context, &options);
        let json = serde_json::to_value(&req).expect("serialize");

        let contents = json["contents"].as_array().expect("contents");
        assert_eq!(contents.len(), 3); // user, model, user (tool result)

        // First: user message
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "Read /tmp/a.txt");

        // Second: model with function call
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["functionCall"]["name"], "read");

        // Third: function response (sent as user role)
        assert_eq!(contents[2]["role"], "user");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "read");
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["response"]["result"],
            "file contents"
        );
    }
}
