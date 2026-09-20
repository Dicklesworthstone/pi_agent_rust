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
use crate::model::StreamEvent;
#[cfg(test)]
use crate::model::{ContentBlock, StopReason, TextContent};
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamOptions};
use crate::providers::gemini::{
    self, GeminiContent, GeminiFunctionCallingConfig, GeminiGenerationConfig, GeminiPart,
    GeminiRequest, GeminiTool, GeminiToolConfig, StreamState,
};
use crate::sse::SseStream;
use async_trait::async_trait;
#[cfg(test)]
use futures::StreamExt;
use futures::stream::Stream;
use std::pin::Pin;

#[cfg(test)]
mod tests_transport;

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
            "streamGenerateContent?alt=sse"
        };
        // Global requests use the unprefixed host, not the non-existent
        // global-aiplatform.googleapis.com endpoint.
        let host = if location == "global" {
            "aiplatform.googleapis.com".to_string()
        } else {
            format!("{location}-aiplatform.googleapis.com")
        };

        format!(
            "https://{host}/v1/projects/{project}/locations/{location}/publishers/{publisher}/models/{model}:{method}",
            publisher = self.publisher,
            model = self.model,
        )
    }

    /// Build the base Gemini request. Live requests apply model-specific
    /// thinking controls before the request-rewrite hook.
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
        // Select the protocol before dispatch. An unknown publisher must not
        // receive a Gemini request with the user's Google credentials.
        if !matches!(self.publisher.as_str(), "google" | "anthropic") {
            return Err(Error::provider(
                self.name(),
                "Unsupported Vertex AI publisher",
            ));
        }
        let authorization = vertex_authorization(options, self.compat.as_ref(), |name| {
            std::env::var(name).ok()
        })?;
        let project = self.resolve_project()?;
        let location = self.resolve_location();
        let url = self.streaming_url(&project, &location);

        if self.publisher == "anthropic" {
            let provider = super::anthropic::AnthropicProvider::new(self.model.clone())
                .with_provider_name(self.name())
                .with_base_url(url)
                .with_client(self.client.clone())
                .with_compat(self.compat.clone());
            return Box::pin(provider.stream_vertex(context, options, &authorization)).await;
        }

        // Apply the same native thinking contract as Developer API and CLI.
        let request_body = self.build_gemini_request(context, options);
        let request_body = gemini::reasoning::prepare_request(&self.model, options, &request_body)?;
        let mut request = self.client.post(&url).header("Accept", "text/event-stream");
        if let Some(headers) = self
            .compat
            .as_ref()
            .and_then(|compat| compat.custom_headers.as_ref())
        {
            request = super::apply_headers_ignoring_blank_auth_overrides(
                request,
                headers,
                &["authorization"],
            );
        }
        request = super::apply_headers_ignoring_blank_auth_overrides(
            request,
            &options.headers,
            &["authorization"],
        );
        request = request.header("Authorization", authorization);

        let rewritten_body = super::offer_before_provider_request(
            options,
            self.name(),
            self.api(),
            self.model_id(),
            &url,
            &request_body,
            |value| super::validate_streamed_json_rewrite(value, &[], &["contents"], &[]),
        )
        .await;
        let request = match &rewritten_body {
            Some(body) => request.json(body)?,
            None => request.json(&request_body)?,
        };

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

        // Google-native Vertex shares the exact decoder, content lifecycle,
        // usage state and terminal-error handling with the other Google routes.
        let state = StreamState::new(
            SseStream::new(response.bytes_stream()),
            self.model.clone(),
            self.api().to_string(),
            self.name().to_string(),
        );
        Ok(Box::pin(state.into_stream(false)))
    }
}

/// Resolve only Google-scoped credentials, with case-insensitive non-empty
/// request headers taking precedence over compatibility headers and tokens.
fn vertex_authorization(
    options: &StreamOptions,
    compat: Option<&CompatConfig>,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Result<String> {
    let explicit =
        super::first_non_empty_header_value_case_insensitive(&options.headers, &["authorization"])
            .or_else(|| {
                compat
                    .and_then(|compat| compat.custom_headers.as_ref())
                    .and_then(|headers| {
                        super::first_non_empty_header_value_case_insensitive(
                            headers,
                            &["authorization"],
                        )
                    })
            });
    if let Some(authorization) = explicit {
        return Ok(authorization);
    }
    let non_empty = |value: String| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    let token = options
        .api_key
        .clone()
        .and_then(non_empty)
        .or_else(|| env_lookup("GOOGLE_CLOUD_API_KEY").and_then(non_empty))
        .or_else(|| env_lookup("VERTEX_API_KEY").and_then(non_empty))
        .ok_or_else(|| {
            Error::provider(
                "google-vertex",
                "Missing Vertex AI access token. Configure Google credentials, an Authorization header, or GOOGLE_CLOUD_API_KEY / VERTEX_API_KEY.",
            )
        })?;
    Ok(format!("Bearer {token}"))
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

    // Keep the complete region (us-east5, not just us), and never infer a
    // location from an unrelated custom hostname. Explicit path fields below
    // still take precedence over this host-derived fallback.
    let location_from_host = url::Url::parse(base_url).ok().and_then(|url| {
        let host = url.host_str()?;
        if host == "aiplatform.googleapis.com" {
            return Some("global".to_string());
        }
        host.strip_suffix("-aiplatform.googleapis.com")
            .filter(|location| !location.is_empty())
            .map(ToString::to_string)
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
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-2.0-flash:streamGenerateContent?alt=sse"
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

    /// gh #213 (truncated streams): a transport close before any chunk with
    /// `finishReason` must be an error, never a `Done` that commits the
    /// partial text as a clean stop.
    #[test]
    fn test_stream_eof_before_finish_reason_is_an_error() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let error =
            runtime.block_on(async move {
                let event = serde_json::json!({
                    "candidates": [{"content": {"parts": [{"text": "partial"}]}}]
                });
                let byte_stream = stream::iter(vec![Ok(format!("data: {event}\n\n").into_bytes())]);
                let mut state = StreamState::new(
                    crate::sse::SseStream::new(Box::pin(byte_stream)),
                    "gemini-test".to_string(),
                    "google-vertex".to_string(),
                    "google-vertex".to_string(),
                );
                while let Some(item) = state.event_source.next().await {
                    let msg = item.expect("SSE event");
                    state.process_event(&msg.data).expect("process_event");
                }
                assert!(state.pending_events.iter().any(
                    |e| matches!(e, StreamEvent::TextDelta { delta, .. } if delta == "partial")
                ));
                state.finish_at_eof().expect_err("EOF without finishReason")
            });
        let text = error.to_string();
        assert!(text.contains("unexpected EOF"), "{text}");
        assert!(
            crate::error::is_retryable_error(&text, None, None),
            "{text}"
        );
    }

    #[test]
    fn test_blocked_prompt_is_terminal_error() {
        let events = vec![serde_json::json!({
            "promptFeedback": {"blockReason": "PROHIBITED_CONTENT"}
        })];
        let stream_events = collect_events(&events);
        let Some(StreamEvent::Done { reason, message }) = stream_events.last() else {
            panic!("expected Done: {stream_events:?}");
        };
        assert_eq!(*reason, StopReason::Error);
        assert_eq!(
            message.error_message.as_deref(),
            Some("Vertex AI blocked the prompt: PROHIBITED_CONTENT")
        );
    }

    #[test]
    fn signed_tool_call_survives_vertex_stream_session_and_replay() {
        let events = [
            serde_json::json!({"candidates": [{"content": {"parts": [{
                "functionCall": {"name": "read", "args": {"path": "a.txt"}},
                "thoughtSignature": "dmVydGV4"
            }]}}]}),
            serde_json::json!({"candidates": [{"finishReason": "STOP"}]}),
        ];
        let stream_events = collect_events(&events);
        let call = stream_events
            .iter()
            .find_map(|event| match event {
                StreamEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
                _ => None,
            })
            .expect("completed tool call");
        assert_eq!(call.thought_signature.as_deref(), Some("dmVydGV4"));
        let Some(StreamEvent::Done { reason, message }) = stream_events.last() else {
            panic!("expected Done");
        };
        assert_eq!(*reason, StopReason::ToolUse);
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        let stored = serde_json::to_string(&Message::assistant(message.clone())).unwrap();
        let replay: Message = serde_json::from_str(&stored).unwrap();
        let context = Context::owned(
            None,
            vec![
                Message::User(crate::model::UserMessage {
                    content: UserContent::Text("Read a.txt".to_string()),
                    timestamp: 0,
                }),
                replay,
                Message::tool_result(crate::model::ToolResultMessage {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    content: vec![ContentBlock::Text(TextContent::new("contents"))],
                    details: None,
                    is_error: false,
                    timestamp: 1,
                }),
            ],
            Vec::new(),
        );
        let provider = VertexProvider::new("gemini-3-pro");
        let wire = serde_json::to_value(
            provider.build_gemini_request(&context, &StreamOptions::default()),
        )
        .unwrap();
        assert_eq!(
            wire["contents"][1]["parts"][0]["thoughtSignature"],
            "dmVydGV4"
        );
        assert_eq!(
            wire["contents"][1]["parts"][0]["functionCall"]["args"],
            serde_json::json!({"path": "a.txt"})
        );
        assert_eq!(
            wire["contents"][2]["parts"][0]["functionResponse"]["name"],
            "read"
        );
    }

    #[test]
    fn vertex_terminal_failure_is_preserved_with_a_tool_call() {
        for (finish, expected) in [
            ("SAFETY", StopReason::Error),
            ("MAX_TOKENS", StopReason::Length),
        ] {
            let events = [serde_json::json!({"candidates": [{
                "content": {"parts": [{"functionCall": {"name": "read", "args": {}}}]},
                "finishReason": finish
            }]})];
            let stream_events = collect_events(&events);
            let Some(StreamEvent::Done { reason, message }) = stream_events.last() else {
                panic!("expected Done");
            };
            assert_eq!(*reason, expected);
            assert_eq!(message.stop_reason, expected);
        }
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
                        out.push(state.finish_at_eof().expect("terminal chunk seen"));
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
