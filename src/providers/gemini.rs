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
use crate::models::CompatConfig;
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
const GOOGLE_GEMINI_CLI_BASE: &str = "https://cloudcode-pa.googleapis.com";
const GOOGLE_ANTIGRAVITY_BASE: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 8192;

fn authorization_override(
    options: &StreamOptions,
    compat: Option<&CompatConfig>,
) -> Option<String> {
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
        })
}

fn google_api_key_override(
    options: &StreamOptions,
    compat: Option<&CompatConfig>,
) -> Option<String> {
    super::first_non_empty_header_value_case_insensitive(&options.headers, &["x-goog-api-key"])
        .or_else(|| {
            compat
                .and_then(|compat| compat.custom_headers.as_ref())
                .and_then(|headers| {
                    super::first_non_empty_header_value_case_insensitive(
                        headers,
                        &["x-goog-api-key"],
                    )
                })
        })
}

// ============================================================================
// Gemini Provider
// ============================================================================

/// Google Gemini API provider.
pub struct GeminiProvider {
    client: Client,
    model: String,
    base_url: String,
    provider: String,
    api: String,
    google_cli_mode: bool,
    compat: Option<CompatConfig>,
}

impl GeminiProvider {
    /// Create a new Gemini provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            base_url: GEMINI_API_BASE.to_string(),
            provider: "google".to_string(),
            api: "google-generative-ai".to_string(),
            google_cli_mode: false,
            compat: None,
        }
    }

    /// Override provider name reported in streamed events.
    #[must_use]
    pub fn with_provider_name(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    /// Override API identifier reported in streamed events.
    #[must_use]
    pub fn with_api_name(mut self, api: impl Into<String>) -> Self {
        self.api = api.into();
        self
    }

    /// Enable Google Cloud Code Assist mode (`google-gemini-cli` / `google-antigravity`).
    #[must_use]
    pub const fn with_google_cli_mode(mut self, enabled: bool) -> Self {
        self.google_cli_mode = enabled;
        self
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

    /// Attach provider-specific compatibility overrides.
    #[must_use]
    pub fn with_compat(mut self, compat: Option<CompatConfig>) -> Self {
        self.compat = compat;
        self
    }

    /// Build the streaming URL.
    pub fn streaming_url(&self) -> String {
        let base = {
            let trimmed = self.base_url.trim();
            if trimmed.is_empty() {
                if self.google_cli_mode {
                    if self.provider.eq_ignore_ascii_case("google-antigravity") {
                        GOOGLE_ANTIGRAVITY_BASE
                    } else {
                        GOOGLE_GEMINI_CLI_BASE
                    }
                } else {
                    GEMINI_API_BASE
                }
            } else {
                trimmed
            }
        };
        if self.google_cli_mode {
            format!("{base}/v1internal:streamGenerateContent?alt=sse")
        } else {
            format!("{base}/models/{}:streamGenerateContent?alt=sse", self.model)
        }
    }

    /// Build the request body for the Gemini API.
    #[allow(clippy::unused_self)]
    pub fn build_request(&self, context: &Context<'_>, options: &StreamOptions) -> GeminiRequest {
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
                function_declarations: context.tools.iter().map(convert_tool_to_gemini).collect(),
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
                max_output_tokens: options.max_tokens.or(Some(DEFAULT_MAX_TOKENS)),
                temperature: options.temperature,
                candidate_count: Some(1),
            }),
        }
    }

    /// Build the contents array from context messages.
    fn build_contents(context: &Context<'_>) -> Vec<GeminiContent> {
        let mut contents = Vec::with_capacity(context.messages.len());

        for message in context.messages.iter() {
            contents.extend(convert_message_to_gemini(message));
        }

        contents
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CloudCodeAssistRequest {
    project: String,
    model: String,
    request: GeminiRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_type: Option<String>,
    user_agent: String,
    request_id: String,
}

fn build_google_cli_request(
    model_id: &str,
    project_id: &str,
    request: GeminiRequest,
    is_antigravity: bool,
) -> std::result::Result<CloudCodeAssistRequest, &'static str> {
    let safe_project = project_id.trim();
    if safe_project.is_empty() {
        return Err(
            "Missing Google Cloud project ID for Gemini CLI. Set GOOGLE_CLOUD_PROJECT (or configure gcloud) and re-authenticate with /login google-gemini-cli.",
        );
    }
    let project = if safe_project.starts_with("projects/") {
        safe_project.to_string()
    } else {
        format!("projects/{safe_project}/locations/global")
    };
    Ok(CloudCodeAssistRequest {
        project,
        model: model_id.to_string(),
        request,
        request_type: is_antigravity.then(|| "agent".to_string()),
        user_agent: if is_antigravity {
            "antigravity".to_string()
        } else {
            "pi-coding-agent".to_string()
        },
        request_id: format!(
            "{}-{}",
            if is_antigravity { "agent" } else { "pi" },
            uuid::Uuid::new_v4().simple()
        ),
    })
}

fn decode_project_scoped_access_payload(payload: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let token = value
        .get("token")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let project_id = value
        .get("projectId")
        .or_else(|| value.get("project_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    Some((token, project_id))
}

#[async_trait]
impl Provider for GeminiProvider {
    fn name(&self) -> &str {
        &self.provider
    }

    fn api(&self) -> &str {
        &self.api
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
        let request_body = self.build_request(context, options);
        let url = self.streaming_url();

        // Build request (Content-Type set by .json() below)
        let mut request = self.client.post(&url).header("Accept", "text/event-stream");

        if self.google_cli_mode {
            let api_payload = options.api_key.clone().ok_or_else(|| {
                Error::provider(
                    self.name(),
                    "Google Gemini CLI requires OAuth credentials. Run /login google-gemini-cli.",
                )
            })?;
            let (access_token, project_id) = decode_project_scoped_access_payload(&api_payload)
                .ok_or_else(|| {
                    Error::provider(
                        self.name(),
                        "Invalid Google Gemini CLI OAuth payload (expected JSON {token, projectId}). Run /login google-gemini-cli again.",
                    )
                })?;
            let is_antigravity = self.provider.eq_ignore_ascii_case("google-antigravity");

            // Build dynamic identity strings from the platform module.
            let platform = crate::platform::platform_tag();
            let pi_version = crate::platform::VERSION;
            let client_metadata = format!(
                r#"{{"ideType":"CLI","platform":"{}","pluginType":"GEMINI"}}"#,
                crate::platform::os_name().to_ascii_uppercase(),
            );
            let api_client_tag = format!("pi-agent-rust/{pi_version}");

            request = request
                .header("Authorization", format!("Bearer {access_token}"))
                .header("Content-Type", "application/json")
                .header("x-goog-api-client", api_client_tag)
                .header("client-metadata", client_metadata);

            if is_antigravity {
                let antigravity_version = std::env::var("PI_AI_ANTIGRAVITY_VERSION")
                    .unwrap_or_else(|_| pi_version.to_string());
                request = request.header(
                    "User-Agent",
                    format!("antigravity/{antigravity_version} {platform}"),
                );
            } else {
                request = request.header("User-Agent", crate::platform::pi_user_agent());
            }

            // Apply provider-specific custom headers from compat config.
            if let Some(compat) = &self.compat
                && let Some(custom_headers) = &compat.custom_headers
            {
                request = super::apply_headers_ignoring_blank_auth_overrides(
                    request,
                    custom_headers,
                    &["authorization", "x-goog-api-key"],
                );
            }

            // Per-request headers from StreamOptions (highest priority).
            request = super::apply_headers_ignoring_blank_auth_overrides(
                request,
                &options.headers,
                &["authorization", "x-goog-api-key"],
            );

            let cli_request =
                build_google_cli_request(&self.model, &project_id, request_body, is_antigravity)
                    .map_err(|message| Error::provider(self.name(), message.to_string()))?;
            let request = request.json(&cli_request)?;
            let response = Box::pin(request.send()).await?;
            let status = response.status();
            if !(200..300).contains(&status) {
                let body = response
                    .text()
                    .await
                    .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
                return Err(Error::provider(
                    self.name(),
                    format!("Gemini CLI API error (HTTP {status}): {body}"),
                ));
            }

            // Create SSE stream for streaming responses.
            let event_source = SseStream::new(response.bytes_stream());
            let model = self.model.clone();
            let api = self.api().to_string();
            let provider = self.name().to_string();
            let cloud_cli_mode = self.google_cli_mode;

            let stream = stream::unfold(
                StreamState::new(event_source, model, api, provider),
                move |mut state| async move {
                    if state.finished {
                        return None;
                    }
                    loop {
                        // Drain pending events before polling for more SSE data
                        if let Some(event) = state.pending_events.pop_front() {
                            return Some((Ok(event), state));
                        }

                        match state.event_source.next().await {
                            Some(Ok(msg)) => {
                                state.transient_error_count = 0;
                                if msg.event == "ping" {
                                    continue;
                                }

                                let processing = if cloud_cli_mode {
                                    state.process_cloud_code_event(&msg.data)
                                } else {
                                    state.process_event(&msg.data)
                                };
                                if let Err(e) = processing {
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
                                    if state.transient_error_count
                                        <= MAX_CONSECUTIVE_TRANSIENT_ERRORS
                                    {
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
                                // Stream ended naturally
                                state.finished = true;
                                let reason = state.partial.stop_reason;
                                let message = std::mem::take(&mut state.partial);
                                return Some((Ok(StreamEvent::Done { reason, message }), state));
                            }
                        }
                    }
                },
            );

            return Ok(Box::pin(stream));
        }

        let has_auth_override = google_api_key_override(options, self.compat.as_ref()).is_some()
            || authorization_override(options, self.compat.as_ref()).is_some();
        let auth_value = if has_auth_override {
            None
        } else {
            Some(
                options
                    .api_key
                    .clone()
                    .or_else(|| std::env::var("GOOGLE_API_KEY").ok())
                    .or_else(|| std::env::var("GEMINI_API_KEY").ok())
                    .ok_or_else(|| {
                        Error::provider(
                            self.name(),
                            "Missing API key for provider. Configure credentials with /login <provider> or set the provider's API key env var.",
                        )
                    })?,
            )
        };

        if let Some(auth_value) = auth_value {
            request = request.header("x-goog-api-key", &auth_value);
        }

        // Apply provider-specific custom headers from compat config.
        if let Some(compat) = &self.compat
            && let Some(custom_headers) = &compat.custom_headers
        {
            request = super::apply_headers_ignoring_blank_auth_overrides(
                request,
                custom_headers,
                &["authorization", "x-goog-api-key"],
            );
        }

        // Per-request headers from StreamOptions (highest priority).
        request = super::apply_headers_ignoring_blank_auth_overrides(
            request,
            &options.headers,
            &["authorization", "x-goog-api-key"],
        );

        let request = request.json(&request_body)?;

        let response = Box::pin(request.send()).await?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let body = response
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
            return Err(Error::provider(
                self.name(),
                format!("Gemini API error (HTTP {status}): {body}"),
            ));
        }

        // Create SSE stream for streaming responses.
        let event_source = SseStream::new(response.bytes_stream());

        // Create stream state
        let model = self.model.clone();
        let api = self.api().to_string();
        let provider = self.name().to_string();
        let cloud_cli_mode = self.google_cli_mode;

        let stream = stream::unfold(
            StreamState::new(event_source, model, api, provider),
            move |mut state| async move {
                if state.finished {
                    return None;
                }
                loop {
                    // Drain pending events before polling for more SSE data
                    if let Some(event) = state.pending_events.pop_front() {
                        return Some((Ok(event), state));
                    }

                    match state.event_source.next().await {
                        Some(Ok(msg)) => {
                            state.transient_error_count = 0;
                            if msg.event == "ping" {
                                continue;
                            }

                            let processing = if cloud_cli_mode {
                                state.process_cloud_code_event(&msg.data)
                            } else {
                                state.process_event(&msg.data)
                            };
                            if let Err(e) = processing {
                                state.finished = true;
                                return Some((Err(e), state));
                            }
                        }
                        Some(Err(e)) => {
                            const MAX_CONSECUTIVE_WRITE_ZERO: usize = 5;
                            if e.kind() == std::io::ErrorKind::WriteZero {
                                state.transient_error_count += 1;
                                if state.transient_error_count <= MAX_CONSECUTIVE_WRITE_ZERO {
                                    tracing::warn!(
                                        count = state.transient_error_count,
                                        "Transient WriteZero error in SSE stream, continuing"
                                    );
                                    continue;
                                }
                                tracing::warn!(
                                    "WriteZero error persisted after {MAX_CONSECUTIVE_WRITE_ZERO} \
                                     consecutive attempts, treating as fatal"
                                );
                            }
                            state.finished = true;
                            let err = Error::sse(&e);
                            return Some((Err(err), state));
                        }
                        None => {
                            // Stream ended naturally
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
// Stream State
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
        self.process_response(response)
    }

    fn process_response(&mut self, response: GeminiStreamResponse) -> Result<()> {
        // Handle usage metadata
        if let Some(metadata) = response.usage_metadata {
            self.partial.usage.input = metadata.prompt_token_count.unwrap_or(0);
            self.partial.usage.output = metadata.candidates_token_count.unwrap_or(0);
            self.partial.usage.total_tokens = metadata.total_token_count.unwrap_or(0);
        }

        // Process candidates
        if let Some(candidates) = response.candidates
            && let Some(candidate) = candidates.into_iter().next()
        {
            self.process_candidate(candidate)?;
        }

        Ok(())
    }

    fn process_cloud_code_event(&mut self, data: &str) -> Result<()> {
        let wrapped: CloudCodeAssistResponseChunk = serde_json::from_str(data)
            .map_err(|e| Error::api(format!("JSON parse error: {e}\nData: {data}")))?;
        let Some(response) = wrapped.response else {
            return Ok(());
        };
        self.process_response(GeminiStreamResponse {
            candidates: response.candidates,
            usage_metadata: response.usage_metadata,
        })
    }

    #[allow(clippy::unnecessary_wraps)]
    fn process_candidate(&mut self, candidate: GeminiCandidate) -> Result<()> {
        let has_finish_reason = candidate.finish_reason.is_some();

        // Handle finish reason
        if let Some(reason) = candidate.finish_reason.as_deref() {
            self.partial.stop_reason = match reason {
                "MAX_TOKENS" => StopReason::Length,
                "SAFETY" | "RECITATION" | "OTHER" => StopReason::Error,
                "FUNCTION_CALL" => StopReason::ToolUse,
                // STOP and any other reason treated as normal stop
                _ => StopReason::Stop,
            };
        }

        // Process content parts — queue all events into pending_events
        if let Some(content) = candidate.content {
            for part in content.parts {
                match part {
                    GeminiPart::Text { text } => {
                        // Accumulate text into partial
                        let last_is_text =
                            matches!(self.partial.content.last(), Some(ContentBlock::Text(_)));

                        // Ensure Start is emitted before any TextStart/TextDelta events
                        // so downstream consumers see the correct event order:
                        // Start → TextStart → TextDelta
                        self.ensure_started();

                        let content_index = if last_is_text {
                            self.partial.content.len() - 1
                        } else {
                            let idx = self.partial.content.len();
                            self.partial
                                .content
                                .push(ContentBlock::Text(TextContent::new("")));
                            self.pending_events
                                .push_back(StreamEvent::TextStart { content_index: idx });
                            idx
                        };

                        if let Some(ContentBlock::Text(t)) =
                            self.partial.content.get_mut(content_index)
                        {
                            t.text.push_str(&text);
                        }

                        self.pending_events.push_back(StreamEvent::TextDelta {
                            content_index,
                            delta: text,
                        });
                    }
                    GeminiPart::FunctionCall { function_call } => {
                        // Generate a unique ID for this tool call
                        let id = format!("call_{}", uuid::Uuid::new_v4().simple());

                        // Serialize args for the delta event
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

                        // Update stop reason for tool use
                        self.partial.stop_reason = StopReason::ToolUse;

                        self.ensure_started();

                        // Emit full ToolCallStart → ToolCallDelta → ToolCallEnd sequence
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
                        // InlineData/FunctionResponse are for input, not output.
                        // Unknown parts are silently skipped so new Gemini API
                        // features don't break existing streams.
                    }
                }
            }
        }

        // Emit TextEnd/ThinkingEnd for all open text/thinking blocks (not just the last
        // one, since text/thinking may precede tool calls).
        if has_finish_reason {
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
// Gemini API Types
// ============================================================================

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiRequest {
    pub(crate) contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tools: Option<Vec<GeminiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_config: Option<GeminiToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) generation_config: Option<GeminiGenerationConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) role: Option<String>,
    pub(crate) parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum GeminiPart {
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
    /// Catch-all for unrecognized part types (e.g. `executableCode`,
    /// `codeExecutionResult`) so that new Gemini API features don't
    /// cause hard deserialization failures that terminate the stream.
    Unknown(serde_json::Value),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiBlob {
    pub(crate) mime_type: String,
    pub(crate) data: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct GeminiFunctionCall {
    pub(crate) name: String,
    pub(crate) args: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct GeminiFunctionResponse {
    pub(crate) name: String,
    pub(crate) response: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiTool {
    pub(crate) function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GeminiFunctionDeclaration {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiToolConfig {
    pub(crate) function_calling_config: GeminiFunctionCallingConfig,
}

#[derive(Debug, Serialize)]
pub(crate) struct GeminiFunctionCallingConfig {
    pub(crate) mode: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) candidate_count: Option<u32>,
}

// ============================================================================
// Streaming Response Types
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiStreamResponse {
    #[serde(default)]
    pub(crate) candidates: Option<Vec<GeminiCandidate>>,
    #[serde(default)]
    pub(crate) usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloudCodeAssistResponseChunk {
    #[serde(default)]
    response: Option<CloudCodeAssistResponse>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloudCodeAssistResponse {
    #[serde(default)]
    candidates: Option<Vec<GeminiCandidate>>,
    #[serde(default)]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiCandidate {
    #[serde(default)]
    pub(crate) content: Option<GeminiContent>,
    #[serde(default)]
    pub(crate) finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_field_names)]
pub(crate) struct GeminiUsageMetadata {
    #[serde(default)]
    pub(crate) prompt_token_count: Option<u64>,
    #[serde(default)]
    pub(crate) candidates_token_count: Option<u64>,
    #[serde(default)]
    pub(crate) total_token_count: Option<u64>,
}

// ============================================================================
// Conversion Functions
// ============================================================================

pub(crate) fn convert_message_to_gemini(message: &Message) -> Vec<GeminiContent> {
    match message {
        Message::User(user) => vec![GeminiContent {
            role: Some("user".into()),
            parts: convert_user_content_to_parts(&user.content),
        }],
        Message::Custom(custom) => vec![GeminiContent {
            role: Some("user".into()),
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
                    ContentBlock::Thinking(_)
                    | ContentBlock::Image(_)
                    | ContentBlock::RedactedThinking(_) => {
                        // Anthropic-shaped thinking blocks (including redacted
                        // markers) and image blocks have no Gemini equivalent.
                    }
                }
            }

            if parts.is_empty() {
                return Vec::new();
            }

            vec![GeminiContent {
                role: Some("model".into()),
                parts,
            }]
        }
        Message::ToolResult(result) => {
            // Gemini expects function responses as user role with functionResponse part
            let content_text = result
                .content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text(t) => t.text.clone(),
                    ContentBlock::Image(img) => format!("[Image ({}) omitted]", img.mime_type),
                    _ => String::new(),
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n");

            let response_value = if result.is_error {
                serde_json::json!({ "error": content_text })
            } else {
                serde_json::json!({ "result": content_text })
            };

            vec![GeminiContent {
                role: Some("user".into()),
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

pub(crate) fn convert_user_content_to_parts(content: &UserContent) -> Vec<GeminiPart> {
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

pub(crate) fn convert_tool_to_gemini(tool: &ToolDef) -> GeminiFunctionDeclaration {
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
        let url = provider.streaming_url();
        assert!(url.contains("gemini-2.0-flash"));
        assert!(url.contains("streamGenerateContent"));
        assert!(!url.contains("key="));
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

    #[derive(Debug)]
    struct CapturedRequest {
        headers: HashMap<String, String>,
        body: String,
    }

    #[test]
    fn test_stream_compat_google_api_key_header_works_without_api_key() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let mut custom_headers = HashMap::new();
        custom_headers.insert(
            "x-goog-api-key".to_string(),
            "compat-google-key".to_string(),
        );
        let provider = GeminiProvider::new("gemini-2.0-flash")
            .with_base_url(base_url)
            .with_compat(Some(CompatConfig {
                custom_headers: Some(custom_headers),
                ..CompatConfig::default()
            }));
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider
                .stream(&context, &StreamOptions::default())
                .await
                .expect("stream");
            while let Some(event) = stream.next().await {
                if matches!(event.expect("stream event"), StreamEvent::Done { .. }) {
                    break;
                }
            }
        });

        let captured = rx.recv_timeout(Duration::from_secs(2)).expect("captured");
        assert_eq!(
            captured.headers.get("x-goog-api-key").map(String::as_str),
            Some("compat-google-key")
        );
        let body: Value = serde_json::from_str(&captured.body).expect("body json");
        assert_eq!(body["contents"][0]["role"], "user");
    }

    #[test]
    fn test_stream_option_authorization_header_works_without_api_key() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = GeminiProvider::new("gemini-2.0-flash").with_base_url(base_url);
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let mut headers = HashMap::new();
        headers.insert(
            "Authorization".to_string(),
            "Bearer compat-gemini-token".to_string(),
        );

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider
                .stream(
                    &context,
                    &StreamOptions {
                        headers,
                        ..Default::default()
                    },
                )
                .await
                .expect("stream");
            while let Some(event) = stream.next().await {
                if matches!(event.expect("stream event"), StreamEvent::Done { .. }) {
                    break;
                }
            }
        });

        let captured = rx.recv_timeout(Duration::from_secs(2)).expect("captured");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer compat-gemini-token")
        );
        let body: Value = serde_json::from_str(&captured.body).expect("body json");
        assert_eq!(body["contents"][0]["role"], "user");
    }

    #[test]
    fn test_blank_request_google_api_key_header_does_not_override_builtin_api_key() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = GeminiProvider::new("gemini-2.0-flash").with_base_url(base_url);
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let mut headers = HashMap::new();
        headers.insert("X-Goog-Api-Key".to_string(), "   ".to_string());
        let options = StreamOptions {
            api_key: Some("fallback-google-key".to_string()),
            headers,
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

        let captured = rx.recv_timeout(Duration::from_secs(2)).expect("captured");
        assert_eq!(
            captured.headers.get("x-goog-api-key").map(String::as_str),
            Some("fallback-google-key")
        );
        let body: Value = serde_json::from_str(&captured.body).expect("body json");
        assert_eq!(body["contents"][0]["role"], "user");
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

    fn success_sse_body() -> String {
        [
            r#"data: {"candidates":[{"content":{"parts":[{"text":"ok"}]}}]}"#,
            "",
            r#"data: {"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#,
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
                    Err(err) => panic!("{err}"),
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
                    Err(err) => panic!("{err}"),
                }
            }

            tx.send(CapturedRequest {
                headers,
                body: String::from_utf8_lossy(&request_body).to_string(),
            })
            .expect("send captured request");

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
            StopReason::PauseTurn => "pause_turn",
            StopReason::Refusal => "refusal",
            StopReason::Error => "error",
            StopReason::Aborted => "aborted",
        }
        .to_string()
    }

    // ─── Request body format tests ──────────────────────────────────────

    #[test]
    fn test_build_request_basic_text() {
        let provider = GeminiProvider::new("gemini-2.0-flash");
        let context = Context::owned(
            Some("You are helpful.".to_string()),
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("What is Rust?".to_string()),
                timestamp: 0,
            })],
            vec![],
        );
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
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Read a file".to_string()),
                timestamp: 0,
            })],
            vec![
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
        );
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
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("hi".to_string()),
                timestamp: 0,
            })],
            vec![],
        );
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
    fn test_streaming_url_no_key_query_param() {
        let provider = GeminiProvider::new("gemini-2.0-flash");
        let url = provider.streaming_url();

        // API key should NOT be in the query string.
        assert!(
            !url.contains("key="),
            "API key should not be in query param"
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
        let url = provider.streaming_url();

        assert!(url.starts_with("https://custom.example.com/v1/models/gemini-pro"));
        assert!(!url.contains("key="));
    }

    // ─── Content part mapping tests ─────────────────────────────────────

    #[test]
    fn test_convert_user_text_to_gemini_parts() {
        let parts = convert_user_content_to_parts(&UserContent::Text("hello world".to_string()));
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            GeminiPart::Text { text } => assert_eq!(text, "hello world"),
            _ => panic!(),
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
            _ => panic!(),
        }
        match &parts[1] {
            GeminiPart::InlineData { inline_data } => {
                assert_eq!(inline_data.mime_type, "image/png");
                assert_eq!(inline_data.data, "aGVsbG8=");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn test_convert_assistant_message_with_tool_call() {
        let message = Message::assistant(AssistantMessage {
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
            stop_details: None,
            error_message: None,
            timestamp: 0,
        });

        let converted = convert_message_to_gemini(&message);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, Some("model".to_string()));
        assert_eq!(converted[0].parts.len(), 2);

        match &converted[0].parts[0] {
            GeminiPart::Text { text } => assert_eq!(text, "Let me read that file."),
            _ => panic!(),
        }
        match &converted[0].parts[1] {
            GeminiPart::FunctionCall { function_call } => {
                assert_eq!(function_call.name, "read");
                assert_eq!(function_call.args["path"], "/tmp/test.txt");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn test_convert_assistant_empty_content_returns_empty() {
        let message = Message::assistant(AssistantMessage {
            content: vec![],
            api: "google".to_string(),
            provider: "google".to_string(),
            model: "gemini-2.0-flash".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        });

        let converted = convert_message_to_gemini(&message);
        assert!(converted.is_empty());
    }

    #[test]
    fn test_convert_tool_result_success() {
        let message = Message::tool_result(crate::model::ToolResultMessage {
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
            _ => panic!(),
        }
    }

    #[test]
    fn test_convert_tool_result_error() {
        let message = Message::tool_result(crate::model::ToolResultMessage {
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
            _ => panic!(),
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
            _ => panic!(),
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
        let context = Context::owned(
            Some("Be concise.".to_string()),
            vec![
                Message::User(crate::model::UserMessage {
                    content: UserContent::Text("Read /tmp/a.txt".to_string()),
                    timestamp: 0,
                }),
                Message::assistant(AssistantMessage {
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
                    stop_details: None,
                    error_message: None,
                    timestamp: 1,
                }),
                Message::tool_result(crate::model::ToolResultMessage {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "read".to_string(),
                    content: vec![ContentBlock::Text(TextContent::new("file contents"))],
                    details: None,
                    is_error: false,
                    timestamp: 2,
                }),
            ],
            vec![ToolDef {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
        );
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

    // ========================================================================
    // Proptest — process_event() fuzz coverage (FUZZ-P1.3)
    // ========================================================================

    mod proptest_process_event {
        use super::*;
        use proptest::prelude::*;

        fn make_state()
        -> StreamState<impl Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin>
        {
            let empty = stream::empty::<std::result::Result<Vec<u8>, std::io::Error>>();
            let sse = crate::sse::SseStream::new(Box::pin(empty));
            StreamState::new(
                sse,
                "gemini-test".into(),
                "google-generative".into(),
                "google".into(),
            )
        }

        fn small_string() -> impl Strategy<Value = String> {
            prop_oneof![Just(String::new()), "[a-zA-Z0-9_]{1,16}", "[ -~]{0,32}",]
        }

        fn token_count() -> impl Strategy<Value = u64> {
            prop_oneof![
                5 => 0u64..10_000u64,
                2 => Just(0u64),
                1 => Just(u64::MAX),
                1 => (u64::MAX - 100)..=u64::MAX,
            ]
        }

        fn finish_reason() -> impl Strategy<Value = Option<String>> {
            prop_oneof![
                3 => Just(None),
                1 => Just(Some("STOP".to_string())),
                1 => Just(Some("MAX_TOKENS".to_string())),
                1 => Just(Some("SAFETY".to_string())),
                1 => Just(Some("RECITATION".to_string())),
                1 => Just(Some("OTHER".to_string())),
                1 => small_string().prop_map(Some),
            ]
        }

        /// Generate a JSON `Value` representing a Gemini function call args object.
        fn json_args() -> impl Strategy<Value = serde_json::Value> {
            prop_oneof![
                Just(serde_json::json!({})),
                Just(serde_json::json!({"key": "value"})),
                Just(serde_json::json!({"a": 1, "b": true, "c": null})),
                small_string().prop_map(|s| serde_json::json!({"input": s})),
            ]
        }

        /// Strategy for Gemini text parts.
        fn text_part() -> impl Strategy<Value = serde_json::Value> {
            small_string().prop_map(|t| serde_json::json!({"text": t}))
        }

        /// Strategy for Gemini function call parts.
        fn function_call_part() -> impl Strategy<Value = serde_json::Value> {
            (small_string(), json_args()).prop_map(
                |(name, args)| serde_json::json!({"functionCall": {"name": name, "args": args}}),
            )
        }

        /// Strategy for content parts (mix of text and function calls).
        fn parts_strategy() -> impl Strategy<Value = Vec<serde_json::Value>> {
            prop::collection::vec(
                prop_oneof![3 => text_part(), 1 => function_call_part(),],
                0..5,
            )
        }

        /// Generate valid `GeminiStreamResponse` JSON strings.
        fn gemini_response_json() -> impl Strategy<Value = String> {
            prop_oneof![
                // Text response with candidate
                3 => (parts_strategy(), finish_reason()).prop_map(|(parts, fr)| {
                    let mut candidate = serde_json::json!({
                        "content": {"parts": parts}
                    });
                    if let Some(r) = fr {
                        candidate["finishReason"] = serde_json::Value::String(r);
                    }
                    serde_json::json!({"candidates": [candidate]}).to_string()
                }),
                // Usage-only response
                2 => (token_count(), token_count(), token_count()).prop_map(|(p, c, t)| {
                    serde_json::json!({
                        "usageMetadata": {
                            "promptTokenCount": p,
                            "candidatesTokenCount": c,
                            "totalTokenCount": t
                        }
                    })
                    .to_string()
                }),
                // Empty candidates
                1 => Just(r#"{"candidates":[]}"#.to_string()),
                // No candidates, no usage
                1 => Just(r"{}".to_string()),
                // Candidate with finish reason only (no content)
                1 => finish_reason()
                    .prop_filter_map("some reason", |fr| fr)
                    .prop_map(|reason| {
                        serde_json::json!({
                            "candidates": [{"finishReason": reason}]
                        })
                        .to_string()
                    }),
                // Both candidate and usage
                2 => (parts_strategy(), finish_reason(), token_count(), token_count(), token_count())
                    .prop_map(|(parts, fr, p, c, t)| {
                        let mut candidate = serde_json::json!({
                            "content": {"parts": parts}
                        });
                        if let Some(r) = fr {
                            candidate["finishReason"] = serde_json::Value::String(r);
                        }
                        serde_json::json!({
                            "candidates": [candidate],
                            "usageMetadata": {
                                "promptTokenCount": p,
                                "candidatesTokenCount": c,
                                "totalTokenCount": t
                            }
                        })
                        .to_string()
                    }),
            ]
        }

        /// Chaos — arbitrary JSON strings.
        fn chaos_json() -> impl Strategy<Value = String> {
            prop_oneof![
                Just(String::new()),
                Just("{}".to_string()),
                Just("[]".to_string()),
                Just("null".to_string()),
                Just("{".to_string()),
                Just(r#"{"candidates":"not_array"}"#.to_string()),
                Just(r#"{"candidates":[{"content":null}]}"#.to_string()),
                Just(r#"{"candidates":[{"content":{"parts":"not_array"}}]}"#.to_string()),
                "[ -~]{0,64}",
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 256,
                max_shrink_iters: 100,
                .. ProptestConfig::default()
            })]

            #[test]
            fn process_event_valid_never_panics(data in gemini_response_json()) {
                let mut state = make_state();
                let _ = state.process_event(&data);
            }

            #[test]
            fn process_event_chaos_never_panics(data in chaos_json()) {
                let mut state = make_state();
                let _ = state.process_event(&data);
            }

            #[test]
            fn process_event_sequence_never_panics(
                events in prop::collection::vec(gemini_response_json(), 1..8)
            ) {
                let mut state = make_state();
                for event in &events {
                    let _ = state.process_event(event);
                }
            }

            #[test]
            fn process_event_mixed_sequence_never_panics(
                events in prop::collection::vec(
                    prop_oneof![gemini_response_json(), chaos_json()],
                    1..12
                )
            ) {
                let mut state = make_state();
                for event in &events {
                    let _ = state.process_event(event);
                }
            }
        }
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

    /// Opaque wrapper around the Gemini stream processor state.
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
                "gemini-fuzz".into(),
                "google-generative".into(),
                "google".into(),
            ))
        }

        /// Feed one SSE data payload and return any emitted `StreamEvent`s.
        pub fn process_event(&mut self, data: &str) -> crate::error::Result<Vec<StreamEvent>> {
            self.0.process_event(data)?;
            Ok(self.0.pending_events.drain(..).collect())
        }
    }
}
