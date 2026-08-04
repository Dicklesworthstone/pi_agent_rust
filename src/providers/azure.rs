//! Azure OpenAI Chat Completions API provider implementation.
//!
//! This module implements the Provider trait for Azure OpenAI, using the same
//! streaming protocol as OpenAI but with Azure-specific authentication and endpoints.
//!
//! Azure OpenAI URL format:
//! `https://{resource}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={version}`

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, Usage, UserContent,
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

/// Default Azure OpenAI API version.  Override via `PI_AZURE_API_VERSION`.
pub(crate) const DEFAULT_API_VERSION: &str = "2024-12-01-preview";
const DEFAULT_MAX_TOKENS: u32 = 4096;

pub(crate) fn azure_api_version() -> String {
    std::env::var("PI_AZURE_API_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_API_VERSION.to_string())
}

/// Normalize Azure role names while preserving unknown compat overrides as-is.
fn normalize_role(role: &str) -> String {
    let trimmed = role.trim();
    match trimmed {
        "system" | "developer" | "user" | "assistant" | "tool" | "function" => trimmed.to_string(),
        _ => {
            let lowered = trimmed.to_ascii_lowercase();
            match lowered.as_str() {
                "system" | "developer" | "user" | "assistant" | "tool" | "function" => lowered,
                _ => trimmed.to_string(),
            }
        }
    }
}

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

fn api_key_override(options: &StreamOptions, compat: Option<&CompatConfig>) -> Option<String> {
    super::first_non_empty_header_value_case_insensitive(&options.headers, &["api-key"]).or_else(
        || {
            compat
                .and_then(|compat| compat.custom_headers.as_ref())
                .and_then(|headers| {
                    super::first_non_empty_header_value_case_insensitive(headers, &["api-key"])
                })
        },
    )
}

// ============================================================================
// Azure OpenAI Provider
// ============================================================================

/// Azure OpenAI Chat Completions API provider.
pub struct AzureOpenAIProvider {
    client: Client,
    /// Provider name for event reporting (defaults to "azure").
    provider: String,
    /// The deployment name (model deployment in Azure)
    deployment: String,
    /// Azure resource name (part of the URL)
    resource: String,
    /// API version string
    api_version: String,
    /// Optional override for the full endpoint URL (primarily for deterministic tests).
    endpoint_url_override: Option<String>,
    compat: Option<CompatConfig>,
}

impl AzureOpenAIProvider {
    /// Create a new Azure OpenAI provider.
    ///
    /// # Arguments
    /// * `resource` - Azure OpenAI resource name
    /// * `deployment` - Model deployment name
    pub fn new(resource: impl Into<String>, deployment: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            provider: "azure".to_string(),
            deployment: deployment.into(),
            resource: resource.into(),
            api_version: azure_api_version(),
            endpoint_url_override: None,
            compat: None,
        }
    }

    /// Set the provider name for event reporting.
    #[must_use]
    pub fn with_provider_name(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    /// Set the API version.
    #[must_use]
    pub fn with_api_version(mut self, version: impl Into<String>) -> Self {
        self.api_version = version.into();
        self
    }

    /// Override the full endpoint URL.
    ///
    /// This is intended for deterministic, offline tests (e.g. mock servers). Production
    /// code should rely on the standard Azure endpoint URL format.
    #[must_use]
    pub fn with_endpoint_url(mut self, endpoint_url: impl Into<String>) -> Self {
        self.endpoint_url_override = Some(endpoint_url.into());
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

    /// Get the full endpoint URL.
    fn endpoint_url(&self) -> String {
        if let Some(url) = &self.endpoint_url_override {
            return url.clone();
        }
        format!(
            "https://{}.openai.azure.com/openai/deployments/{}/chat/completions?api-version={}",
            self.resource, self.deployment, self.api_version
        )
    }

    /// Build the request body for Azure OpenAI (same format as OpenAI).
    #[allow(clippy::unused_self)]
    pub fn build_request(&self, context: &Context<'_>, options: &StreamOptions) -> AzureRequest {
        let messages = self.build_messages(context);

        let tools: Option<Vec<AzureTool>> = if context.tools.is_empty() {
            None
        } else {
            Some(context.tools.iter().map(convert_tool_to_azure).collect())
        };

        AzureRequest {
            messages,
            max_tokens: options.max_tokens.or(Some(DEFAULT_MAX_TOKENS)),
            temperature: options.temperature,
            tools,
            stream: true,
            stream_options: Some(AzureStreamOptions {
                include_usage: true,
            }),
        }
    }

    /// Build the messages array with system prompt prepended.
    fn build_messages(&self, context: &Context<'_>) -> Vec<AzureMessage> {
        let mut messages = Vec::new();
        let system_role = self
            .compat
            .as_ref()
            .and_then(|c| c.system_role_name.as_deref())
            .unwrap_or("system");

        // Add system prompt as first message
        if let Some(system) = &context.system_prompt {
            messages.push(AzureMessage {
                role: normalize_role(system_role),
                content: Some(AzureContent::Text(system.to_string())),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        // Convert conversation messages
        for message in context.messages.iter() {
            messages.extend(convert_message_to_azure(message));
        }

        messages
    }
}

#[async_trait]
#[allow(clippy::too_many_lines)]
impl Provider for AzureOpenAIProvider {
    fn name(&self) -> &str {
        &self.provider
    }

    fn api(&self) -> &'static str {
        "azure-openai"
    }

    fn model_id(&self) -> &str {
        &self.deployment
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let has_auth_override = api_key_override(options, self.compat.as_ref()).is_some()
            || authorization_override(options, self.compat.as_ref()).is_some();
        let auth_value = if has_auth_override {
            None
        } else {
            Some(
                options
                    .api_key
                    .clone()
                    .or_else(|| std::env::var("AZURE_OPENAI_API_KEY").ok())
                    .ok_or_else(|| Error::provider("azure-openai", "Missing API key for provider. Configure credentials with /login <provider> or set the provider's API key env var."))?,
            )
        };

        let request_body = self.build_request(context, options);

        let endpoint_url = self.endpoint_url();

        // Build request with Azure-specific headers (Content-Type set by .json() below)
        let mut request = self
            .client
            .post(&endpoint_url)
            .header("Accept", "text/event-stream");

        if let Some(auth_value) = auth_value {
            request = request.header("api-key", &auth_value); // Azure uses api-key header, not Authorization
        }

        // Apply provider-specific custom headers from compat config.
        if let Some(compat) = &self.compat
            && let Some(custom_headers) = &compat.custom_headers
        {
            request = super::apply_headers_ignoring_blank_auth_overrides(
                request,
                custom_headers,
                &["authorization", "api-key"],
            );
        }

        request = super::apply_headers_ignoring_blank_auth_overrides(
            request,
            &options.headers,
            &["authorization", "api-key"],
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
                "azure-openai",
                format!("Azure OpenAI API error (HTTP {status}): {body}"),
            ));
        }

        // Create SSE stream for streaming responses.
        let event_source = SseStream::new(response.bytes_stream());

        // Create stream state
        let model = self.deployment.clone();
        let api = self.api().to_string();
        let provider = self.name().to_string();

        let stream = stream::unfold(
            StreamState::new(event_source, model, api, provider),
            |mut state| async move {
                if state.done {
                    return None;
                }
                loop {
                    if let Some(event) = state.pending_events.pop_front() {
                        return Some((Ok(event), state));
                    }

                    match state.event_source.next().await {
                        Some(Ok(msg)) => {
                            state.transient_error_count = 0;
                            // Azure also sends "[DONE]" as final message
                            if msg.data == "[DONE]" {
                                state.done = true;
                                let reason = state.partial.stop_reason;
                                let message = std::mem::take(&mut state.partial);
                                return Some((Ok(StreamEvent::Done { reason, message }), state));
                            }

                            if let Err(e) = state.process_event(&msg.data) {
                                state.done = true;
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
                            state.done = true;
                            let err = Error::sse(&e);
                            return Some((Err(err), state));
                        }
                        // Stream ended without [DONE] sentinel (e.g.
                        // premature server disconnect).  Emit Done so the
                        // agent loop receives the accumulated partial
                        // instead of silently losing it.
                        None => {
                            state.done = true;
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
    tool_calls: Vec<ToolCallState>,
    pending_events: VecDeque<StreamEvent>,
    started: bool,
    done: bool,
    /// Consecutive WriteZero errors seen without a successful event in between.
    transient_error_count: usize,
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
                stop_details: None,
                error_message: None,
                timestamp: chrono::Utc::now().timestamp_millis(),
            },
            tool_calls: Vec::new(),
            pending_events: VecDeque::new(),
            started: false,
            done: false,
            transient_error_count: 0,
        }
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

    fn push_text_delta(&mut self, text: String) -> StreamEvent {
        let last_is_text = matches!(self.partial.content.last(), Some(ContentBlock::Text(_)));
        if !last_is_text {
            let content_index = self.partial.content.len();
            self.partial
                .content
                .push(ContentBlock::Text(crate::model::TextContent::new("")));
            self.pending_events
                .push_back(StreamEvent::TextStart { content_index });
        }
        let content_index = self.partial.content.len() - 1;

        if let Some(ContentBlock::Text(t)) = self.partial.content.get_mut(content_index) {
            t.text.push_str(&text);
        }

        StreamEvent::TextDelta {
            content_index,
            delta: text,
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

    #[allow(clippy::unnecessary_wraps, clippy::too_many_lines)]
    fn process_event(&mut self, data: &str) -> Result<()> {
        let chunk: AzureStreamChunk = serde_json::from_str(data)
            .map_err(|e| Error::api(format!("JSON parse error: {e}\nData: {data}")))?;

        // Process usage if present
        if let Some(usage) = chunk.usage {
            self.partial.usage.input = usage.prompt_tokens;
            self.partial.usage.output = usage.completion_tokens.unwrap_or(0);
            self.partial.usage.total_tokens = usage.total_tokens;
        }

        let choices = chunk.choices;
        if !self.started {
            let first = choices.first();
            let delta_is_empty = first.is_some_and(|choice| {
                choice.finish_reason.is_none()
                    && choice.delta.content.is_none()
                    && choice.delta.tool_calls.is_none()
            });
            if delta_is_empty {
                self.ensure_started();
                return Ok(());
            }
        }

        // Process choices — handle content deltas BEFORE finish_reason so that
        // TextEnd/ToolCallEnd events always follow the final delta (matching the
        // OpenAI provider event ordering contract).
        for choice in choices {
            // Handle text content
            if let Some(text) = choice.delta.content {
                self.ensure_started();
                let event = self.push_text_delta(text);
                self.pending_events.push_back(event);
            }

            // Handle tool calls
            if let Some(tool_calls) = choice.delta.tool_calls {
                self.ensure_started();

                for tc in tool_calls {
                    let idx = tc.index as usize;

                    // Azure may emit sparse tool-call indices. Match by logical index
                    // instead of assuming contiguous 0..N ordering in arrival order.
                    let tool_state_idx = if let Some(existing_idx) =
                        self.tool_calls.iter().position(|tc| tc.index == idx)
                    {
                        existing_idx
                    } else {
                        let content_index = self.partial.content.len();
                        self.tool_calls.push(ToolCallState {
                            index: idx,
                            content_index,
                            id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                        });

                        // Initialize block in partial
                        self.partial
                            .content
                            .push(ContentBlock::ToolCall(crate::model::ToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: serde_json::Value::Null,
                                thought_signature: None,
                            }));

                        // Emit ToolCallStart (#129: carry the id/name the
                        // opening chunk already provides so agent-side
                        // partials are correlatable from the first delta).
                        self.pending_events.push_back(StreamEvent::ToolCallStart {
                            content_index,
                            id: tc.id.clone().unwrap_or_default(),
                            name: tc
                                .function
                                .as_ref()
                                .and_then(|f| f.name.clone())
                                .unwrap_or_default(),
                        });
                        self.tool_calls.len() - 1
                    };

                    let tc_state = &mut self.tool_calls[tool_state_idx];
                    let content_index = tc_state.content_index;

                    // Update the tool call state
                    if let Some(id) = tc.id {
                        tc_state.id.push_str(&id);
                        if let Some(ContentBlock::ToolCall(block)) =
                            self.partial.content.get_mut(content_index)
                        {
                            block.id.clone_from(&tc_state.id);
                        }
                    }
                    if let Some(func) = tc.function {
                        if let Some(name) = func.name {
                            tc_state.name.push_str(&name);
                            if let Some(ContentBlock::ToolCall(block)) =
                                self.partial.content.get_mut(content_index)
                            {
                                block.name.clone_from(&tc_state.name);
                            }
                        }
                        if let Some(args) = func.arguments {
                            tc_state.arguments.push_str(&args);
                            // Note: we don't update partial arguments here as they need to be valid JSON.
                            // We do that at the end.

                            self.pending_events.push_back(StreamEvent::ToolCallDelta {
                                content_index,
                                delta: args,
                            });
                        }
                    }
                }
            }

            // Handle finish reason (MUST come after delta processing so TextEnd/ToolCallEnd
            // events contain the complete accumulated content).
            // Ensure Start is emitted even when finish arrives in an empty-delta chunk.
            if choice.finish_reason.is_some() {
                self.ensure_started();
            }
            if let Some(reason) = choice.finish_reason {
                self.partial.stop_reason = match reason.as_str() {
                    "length" => StopReason::Length,
                    "content_filter" => StopReason::Error,
                    "tool_calls" => StopReason::ToolUse,
                    // "stop" and any other reason treated as normal stop
                    _ => StopReason::Stop,
                };

                // Finalize tool call arguments
                self.finalize_tool_call_arguments();

                // Emit TextEnd/ThinkingEnd for all open text/thinking blocks.
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

                // Emit ToolCallEnd for each accumulated tool call
                for tc in &self.tool_calls {
                    if let Some(ContentBlock::ToolCall(tool_call)) =
                        self.partial.content.get(tc.content_index)
                    {
                        self.pending_events.push_back(StreamEvent::ToolCallEnd {
                            content_index: tc.content_index,
                            tool_call: tool_call.clone(),
                        });
                    }
                }
            }
        }

        Ok(())
    }
}

// ============================================================================
// Request Types
// ============================================================================

#[derive(Debug, Serialize)]
pub struct AzureRequest {
    messages: Vec<AzureMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AzureTool>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<AzureStreamOptions>,
}

#[derive(Debug, Serialize)]
struct AzureStreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
struct AzureMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<AzureContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<AzureToolCallRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum AzureContent {
    Text(String),
    Parts(Vec<AzureContentPart>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum AzureContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: AzureImageUrl },
}

#[derive(Debug, Serialize)]
struct AzureImageUrl {
    url: String,
}

#[derive(Debug, Serialize)]
struct AzureToolCallRef {
    id: String,
    r#type: &'static str,
    function: AzureFunctionRef,
}

#[derive(Debug, Serialize)]
struct AzureFunctionRef {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct AzureTool {
    r#type: &'static str,
    function: AzureFunction,
}

#[derive(Debug, Serialize)]
struct AzureFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

// ============================================================================
// Streaming Response Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct AzureStreamChunk {
    #[serde(default)]
    choices: Vec<AzureChoice>,
    #[serde(default)]
    usage: Option<AzureUsage>,
}

#[derive(Debug, Deserialize)]
struct AzureChoice {
    delta: AzureDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AzureDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<AzureToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct AzureToolCallDelta {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<AzureFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct AzureFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
struct AzureUsage {
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[allow(dead_code)]
    total_tokens: u64,
}

// ============================================================================
// Conversion Functions
// ============================================================================

#[allow(clippy::too_many_lines)]
fn convert_message_to_azure(message: &Message) -> Vec<AzureMessage> {
    match message {
        Message::User(user) => vec![AzureMessage {
            role: "user".to_string(),
            content: Some(convert_user_content(&user.content)),
            tool_calls: None,
            tool_call_id: None,
        }],
        Message::Custom(custom) => vec![AzureMessage {
            role: "user".to_string(),
            content: Some(AzureContent::Text(custom.content.clone())),
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
                .join("\n\n");

            // Collect tool calls
            let tool_calls: Vec<AzureToolCallRef> = assistant
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall(tc) => Some(AzureToolCallRef {
                        id: tc.id.clone(),
                        r#type: "function",
                        function: AzureFunctionRef {
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
                Some(AzureContent::Text(text))
            };

            let tool_calls = if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            };

            messages.push(AzureMessage {
                role: "assistant".to_string(),
                content,
                tool_calls,
                tool_call_id: None,
            });

            messages
        }
        Message::ToolResult(result) => {
            let mut text_parts = Vec::new();
            let mut image_parts = Vec::new();

            for block in &result.content {
                match block {
                    ContentBlock::Text(t) => text_parts.push(t.text.clone()),
                    ContentBlock::Image(img) => {
                        let url = format!("data:{};base64,{}", img.mime_type, img.data);
                        image_parts.push(AzureContentPart::ImageUrl {
                            image_url: AzureImageUrl { url },
                        });
                    }
                    _ => {}
                }
            }

            let text_content = if text_parts.is_empty() {
                if image_parts.is_empty() {
                    None
                } else {
                    Some(AzureContent::Text("(see attached image)".to_string()))
                }
            } else {
                Some(AzureContent::Text(text_parts.join("\n")))
            };

            let mut messages = vec![AzureMessage {
                role: "tool".to_string(),
                content: text_content,
                tool_calls: None,
                tool_call_id: Some(result.tool_call_id.clone()),
            }];

            if !image_parts.is_empty() {
                let mut parts = vec![AzureContentPart::Text {
                    text: "Attached image(s) from tool result:".to_string(),
                }];
                parts.extend(image_parts);
                messages.push(AzureMessage {
                    role: "user".to_string(),
                    content: Some(AzureContent::Parts(parts)),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }

            messages
        }
    }
}

fn convert_user_content(content: &UserContent) -> AzureContent {
    match content {
        UserContent::Text(text) => AzureContent::Text(text.clone()),
        UserContent::Blocks(blocks) => {
            let parts: Vec<AzureContentPart> = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(t) => Some(AzureContentPart::Text {
                        text: t.text.clone(),
                    }),
                    ContentBlock::Image(img) => {
                        let url = format!("data:{};base64,{}", img.mime_type, img.data);
                        Some(AzureContentPart::ImageUrl {
                            image_url: AzureImageUrl { url },
                        })
                    }
                    _ => None,
                })
                .collect();
            AzureContent::Parts(parts)
        }
    }
}

fn convert_tool_to_azure(tool: &ToolDef) -> AzureTool {
    AzureTool {
        r#type: "function",
        function: AzureFunction {
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
    use crate::model::{ImageContent, TextContent, ToolCall, ToolResultMessage, UserMessage};
    use crate::provider::ToolDef;
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
    fn test_azure_provider_creation() {
        let provider = AzureOpenAIProvider::new("my-resource", "gpt-4");
        assert_eq!(provider.name(), "azure");
        assert_eq!(provider.api(), "azure-openai");
    }

    #[test]
    fn test_azure_model_id_uses_deployment() {
        let provider = AzureOpenAIProvider::new("my-resource", "gpt-4o-mini");
        assert_eq!(provider.model_id(), "gpt-4o-mini");
    }

    #[test]
    fn test_azure_endpoint_url() {
        let provider = AzureOpenAIProvider::new("contoso", "gpt-4-turbo");
        let url = provider.endpoint_url();
        assert!(url.contains("contoso.openai.azure.com"));
        assert!(url.contains("gpt-4-turbo"));
        assert!(url.contains("api-version="));
    }

    #[test]
    fn test_azure_endpoint_url_custom_version() {
        let provider = AzureOpenAIProvider::new("contoso", "gpt-4").with_api_version("2024-06-01");
        let url = provider.endpoint_url();
        assert!(url.contains("api-version=2024-06-01"));
    }

    #[test]
    fn test_azure_endpoint_url_exact_default_shape() {
        let provider = AzureOpenAIProvider::new("contoso", "gpt-4o");
        let url = provider.endpoint_url();
        assert_eq!(
            url,
            "https://contoso.openai.azure.com/openai/deployments/gpt-4o/chat/completions?api-version=2024-12-01-preview"
        );
    }

    #[test]
    fn test_azure_endpoint_url_override_takes_precedence() {
        let provider = AzureOpenAIProvider::new("contoso", "gpt-4o")
            .with_api_version("2025-01-01")
            .with_endpoint_url("http://127.0.0.1:1234/mock-endpoint");
        let url = provider.endpoint_url();
        assert_eq!(url, "http://127.0.0.1:1234/mock-endpoint");
    }

    #[test]
    fn test_azure_build_request_includes_system_messages_and_tools() {
        let provider = AzureOpenAIProvider::new("contoso", "gpt-4o");
        let context = Context {
            system_prompt: Some("You are deterministic.".to_string().into()),
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Hello".to_string()),
                    timestamp: 0,
                }),
                Message::assistant(AssistantMessage {
                    content: vec![
                        ContentBlock::Text(TextContent::new("Need tool output")),
                        ContentBlock::ToolCall(ToolCall {
                            id: "tool_1".to_string(),
                            name: "echo".to_string(),
                            arguments: json!({"text":"ping"}),
                            thought_signature: None,
                        }),
                    ],
                    api: "azure-openai".to_string(),
                    provider: "azure".to_string(),
                    model: "gpt-4o".to_string(),
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    stop_details: None,
                    error_message: None,
                    timestamp: 0,
                }),
            ]
            .into(),
            tools: vec![ToolDef {
                name: "echo".to_string(),
                description: "Echo text".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "text": {"type":"string"}
                    },
                    "required": ["text"]
                }),
            }]
            .into(),
        };
        let options = StreamOptions {
            max_tokens: Some(512),
            temperature: Some(0.0),
            ..Default::default()
        };

        let request = provider.build_request(&context, &options);
        let request_json = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(request_json["max_tokens"], json!(512));
        assert_eq!(request_json["temperature"], json!(0.0));
        assert_eq!(request_json["stream"], json!(true));
        assert_eq!(request_json["messages"][0]["role"], json!("system"));
        assert_eq!(
            request_json["messages"][0]["content"],
            json!("You are deterministic.")
        );
        assert_eq!(request_json["messages"][1]["role"], json!("user"));
        assert_eq!(request_json["messages"][2]["role"], json!("assistant"));
        assert_eq!(request_json["tools"][0]["type"], json!("function"));
        assert_eq!(request_json["tools"][0]["function"]["name"], json!("echo"));
    }

    #[test]
    fn test_azure_build_request_defaults_max_tokens() {
        let provider = AzureOpenAIProvider::new("contoso", "gpt-4o");
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("Hello".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions::default();

        let request = provider.build_request(&context, &options);
        let request_json = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(request_json["max_tokens"], json!(DEFAULT_MAX_TOKENS));
        assert_eq!(request_json["stream"], json!(true));
        assert!(request_json.get("tools").is_none());
    }

    #[test]
    fn test_azure_build_request_normalizes_known_system_role_name() {
        let provider =
            AzureOpenAIProvider::new("contoso", "gpt-4o").with_compat(Some(CompatConfig {
                system_role_name: Some("SYSTEM ".to_string()),
                ..CompatConfig::default()
            }));
        let context = Context {
            system_prompt: Some("You are deterministic.".to_string().into()),
            messages: Vec::new().into(),
            tools: Vec::new().into(),
        };

        let request = provider.build_request(&context, &StreamOptions::default());
        let request_json = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(request_json["messages"][0]["role"], json!("system"));
    }

    #[test]
    fn test_azure_build_request_preserves_unknown_system_role_name() {
        let provider =
            AzureOpenAIProvider::new("contoso", "gpt-4o").with_compat(Some(CompatConfig {
                system_role_name: Some("custom_role".to_string()),
                ..CompatConfig::default()
            }));
        let context = Context {
            system_prompt: Some("You are deterministic.".to_string().into()),
            messages: Vec::new().into(),
            tools: Vec::new().into(),
        };

        let request = provider.build_request(&context, &StreamOptions::default());
        let request_json = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(request_json["messages"][0]["role"], json!("custom_role"));
    }

    #[test]
    fn test_azure_message_conversion() {
        let message = Message::User(UserMessage {
            content: UserContent::Text("Hello".to_string()),
            timestamp: chrono::Utc::now().timestamp_millis(),
        });

        let azure_messages = convert_message_to_azure(&message);
        assert_eq!(azure_messages.len(), 1);
        assert_eq!(azure_messages[0].role, "user");
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
        let fixture = load_fixture("azure_stream.json");
        for case in fixture.cases {
            let events = collect_events(&case.events);
            let summaries: Vec<EventSummary> = events.iter().map(summarize_event).collect();
            assert_eq!(summaries, case.expected, "case {}", case.name);
        }
    }

    #[test]
    fn test_stream_handles_sparse_tool_call_index_without_panic() {
        let events = vec![
            json!({ "choices": [{ "delta": {} }] }),
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 3,
                            "id": "call_sparse",
                            "function": {
                                "name": "lookup",
                                "arguments": "{\"q\":\"azure\"}"
                            }
                        }]
                    }
                }]
            }),
            json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
            Value::String("[DONE]".to_string()),
        ];

        let out = collect_events(&events);
        let done = out
            .iter()
            .find_map(|event| match event {
                StreamEvent::Done { message, .. } => Some(message),
                _ => None,
            })
            .expect("done event");
        let tool_calls: Vec<&ToolCall> = done
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .collect();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_sparse");
        assert_eq!(tool_calls[0].name, "lookup");
        assert_eq!(tool_calls[0].arguments, json!({ "q": "azure" }));
        assert!(
            out.iter()
                .any(|event| matches!(event, StreamEvent::ToolCallStart { .. })),
            "expected tool call start event"
        );
    }

    #[derive(Debug)]
    struct CapturedRequest {
        headers: HashMap<String, String>,
        body: String,
    }

    #[test]
    fn test_stream_compat_api_key_header_works_without_api_key() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let mut custom_headers = HashMap::new();
        custom_headers.insert("api-key".to_string(), "compat-azure-key".to_string());
        let provider = AzureOpenAIProvider::new("contoso", "gpt-4o")
            .with_endpoint_url(base_url)
            .with_compat(Some(CompatConfig {
                custom_headers: Some(custom_headers),
                ..CompatConfig::default()
            }));
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };

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
            captured.headers.get("api-key").map(String::as_str),
            Some("compat-azure-key")
        );
        let body: Value = serde_json::from_str(&captured.body).expect("body json");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn test_stream_compat_authorization_header_works_without_api_key() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let mut custom_headers = HashMap::new();
        custom_headers.insert(
            "Authorization".to_string(),
            "Bearer compat-azure-token".to_string(),
        );
        let provider = AzureOpenAIProvider::new("contoso", "gpt-4o")
            .with_endpoint_url(base_url)
            .with_compat(Some(CompatConfig {
                custom_headers: Some(custom_headers),
                ..CompatConfig::default()
            }));
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };

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
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer compat-azure-token")
        );
        let body: Value = serde_json::from_str(&captured.body).expect("body json");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn test_blank_compat_api_key_header_does_not_override_builtin_api_key() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let mut custom_headers = HashMap::new();
        custom_headers.insert("api-key".to_string(), "   ".to_string());
        let provider = AzureOpenAIProvider::new("contoso", "gpt-4o")
            .with_endpoint_url(base_url)
            .with_compat(Some(CompatConfig {
                custom_headers: Some(custom_headers),
                ..CompatConfig::default()
            }));
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions {
            api_key: Some("fallback-azure-key".to_string()),
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
            captured.headers.get("api-key").map(String::as_str),
            Some("fallback-azure-key")
        );
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
                "gpt-test".to_string(),
                "azure-openai".to_string(),
                "azure".to_string(),
            );
            let mut out = Vec::new();

            while let Some(item) = state.event_source.next().await {
                let msg = item.expect("SSE event");
                if msg.data == "[DONE]" {
                    out.extend(state.pending_events.drain(..));
                    let reason = state.partial.stop_reason;
                    out.push(StreamEvent::Done {
                        reason,
                        message: std::mem::take(&mut state.partial),
                    });
                    break;
                }
                state.process_event(&msg.data).expect("process_event");
                out.extend(state.pending_events.drain(..));
            }

            out
        })
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

        (format!("http://{addr}/azure"), rx)
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

    fn make_tool_result(content: Vec<ContentBlock>) -> Message {
        Message::tool_result(ToolResultMessage {
            tool_call_id: "call_123".to_string(),
            tool_name: "test_tool".to_string(),
            content,
            details: None,
            is_error: false,
            timestamp: 0,
        })
    }

    #[test]
    fn tool_result_text_only_produces_single_tool_message() {
        let msg = make_tool_result(vec![ContentBlock::Text(TextContent {
            text: "result text".to_string(),
            text_signature: None,
        })]);
        let azure_msgs = convert_message_to_azure(&msg);
        assert_eq!(azure_msgs.len(), 1);
        assert_eq!(azure_msgs[0].role, "tool");
        assert_eq!(azure_msgs[0].tool_call_id.as_deref(), Some("call_123"));
        let json = serde_json::to_value(&azure_msgs[0]).expect("serialize");
        assert_eq!(json["content"], "result text");
    }

    #[test]
    fn tool_result_image_only_produces_tool_plus_user_message() {
        let msg = make_tool_result(vec![ContentBlock::Image(ImageContent {
            data: "aW1hZ2U=".to_string(),
            mime_type: "image/png".to_string(),
        })]);
        let azure_msgs = convert_message_to_azure(&msg);
        assert_eq!(
            azure_msgs.len(),
            2,
            "image-only should produce tool + user messages"
        );
        assert_eq!(azure_msgs[0].role, "tool");
        assert_eq!(azure_msgs[1].role, "user");

        let tool_json = serde_json::to_value(&azure_msgs[0]).expect("serialize tool");
        assert_eq!(tool_json["content"], "(see attached image)");

        let user_json = serde_json::to_value(&azure_msgs[1]).expect("serialize user");
        let parts = user_json["content"].as_array().expect("parts array");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert!(
            parts[1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
    }

    #[test]
    fn tool_result_mixed_text_and_image_splits_correctly() {
        let msg = make_tool_result(vec![
            ContentBlock::Text(TextContent {
                text: "line one".to_string(),
                text_signature: None,
            }),
            ContentBlock::Image(ImageContent {
                data: "aW1hZ2U=".to_string(),
                mime_type: "image/jpeg".to_string(),
            }),
            ContentBlock::Text(TextContent {
                text: "line two".to_string(),
                text_signature: None,
            }),
        ]);
        let azure_msgs = convert_message_to_azure(&msg);
        assert_eq!(
            azure_msgs.len(),
            2,
            "mixed content should produce tool + user messages"
        );

        let tool_json = serde_json::to_value(&azure_msgs[0]).expect("serialize tool");
        assert_eq!(tool_json["content"], "line one\nline two");
        assert_eq!(tool_json["tool_call_id"], "call_123");

        let user_json = serde_json::to_value(&azure_msgs[1]).expect("serialize user");
        let parts = user_json["content"].as_array().expect("parts array");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
    }

    #[test]
    fn tool_result_empty_content_produces_single_tool_message_with_no_content() {
        let msg = make_tool_result(vec![]);
        let azure_msgs = convert_message_to_azure(&msg);
        assert_eq!(azure_msgs.len(), 1);
        assert_eq!(azure_msgs[0].role, "tool");
        let json = serde_json::to_value(&azure_msgs[0]).expect("serialize");
        assert!(
            json["content"].is_null(),
            "empty tool result should have null content"
        );
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

    /// Opaque wrapper around the Azure OpenAI stream processor state.
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
                "azure-fuzz".into(),
                "azure-openai".into(),
                "azure".into(),
            ))
        }

        /// Feed one SSE data payload and return any emitted `StreamEvent`s.
        pub fn process_event(&mut self, data: &str) -> crate::error::Result<Vec<StreamEvent>> {
            self.0.process_event(data)?;
            Ok(self.0.pending_events.drain(..).collect())
        }
    }
}
