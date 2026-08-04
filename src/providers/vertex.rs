//! Google Vertex AI provider implementation.
//!
//! This module implements the Provider trait for Google Cloud Vertex AI,
//! supporting both Google-native models (Gemini via Vertex) and Anthropic
//! models hosted on Vertex AI.
//!
//! Vertex AI URL format (Google models):
//! `https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/google/models/{model}:streamGenerateContent`
//!
//! Vertex AI URL format (Anthropic models):
//! `https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/anthropic/models/{model}:streamRawPredict`

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{
    AssistantMessage, ContentBlock, StopReason, StreamEvent, TextContent, ToolCall, Usage,
};
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamOptions};
use crate::providers::gemini::{
    self, GeminiCandidate, GeminiContent, GeminiFunctionCall, GeminiFunctionCallingConfig,
    GeminiGenerationConfig, GeminiPart, GeminiRequest, GeminiStreamResponse, GeminiTool,
    GeminiToolConfig,
};
use crate::sse::SseStream;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, Stream};
use std::collections::VecDeque;
use std::pin::Pin;

// ============================================================================
// Constants
// ============================================================================

const VERTEX_DEFAULT_REGION: &str = "us-central1";

/// Environment variable for the Google Cloud project ID.
const VERTEX_PROJECT_ENV: &str = "GOOGLE_CLOUD_PROJECT";
/// Fallback: `VERTEX_PROJECT` is a common alternative.
const VERTEX_PROJECT_ENV_ALT: &str = "VERTEX_PROJECT";

/// Environment variable for the Vertex AI region/location.
const VERTEX_LOCATION_ENV: &str = "GOOGLE_CLOUD_LOCATION";
/// Fallback: `VERTEX_LOCATION` is a common alternative.
const VERTEX_LOCATION_ENV_ALT: &str = "VERTEX_LOCATION";

// ============================================================================
// Vertex AI Provider
// ============================================================================

/// Google Vertex AI provider supporting both Google-native (Gemini) and
/// Anthropic models via Vertex endpoints.
pub struct VertexProvider {
    client: Client,
    model: String,
    /// GCP project ID (required).
    project: Option<String>,
    /// GCP region / location (default: `us-central1`).
    location: String,
    /// Publisher: `"google"` for Gemini models, `"anthropic"` for Claude models.
    publisher: String,
    /// Optional override for the full endpoint URL (for tests).
    endpoint_url_override: Option<String>,
    compat: Option<CompatConfig>,
}

impl VertexProvider {
    /// Create a new Vertex AI provider for Google-native (Gemini) models.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            project: None,
            location: VERTEX_DEFAULT_REGION.to_string(),
            publisher: "google".to_string(),
            endpoint_url_override: None,
            compat: None,
        }
    }

    /// Set the GCP project ID.
    #[must_use]
    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// Set the GCP region/location.
    #[must_use]
    pub fn with_location(mut self, location: impl Into<String>) -> Self {
        self.location = location.into();
        self
    }

    /// Set the publisher (`"google"` or `"anthropic"`).
    #[must_use]
    pub fn with_publisher(mut self, publisher: impl Into<String>) -> Self {
        self.publisher = publisher.into();
        self
    }

    /// Override the full endpoint URL (for deterministic tests).
    #[must_use]
    pub fn with_endpoint_url(mut self, url: impl Into<String>) -> Self {
        self.endpoint_url_override = Some(url.into());
        self
    }

    /// Attach provider-specific compatibility overrides.
    #[must_use]
    pub fn with_compat(mut self, compat: Option<CompatConfig>) -> Self {
        self.compat = compat;
        self
    }

    /// Create with a custom HTTP client (VCR, test harness, etc.).
    #[must_use]
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Resolve the GCP project from explicit config or environment.
    fn resolve_project(&self) -> Result<String> {
        if let Some(project) = &self.project {
            return Ok(project.clone());
        }
        std::env::var(VERTEX_PROJECT_ENV)
            .or_else(|_| std::env::var(VERTEX_PROJECT_ENV_ALT))
            .map_err(|_| {
                Error::provider(
                    "google-vertex",
                    format!(
                        "Missing GCP project. Set {VERTEX_PROJECT_ENV} or {VERTEX_PROJECT_ENV_ALT}, \
                         or configure `project` in provider settings."
                    ),
                )
            })
    }

    /// Resolve the GCP location from explicit config or environment.
    fn resolve_location(&self) -> String {
        if self.location != VERTEX_DEFAULT_REGION {
            return self.location.clone();
        }
        std::env::var(VERTEX_LOCATION_ENV)
            .or_else(|_| std::env::var(VERTEX_LOCATION_ENV_ALT))
            .unwrap_or_else(|_| VERTEX_DEFAULT_REGION.to_string())
    }

    /// Build the streaming endpoint URL.
    ///
    /// Google models: `.../publishers/google/models/{model}:streamGenerateContent`
    /// Anthropic models: `.../publishers/anthropic/models/{model}:streamRawPredict`
    fn streaming_url(&self, project: &str, location: &str) -> String {
        if let Some(url) = &self.endpoint_url_override {
            return url.clone();
        }

        let method = if self.publisher == "anthropic" {
            "streamRawPredict"
        } else {
            "streamGenerateContent"
        };

        format!(
            "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/{publisher}/models/{model}:{method}",
            location = location,
            project = project,
            publisher = self.publisher,
            model = self.model,
            method = method,
        )
    }

    /// Build the Gemini-format request body (for Google-native models).
    #[allow(clippy::unused_self)]
    pub fn build_gemini_request(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> GeminiRequest {
        let contents = Self::build_contents(context);
        let system_instruction = context.system_prompt.as_deref().map(|s| GeminiContent {
            role: None,
            parts: vec![GeminiPart::Text {
                text: s.to_string(),
            }],
        });

        let tools: Option<Vec<GeminiTool>> = if context.tools.is_empty() {
            None
        } else {
            Some(vec![GeminiTool {
                function_declarations: context
                    .tools
                    .iter()
                    .map(gemini::convert_tool_to_gemini)
                    .collect(),
            }])
        };

        let tool_config = if tools.is_some() {
            Some(GeminiToolConfig {
                function_calling_config: GeminiFunctionCallingConfig { mode: "AUTO" },
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
                max_output_tokens: options.max_tokens.or(Some(gemini::DEFAULT_MAX_TOKENS)),
                temperature: options.temperature,
                candidate_count: Some(1),
            }),
        }
    }

    /// Build the contents array from context messages.
    fn build_contents(context: &Context<'_>) -> Vec<GeminiContent> {
        let mut contents = Vec::new();
        for message in context.messages.iter() {
            contents.extend(gemini::convert_message_to_gemini(message));
        }
        contents
    }
}

#[async_trait]
impl Provider for VertexProvider {
    fn name(&self) -> &'static str {
        "google-vertex"
    }

    fn api(&self) -> &'static str {
        "google-vertex"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    #[allow(clippy::too_many_lines)]
    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        // Resolve auth: Bearer token for Vertex AI.
        let auth_value = options
            .api_key
            .clone()
            .or_else(|| std::env::var("GOOGLE_CLOUD_API_KEY").ok())
            .or_else(|| std::env::var("VERTEX_API_KEY").ok())
            .ok_or_else(|| {
                Error::provider(
                    "google-vertex",
                    "Missing Vertex AI API key / access token. \
                     Set GOOGLE_CLOUD_API_KEY or VERTEX_API_KEY.",
                )
            })?;

        let project = self.resolve_project()?;
        let location = self.resolve_location();
        let url = self.streaming_url(&project, &location);

        // Build request body in Gemini format (Google-native models).
        let request_body = self.build_gemini_request(context, options);

        // Build HTTP request with Bearer auth.
        let mut request = self
            .client
            .post(&url)
            .header("Accept", "text/event-stream")
            .header("Authorization", format!("Bearer {auth_value}"));

        // Apply provider-specific custom headers from compat config.
        if let Some(compat) = &self.compat
            && let Some(custom_headers) = &compat.custom_headers
        {
            for (key, value) in custom_headers {
                request = request.header(key, value);
            }
        }

        // Per-request headers from `StreamOptions` (highest priority).
        for (key, value) in &options.headers {
            request = request.header(key, value);
        }

        let request = request.json(&request_body)?;

        let response = Box::pin(request.send()).await?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let body = response
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
            return Err(Error::provider(
                "google-vertex",
                format!("Vertex AI API error (HTTP {status}): {body}"),
            ));
        }

        // Create SSE stream for streaming responses.
        let event_source = SseStream::new(response.bytes_stream());

        // Create stream state — same response format as Gemini.
        let model = self.model.clone();
        let api = self.api().to_string();
        let provider = self.name().to_string();

        let stream = stream::unfold(
            StreamState::new(event_source, model, api, provider),
            |mut state| async move {
                if state.finished {
                    return None;
                }
                loop {
                    // Drain pending events before polling for more SSE data.
                    if let Some(event) = state.pending_events.pop_front() {
                        return Some((Ok(event), state));
                    }

                    match state.event_source.next().await {
                        Some(Ok(msg)) => {
                            state.transient_error_count = 0;
                            if msg.event == "ping" {
                                continue;
                            }

                            if let Err(e) = state.process_event(&msg.data) {
                                state.finished = true;
                                return Some((Err(e), state));
                            }
                        }
                        Some(Err(e)) => {
                            // WriteZero, WouldBlock, and TimedOut errors are treated as transient.
                            // Skip them and keep reading the stream, but cap
                            // consecutive occurrences to avoid infinite loops.
                            const MAX_CONSECUTIVE_TRANSIENT_ERRORS: usize = 5;
                            if e.kind() == std::io::ErrorKind::WriteZero
                                || e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut
                            {
                                state.transient_error_count += 1;
                                if state.transient_error_count <= MAX_CONSECUTIVE_TRANSIENT_ERRORS {
                                    tracing::warn!(
                                        kind = ?e.kind(),
                                        count = state.transient_error_count,
                                        "Transient error in SSE stream, continuing"
                                    );
                                    continue;
                                }
                                tracing::warn!(
                                    kind = ?e.kind(),
                                    "Error persisted after {MAX_CONSECUTIVE_TRANSIENT_ERRORS} \
                                     consecutive attempts, treating as fatal"
                                );
                            }
                            state.finished = true;
                            let err = Error::sse(&e);
                            return Some((Err(err), state));
                        }
                        None => {
                            // Stream ended naturally.
                            state.finished = true;
                            let reason = state.partial.stop_reason;
                            let message = std::mem::take(&mut state.partial);
                            return Some((Ok(StreamEvent::Done { reason, message }), state));
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }
}

// ============================================================================
// Stream State (reuses Gemini response format)
// ============================================================================

struct StreamState<S>
where
    S: Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin,
{
    event_source: SseStream<S>,
    partial: AssistantMessage,
    pending_events: VecDeque<StreamEvent>,
    started: bool,
    finished: bool,
    /// Consecutive WriteZero errors seen without a successful event in between.
    transient_error_count: usize,
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
                stop_details: None,
                error_message: None,
                timestamp: chrono::Utc::now().timestamp_millis(),
            },
            pending_events: VecDeque::new(),
            started: false,
            finished: false,
            transient_error_count: 0,
        }
    }

    fn process_event(&mut self, data: &str) -> Result<()> {
        let response: GeminiStreamResponse = serde_json::from_str(data)
            .map_err(|e| Error::api(format!("JSON parse error: {e}\nData: {data}")))?;

        // Handle usage metadata.
        if let Some(metadata) = response.usage_metadata {
            self.partial.usage.input = metadata.prompt_token_count.unwrap_or(0);
            self.partial.usage.output = metadata.candidates_token_count.unwrap_or(0);
            self.partial.usage.total_tokens = metadata.total_token_count.unwrap_or(0);
        }

        // Process candidates.
        if let Some(candidates) = response.candidates
            && let Some(candidate) = candidates.into_iter().next()
        {
            self.process_candidate(candidate)?;
        }

        Ok(())
    }

    #[allow(clippy::unnecessary_wraps)]
    fn process_candidate(&mut self, candidate: GeminiCandidate) -> Result<()> {
        // Handle finish reason.
        if let Some(ref reason) = candidate.finish_reason {
            self.partial.stop_reason = match reason.as_str() {
                "MAX_TOKENS" => StopReason::Length,
                "SAFETY" | "RECITATION" | "OTHER" => StopReason::Error,
                "FUNCTION_CALL" => StopReason::ToolUse,
                _ => StopReason::Stop,
            };
        }

        // Process content parts — queue all events into pending_events.
        if let Some(content) = candidate.content {
            for part in content.parts {
                match part {
                    GeminiPart::Text { text } => {
                        let last_is_text =
                            matches!(self.partial.content.last(), Some(ContentBlock::Text(_)));
                        if !last_is_text {
                            let content_index = self.partial.content.len();
                            self.partial
                                .content
                                .push(ContentBlock::Text(TextContent::new("")));

                            self.ensure_started();

                            self.pending_events
                                .push_back(StreamEvent::TextStart { content_index });
                        }
                        let content_index = self.partial.content.len() - 1;

                        if let Some(ContentBlock::Text(t)) =
                            self.partial.content.get_mut(content_index)
                        {
                            t.text.push_str(&text);
                        }

                        self.ensure_started();

                        self.pending_events.push_back(StreamEvent::TextDelta {
                            content_index,
                            delta: text,
                        });
                    }
                    GeminiPart::FunctionCall { function_call } => {
                        let id = format!("call_{}", uuid::Uuid::new_v4().simple());

                        let args_str = serde_json::to_string(&function_call.args)
                            .unwrap_or_else(|_| "{}".to_string());
                        let GeminiFunctionCall { name, args } = function_call;

                        let tool_call = ToolCall {
                            id,
                            name,
                            arguments: args,
                            thought_signature: None,
                        };

                        self.partial
                            .content
                            .push(ContentBlock::ToolCall(tool_call.clone()));
                        let content_index = self.partial.content.len() - 1;

                        self.partial.stop_reason = StopReason::ToolUse;

                        self.ensure_started();

                        self.pending_events.push_back(StreamEvent::ToolCallStart {
                            content_index,
                            id: tool_call.id.clone(),
                            name: tool_call.name.clone(),
                        });
                        self.pending_events.push_back(StreamEvent::ToolCallDelta {
                            content_index,
                            delta: args_str,
                        });
                        self.pending_events.push_back(StreamEvent::ToolCallEnd {
                            content_index,
                            tool_call,
                        });
                    }
                    GeminiPart::InlineData { .. }
                    | GeminiPart::FunctionResponse { .. }
                    | GeminiPart::Unknown(_) => {
                        // Input-only parts are skipped.
                        // Unknown parts are also skipped so new Gemini API part
                        // variants don't break streaming.
                    }
                }
            }
        }

        // Emit TextEnd/ThinkingEnd for all open text/thinking blocks when a finish reason
        // is present.
        if candidate.finish_reason.is_some() {
            for (content_index, block) in self.partial.content.iter().enumerate() {
                if let ContentBlock::Text(t) = block {
                    self.pending_events.push_back(StreamEvent::TextEnd {
                        content_index,
                        content: t.text.clone(),
                    });
                } else if let ContentBlock::Thinking(t) = block {
                    self.pending_events.push_back(StreamEvent::ThinkingEnd {
                        content_index,
                        content: t.thinking.clone(),
                    });
                }
            }
        }

        Ok(())
    }

    fn ensure_started(&mut self) {
        if !self.started {
            self.started = true;
            self.pending_events.push_back(StreamEvent::Start {
                partial: self.partial.clone(),
            });
        }
    }
}

// ============================================================================
// Vertex Runtime Resolution (similar to Azure runtime resolution)
// ============================================================================

/// Resolved Vertex AI runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VertexProviderRuntime {
    pub(crate) project: String,
    pub(crate) location: String,
    pub(crate) publisher: String,
    pub(crate) model: String,
}

/// Resolve Vertex AI provider runtime from a `ModelEntry`.
///
/// Configuration sources (highest priority first):
/// 1. Explicit fields parsed from `base_url`
/// 2. Environment variables (`GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_LOCATION`)
/// 3. Defaults (location: `us-central1`, publisher: `google`)
pub(crate) fn resolve_vertex_provider_runtime(
    entry: &crate::models::ModelEntry,
) -> Result<VertexProviderRuntime> {
    // Try to parse project/location/publisher from base_url.
    let (url_project, url_location, url_publisher) = parse_vertex_base_url(&entry.model.base_url);

    let project = url_project
        .or_else(|| std::env::var(VERTEX_PROJECT_ENV).ok())
        .or_else(|| std::env::var(VERTEX_PROJECT_ENV_ALT).ok())
        .ok_or_else(|| {
            Error::provider(
                "google-vertex",
                format!(
                    "Missing GCP project. Set {VERTEX_PROJECT_ENV} or provide a Vertex AI base URL \
                     like https://REGION-aiplatform.googleapis.com/v1/projects/PROJECT/locations/REGION/..."
                ),
            )
        })?;

    let location = url_location
        .or_else(|| std::env::var(VERTEX_LOCATION_ENV).ok())
        .or_else(|| std::env::var(VERTEX_LOCATION_ENV_ALT).ok())
        .unwrap_or_else(|| VERTEX_DEFAULT_REGION.to_string());

    let publisher = url_publisher.unwrap_or_else(|| "google".to_string());

    Ok(VertexProviderRuntime {
        project,
        location,
        publisher,
        model: entry.model.id.clone(),
    })
}

/// Parse project, location, and publisher from a Vertex AI base URL.
///
/// Expected format:
/// `https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/{publisher}/...`
fn parse_vertex_base_url(base_url: &str) -> (Option<String>, Option<String>, Option<String>) {
    if base_url.is_empty() {
        return (None, None, None);
    }

    // Extract location from hostname: "{location}-aiplatform.googleapis.com"
    let location_from_host = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .and_then(|rest| rest.split('-').next())
        .and_then(|loc| {
            // Validate it looks like a region (e.g. "us", "europe", "asia").
            if loc.chars().all(|c| c.is_ascii_lowercase() || c == '-') && !loc.is_empty() {
                Some(loc.to_string())
            } else {
                None
            }
        });

    // Extract project, location, publisher from path segments.
    let path_segments: Vec<&str> = base_url.split('/').collect();

    let project = path_segments
        .iter()
        .zip(path_segments.iter().skip(1))
        .find(|(key, _)| **key == "projects")
        .map(|(_, val)| (*val).to_string());

    let location = path_segments
        .iter()
        .zip(path_segments.iter().skip(1))
        .find(|(key, _)| **key == "locations")
        .map(|(_, val)| (*val).to_string())
        .or(location_from_host);

    let publisher = path_segments
        .iter()
        .zip(path_segments.iter().skip(1))
        .find(|(key, _)| **key == "publishers")
        .map(|(_, val)| (*val).to_string());

    (project, location, publisher)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, UserContent};
    use crate::provider::ToolDef;
    use asupersync::runtime::RuntimeBuilder;
    use futures::{StreamExt, stream};
    use serde_json::Value;

    #[test]
    fn test_provider_info() {
        let provider = VertexProvider::new("gemini-2.0-flash");
        assert_eq!(provider.name(), "google-vertex");
        assert_eq!(provider.api(), "google-vertex");
        assert_eq!(provider.model_id(), "gemini-2.0-flash");
    }

    #[test]
    fn test_streaming_url_google_publisher() {
        let provider = VertexProvider::new("gemini-2.0-flash")
            .with_project("my-project")
            .with_location("us-central1");

        let url = provider.streaming_url("my-project", "us-central1");
        assert_eq!(
            url,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-2.0-flash:streamGenerateContent"
        );
    }

    #[test]
    fn test_streaming_url_anthropic_publisher() {
        let provider = VertexProvider::new("claude-sonnet-4-20250514")
            .with_project("my-project")
            .with_location("europe-west1")
            .with_publisher("anthropic");

        let url = provider.streaming_url("my-project", "europe-west1");
        assert_eq!(
            url,
            "https://europe-west1-aiplatform.googleapis.com/v1/projects/my-project/locations/europe-west1/publishers/anthropic/models/claude-sonnet-4-20250514:streamRawPredict"
        );
    }

    #[test]
    fn test_streaming_url_override() {
        let provider =
            VertexProvider::new("gemini-2.0-flash").with_endpoint_url("http://127.0.0.1:8080/mock");

        let url = provider.streaming_url("ignored", "ignored");
        assert_eq!(url, "http://127.0.0.1:8080/mock");
    }

    #[test]
    fn test_build_gemini_request_basic() {
        let provider = VertexProvider::new("gemini-2.0-flash");
        let context = Context::owned(
            Some("You are helpful.".to_string()),
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("What is Vertex AI?".to_string()),
                timestamp: 0,
            })],
            vec![],
        );
        let options = StreamOptions {
            max_tokens: Some(1024),
            temperature: Some(0.7),
            ..Default::default()
        };

        let req = provider.build_gemini_request(&context, &options);
        let json = serde_json::to_value(&req).expect("serialize");

        let contents = json["contents"].as_array().expect("contents");
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "What is Vertex AI?");

        assert_eq!(
            json["systemInstruction"]["parts"][0]["text"],
            "You are helpful."
        );
        assert_eq!(json["generationConfig"]["maxOutputTokens"], 1024);
    }

    #[test]
    fn test_build_gemini_request_with_tools() {
        let provider = VertexProvider::new("gemini-2.0-flash");
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Read a file".to_string()),
                timestamp: 0,
            })],
            vec![ToolDef {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "path": {"type": "string"} },
                    "required": ["path"]
                }),
            }],
        );
        let options = StreamOptions::default();

        let req = provider.build_gemini_request(&context, &options);
        let json = serde_json::to_value(&req).expect("serialize");

        let tools = json["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 1);
        let decls = tools[0]["functionDeclarations"]
            .as_array()
            .expect("declarations");
        assert_eq!(decls[0]["name"], "read");
        assert_eq!(json["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
    }

    #[test]
    fn test_parse_vertex_base_url_full() {
        let url = "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1/publishers/google/models/gemini-2.0-flash";
        let (project, location, publisher) = parse_vertex_base_url(url);
        assert_eq!(project.as_deref(), Some("my-proj"));
        assert_eq!(location.as_deref(), Some("us-central1"));
        assert_eq!(publisher.as_deref(), Some("google"));
    }

    #[test]
    fn test_parse_vertex_base_url_anthropic() {
        let url = "https://europe-west1-aiplatform.googleapis.com/v1/projects/corp-ai/locations/europe-west1/publishers/anthropic/models/claude-sonnet-4-20250514";
        let (project, location, publisher) = parse_vertex_base_url(url);
        assert_eq!(project.as_deref(), Some("corp-ai"));
        assert_eq!(location.as_deref(), Some("europe-west1"));
        assert_eq!(publisher.as_deref(), Some("anthropic"));
    }

    #[test]
    fn test_parse_vertex_base_url_empty() {
        let (project, location, publisher) = parse_vertex_base_url("");
        assert!(project.is_none());
        assert!(location.is_none());
        assert!(publisher.is_none());
    }

    #[test]
    fn test_parse_vertex_base_url_partial() {
        let url = "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1";
        let (project, location, publisher) = parse_vertex_base_url(url);
        assert_eq!(project.as_deref(), Some("my-proj"));
        assert_eq!(location.as_deref(), Some("us-central1"));
        assert!(publisher.is_none());
    }

    #[test]
    fn test_resolve_vertex_provider_runtime_from_url() {
        let entry = crate::models::ModelEntry {
            model: crate::provider::Model {
                id: "gemini-2.0-flash".to_string(),
                name: "Gemini 2.0 Flash".to_string(),
                api: "google-vertex".to_string(),
                provider: "google-vertex".to_string(),
                base_url: "https://us-central1-aiplatform.googleapis.com/v1/projects/test-proj/locations/us-central1/publishers/google/models/gemini-2.0-flash".to_string(),
                reasoning: false,
                input: vec![],
                cost: crate::provider::ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 8192,
                headers: std::collections::HashMap::new(),
            },
            api_key: None,
            headers: std::collections::HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        };

        let runtime = resolve_vertex_provider_runtime(&entry).expect("resolve");
        assert_eq!(runtime.project, "test-proj");
        assert_eq!(runtime.location, "us-central1");
        assert_eq!(runtime.publisher, "google");
        assert_eq!(runtime.model, "gemini-2.0-flash");
    }

    // ─── Streaming response parsing ──────────────────────────────────────

    #[test]
    fn test_stream_text_response() {
        let events = vec![
            serde_json::json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{"text": "Hello from "}]
                    }
                }]
            }),
            serde_json::json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{"text": "Vertex AI!"}]
                    },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 10,
                    "candidatesTokenCount": 5,
                    "totalTokenCount": 15
                }
            }),
        ];

        let stream_events = collect_events(&events);

        // Should have: Start, TextDelta("Hello from "), TextDelta("Vertex AI!"), Done
        assert!(
            stream_events
                .iter()
                .any(|e| matches!(e, StreamEvent::Start { .. })),
            "should emit Start"
        );

        let text_deltas: Vec<&str> = stream_events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text_deltas, vec!["Hello from ", "Vertex AI!"]);

        let done = stream_events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Done { message, .. } => Some(message),
                _ => None,
            })
            .expect("done event");
        assert_eq!(done.usage.input, 10);
        assert_eq!(done.usage.output, 5);
    }

    #[test]
    fn test_stream_tool_call_response() {
        let events = vec![serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{
                        "functionCall": {
                            "name": "read",
                            "args": {"path": "/tmp/test.txt"}
                        }
                    }]
                },
                "finishReason": "STOP"
            }]
        })];

        let stream_events = collect_events(&events);

        assert!(
            stream_events
                .iter()
                .any(|e| matches!(e, StreamEvent::ToolCallStart { .. })),
            "should emit ToolCallStart"
        );
        assert!(
            stream_events
                .iter()
                .any(|e| matches!(e, StreamEvent::ToolCallEnd { .. })),
            "should emit ToolCallEnd"
        );

        let done = stream_events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Done { message, .. } => Some(message),
                _ => None,
            })
            .expect("done event");
        assert_eq!(done.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn test_stream_ignores_unknown_parts() {
        let events = vec![serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {
                            "executableCode": {
                                "language": "python",
                                "code": "print('x')"
                            }
                        },
                        {"text": "still works"}
                    ]
                },
                "finishReason": "STOP"
            }]
        })];

        let stream_events = collect_events(&events);

        let text_deltas: Vec<&str> = stream_events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text_deltas, vec!["still works"]);
        assert!(
            stream_events
                .iter()
                .any(|e| matches!(e, StreamEvent::Done { .. })),
            "should emit Done even when unknown parts are present"
        );
    }

    // ─── Test helpers ────────────────────────────────────────────────────

    fn collect_events(events: &[Value]) -> Vec<StreamEvent> {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let byte_stream = stream::iter(
                events
                    .iter()
                    .map(|event| {
                        let data = serde_json::to_string(event).expect("serialize event");
                        format!("data: {data}\n\n").into_bytes()
                    })
                    .map(Ok),
            );
            let event_source = crate::sse::SseStream::new(Box::pin(byte_stream));
            let mut state = StreamState::new(
                event_source,
                "gemini-test".to_string(),
                "google-vertex".to_string(),
                "google-vertex".to_string(),
            );
            let mut out = Vec::new();

            loop {
                let Some(item) = state.event_source.next().await else {
                    if !state.finished {
                        state.finished = true;
                        out.push(StreamEvent::Done {
                            reason: state.partial.stop_reason,
                            message: std::mem::take(&mut state.partial),
                        });
                    }
                    break;
                };

                let msg = item.expect("SSE event");
                if msg.event == "ping" {
                    continue;
                }
                state.process_event(&msg.data).expect("process_event");
                out.extend(state.pending_events.drain(..));
            }

            out
        })
    }
}

// ============================================================================
// Fuzzing support
// ============================================================================

#[cfg(feature = "fuzzing")]
pub mod fuzz {
    use super::*;
    use futures::stream;
    use std::pin::Pin;

    type FuzzStream =
        Pin<Box<futures::stream::Empty<std::result::Result<Vec<u8>, std::io::Error>>>>;

    /// Opaque wrapper around the Vertex AI stream processor state.
    pub struct Processor(StreamState<FuzzStream>);

    impl Default for Processor {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Processor {
        /// Create a fresh processor with default state.
        pub fn new() -> Self {
            let empty = stream::empty::<std::result::Result<Vec<u8>, std::io::Error>>();
            Self(StreamState::new(
                crate::sse::SseStream::new(Box::pin(empty)),
                "vertex-fuzz".into(),
                "vertex-ai".into(),
                "vertex".into(),
            ))
        }

        /// Feed one SSE data payload and return any emitted `StreamEvent`s.
        pub fn process_event(&mut self, data: &str) -> crate::error::Result<Vec<StreamEvent>> {
            self.0.process_event(data)?;
            Ok(self.0.pending_events.drain(..).collect())
        }
    }
}
