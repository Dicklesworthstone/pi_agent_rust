//! Google Gemini API provider implementation.
//!
//! This module implements the Provider trait for the Google Gemini API,
//! supporting streaming responses and function calling (tool use).

use crate::error::{Error, Result};
use crate::http::client::{Client, RequestBuilder};
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, ToolCall, ToolResultMessage,
    Usage, UserContent,
};
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use crate::sse::SseStream;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;

mod files;
pub(super) mod reasoning;
mod wire;

// ============================================================================
// Constants
// ============================================================================

const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const GOOGLE_GEMINI_CLI_BASE: &str = "https://cloudcode-pa.googleapis.com";
const GOOGLE_ANTIGRAVITY_BASE: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";
const MAX_GENERATION_ERROR_BYTES: usize = 64 * 1024;
const MAX_READY_EVENTS_PER_YIELD: usize = 64;
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
    files: files::FileCache,
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
            files: files::FileCache::default(),
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
        self.files = files::FileCache::default();
        self
    }

    /// Attach provider-specific compatibility overrides.
    #[must_use]
    pub fn with_compat(mut self, compat: Option<CompatConfig>) -> Self {
        self.compat = compat;
        self
    }

    /// Rebuild a Developer API request without re-running extension hooks or
    /// resolving credentials again when one staged file needs replacement.
    fn developer_request(
        &self,
        url: &str,
        options: &StreamOptions,
        fallback_api_key: Option<&str>,
    ) -> RequestBuilder<'_> {
        let mut request = self.client.post(url).header("Accept", "text/event-stream");
        if let Some(key) = fallback_api_key {
            request = request.header("x-goog-api-key", key);
        }
        if let Some(compat) = &self.compat
            && let Some(custom_headers) = &compat.custom_headers
        {
            request = super::apply_headers_ignoring_blank_auth_overrides(
                request,
                custom_headers,
                &["authorization", "x-goog-api-key"],
            );
        }
        super::apply_headers_ignoring_blank_auth_overrides(
            request,
            &options.headers,
            &["authorization", "x-goog-api-key"],
        )
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

    /// Build the base request body. The live request additionally applies
    /// model-specific thinking controls before the request-rewrite hook.
    #[allow(clippy::unused_self)]
    pub fn build_request(&self, context: &Context<'_>, options: &StreamOptions) -> GeminiRequest {
        let contents = convert_messages_to_gemini(&context.messages);
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
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CloudCodeAssistRequest {
    project: String,
    model: String,
    request: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_type: Option<String>,
    user_agent: String,
    request_id: String,
}

fn build_google_cli_request(
    model_id: &str,
    project_id: &str,
    request: serde_json::Value,
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
        let request_body = reasoning::prepare_request(&self.model, options, &request_body)?;
        let url = self.streaming_url();

        if self.google_cli_mode {
            // Content-Type is set by .json() below.
            let mut request = self.client.post(&url).header("Accept", "text/event-stream");
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

            // Offer the inner Gemini request (not the CloudCodeAssist
            // wrapper) to the rewrite hook: the wrapper is transport detail
            // and its project/user-agent fields must stay host-controlled.
            let rewritten_inner = super::offer_before_provider_request(
                options,
                self.name(),
                self.api(),
                self.model_id(),
                &url,
                &request_body,
                |value| super::validate_streamed_json_rewrite(value, &[], &["contents"], &[]),
            )
            .await;
            let cli_request = build_google_cli_request(
                &self.model,
                &project_id,
                rewritten_inner.unwrap_or(request_body),
                is_antigravity,
            )
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

            let stream = StreamState::new(event_source, model, api, provider)
                .with_usage_observer(options.usage_observer.clone())
                .into_stream(cloud_cli_mode);

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

        let upload_auth =
            files::UploadAuth::for_request(options, self.compat.as_ref(), auth_value.as_deref());

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
        // Moving the fallback also drops its media copy when a rewrite won.
        let mut body = rewritten_body.unwrap_or(request_body);
        // Stage the final payload after extension rewrites. Session originals
        // stay inline and portable; remote file URIs are transport-only state.
        // Cloud Code Assist returned above; Vertex uses its separate provider.
        let mut prepared =
            Box::pin(
                self.files
                    .prepare(&self.client, &self.base_url, &upload_auth, &mut body),
            )
            .await?;
        let response = loop {
            let request = self
                .developer_request(&url, options, auth_value.as_deref())
                .json(&body)?;
            let response = Box::pin(request.send()).await?;
            let status = response.status();
            if (200..300).contains(&status) {
                break response;
            }
            let error_body = response
                .text_limited(MAX_GENERATION_ERROR_BYTES)
                .await
                .unwrap_or_else(|_| "<failed to read bounded error body>".to_string());
            // Recovery owns a single retry, and confirms stale Pi-staged file
            // resources before changing the request. Once an SSE response is
            // accepted, stream failures retain the normal agent retry policy.
            if let Some(prepared) = prepared.as_mut()
                && Box::pin(prepared.recover(
                    &self.client,
                    &upload_auth,
                    &mut body,
                    status,
                    &error_body,
                ))
                .await?
            {
                continue;
            }
            return Err(Error::provider(
                self.name(),
                format!("Gemini API error (HTTP {status}): {error_body}"),
            ));
        };

        // Create SSE stream for streaming responses.
        let event_source = SseStream::new(response.bytes_stream());

        // Create stream state
        let model = self.model.clone();
        let api = self.api().to_string();
        let provider = self.name().to_string();
        let cloud_cli_mode = self.google_cli_mode;

        let stream = StreamState::new(event_source, model, api, provider)
            .with_usage_observer(options.usage_observer.clone())
            .into_stream(cloud_cli_mode);

        Ok(Box::pin(stream))
    }
}

// ============================================================================
// Stream State
// ============================================================================

/// Heartbeats and usage-only frames do not produce public events. A ready
/// transport must still let the consumer's abort branch and sibling sessions
/// run; this yield remains owned by the response rather than a detached task.
async fn yield_stream_turn() {
    let mut yielded = false;
    std::future::poll_fn(|task| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            task.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}

/// Shared by Developer API, Cloud Code Assist and Google-native Vertex.
/// Keeping one parser also keeps signature boundaries and usage accounting
/// identical across the three transports.
pub(super) struct StreamState<S>
where
    S: Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin,
{
    pub(super) event_source: SseStream<S>,
    partial: AssistantMessage,
    pub(super) pending_events: VecDeque<StreamEvent>,
    started: bool,
    pub(super) finished: bool,
    /// Whether a chunk carried a terminal marker (`finishReason` on the
    /// candidate, or a `promptFeedback.blockReason`). Gemini's final chunk
    /// always carries one; a transport close without it is a truncated
    /// stream, not a complete answer.
    saw_terminal: bool,
    content_state: reasoning::ContentState,
    usage_state: reasoning::UsageState,
    usage_observer: Option<crate::provider::UsageObserver>,
}

impl<S> StreamState<S>
where
    S: Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin,
{
    pub(super) fn new(
        event_source: SseStream<S>,
        model: String,
        api: String,
        provider: String,
    ) -> Self {
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
            saw_terminal: false,
            content_state: reasoning::ContentState::default(),
            usage_state: reasoning::UsageState::default(),
            usage_observer: None,
        }
    }

    pub(super) fn with_usage_observer(
        mut self,
        observer: Option<crate::provider::UsageObserver>,
    ) -> Self {
        self.usage_observer = observer;
        self
    }

    /// The returned stream is fused: once it yields `None` every later poll
    /// also yields `None`, so a caller that repolls a terminal response sees
    /// EOF instead of `Unfold`'s poll-after-completion panic.
    pub(super) fn into_stream(
        self,
        cloud_cli_mode: bool,
    ) -> impl Stream<Item = Result<StreamEvent>> {
        stream::unfold(Some(self), move |active| async move {
            let mut state = active?;
            let mut ready_events = 0;
            loop {
                if let Some(event) = state.pending_events.pop_front() {
                    let active =
                        (!matches!(&event, StreamEvent::Done { .. } | StreamEvent::Error { .. }))
                            .then_some(state);
                    return Some((Ok(event), active));
                }
                if state.finished {
                    return None;
                }
                if ready_events == MAX_READY_EVENTS_PER_YIELD {
                    yield_stream_turn().await;
                    ready_events = 0;
                }
                ready_events += 1;
                match state.event_source.next().await {
                    Some(Ok(msg)) => {
                        if msg.event == "ping" {
                            continue;
                        }
                        let result = if cloud_cli_mode {
                            state.process_cloud_code_event(&msg.data)
                        } else {
                            state.process_event(&msg.data)
                        };
                        if let Err(error) = result {
                            return Some((Err(error), None));
                        }
                    }
                    Some(Err(error)) => {
                        // SseStream errors are terminal. Swallowing a timeout
                        // here would turn its next EOF into a clean success
                        // when a finish marker had already been observed.
                        return Some((Err(Error::sse(&error)), None));
                    }
                    None => {
                        let outcome = state.finish_at_eof();
                        // Keep reading through EOF for trailing usage and late
                        // signatures, then release the source and its owner
                        // before the consumer can observe the terminal result.
                        return Some((outcome, None));
                    }
                }
            }
        })
        .fuse()
    }

    /// Terminal outcome once the transport closes. A close without a
    /// terminal marker means the response was cut off mid-stream (proxy
    /// reset, idle timeout, dropped connection): surface a retryable error
    /// instead of committing the partial text as a clean `Stop`.
    pub(super) fn finish_at_eof(&mut self) -> Result<StreamEvent> {
        self.finished = true;
        if !self.saw_terminal {
            return Err(Error::api(format!(
                "{} stream ended before finishReason (unexpected EOF)",
                self.service_label()
            )));
        }
        // Gemini commonly sends STOP in a separate final chunk after the
        // function calls. It completes the model step, not the agent's turn.
        if self.partial.stop_reason == StopReason::Stop
            && self
                .partial
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolCall(_)))
        {
            self.partial.stop_reason = StopReason::ToolUse;
        }
        let reason = self.partial.stop_reason;
        let message = std::mem::take(&mut self.partial);
        Ok(StreamEvent::Done { reason, message })
    }

    fn service_label(&self) -> &'static str {
        if self.partial.api == "google-vertex" {
            "Vertex AI"
        } else {
            "Gemini"
        }
    }

    pub(super) fn process_event(&mut self, data: &str) -> Result<()> {
        let response: GeminiStreamResponse =
            serde_json::from_str(data).map_err(wire::response_parse_error)?;
        self.process_response(response)
    }

    fn process_response(&mut self, response: GeminiStreamResponse) -> Result<()> {
        // Usage chunks are cumulative and can omit individual counters.
        if let Some(metadata) = response.usage_metadata {
            self.usage_state.update(
                &mut self.partial.usage,
                metadata.prompt_token_count,
                metadata.candidates_token_count,
                metadata.thoughts_token_count,
                metadata.cached_content_token_count,
                metadata.total_token_count,
            );
            if let Some(observer) = &self.usage_observer {
                observer.observe(&self.partial.usage)?;
            }
        }

        // A blocked prompt arrives as `promptFeedback.blockReason` with no
        // candidates and no finishReason: terminal, and an error rather
        // than an empty success.
        if let Some(reason) = response
            .prompt_feedback
            .as_ref()
            .and_then(|feedback| feedback.block_reason.as_deref())
        {
            self.saw_terminal = true;
            self.partial.stop_reason = StopReason::Error;
            self.partial.error_message = Some(format!(
                "{} blocked the prompt: {reason}",
                self.service_label()
            ));
            self.content_state
                .close(&self.partial, &mut self.pending_events);
            // A refused prompt must not dispatch content/tool calls even if
            // a malformed response also supplies candidates.
            return Ok(());
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
        let wrapped: CloudCodeAssistResponseChunk =
            serde_json::from_str(data).map_err(wire::response_parse_error)?;
        let Some(response) = wrapped.response else {
            return Ok(());
        };
        self.process_response(GeminiStreamResponse {
            candidates: response.candidates,
            usage_metadata: response.usage_metadata,
            prompt_feedback: response.prompt_feedback,
        })
    }

    #[allow(clippy::unnecessary_wraps, clippy::too_many_lines)]
    fn process_candidate(&mut self, candidate: GeminiCandidate) -> Result<()> {
        // A failed generation cannot be made executable by a later STOP or
        // function-call chunk. Usage metadata is still collected by the
        // response handler before candidates reach this boundary.
        if self.partial.stop_reason == StopReason::Error {
            return Ok(());
        }
        let has_finish_reason = candidate.finish_reason.is_some();
        self.saw_terminal |= has_finish_reason;

        // Handle finish reason
        if let Some(reason) = candidate.finish_reason.as_deref() {
            self.partial.stop_reason = match reason {
                "MAX_TOKENS" => StopReason::Length,
                // Developer API and Vertex both report failed generations
                // through finishReason, including invalid function calls.
                // Such candidates must never dispatch otherwise valid peers.
                "SAFETY"
                | "RECITATION"
                | "LANGUAGE"
                | "OTHER"
                | "BLOCKLIST"
                | "PROHIBITED_CONTENT"
                | "SPII"
                | "MALFORMED_FUNCTION_CALL"
                | "MODEL_ARMOR"
                | "IMAGE_SAFETY"
                | "IMAGE_PROHIBITED_CONTENT"
                | "IMAGE_OTHER"
                | "NO_IMAGE"
                | "IMAGE_RECITATION"
                | "UNEXPECTED_TOOL_CALL"
                | "TOO_MANY_TOOL_CALLS"
                | "MISSING_THOUGHT_SIGNATURE"
                | "MALFORMED_RESPONSE"
                | "ESCALATION"
                | "PUP_LIMITED_DISABLED" => StopReason::Error,
                "FUNCTION_CALL" => StopReason::ToolUse,
                // Preserve compatibility with unrecognized finish strings.
                _ => StopReason::Stop,
            };
            if self.partial.stop_reason == StopReason::Error {
                self.partial.error_message = Some(format!(
                    "{} stopped generation: {reason}",
                    self.service_label()
                ));
                self.content_state
                    .close(&self.partial, &mut self.pending_events);
                return Ok(());
            }
        }

        // Process content parts — queue all events into pending_events
        if let Some(content) = candidate.content {
            for part in content.parts {
                match part {
                    GeminiPart::Text { text } => {
                        self.content_state.append(
                            &mut self.partial,
                            &mut self.pending_events,
                            &mut self.started,
                            text,
                            false,
                            None,
                        );
                    }
                    GeminiPart::SignedText {
                        text,
                        thought_signature,
                    } => {
                        self.content_state.append(
                            &mut self.partial,
                            &mut self.pending_events,
                            &mut self.started,
                            text,
                            false,
                            Some(thought_signature),
                        );
                    }
                    GeminiPart::Thought {
                        text,
                        thought_signature,
                        ..
                    } => {
                        self.content_state.append(
                            &mut self.partial,
                            &mut self.pending_events,
                            &mut self.started,
                            text,
                            true,
                            thought_signature,
                        );
                    }
                    GeminiPart::FunctionCall {
                        function_call,
                        thought_signature,
                    } => {
                        self.content_state
                            .close(&self.partial, &mut self.pending_events);
                        self.ensure_started();
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
                            thought_signature,
                        };

                        self.partial
                            .content
                            .push(ContentBlock::ToolCall(tool_call.clone()));
                        let content_index = self.partial.content.len() - 1;

                        // Preserve explicit failure/length outcomes, even when
                        // the terminal candidate also contains function calls.
                        if self.partial.stop_reason == StopReason::Stop {
                            self.partial.stop_reason = StopReason::ToolUse;
                        }

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

        // Seal each block exactly once, including a late signature-bearing
        // empty text part after the terminal candidate but before EOF.
        if self.saw_terminal {
            self.content_state
                .close(&self.partial, &mut self.pending_events);
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

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum GeminiPart {
    Thought {
        text: String,
        thought: bool,
        #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    SignedText {
        text: String,
        #[serde(rename = "thoughtSignature")]
        thought_signature: String,
    },
    Text {
        text: String,
    },
    InlineData {
        inline_data: GeminiBlob,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GeminiFunctionCall,
        /// Opaque model state attached to this exact call, not its arguments.
        /// Gemini 3 requires it on replay; unsigned parallel calls stay unsigned.
        #[serde(
            default,
            rename = "thoughtSignature",
            alias = "thought_signature",
            skip_serializing_if = "Option::is_none"
        )]
        thought_signature: Option<String>,
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
    #[serde(default = "empty_function_arguments")]
    pub(crate) args: serde_json::Value,
}

fn empty_function_arguments() -> serde_json::Value {
    serde_json::json!({})
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
    #[serde(default)]
    pub(crate) prompt_feedback: Option<GeminiPromptFeedback>,
}

/// `promptFeedback` on a chunk: present with `blockReason` when the prompt
/// itself was refused (no candidates follow).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiPromptFeedback {
    #[serde(default)]
    pub(crate) block_reason: Option<String>,
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
    #[serde(default)]
    prompt_feedback: Option<GeminiPromptFeedback>,
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
    pub(crate) thoughts_token_count: Option<u64>,
    #[serde(default)]
    pub(crate) cached_content_token_count: Option<u64>,
    #[serde(default)]
    pub(crate) total_token_count: Option<u64>,
}

// ============================================================================
// Conversion Functions
// ============================================================================

/// Keep every parallel function response in the same user turn. Gemini
/// validates that the response turn answers all calls in the preceding model
/// turn; separate user turns fail that contract even for text-only results.
/// Responses without wire IDs are matched by position, so restore call order
/// when parallel tools completed or were persisted in a different order.
/// Media follows the complete response turn, with its originating call named.
///
/// Shared by the Developer API, Cloud Code Assist and Vertex adapters.
/// https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures#parallel-function-calling-example
pub(crate) fn convert_messages_to_gemini(messages: &[Message]) -> Vec<GeminiContent> {
    let mut contents = Vec::with_capacity(messages.len());
    let mut results = Vec::new();
    let mut call_order = HashMap::new();
    let mut next_call_ordinal = 0;
    for message in messages {
        if let Message::ToolResult(result) = message {
            results.push(result.as_ref());
        } else {
            append_ordered_tool_result_turns(&mut contents, &mut results, &call_order);
            if let Message::Assistant(assistant) = message {
                for block in &assistant.content {
                    if let ContentBlock::ToolCall(call) = block {
                        call_order.insert(call.id.as_str(), next_call_ordinal);
                        next_call_ordinal += 1;
                    }
                }
            }
            contents.extend(convert_message_to_gemini(message));
        }
    }
    append_ordered_tool_result_turns(&mut contents, &mut results, &call_order);
    contents
}

fn append_ordered_tool_result_turns(
    contents: &mut Vec<GeminiContent>,
    results: &mut Vec<&ToolResultMessage>,
    call_order: &HashMap<&str, usize>,
) {
    if results.is_empty() {
        return;
    }
    let mut known_results: Vec<_> = results
        .iter()
        .filter_map(|result| {
            call_order
                .get(result.tool_call_id.as_str())
                .map(|&ordinal| (ordinal, *result))
        })
        .collect();
    known_results.sort_by_key(|(ordinal, _)| *ordinal);
    // Only matched slots move. Imported or incomplete histories may contain
    // results with no recorded call; preserve those results where they were.
    for (slot, (_, result)) in results
        .iter_mut()
        .filter(|result| call_order.contains_key(result.tool_call_id.as_str()))
        .zip(known_results)
    {
        *slot = result;
    }

    let mut responses = Vec::with_capacity(results.len());
    let mut media = Vec::new();
    for result in results.drain(..) {
        append_tool_result_parts(result, &mut responses, &mut media);
    }
    append_tool_result_turns(contents, &mut responses, &mut media);
}

#[allow(clippy::too_many_lines)]
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
            let google_history = reasoning::is_google_message(assistant);

            for block in &assistant.content {
                match block {
                    ContentBlock::Text(t) => {
                        if let Some(signature) =
                            t.text_signature.as_ref().filter(|_| google_history)
                        {
                            parts.push(GeminiPart::SignedText {
                                text: t.text.clone(),
                                thought_signature: signature.clone(),
                            });
                        } else {
                            parts.push(GeminiPart::Text {
                                text: t.text.clone(),
                            });
                        }
                    }
                    ContentBlock::Thinking(t) if google_history => {
                        parts.push(GeminiPart::Thought {
                            text: t.thinking.clone(),
                            thought: true,
                            thought_signature: t.thinking_signature.clone(),
                        });
                    }
                    ContentBlock::ToolCall(tc) => {
                        parts.push(GeminiPart::FunctionCall {
                            function_call: GeminiFunctionCall {
                                name: tc.name.clone(),
                                args: tc.arguments.clone(),
                            },
                            thought_signature: tc
                                .thought_signature
                                .clone()
                                .filter(|_| google_history),
                        });
                    }
                    ContentBlock::Thinking(_)
                    | ContentBlock::Image(_)
                    | ContentBlock::Media(_)
                    | ContentBlock::RedactedThinking(_) => {
                        // Foreign-provider reasoning/signatures are not Google
                        // state. Redacted markers and model-side media are not
                        // replayed as reasoning text.
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
            let mut contents = Vec::new();
            let mut responses = Vec::new();
            let mut media = Vec::new();
            append_tool_result_parts(result, &mut responses, &mut media);
            append_tool_result_turns(&mut contents, &mut responses, &mut media);
            contents
        }
    }
}

fn append_tool_result_parts(
    result: &crate::model::ToolResultMessage,
    responses: &mut Vec<GeminiPart>,
    media: &mut Vec<GeminiPart>,
) {
    let inline_parts = tool_result_inline_parts(result);
    let mut content_text = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if content_text.is_empty() && !inline_parts.is_empty() {
        content_text = "(see attached media)".to_string();
    }
    let response = if result.is_error {
        serde_json::json!({ "error": content_text })
    } else {
        serde_json::json!({ "result": content_text })
    };
    responses.push(GeminiPart::FunctionResponse {
        function_response: GeminiFunctionResponse {
            name: result.tool_name.clone(),
            response,
        },
    });
    if !inline_parts.is_empty() {
        media.push(GeminiPart::Text {
            text: format!(
                "Media from {} (tool call {}):",
                result.tool_name, result.tool_call_id
            ),
        });
        media.extend(inline_parts);
    }
}

fn append_tool_result_turns(
    contents: &mut Vec<GeminiContent>,
    responses: &mut Vec<GeminiPart>,
    media: &mut Vec<GeminiPart>,
) {
    if responses.is_empty() {
        return;
    }
    contents.push(GeminiContent {
        role: Some("user".into()),
        parts: std::mem::take(responses),
    });
    if !media.is_empty() {
        let mut parts = vec![GeminiPart::Text {
            text: "Tool result media:".to_string(),
        }];
        parts.append(media);
        contents.push(GeminiContent {
            role: Some("user".into()),
            parts,
        });
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
                // Video/audio go out as the same `inline_data` blob shape the
                // Gemini API documents for all inline media (gh #212).
                ContentBlock::Media(media) => Some(GeminiPart::InlineData {
                    inline_data: GeminiBlob {
                        mime_type: media.mime_type.clone(),
                        data: media.data.clone(),
                    },
                }),
                _ => None,
            })
            .collect(),
    }
}

/// Inline blobs (images, video, audio) carried by a tool result.
///
/// Gemini's `functionResponse.response` is a JSON object, so binary tool
/// output cannot ride inside it on every model generation. Mirroring the
/// Node reference's cross-model fallback, the blobs are attached as a
/// separate trailing `user` turn so the model still receives them.
fn tool_result_inline_parts(result: &crate::model::ToolResultMessage) -> Vec<GeminiPart> {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image(img) => Some(GeminiPart::InlineData {
                inline_data: GeminiBlob {
                    mime_type: img.mime_type.clone(),
                    data: img.data.clone(),
                },
            }),
            ContentBlock::Media(media) => Some(GeminiPart::InlineData {
                inline_data: GeminiBlob {
                    mime_type: media.mime_type.clone(),
                    data: media.data.clone(),
                },
            }),
            _ => None,
        })
        .collect()
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
    mod integration_reasoning;

    mod stream_lifecycle {
        use super::super::*;
        use asupersync::runtime::RuntimeBuilder;
        use futures::FutureExt as _;
        use serde_json::{Value, json};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context as TaskContext, Poll, Wake, Waker};

        type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;

        #[derive(Clone, Copy)]
        struct Mode {
            api: &'static str,
            provider: &'static str,
            cloud_cli: bool,
        }

        const MODES: [Mode; 3] = [
            Mode {
                api: "google-generative-ai",
                provider: "google",
                cloud_cli: false,
            },
            Mode {
                api: "google-vertex",
                provider: "google-vertex",
                cloud_cli: false,
            },
            Mode {
                api: "google-gemini-cli",
                provider: "google-gemini-cli",
                cloud_cli: true,
            },
        ];

        enum Tail {
            Eof,
            Pending,
            Repeat(Vec<u8>),
        }

        struct ObservedSource {
            chunks: VecDeque<std::io::Result<Vec<u8>>>,
            tail: Tail,
            repeated_polls: usize,
            dropped: Arc<AtomicBool>,
        }

        impl Stream for ObservedSource {
            type Item = std::io::Result<Vec<u8>>;

            fn poll_next(
                self: Pin<&mut Self>,
                _: &mut TaskContext<'_>,
            ) -> Poll<Option<Self::Item>> {
                let this = self.get_mut();
                if let Some(chunk) = this.chunks.pop_front() {
                    return Poll::Ready(Some(chunk));
                }
                match &this.tail {
                    Tail::Eof => Poll::Ready(None),
                    Tail::Pending => Poll::Pending,
                    Tail::Repeat(frame) => {
                        this.repeated_polls += 1;
                        // A regression must fail rather than hang this test on
                        // an endlessly ready source that never yields a result.
                        assert!(
                            this.repeated_polls <= MAX_READY_EVENTS_PER_YIELD * 4,
                            "silent frames monopolized the consumer's poll"
                        );
                        Poll::Ready(Some(Ok(frame.clone())))
                    }
                }
            }
        }

        impl Drop for ObservedSource {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }

        fn observed(
            mode: Mode,
            chunks: impl IntoIterator<Item = std::io::Result<Vec<u8>>>,
            tail: Tail,
        ) -> (EventStream, Arc<AtomicBool>) {
            let dropped = Arc::new(AtomicBool::new(false));
            let source = ObservedSource {
                chunks: chunks.into_iter().collect(),
                tail,
                repeated_polls: 0,
                dropped: Arc::clone(&dropped),
            };
            let state = StreamState::new(
                SseStream::new(source),
                "gemini-test".into(),
                mode.api.into(),
                mode.provider.into(),
            );
            (Box::pin(state.into_stream(mode.cloud_cli)), dropped)
        }

        fn wire(mode: Mode, value: Value) -> Vec<u8> {
            let value = if mode.cloud_cli {
                json!({"response": value})
            } else {
                value
            };
            format!("data: {value}\n\n").into_bytes()
        }

        fn usage() -> Value {
            json!({"usageMetadata": {
                "promptTokenCount": 11,
                "candidatesTokenCount": 3,
                "thoughtsTokenCount": 2,
                "cachedContentTokenCount": 4,
                "totalTokenCount": 16
            }})
        }

        fn assert_usage(message: &AssistantMessage) {
            assert_eq!(message.usage.input, 7);
            assert_eq!(message.usage.cache_read, 4);
            assert_eq!(message.usage.output, 5);
            assert_eq!(message.usage.total_tokens, 16);
        }

        fn assert_retired(output: &mut EventStream, dropped: &AtomicBool) {
            // Check before another poll: the consumer can retain a stream
            // indefinitely after receiving its terminal outcome.
            assert!(
                dropped.load(Ordering::SeqCst),
                "terminal delivery retained the response source"
            );
            for _ in 0..3 {
                assert!(
                    output
                        .next()
                        .now_or_never()
                        .expect("terminal stream must stay ready")
                        .is_none()
                );
            }
        }

        struct UsageWireProvider {
            mode: Mode,
            response: std::sync::Mutex<Option<(Vec<std::io::Result<Vec<u8>>>, Tail)>>,
            dropped: Arc<AtomicBool>,
        }

        #[async_trait]
        impl Provider for UsageWireProvider {
            fn name(&self) -> &str {
                self.mode.provider
            }
            fn api(&self) -> &str {
                self.mode.api
            }
            fn model_id(&self) -> &str {
                "gemini-usage-fixture"
            }

            fn model_cost(&self) -> Option<crate::provider::ModelCost> {
                Some(crate::provider::ModelCost {
                    input: 1_000_000.0,
                    output: 1_000_000.0,
                    cache_read: 1_000_000.0,
                    cache_write: 0.0,
                })
            }

            async fn stream(
                &self,
                _context: &Context<'_>,
                options: &StreamOptions,
            ) -> Result<EventStream> {
                let (chunks, tail) = self
                    .response
                    .lock()
                    .unwrap()
                    .take()
                    .expect("one wire request");
                let source = ObservedSource {
                    chunks: chunks.into(),
                    tail,
                    repeated_polls: 0,
                    dropped: Arc::clone(&self.dropped),
                };
                let state = StreamState::new(
                    SseStream::new(source),
                    self.model_id().into(),
                    self.api().into(),
                    self.name().into(),
                )
                .with_usage_observer(options.usage_observer.clone());
                Ok(Box::pin(state.into_stream(self.mode.cloud_cli)))
            }
        }

        fn guarded_usage_wire(
            mode: Mode,
            chunks: Vec<std::io::Result<Vec<u8>>>,
            tail: Tail,
        ) -> (Arc<dyn Provider>, crate::cost::CostBudget, Arc<AtomicBool>) {
            let dropped = Arc::new(AtomicBool::new(false));
            let provider = Arc::new(UsageWireProvider {
                mode,
                response: std::sync::Mutex::new(Some((chunks, tail))),
                dropped: Arc::clone(&dropped),
            });
            let budget = crate::cost::CostBudget::new(None, Some(15.0), 0.0).unwrap();
            (
                crate::provider::guard_with_cost_budget(provider, budget.clone()),
                budget,
                dropped,
            )
        }

        #[test]
        fn usage_only_pending_wire_updates_budget_before_any_public_event_and_survives_drop() {
            futures::executor::block_on(async {
                for mode in MODES {
                    let (provider, budget, dropped) =
                        guarded_usage_wire(mode, vec![Ok(wire(mode, usage()))], Tail::Pending);
                    let mut output = provider
                        .stream(&Context::default(), &StreamOptions::default())
                        .await
                        .unwrap();
                    assert!(output.next().now_or_never().is_none());
                    assert_eq!(budget.snapshot().pending, 16.0);
                    assert_eq!(budget.spent(), 0.0);
                    assert!(!dropped.load(Ordering::SeqCst));
                    assert!(matches!(
                        provider
                            .stream(&Context::default(), &StreamOptions::default())
                            .await,
                        Err(Error::CostCap { .. })
                    ));
                    drop(output);
                    assert!(dropped.load(Ordering::SeqCst));
                    assert_eq!(budget.spent(), 16.0);
                    assert_eq!(budget.snapshot().pending, 0.0);
                }
            });
        }

        #[test]
        fn later_wire_usage_survives_transport_parser_and_incomplete_failures_without_double_billing()
         {
            futures::executor::block_on(async {
                for mode in MODES {
                    for outcome in ["transport", "parser", "incomplete", "done"] {
                        let mut chunks = vec![
                            Ok(wire(
                                mode,
                                json!({"candidates": [{"content": {"parts": [{"text": "answer"}]}}]}),
                            )),
                            Ok(wire(mode, usage())),
                            Ok(wire(mode, usage())),
                        ];
                        match outcome {
                            "transport" => chunks.push(Err(std::io::Error::new(
                                std::io::ErrorKind::ConnectionReset,
                                "fixture",
                            ))),
                            "parser" => chunks.push(Ok(b"data: {broken json}\n\n".to_vec())),
                            "done" => chunks.push(Ok(wire(
                                mode,
                                json!({"candidates": [{"finishReason": "STOP"}]}),
                            ))),
                            _ => {}
                        }
                        let (provider, budget, dropped) =
                            guarded_usage_wire(mode, chunks, Tail::Eof);
                        let mut output = provider
                            .stream(&Context::default(), &StreamOptions::default())
                            .await
                            .unwrap();
                        let mut terminal = false;
                        for _ in 0..12 {
                            let Some(event) =
                                output.next().now_or_never().expect("bounded ready wire")
                            else {
                                break;
                            };
                            match event {
                                Ok(StreamEvent::Done { message, .. }) => {
                                    assert_eq!(outcome, "done");
                                    assert_usage(&message);
                                    assert_eq!(message.usage.cost.total, 16.0);
                                    terminal = true;
                                }
                                Err(_) => {
                                    assert_ne!(outcome, "done");
                                    terminal = true;
                                }
                                _ => continue,
                            }
                            assert!(dropped.load(Ordering::SeqCst));
                            assert_eq!(budget.spent(), 16.0);
                            break;
                        }
                        assert!(terminal);
                        assert_retired(&mut output, &dropped);
                        drop(output);
                        assert_eq!(budget.spent(), 16.0);
                        assert_eq!(budget.snapshot().pending, 0.0);
                    }
                }
            });
        }

        #[test]
        fn terminal_releases_source_after_collecting_late_usage_and_signatures() {
            for mode in MODES {
                let chunks = [
                    json!({"candidates": [{"content": {"parts": [{"text": "answer"}]}}]}),
                    json!({"candidates": [{"finishReason": "STOP"}]}),
                    json!({"candidates": [{"content": {"parts": [{
                        "text": "", "thoughtSignature": "bGF0ZQ=="
                    }]}}]}),
                    usage(),
                ]
                .map(|value| Ok(wire(mode, value)));
                let (mut output, dropped) = observed(mode, chunks, Tail::Eof);
                let mut terminal = None;
                for _ in 0..12 {
                    let event = output.next().now_or_never().unwrap().unwrap().unwrap();
                    if let StreamEvent::Done { reason, message } = event {
                        assert_eq!(reason, StopReason::Stop);
                        terminal = Some(message);
                        break;
                    }
                    assert!(!dropped.load(Ordering::SeqCst));
                }
                let message = terminal.expect("missing terminal outcome");
                assert_eq!(message.api, mode.api);
                assert_eq!(message.provider, mode.provider);
                assert_usage(&message);
                assert_eq!(message.content.len(), 2);
                let ContentBlock::Text(text) = &message.content[1] else {
                    panic!("missing late signed text");
                };
                // ubs:ignore[rust.security.constant-time-compare] Public fixture equality, not authentication.
                assert_eq!(text.text_signature.as_deref(), Some("bGF0ZQ=="));
                assert_retired(&mut output, &dropped);
            }
        }

        #[test]
        fn refused_prompt_retains_late_usage_and_retires_its_source() {
            for mode in MODES {
                let chunks = [
                    json!({"promptFeedback": {"blockReason": "SAFETY"}}),
                    usage(),
                ]
                .map(|value| Ok(wire(mode, value)));
                let (mut output, dropped) = observed(mode, chunks, Tail::Eof);
                let Some(Ok(StreamEvent::Done { reason, message })) =
                    output.next().now_or_never().unwrap()
                else {
                    panic!("missing refused terminal outcome");
                };
                assert_eq!(reason, StopReason::Error);
                assert!(message.error_message.as_deref().unwrap().contains("SAFETY"));
                assert!(message.content.is_empty());
                assert_usage(&message);
                assert_retired(&mut output, &dropped);
            }
        }

        #[test]
        fn parser_transport_and_incomplete_errors_retire_before_delivery() {
            for mode in MODES {
                let cases = [
                    (vec![Ok(b"data: {broken json}\n\n".to_vec())], Tail::Pending),
                    (
                        vec![Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            "fixture",
                        ))],
                        Tail::Pending,
                    ),
                    (Vec::new(), Tail::Eof),
                    (
                        vec![
                            Ok(wire(
                                mode,
                                json!({"candidates": [{"finishReason": "STOP"}]}),
                            )),
                            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "fixture")),
                        ],
                        Tail::Pending,
                    ),
                ];
                for (chunks, tail) in cases {
                    let (mut output, dropped) = observed(mode, chunks, tail);
                    let event = output.next().now_or_never().unwrap().unwrap();
                    assert!(event.is_err(), "failure became a success: {event:?}");
                    assert_retired(&mut output, &dropped);
                }
            }
        }

        #[test]
        fn dropping_a_pending_stream_releases_the_source() {
            for mode in MODES {
                let (mut output, dropped) = observed(mode, [], Tail::Pending);
                assert!(output.next().now_or_never().is_none());
                assert!(!dropped.load(Ordering::SeqCst));
                drop(output);
                assert!(dropped.load(Ordering::SeqCst));
            }
        }

        struct WakeFlag(AtomicBool);

        impl Wake for WakeFlag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        fn silent_frame(mode: Mode, metadata: bool) -> Vec<u8> {
            if metadata {
                wire(mode, usage())
            } else {
                b"event: ping\ndata: {}\n\n".to_vec()
            }
        }

        #[test]
        fn buffered_heartbeat_and_usage_bursts_yield_and_resume() {
            for mode in MODES {
                for metadata in [false, true] {
                    let mut bytes =
                        silent_frame(mode, metadata).repeat(MAX_READY_EVENTS_PER_YIELD * 2);
                    bytes.extend(wire(mode, usage()));
                    bytes.extend(wire(
                        mode,
                        json!({"candidates": [{"finishReason": "STOP"}]}),
                    ));
                    let (mut output, dropped) = observed(mode, [Ok(bytes)], Tail::Eof);
                    let flag = Arc::new(WakeFlag(AtomicBool::new(false)));
                    let waker = Waker::from(Arc::clone(&flag));
                    let mut task = TaskContext::from_waker(&waker);
                    assert!(
                        output.as_mut().poll_next(&mut task).is_pending(),
                        "ready silent frames must yield before completing the burst"
                    );
                    assert!(
                        flag.0.load(Ordering::SeqCst),
                        "yield did not arrange another poll"
                    );
                    assert!(!dropped.load(Ordering::SeqCst));
                    let mut terminal = None;
                    for _ in 0..4 {
                        if let Poll::Ready(Some(Ok(StreamEvent::Done { message, .. }))) =
                            output.as_mut().poll_next(&mut task)
                        {
                            terminal = Some(message);
                            break;
                        }
                    }
                    assert_usage(&terminal.expect("yielded stream did not finish"));
                    assert_retired(&mut output, &dropped);
                }
            }
        }

        #[test]
        fn continuously_ready_frames_allow_abort_and_release_on_drop() {
            let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
            for mode in MODES {
                for metadata in [false, true] {
                    let (mut output, dropped) =
                        observed(mode, [], Tail::Repeat(silent_frame(mode, metadata)));
                    runtime.block_on(async {
                        let (abort, signal) = crate::agent::AbortHandle::new();
                        // Poll the response first. Its cooperative yield must
                        // let the other branch run and deliver cancellation.
                        let next = Box::pin(output.next());
                        let cancel = Box::pin(async move {
                            abort.abort();
                            signal.wait().await;
                        });
                        match futures::future::select(next, cancel).await {
                            futures::future::Either::Right(((), pending)) => drop(pending),
                            futures::future::Either::Left(_) => {
                                panic!("ready frame flood starved cancellation");
                            }
                        }
                    });
                    assert!(!dropped.load(Ordering::SeqCst));
                    drop(output);
                    assert!(dropped.load(Ordering::SeqCst));
                }
            }
        }
    }

    use super::*;
    use crate::model::TextContent;
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

    /// Drive the real `Provider::stream` against a canned SSE body and
    /// collect every item, including the terminal error.
    fn collect_stream_items_from_body(body: &str) -> Vec<Result<StreamEvent>> {
        let (base_url, _rx) = spawn_test_server(200, "text/event-stream", body);
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
        headers.insert("Authorization".to_string(), "Bearer test".to_string());
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
            let mut out = Vec::new();
            while let Some(item) = stream.next().await {
                out.push(item);
            }
            out
        })
    }

    /// gh #213 (truncated streams): a transport close before the chunk that
    /// carries `finishReason` is a cut-off response — it must surface as a
    /// retryable error, never as a clean `Done`/`Stop` that commits the
    /// partial text to the session.
    #[test]
    fn test_stream_rejects_transport_eof_before_finish_reason() {
        let body = [
            r#"data: {"candidates":[{"content":{"parts":[{"text":"partial"}]}}]}"#,
            "",
            "",
        ]
        .join("\n");
        let out = collect_stream_items_from_body(&body);
        assert!(
            out.iter().any(|item| matches!(
                item,
                Ok(StreamEvent::TextDelta { delta, .. }) if delta == "partial"
            )),
            "partial content is still streamed before the terminal error: {out:?}"
        );
        assert!(
            !out.iter()
                .any(|item| matches!(item, Ok(StreamEvent::Done { .. }))),
            "premature EOF must never be reported as Done: {out:?}"
        );
        let error = out
            .last()
            .expect("terminal stream item")
            .as_ref()
            .expect_err("premature EOF must be a stream error")
            .to_string();
        assert!(error.contains("finishReason"), "{error}");
        assert!(error.contains("unexpected EOF"), "{error}");
        assert!(
            crate::error::is_retryable_error(&error, None, None),
            "a cut-off stream must be retryable: {error}"
        );
    }

    /// The happy path through the same driver: `finishReason` seen, then the
    /// transport closes → exactly one `Done` and no error.
    #[test]
    fn test_stream_eof_after_finish_reason_is_done() {
        let out = collect_stream_items_from_body(&success_sse_body());
        assert!(out.iter().all(Result::is_ok), "{out:?}");
        let done = out
            .iter()
            .filter(|item| matches!(item, Ok(StreamEvent::Done { .. })))
            .count();
        assert_eq!(done, 1, "{out:?}");
        assert!(
            matches!(
                out.last(),
                Some(Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    ..
                }))
            ),
            "{out:?}"
        );
    }

    /// A refused prompt (`promptFeedback.blockReason`, no candidates) is
    /// terminal and an error — not an empty successful turn, and not a
    /// truncated-stream retry.
    #[test]
    fn test_blocked_prompt_is_terminal_error_not_empty_success() {
        let body = [
            r#"data: {"promptFeedback":{"blockReason":"SAFETY","safetyRatings":[]}}"#,
            "",
            r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"write","args":{}}}]},"finishReason":"STOP"}]}"#,
            "",
            "",
        ]
        .join("\n");
        let out = collect_stream_items_from_body(&body);
        let Some(Ok(StreamEvent::Done { reason, message })) = out.last() else {
            panic!("expected Done: {out:?}");
        };
        assert_eq!(*reason, StopReason::Error);
        assert_eq!(
            message.error_message.as_deref(),
            Some("Gemini blocked the prompt: SAFETY")
        );
        assert!(message.content.is_empty());
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
            // 250ms is the POLLING interval; the deadline below is the budget.
            // Treating a timed-out read as end-of-request truncated the buffer
            // and the header scan then failed as a malformed request rather
            // than a slow one (bd-eg6ng).
            socket
                .set_read_timeout(Some(Duration::from_millis(250)))
                .expect("set read timeout");
            let deadline = std::time::Instant::now() + Duration::from_secs(30);

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
                        assert!(
                            std::time::Instant::now() < deadline,
                            "fixture timed out waiting for the request"
                        );
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
                        assert!(
                            std::time::Instant::now() < deadline,
                            "fixture timed out waiting for the request"
                        );
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

    fn assert_legacy_gemini_schema_fields(schema: &Value) {
        // Field vocabulary from https://ai.google.dev/api/generate-content#Schema.
        // This checks the outgoing contract, without emulating a live provider.
        let object = schema.as_object().expect("schema object");
        for keyword in object.keys() {
            assert!(
                matches!(
                    keyword.as_str(),
                    "type"
                        | "format"
                        | "title"
                        | "description"
                        | "nullable"
                        | "enum"
                        | "maxItems"
                        | "minItems"
                        | "properties"
                        | "required"
                        | "minProperties"
                        | "maxProperties"
                        | "minLength"
                        | "maxLength"
                        | "pattern"
                        | "example"
                        | "anyOf"
                        | "propertyOrdering"
                        | "default"
                        | "items"
                        | "minimum"
                        | "maximum"
                ),
                "unsupported Gemini Schema field {keyword}: {schema}"
            );
        }
        assert!(
            schema
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    matches!(
                        kind,
                        "object" | "array" | "string" | "integer" | "number" | "boolean"
                    )
                })
                || object.contains_key("anyOf"),
            "schema needs a type or typed alternatives: {schema}"
        );
        if let Some(properties) = object.get("properties") {
            for property in properties.as_object().expect("properties object").values() {
                assert_legacy_gemini_schema_fields(property);
            }
        }
        if let Some(items) = object.get("items") {
            assert_legacy_gemini_schema_fields(items);
        }
        if let Some(alternatives) = object.get("anyOf") {
            let alternatives = alternatives.as_array().expect("anyOf array");
            assert!(!alternatives.is_empty(), "empty alternatives: {schema}");
            for alternative in alternatives {
                assert_legacy_gemini_schema_fields(alternative);
            }
        }
        if let Some(required) = object.get("required") {
            let properties = schema["properties"]
                .as_object()
                .expect("required properties");
            for name in required.as_array().expect("required array") {
                let name = name.as_str().expect("required property name");
                assert!(
                    properties.contains_key(name),
                    "undefined required property {name}: {schema}"
                );
            }
        }
    }

    #[test]
    fn test_write_tool_request_uses_legacy_gemini_schema_fields() {
        use crate::tools::Tool as _;

        let write = crate::tools::WriteTool::new(std::path::Path::new("."));
        let provider = GeminiProvider::new("gemini-2.0-flash");
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Resolve the inspected conflict.".to_string()),
                timestamp: 0,
            })],
            vec![ToolDef {
                name: write.name().to_string(),
                description: write.description().to_string(),
                parameters: write.parameters(),
            }],
        );
        let request =
            serde_json::to_value(provider.build_request(&context, &StreamOptions::default()))
                .expect("serialize real write tool request");
        let declarations = request["tools"][0]["functionDeclarations"]
            .as_array()
            .expect("function declarations");
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0]["name"], "write");
        let parameters = &declarations[0]["parameters"];
        assert_legacy_gemini_schema_fields(parameters);
        assert_eq!(parameters["required"], serde_json::json!(["path"]));
        assert_eq!(parameters["properties"]["content"]["type"], "string");
        let choices = &parameters["properties"]["conflicts"];
        assert_eq!(choices["type"], "array");
        assert_eq!(choices["minItems"], 1);
        assert_eq!(choices["maxItems"], 64);
        let resolution = &choices["items"]["properties"]["resolution"]["anyOf"];
        assert_eq!(
            resolution[0]["properties"]["kind"]["enum"],
            serde_json::json!(["ours", "theirs", "base"])
        );
        assert_eq!(
            resolution[1]["properties"]["kind"]["enum"],
            serde_json::json!(["text"])
        );
        assert_eq!(
            resolution[1]["required"],
            serde_json::json!(["kind", "value"])
        );
        assert_eq!(
            parameters["anyOf"][0]["properties"]["content"],
            parameters["properties"]["content"]
        );
        assert_eq!(parameters["anyOf"][1]["properties"]["conflicts"], *choices);
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

    /// gh #212: video/audio blocks serialize as the same `inline_data` blob
    /// shape as images, with the MIME type passed through verbatim.
    #[test]
    fn test_convert_user_blocks_with_media_to_gemini_inline_data() {
        let content = UserContent::Blocks(vec![
            ContentBlock::Text(TextContent::new("what happens in this clip?")),
            ContentBlock::Media(crate::model::MediaContent {
                data: "AAAA".to_string(),
                mime_type: "video/mp4".to_string(),
                name: Some("clip.mp4".to_string()),
            }),
            ContentBlock::Media(crate::model::MediaContent {
                data: "BBBB".to_string(),
                mime_type: "audio/wav".to_string(),
                name: None,
            }),
        ]);

        let parts = convert_user_content_to_parts(&content);
        assert_eq!(parts.len(), 3);
        let wire = serde_json::to_value(&parts).expect("serialize parts");
        assert_eq!(
            wire,
            serde_json::json!([
                { "text": "what happens in this clip?" },
                { "inline_data": { "mimeType": "video/mp4", "data": "AAAA" } },
                { "inline_data": { "mimeType": "audio/wav", "data": "BBBB" } },
            ])
        );
    }

    /// gh #212: a tool result carrying media (the `read_media` shape) keeps
    /// its text inside `functionResponse` and attaches the blobs on a
    /// trailing user turn, so the model actually receives them.
    #[test]
    fn test_convert_tool_result_with_media_attaches_inline_turn() {
        let message = Message::tool_result(crate::model::ToolResultMessage {
            tool_call_id: "call_media".to_string(),
            tool_name: "read_media".to_string(),
            content: vec![
                ContentBlock::Text(TextContent::new("Read media file clip.mp4")),
                ContentBlock::Media(crate::model::MediaContent {
                    data: "AAAA".to_string(),
                    mime_type: "video/mp4".to_string(),
                    name: Some("clip.mp4".to_string()),
                }),
            ],
            details: None,
            is_error: false,
            timestamp: 0,
        });

        let converted = convert_message_to_gemini(&message);
        assert_eq!(converted.len(), 2, "functionResponse turn + media turn");
        match &converted[0].parts[0] {
            GeminiPart::FunctionResponse { function_response } => {
                assert_eq!(function_response.name, "read_media");
                assert_eq!(
                    function_response.response["result"],
                    "Read media file clip.mp4"
                );
            }
            _ => panic!("expected functionResponse part"),
        }
        assert_eq!(converted[1].role, Some("user".to_string()));
        let wire = serde_json::to_value(&converted[1].parts).expect("serialize parts");
        assert_eq!(
            wire,
            serde_json::json!([
                { "text": "Tool result media:" },
                { "text": "Media from read_media (tool call call_media):" },
                { "inline_data": { "mimeType": "video/mp4", "data": "AAAA" } },
            ])
        );

        // A media-only result gets a stand-in text so the JSON response is
        // never empty; a text-only result stays a single turn.
        let media_only = Message::tool_result(crate::model::ToolResultMessage {
            tool_call_id: "call_media".to_string(),
            tool_name: "read_media".to_string(),
            content: vec![ContentBlock::Media(crate::model::MediaContent {
                data: "AAAA".to_string(),
                mime_type: "audio/mpeg".to_string(),
                name: None,
            })],
            details: None,
            is_error: false,
            timestamp: 0,
        });
        let converted = convert_message_to_gemini(&media_only);
        assert_eq!(converted.len(), 2);
        match &converted[0].parts[0] {
            GeminiPart::FunctionResponse { function_response } => {
                assert_eq!(function_response.response["result"], "(see attached media)");
            }
            _ => panic!("expected functionResponse part"),
        }
        let text_only = Message::tool_result(crate::model::ToolResultMessage {
            tool_call_id: "call_text".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("plain"))],
            details: None,
            is_error: false,
            timestamp: 0,
        });
        assert_eq!(convert_message_to_gemini(&text_only).len(), 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn parallel_function_results_follow_call_order_before_their_media() {
        for (with_media, result_order) in [
            (false, [0, 1, 2, 3, 4]),
            (false, [2, 4, 3, 1, 0]),
            (true, [0, 1, 2, 3, 4]),
            (true, [2, 4, 3, 1, 0]),
        ] {
            let image = ContentBlock::Image(crate::model::ImageContent {
                data: "AQ==".to_string(),
                mime_type: "image/png".to_string(),
            });
            let video = ContentBlock::Media(crate::model::MediaContent {
                data: "Ag==".to_string(),
                mime_type: "video/mp4".to_string(),
                name: Some("clip.mp4".to_string()),
            });
            let audio = ContentBlock::Media(crate::model::MediaContent {
                data: "Aw==".to_string(),
                mime_type: "audio/wav".to_string(),
                name: None,
            });
            let calls = [
                ("call_image", "read"),
                ("call_text", "read"),
                ("call_media", "read_media"),
                ("call_error", "read"),
                ("call_empty", "read"),
            ];
            let mut messages = vec![
                Message::User(crate::model::UserMessage {
                    content: UserContent::Text("Read the files in parallel".to_string()),
                    timestamp: 1,
                }),
                Message::assistant(AssistantMessage {
                    content: calls
                        .iter()
                        .enumerate()
                        .map(|(index, (id, name))| {
                            ContentBlock::ToolCall(ToolCall {
                                id: (*id).to_string(),
                                name: (*name).to_string(),
                                arguments: serde_json::json!({"path": id}),
                                thought_signature: (index == 0).then(|| "c2ln".to_string()),
                            })
                        })
                        .collect(),
                    api: "google-generative-ai".to_string(),
                    provider: "google".to_string(),
                    model: "gemini-3-pro".to_string(),
                    stop_reason: StopReason::ToolUse,
                    ..AssistantMessage::default()
                }),
            ];
            for index in result_order {
                let (id, name) = calls[index];
                let mut content = match index {
                    0 => vec![ContentBlock::Text(TextContent::new("image result"))],
                    1 => vec![ContentBlock::Text(TextContent::new("text result"))],
                    3 => vec![ContentBlock::Text(TextContent::new("not found"))],
                    _ => Vec::new(),
                };
                if with_media {
                    match index {
                        0 => content.push(image.clone()),
                        2 => content.extend([video.clone(), audio.clone()]),
                        _ => {}
                    }
                }
                messages.push(Message::tool_result(crate::model::ToolResultMessage {
                    tool_call_id: id.to_string(),
                    tool_name: name.to_string(),
                    content,
                    details: Some(serde_json::json!({"kept_in_history": true})),
                    is_error: index == 3,
                    timestamp: 2,
                }));
            }
            let context = Context::owned(None, messages, Vec::new());
            let original = serde_json::to_value(&context.messages).unwrap();
            let provider = GeminiProvider::new("gemini-3-pro");
            let wire =
                serde_json::to_value(provider.build_request(&context, &StreamOptions::default()))
                    .unwrap();
            let contents = wire["contents"].as_array().unwrap();
            assert_eq!(contents.len(), if with_media { 4 } else { 3 });
            assert_eq!(contents[1]["role"], "model");
            assert_eq!(contents[1]["parts"].as_array().unwrap().len(), calls.len());
            assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "c2ln");
            assert!(contents[1]["parts"][1].get("thoughtSignature").is_none());
            assert_eq!(contents[2]["role"], "user");
            assert_eq!(
                contents[2]["parts"],
                serde_json::json!([
                    {"functionResponse": {"name": "read", "response": {"result": "image result"}}},
                    {"functionResponse": {"name": "read", "response": {"result": "text result"}}},
                    {"functionResponse": {"name": "read_media", "response": {
                        "result": if with_media { "(see attached media)" } else { "" }
                    }}},
                    {"functionResponse": {"name": "read", "response": {"error": "not found"}}},
                    {"functionResponse": {"name": "read", "response": {"result": ""}}}
                ]),
            );
            if with_media {
                assert_eq!(contents[3]["role"], "user");
                assert_eq!(
                    contents[3]["parts"],
                    serde_json::json!([
                        {"text": "Tool result media:"},
                        {"text": "Media from read (tool call call_image):"},
                        {"inline_data": {"mimeType": "image/png", "data": "AQ=="}},
                        {"text": "Media from read_media (tool call call_media):"},
                        {"inline_data": {"mimeType": "video/mp4", "data": "Ag=="}},
                        {"inline_data": {"mimeType": "audio/wav", "data": "Aw=="}}
                    ]),
                );
            }
            for is_antigravity in [false, true] {
                let cli = build_google_cli_request(
                    "gemini-3-pro",
                    "test-project",
                    wire.clone(),
                    is_antigravity,
                )
                .unwrap();
                let wrapped = serde_json::to_value(cli).unwrap();
                assert_eq!(wrapped["request"]["contents"], wire["contents"]);
            }
            assert_eq!(serde_json::to_value(&context.messages).unwrap(), original);
        }
    }

    #[test]
    fn function_response_batches_preserve_turn_boundaries_and_user_images() {
        let call = |ids: &[&str]| {
            Message::assistant(AssistantMessage {
                content: ids
                    .iter()
                    .map(|id| {
                        ContentBlock::ToolCall(ToolCall {
                            id: (*id).to_string(),
                            name: "read".to_string(),
                            arguments: serde_json::json!({"path": id}),
                            thought_signature: None,
                        })
                    })
                    .collect(),
                stop_reason: StopReason::ToolUse,
                ..AssistantMessage::default()
            })
        };
        let image = |data: &str| {
            ContentBlock::Image(crate::model::ImageContent {
                data: data.to_string(),
                mime_type: "image/png".to_string(),
            })
        };
        let result = |id: &str, data: &str| {
            Message::tool_result(crate::model::ToolResultMessage {
                tool_call_id: id.to_string(),
                tool_name: "read".to_string(),
                content: vec![image(data)],
                details: None,
                is_error: false,
                timestamp: 0,
            })
        };
        let context = Context::owned(
            None,
            vec![
                call(&["call_a", "call_b"]),
                result("call_b", "Ag=="),
                result("call_a", "AQ=="),
                Message::Custom(crate::model::CustomMessage {
                    custom_type: "context".to_string(),
                    content: "Keep the second result separate".to_string(),
                    display: false,
                    details: None,
                    timestamp: 0,
                }),
                // Reused IDs follow this model turn's order, without changing
                // the order of the already completed first turn.
                call(&["call_b", "call_a"]),
                result("call_a", "BA=="),
                result("call_b", "Aw=="),
                Message::User(crate::model::UserMessage {
                    content: UserContent::Blocks(vec![image("BQ==")]),
                    timestamp: 0,
                }),
            ],
            Vec::new(),
        );
        let provider = GeminiProvider::new("gemini-2.5-pro");
        let wire =
            serde_json::to_value(provider.build_request(&context, &StreamOptions::default()))
                .unwrap();
        let contents = wire["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 8);
        for (model_index, response_index, media_index, expected) in [
            (0, 1, 2, [("call_a", "AQ=="), ("call_b", "Ag==")]),
            (4, 5, 6, [("call_b", "Aw=="), ("call_a", "BA==")]),
        ] {
            assert_eq!(contents[model_index]["role"], "model");
            assert_eq!(
                contents[response_index]["parts"].as_array().unwrap().len(),
                2
            );
            assert_eq!(contents[media_index]["parts"].as_array().unwrap().len(), 5);
            for (index, (id, data)) in expected.into_iter().enumerate() {
                assert_eq!(
                    contents[response_index]["parts"][index]["functionResponse"]["response"]["result"],
                    "(see attached media)",
                );
                assert_eq!(
                    contents[media_index]["parts"][1 + index * 2]["text"],
                    format!("Media from read (tool call {id}):"),
                );
                assert_eq!(
                    contents[media_index]["parts"][2 + index * 2]["inline_data"]["data"],
                    data
                );
            }
        }
        assert_eq!(
            contents[3]["parts"][0]["text"],
            "Keep the second result separate"
        );
        assert_eq!(contents[7]["parts"].as_array().unwrap().len(), 1);
        assert_eq!(contents[7]["parts"][0]["inline_data"]["data"], "BQ==");
    }

    #[test]
    fn function_response_order_preserves_unmatched_slots_and_duplicate_results() {
        let mut messages = vec![Message::assistant(AssistantMessage {
            content: ["call_a", "call_b"]
                .into_iter()
                .map(|id| {
                    ContentBlock::ToolCall(ToolCall {
                        id: id.to_string(),
                        name: "read".to_string(),
                        arguments: serde_json::json!({"path": id}),
                        thought_signature: None,
                    })
                })
                .collect(),
            stop_reason: StopReason::ToolUse,
            ..AssistantMessage::default()
        })];
        for (id, text) in [
            ("unknown_first", "first unmatched"),
            ("call_b", "b"),
            ("unknown_middle", "middle unmatched"),
            ("call_a", "first a"),
            ("call_a", "second a"),
            ("unknown_last", "last unmatched"),
        ] {
            messages.push(Message::tool_result(ToolResultMessage {
                tool_call_id: id.to_string(),
                tool_name: "read".to_string(),
                content: vec![ContentBlock::Text(TextContent::new(text))],
                details: None,
                is_error: false,
                timestamp: 0,
            }));
        }
        let wire = serde_json::to_value(convert_messages_to_gemini(&messages)).unwrap();
        let results: Vec<_> = wire[1]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|part| {
                part["functionResponse"]["response"]["result"]
                    .as_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            results,
            [
                "first unmatched",
                "first a",
                "middle unmatched",
                "second a",
                "b",
                "last unmatched",
            ]
        );

        // Without any matching assistant calls, preserve the entire batch.
        let wire = serde_json::to_value(convert_messages_to_gemini(&messages[1..])).unwrap();
        assert_eq!(
            wire[0]["parts"][1]["functionResponse"]["response"]["result"],
            "b"
        );
        assert_eq!(
            wire[0]["parts"][3]["functionResponse"]["response"]["result"],
            "first a"
        );
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
            GeminiPart::FunctionCall { function_call, .. } => {
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
            ("FUNCTION_CALL", StopReason::ToolUse),
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

    #[test]
    fn signed_parallel_calls_survive_stream_session_and_replay() {
        const SIGNATURE: &str = "c2lnbmVkLXRvb2w=";
        let body = [
            r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"path":"a.txt"}},"thoughtSignature":"c2lnbmVkLXRvb2w="},{"functionCall":{"name":"read","args":{"path":"b.txt"}}}]}}]}"#,
            "",
            r#"data: {"candidates":[{"finishReason":"STOP"}]}"#,
            "",
            "",
        ]
        .join("\n");
        let events = collect_stream_items_from_body(&body);
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let signatures: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Ok(StreamEvent::ToolCallEnd { tool_call, .. }) => {
                    Some(tool_call.thought_signature.as_deref())
                }
                _ => None,
            })
            .collect();
        assert_eq!(signatures, vec![Some(SIGNATURE), None]);
        let Some(Ok(StreamEvent::Done { reason, message })) = events.last() else {
            panic!("expected completed tool step: {events:?}");
        };
        assert_eq!(*reason, StopReason::ToolUse);
        assert_eq!(message.stop_reason, StopReason::ToolUse);

        let stored = serde_json::to_string(&Message::assistant(message.clone())).unwrap();
        let replay: Message = serde_json::from_str(&stored).expect("session replay");
        let mut messages = vec![
            Message::User(crate::model::UserMessage {
                content: UserContent::Text("Read a.txt and b.txt".to_string()),
                timestamp: 0,
            }),
            replay,
        ];
        for block in message.content.iter().rev() {
            if let ContentBlock::ToolCall(call) = block {
                messages.push(Message::tool_result(crate::model::ToolResultMessage {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    content: vec![ContentBlock::Text(TextContent::new(format!(
                        "contents of {}",
                        call.arguments["path"].as_str().unwrap()
                    )))],
                    details: None,
                    is_error: false,
                    timestamp: 1,
                }));
            }
        }
        let context = Context::owned(None, messages, Vec::new());
        let provider = GeminiProvider::new("gemini-3-pro");
        let wire =
            serde_json::to_value(provider.build_request(&context, &StreamOptions::default()))
                .unwrap();
        let parts = wire["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["thoughtSignature"], SIGNATURE);
        assert!(parts[1].get("thoughtSignature").is_none());
        assert_eq!(
            parts[0]["functionCall"]["args"],
            serde_json::json!({"path": "a.txt"})
        );
        assert_eq!(
            parts[1]["functionCall"]["args"],
            serde_json::json!({"path": "b.txt"})
        );
        assert_eq!(wire["contents"].as_array().unwrap().len(), 3);
        assert_eq!(wire["contents"][2]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(
            wire["contents"][2]["parts"][0]["functionResponse"]["name"],
            "read"
        );
        assert_eq!(
            wire["contents"][2]["parts"][1]["functionResponse"]["name"],
            "read"
        );
        assert_eq!(
            wire["contents"][2]["parts"][0]["functionResponse"]["response"]["result"],
            "contents of a.txt"
        );
        assert_eq!(
            wire["contents"][2]["parts"][1]["functionResponse"]["response"]["result"],
            "contents of b.txt"
        );
    }

    #[test]
    fn function_call_signature_accepts_both_spellings_without_fabrication() {
        for key in ["thoughtSignature", "thought_signature"] {
            let mut wire = serde_json::json!({
                "functionCall": {"name": "read", "args": {"path": "a.txt"}}
            });
            wire[key] = serde_json::json!("c2lnbmF0dXJl");
            let part: GeminiPart = serde_json::from_value(wire).unwrap();
            let replay = serde_json::to_value(part).unwrap();
            assert_eq!(replay["thoughtSignature"], "c2lnbmF0dXJl");
            assert!(replay.get("thought_signature").is_none());
            assert_eq!(
                replay["functionCall"]["args"],
                serde_json::json!({"path": "a.txt"})
            );
        }
        let unsigned = serde_json::json!({"functionCall": {"name": "read", "args": {}}});
        let part: GeminiPart = serde_json::from_value(unsigned.clone()).unwrap();
        assert_eq!(serde_json::to_value(part).unwrap(), unsigned);
    }

    #[test]
    fn cloud_code_assist_preserves_tool_call_signatures() {
        let source = stream::empty::<std::io::Result<Vec<u8>>>();
        let mut state = StreamState::new(
            SseStream::new(source),
            "gemini-test".to_string(),
            "google-gemini-cli".to_string(),
            "google-gemini-cli".to_string(),
        );
        state
            .process_cloud_code_event(
                r#"{"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{}},"thoughtSignature":"Y2xvdWQ="}]}}]}}"#,
            )
            .unwrap();
        state
            .process_cloud_code_event(r#"{"response":{"candidates":[{"finishReason":"STOP"}]}}"#)
            .unwrap();
        let StreamEvent::Done { reason, message } = state.finish_at_eof().unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(reason, StopReason::ToolUse);
        let ContentBlock::ToolCall(call) = &message.content[0] else {
            panic!("expected tool call");
        };
        assert_eq!(call.thought_signature.as_deref(), Some("Y2xvdWQ="));
    }

    #[test]
    fn terminal_failure_or_length_is_not_overwritten_by_function_calls() {
        for (finish, expected) in [
            ("SAFETY", StopReason::Error),
            ("MAX_TOKENS", StopReason::Length),
        ] {
            for separate_terminal_chunk in [false, true] {
                let source = stream::empty::<std::io::Result<Vec<u8>>>();
                let mut state = StreamState::new(
                    SseStream::new(source),
                    "gemini-test".to_string(),
                    "google".to_string(),
                    "google".to_string(),
                );
                let mut candidate = serde_json::json!({
                    "content": {"parts": [{"functionCall": {"name": "read", "args": {}}}]}
                });
                if !separate_terminal_chunk {
                    candidate["finishReason"] = serde_json::json!(finish);
                }
                state
                    .process_event(&serde_json::json!({"candidates": [candidate]}).to_string())
                    .unwrap();
                if separate_terminal_chunk {
                    state
                        .process_event(
                            &serde_json::json!({
                                "candidates": [{"finishReason": finish}]
                            })
                            .to_string(),
                        )
                        .unwrap();
                }
                let StreamEvent::Done { reason, message } = state.finish_at_eof().unwrap() else {
                    panic!("expected Done");
                };
                assert_eq!(reason, expected);
                assert_eq!(message.stop_reason, expected);
            }
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn failed_generations_reject_adjoining_and_late_calls_across_google_transports() {
        // Published GenerateContent FinishReason values from the Developer
        // API and Vertex. Unknown strings retain the separate mapping test's
        // compatibility behavior.
        let failures = [
            "SAFETY",
            "RECITATION",
            "LANGUAGE",
            "OTHER",
            "BLOCKLIST",
            "PROHIBITED_CONTENT",
            "SPII",
            "MALFORMED_FUNCTION_CALL",
            "MODEL_ARMOR",
            "IMAGE_SAFETY",
            "IMAGE_PROHIBITED_CONTENT",
            "IMAGE_OTHER",
            "NO_IMAGE",
            "IMAGE_RECITATION",
            "UNEXPECTED_TOOL_CALL",
            "TOO_MANY_TOOL_CALLS",
            "MISSING_THOUGHT_SIGNATURE",
            "MALFORMED_RESPONSE",
            "ESCALATION",
            "PUP_LIMITED_DISABLED",
        ];
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        for (api, cloud_cli_mode, service) in [
            ("google-generative-ai", false, "Gemini"),
            ("google-vertex", false, "Vertex AI"),
            ("google-gemini-cli", true, "Gemini"),
        ] {
            for finish in failures {
                for prior_call in [false, true] {
                    let mut frames = Vec::new();
                    if prior_call {
                        frames.push(serde_json::json!({"candidates": [{"content": {
                            "parts": [{"functionCall": {"name": "prior", "args": {}}}]
                        }}]}));
                    }
                    frames.extend([
                        serde_json::json!({"candidates": [{
                            "content": {"parts": [
                                {"text": "discarded failed candidate"},
                                {"functionCall": {"name": "failed", "args": {}}}
                            ]},
                            "finishReason": finish
                        }]}),
                        serde_json::json!({"candidates": [{
                            "content": {"parts": [{"functionCall": {
                                "name": "late", "args": {}
                            }}]},
                            "finishReason": "STOP"
                        }]}),
                        serde_json::json!({"usageMetadata": {
                            "promptTokenCount": 21,
                            "candidatesTokenCount": 7,
                            "thoughtsTokenCount": 3,
                            "totalTokenCount": 31
                        }}),
                    ]);
                    let chunks = frames
                        .into_iter()
                        .map(|frame| {
                            let wire = if cloud_cli_mode {
                                serde_json::json!({"response": frame})
                            } else {
                                frame
                            };
                            Ok(format!("data: {wire}\n\n").into_bytes())
                        })
                        .collect::<Vec<std::io::Result<Vec<u8>>>>();
                    let events = runtime.block_on(async {
                        let source = SseStream::new(stream::iter(chunks));
                        let mut output = Box::pin(
                            StreamState::new(source, "gemini-test".into(), api.into(), api.into())
                                .into_stream(cloud_cli_mode),
                        );
                        let mut events = Vec::new();
                        while let Some(event) = output.next().await {
                            events.push(event.expect("recognized failure is a terminal outcome"));
                        }
                        assert!(output.next().await.is_none());
                        events
                    });
                    let Some(StreamEvent::Done { reason, message }) = events.last() else {
                        panic!("missing terminal failure: {api}/{finish}/{prior_call}: {events:?}");
                    };
                    assert_eq!(*reason, StopReason::Error, "{api}/{finish}/{prior_call}");
                    assert_eq!(message.stop_reason, StopReason::Error);
                    assert_eq!(
                        message.error_message.as_deref(),
                        Some(format!("{service} stopped generation: {finish}").as_str())
                    );
                    assert_eq!(message.usage.input, 21);
                    assert_eq!(message.usage.output, 10);
                    assert_eq!(message.usage.total_tokens, 31);
                    assert_eq!(message.content.len(), usize::from(prior_call));
                    assert_eq!(
                        events
                            .iter()
                            .filter(|event| matches!(event, StreamEvent::Done { .. }))
                            .count(),
                        1
                    );
                    assert_eq!(
                        events
                            .iter()
                            .filter(|event| matches!(event, StreamEvent::ToolCallStart { .. }))
                            .count(),
                        usize::from(prior_call)
                    );
                    assert_eq!(
                        events
                            .iter()
                            .filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
                            .count(),
                        usize::from(prior_call)
                    );
                    for event in &events {
                        if let StreamEvent::ToolCallEnd { tool_call, .. } = event {
                            assert_eq!(tool_call.name, "prior");
                        }
                    }
                }
            }
        }
    }

    #[test]
    #[allow(clippy::format_collect)]
    fn rejected_gemini_tool_batches_never_execute_a_write() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        for finish in [
            "MALFORMED_FUNCTION_CALL",
            "UNEXPECTED_TOOL_CALL",
            "MODEL_ARMOR",
        ] {
            for prior_call in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let marker = dir.path().join("must-not-be-written.txt");
                let candidate = serde_json::json!({"content": {"parts": [{
                    "functionCall": {
                        "name": "write",
                        "args": {"path": marker, "content": "must not execute"}
                    }
                }]}});
                let mut frames = Vec::new();
                if prior_call {
                    frames.push(serde_json::json!({"candidates": [candidate.clone()]}));
                }
                let mut failed = candidate.clone();
                failed["finishReason"] = serde_json::json!(finish);
                frames.push(serde_json::json!({"candidates": [failed]}));
                let mut late = candidate;
                late["finishReason"] = serde_json::json!("STOP");
                frames.push(serde_json::json!({"candidates": [late]}));
                let body = frames
                    .iter()
                    .map(|frame| format!("data: {frame}\n\n"))
                    .collect::<String>();
                let (base_url, _requests) = spawn_test_server(200, "text/event-stream", &body);
                let provider = GeminiProvider::new("gemini-test").with_base_url(base_url);
                let tools = crate::tools::ToolRegistry::from_tools(vec![Box::new(
                    crate::tools::WriteTool::new(dir.path()),
                )]);
                let mut agent = crate::agent::Agent::new(
                    Arc::new(provider),
                    tools,
                    crate::agent::AgentConfig {
                        max_tool_iterations: 1,
                        stream_options: StreamOptions {
                            api_key: Some("test-key".to_string()),
                            ..StreamOptions::default()
                        },
                        ..crate::agent::AgentConfig::default()
                    },
                );
                let executions = Arc::new(AtomicUsize::new(0));
                let observed = Arc::clone(&executions);
                let runtime = RuntimeBuilder::current_thread().build().unwrap();
                let message = runtime
                    .block_on(agent.run("Write the requested file", move |event| {
                        if matches!(event, crate::agent::AgentEvent::ToolExecutionStart { .. }) {
                            observed.fetch_add(1, Ordering::SeqCst);
                        }
                    }))
                    .unwrap();
                assert_eq!(
                    message.stop_reason,
                    StopReason::Error,
                    "{finish}/{prior_call}"
                );
                assert!(
                    message
                        .error_message
                        .as_deref()
                        .is_some_and(|error| error.contains(finish))
                );
                assert_eq!(executions.load(Ordering::SeqCst), 0);
                assert!(
                    !marker.exists(),
                    "a rejected tool batch must not run: {finish}/{prior_call}"
                );
                assert!(
                    !agent
                        .messages()
                        .iter()
                        .any(|message| matches!(message, Message::ToolResult(_)))
                );
            }
        }
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
