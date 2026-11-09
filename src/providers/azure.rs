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
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, ThinkingLevel, Usage,
    UserContent,
};
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use crate::sse::SseStream;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::pin::Pin;

mod usage;

// ============================================================================
// Constants
// ============================================================================

/// Default Azure OpenAI API version.  Override via `PI_AZURE_API_VERSION`.
pub(crate) const DEFAULT_API_VERSION: &str = "2024-12-01-preview";
const DEFAULT_MAX_TOKENS: u32 = 4096;
const MAX_READY_EVENTS_PER_YIELD: usize = 64;

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
    /// Deployment names are arbitrary; use the registry's model capability,
    /// never the deployment name, to select reasoning request fields.
    reasoning: bool,
    model_cost: Option<crate::provider::ModelCost>,
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
            reasoning: false,
            model_cost: None,
        }
    }

    /// Set whether the deployment serves a reasoning model.
    ///
    /// Reasoning models use `max_completion_tokens` by default. Sending an
    /// effort also requires `compat.supportsReasoningEffort`, since some
    /// reasoning deployments (such as o1-mini) do not support that parameter.
    #[must_use]
    pub const fn with_reasoning(mut self, reasoning: bool) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// Bind usage normalization to the selected deployment's configured rates.
    #[must_use]
    pub fn with_model_cost(mut self, cost: crate::provider::ModelCost) -> Self {
        self.model_cost = Some(cost);
        self
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
    pub fn build_request(&self, context: &Context<'_>, options: &StreamOptions) -> AzureRequest {
        let messages = self.build_messages(context);
        let compat = self.compat.as_ref();
        let tools_supported = compat.and_then(|c| c.supports_tools).unwrap_or(true);

        let tools: Option<Vec<AzureTool>> = if context.tools.is_empty() || !tools_supported {
            None
        } else {
            Some(context.tools.iter().map(convert_tool_to_azure).collect())
        };

        // Azure reasoning deployments reject max_tokens. An explicit field
        // override remains authoritative, including an explicit max_tokens.
        let use_completion_tokens = compat
            .and_then(|c| c.max_tokens_field.as_deref())
            .map_or(self.reasoning, |field| field == "max_completion_tokens");
        let token_limit = options.max_tokens.or(Some(DEFAULT_MAX_TOKENS));
        let (max_tokens, max_completion_tokens) = if use_completion_tokens {
            (None, token_limit)
        } else {
            (token_limit, None)
        };

        AzureRequest {
            messages,
            max_tokens,
            max_completion_tokens,
            temperature: options.temperature,
            tools,
            stream: true,
            stream_options: Some(AzureStreamOptions {
                include_usage: compat
                    .and_then(|c| c.supports_usage_in_streaming)
                    .unwrap_or(true),
            }),
            reasoning_effort: self.reasoning_effort(options.thinking_level.unwrap_or_default()),
            prompt_cache_key: options.prompt_cache_key.clone(),
        }
    }

    /// Match the plain OpenAI effort dialect: explicit capability opt-in,
    /// per-tier string/null overrides, and no unmapped `off` field.
    fn reasoning_effort(&self, level: ThinkingLevel) -> Option<String> {
        let compat = self.compat.as_ref()?;
        if !self.reasoning || compat.supports_reasoning_effort != Some(true) {
            return None;
        }
        if compat
            .thinking_format
            .as_deref()
            .map(str::trim)
            .filter(|format| !format.is_empty())
            .is_some_and(|format| !format.eq_ignore_ascii_case("openai"))
        {
            return None;
        }

        let name = level.to_string();
        if let Some(mapped) = compat
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&name))
        {
            return mapped.clone();
        }
        (level != ThinkingLevel::Off).then_some(name)
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

        // Azure requires every tool reply before the next user message too.
        // Images from parallel results share one attachment turn (GH #248).
        let mut tool_images = Vec::new();
        for message in context.messages.iter() {
            if !matches!(message, Message::ToolResult(_)) {
                append_tool_result_images(&mut messages, &mut tool_images);
            }
            messages.push(convert_message_to_azure(message, &mut tool_images));
        }
        append_tool_result_images(&mut messages, &mut tool_images);

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

    fn model_cost(&self) -> Option<crate::provider::ModelCost> {
        self.model_cost
            .as_ref()
            .filter(|cost| cost.is_priced())
            .cloned()
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

        let rewritten_body = super::offer_before_provider_request(
            options,
            self.name(),
            self.api(),
            self.model_id(),
            &endpoint_url,
            &request_body,
            |value| {
                super::validate_streamed_json_rewrite(
                    value,
                    &[],
                    &["messages"],
                    &[("stream", serde_json::Value::Bool(true))],
                )
            },
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

        let stream = StreamState::new(event_source, model, api, provider)
            .with_model_cost(self.model_cost.clone())
            .with_usage_observer(options.usage_observer.clone())
            .into_stream();

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
    /// Block-end events and strict tool arguments have been finalized once.
    finalized: bool,
    /// Consecutive WriteZero errors seen without a successful event in between.
    transient_error_count: usize,
    /// Retained across yielded items so draining visible deltas cannot bypass
    /// the same cooperative budget that bounds silent metadata frames.
    ready_work_since_yield: usize,
    usage_observer: Option<crate::provider::UsageObserver>,
    model_cost: Option<crate::provider::ModelCost>,
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
            finalized: false,
            transient_error_count: 0,
            ready_work_since_yield: 0,
            usage_observer: None,
            model_cost: None,
        }
    }

    fn with_model_cost(mut self, cost: Option<crate::provider::ModelCost>) -> Self {
        self.model_cost = cost;
        self
    }

    fn with_usage_observer(mut self, observer: Option<crate::provider::UsageObserver>) -> Self {
        self.usage_observer = observer;
        self
    }

    fn into_stream(self) -> impl Stream<Item = Result<StreamEvent>> {
        stream::unfold(Some(self), |active| async move {
            let mut state = active?;
            loop {
                // The SSE parser budgets each event separately. Bound both
                // parsing silent frames and delivering queued deltas across
                // calls, so a ready stream cannot starve cancellation.
                if state.ready_work_since_yield == MAX_READY_EVENTS_PER_YIELD {
                    yield_stream_turn().await;
                    state.ready_work_since_yield = 0;
                }
                if let Some(event) = state.pending_events.pop_front() {
                    state.ready_work_since_yield += 1;
                    let terminal =
                        matches!(&event, StreamEvent::Done { .. } | StreamEvent::Error { .. });
                    // Retire the transport before exposing its terminal item:
                    // an SDK caller may retain the stream without repolling it.
                    let active = (!terminal).then_some(state);
                    return Some((Ok(event), active));
                }
                // A terminal frame can enqueue block-end events before Done.
                // Drain that queue even after the transport is finished.
                if state.done {
                    return None;
                }

                state.ready_work_since_yield += 1;

                match state.event_source.next().await {
                    Some(Ok(msg)) => {
                        state.transient_error_count = 0;
                        // Azure also sends "[DONE]" as final message
                        if msg.data.trim() == "[DONE]" {
                            state.finish_response();
                            continue;
                        }

                        if let Err(e) = state.process_event(&msg.data) {
                            return Some((Err(e), None));
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
                        let err = Error::sse(&e);
                        return Some((Err(err), None));
                    }
                    // A clean transport EOF is not a successful Azure
                    // OpenAI completion unless [DONE] was observed. The
                    // agent loop retains partial content on this error.
                    None => {
                        return Some((
                            Err(Error::api(
                                "Azure OpenAI stream ended before [DONE] sentinel (unexpected EOF)",
                            )),
                            None,
                        ));
                    }
                }
            }
        })
        .fuse()
    }

    fn record_error(&mut self, message: String) {
        self.partial.stop_reason = StopReason::Error;
        // A trailing finish reason or secondary parse error must not conceal
        // the provider's original failure.
        if self.partial.error_message.is_none() {
            self.partial.error_message = Some(message);
        }
    }

    fn finalize_tool_call_arguments(&mut self) -> Result<()> {
        // Validate the whole batch before exposing any completed tool call.
        // Best-effort streaming snapshots are for display, never execution.
        let mut ids = HashSet::new();
        let arguments = self
            .tool_calls
            .iter()
            .map(|tc| {
                super::openai::validate_final_tool_identity(&mut ids, &tc.id, &tc.name)
                    .and_then(|()| super::openai::parse_final_tool_arguments(&tc.arguments))
                    .map_err(|message| {
                        Error::api(format!(
                            "Invalid Azure OpenAI tool call at index {}: {message}",
                            tc.index
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;

        for (tc, arguments) in self.tool_calls.iter().zip(arguments) {
            if let Some(ContentBlock::ToolCall(block)) =
                self.partial.content.get_mut(tc.content_index)
            {
                block.arguments = arguments;
            }
        }
        Ok(())
    }

    fn finalize_content(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;

        if let Err(error) = self.finalize_tool_call_arguments() {
            self.record_error(error.to_string());
            // Do not persist a synthetically completed argument snapshot as
            // the final arguments of an invalid response.
            for tc in &self.tool_calls {
                if let Some(ContentBlock::ToolCall(block)) =
                    self.partial.content.get_mut(tc.content_index)
                {
                    block.arguments = serde_json::Value::Null;
                }
            }
        }

        for (content_index, block) in self.partial.content.iter().enumerate() {
            match block {
                ContentBlock::Text(text) => {
                    self.pending_events.push_back(StreamEvent::TextEnd {
                        content_index,
                        content: text.text.clone(),
                    });
                }
                ContentBlock::Thinking(thinking) => {
                    self.pending_events.push_back(StreamEvent::ThinkingEnd {
                        content_index,
                        content: thinking.thinking.clone(),
                    });
                }
                _ => {}
            }
        }

        if self.partial.stop_reason != StopReason::Error {
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

    fn finish_response(&mut self) {
        if self.done {
            return;
        }
        self.ensure_started();
        self.finalize_content();
        self.done = true;
        let reason = self.partial.stop_reason;
        let message = std::mem::take(&mut self.partial);
        self.pending_events
            .push_back(StreamEvent::Done { reason, message });
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
        // A valid bill must survive malformed content in the same frame.
        if let Some(usage) = usage::from_chunk(data, self.model_cost.as_ref()) {
            // Replace the whole snapshot: an earlier catalog estimate must
            // never be mistaken for a provider-reported bill on the next frame.
            self.partial.usage = usage;
            // Usage-only frames need not emit a public event. Acknowledge the
            // cumulative amount before another read can stall or be cancelled.
            if let Some(observer) = &self.usage_observer {
                if self.partial.usage.has_explicit_billing() {
                    observer.observe_authoritative(&mut self.partial.usage)?;
                } else {
                    observer.observe(&self.partial.usage)?;
                }
            }
        }

        let chunk: AzureStreamChunk = serde_json::from_str(data)
            .map_err(|e| Error::api(format!("JSON parse error: {e}\nData: {data}")))?;

        if let Some(error) = chunk.error {
            let message = error
                .message
                .as_deref()
                .map(str::trim)
                .filter(|message| !message.is_empty())
                .unwrap_or("Azure OpenAI returned an error while streaming");
            self.record_error(message.to_string());
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
            if self.finalized {
                if matches!(
                    choice.finish_reason.as_deref(),
                    Some("content_filter" | "error")
                ) {
                    self.record_error(
                        "Azure OpenAI reported a terminal stream failure".to_string(),
                    );
                }
                let has_content = choice
                    .delta
                    .content
                    .as_deref()
                    .is_some_and(|text| !text.is_empty())
                    || choice
                        .delta
                        .tool_calls
                        .as_ref()
                        .is_some_and(|calls| !calls.is_empty());
                if has_content {
                    self.record_error(
                        "Azure OpenAI sent content after a terminal finish reason".to_string(),
                    );
                }
                continue;
            }
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
                if self.partial.stop_reason != StopReason::Error {
                    self.partial.stop_reason = match reason.as_str() {
                        "length" => StopReason::Length,
                        "content_filter" | "error" => StopReason::Error,
                        "tool_calls" => StopReason::ToolUse,
                        _ => StopReason::Stop,
                    };
                }
                self.finalize_content();
            }
        }

        Ok(())
    }
}

/// Yield once without a timer or detached task, retaining the consumer's waker.
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

#[cfg(test)]
#[path = "azure_terminal_safety_tests.rs"]
mod terminal_safety_tests;

// ============================================================================
// Request Types
// ============================================================================

#[derive(Debug, Serialize)]
pub struct AzureRequest {
    messages: Vec<AzureMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AzureTool>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<AzureStreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    /// Cache-affinity key (`prompt_cache_key`, gh #188). Omitted when unset
    /// so the wire format is unchanged. See `StreamOptions::prompt_cache_key`.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
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
    error: Option<AzureStreamError>,
}

#[derive(Debug, Deserialize)]
struct AzureStreamError {
    #[serde(default)]
    message: Option<String>,
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

// ============================================================================
// Conversion Functions
// ============================================================================

#[allow(clippy::too_many_lines)]
fn convert_message_to_azure(
    message: &Message,
    tool_images: &mut Vec<AzureContentPart>,
) -> AzureMessage {
    match message {
        Message::User(user) => AzureMessage {
            role: "user".to_string(),
            content: Some(convert_user_content(&user.content)),
            tool_calls: None,
            tool_call_id: None,
        },
        Message::Custom(custom) => AzureMessage {
            role: "user".to_string(),
            content: Some(AzureContent::Text(custom.content.clone())),
            tool_calls: None,
            tool_call_id: None,
        },
        Message::Assistant(assistant) => {
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

            AzureMessage {
                role: "assistant".to_string(),
                content,
                tool_calls,
                tool_call_id: None,
            }
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
                    ContentBlock::Media(media) => text_parts.push(media.placeholder()),
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

            if !image_parts.is_empty() {
                tool_images.push(AzureContentPart::Text {
                    text: format!(
                        "Images from {} (tool call {}):",
                        result.tool_name, result.tool_call_id
                    ),
                });
                tool_images.extend(image_parts);
            }
            AzureMessage {
                role: "tool".to_string(),
                content: text_content,
                tool_calls: None,
                tool_call_id: Some(result.tool_call_id.clone()),
            }
        }
    }
}

fn append_tool_result_images(messages: &mut Vec<AzureMessage>, images: &mut Vec<AzureContentPart>) {
    if images.is_empty() {
        return;
    }
    let mut parts = Vec::with_capacity(images.len() + 1);
    parts.push(AzureContentPart::Text {
        text: "Attached image(s) from tool results:".to_string(),
    });
    parts.append(images);
    messages.push(AzureMessage {
        role: "user".to_string(),
        content: Some(AzureContent::Parts(parts)),
        tool_calls: None,
        tool_call_id: None,
    });
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
                    ContentBlock::Media(media) => Some(AzureContentPart::Text {
                        text: media.placeholder(),
                    }),
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
    fn test_azure_build_request_prompt_cache_key_serialize_and_omit() {
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

        // Set → serialized verbatim (gh #188).
        let options = StreamOptions {
            prompt_cache_key: Some("sess-123".to_string()),
            ..Default::default()
        };
        let request_json = serde_json::to_value(provider.build_request(&context, &options))
            .expect("serialize request");
        assert_eq!(request_json["prompt_cache_key"], json!("sess-123"));

        // Unset → field absent, wire format byte-identical to before.
        let request_json =
            serde_json::to_value(provider.build_request(&context, &StreamOptions::default()))
                .expect("serialize request");
        assert!(request_json.get("prompt_cache_key").is_none());
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
        assert!(request_json.get("max_completion_tokens").is_none());
        assert!(request_json.get("reasoning_effort").is_none());
        assert_eq!(request_json["stream"], json!(true));
        assert_eq!(request_json["stream_options"]["include_usage"], true);
        assert!(request_json.get("tools").is_none());
    }

    #[test]
    fn test_azure_token_limit_field_preserves_explicit_overrides() {
        for (reasoning, field, expected_field, absent_field) in [
            (false, None, "max_tokens", "max_completion_tokens"),
            (true, None, "max_completion_tokens", "max_tokens"),
            (
                false,
                Some("max_completion_tokens"),
                "max_completion_tokens",
                "max_tokens",
            ),
            (
                true,
                Some("max_tokens"),
                "max_tokens",
                "max_completion_tokens",
            ),
        ] {
            let provider = AzureOpenAIProvider::new("contoso", "production")
                .with_reasoning(reasoning)
                .with_compat(Some(CompatConfig {
                    max_tokens_field: field.map(str::to_string),
                    ..CompatConfig::default()
                }));
            for (limit, expected_limit) in [(None, 4096), (Some(1536), 1536)] {
                let options = StreamOptions {
                    max_tokens: limit,
                    ..Default::default()
                };
                let body =
                    serde_json::to_value(provider.build_request(&Context::default(), &options))
                        .expect("request JSON");
                assert_eq!(body[expected_field], expected_limit, "{body}");
                assert!(body.get(absent_field).is_none(), "{body}");
            }
        }
    }

    #[test]
    fn test_azure_reasoning_requires_model_and_effort_capabilities() {
        for (reasoning, supports_effort, format, expected) in [
            (true, Some(true), None, Some("high")),
            (true, Some(true), Some(" OpenAI "), Some("high")),
            (true, Some(true), Some(" "), Some("high")),
            (false, Some(true), None, None),
            (true, Some(false), None, None),
            (true, None, None, None),
            (true, Some(true), Some("deepseek"), None),
            (true, Some(true), Some("openrouter"), None),
        ] {
            // Even a model-looking deployment name must not infer capabilities.
            let provider = AzureOpenAIProvider::new("contoso", "o3")
                .with_reasoning(reasoning)
                .with_compat(Some(CompatConfig {
                    supports_reasoning_effort: supports_effort,
                    thinking_format: format.map(str::to_string),
                    ..CompatConfig::default()
                }));
            let options = StreamOptions {
                thinking_level: Some(ThinkingLevel::High),
                ..Default::default()
            };
            let body = serde_json::to_value(provider.build_request(&Context::default(), &options))
                .expect("request JSON");
            assert_eq!(
                body.get("reasoning_effort").and_then(Value::as_str),
                expected
            );
            assert!(body.get("thinking").is_none(), "{body}");
            assert!(body.get("reasoning").is_none(), "{body}");
        }
    }

    #[test]
    fn test_azure_reasoning_maps_aliases_off_and_explicit_null() {
        let provider = AzureOpenAIProvider::new("contoso", "production")
            .with_reasoning(true)
            .with_compat(Some(CompatConfig {
                supports_reasoning_effort: Some(true),
                thinking_level_map: Some(HashMap::from([
                    ("xhigh".to_string(), Some("high".to_string())),
                    ("off".to_string(), Some("none".to_string())),
                    ("max".to_string(), None),
                ])),
                ..CompatConfig::default()
            }));
        for (level, expected) in [
            (ThinkingLevel::Low, Some("low")),
            (ThinkingLevel::XHigh, Some("high")),
            (ThinkingLevel::Off, Some("none")),
            (ThinkingLevel::Max, None),
        ] {
            let options = StreamOptions {
                thinking_level: Some(level),
                ..Default::default()
            };
            let body = serde_json::to_value(provider.build_request(&Context::default(), &options))
                .expect("request JSON");
            assert_eq!(
                body.get("reasoning_effort").and_then(Value::as_str),
                expected
            );
        }

        let provider = provider.with_compat(Some(CompatConfig {
            supports_reasoning_effort: Some(true),
            ..CompatConfig::default()
        }));
        for level in [None, Some(ThinkingLevel::Off)] {
            let options = StreamOptions {
                thinking_level: level,
                ..Default::default()
            };
            let body = serde_json::to_value(provider.build_request(&Context::default(), &options))
                .expect("request JSON");
            assert!(body.get("reasoning_effort").is_none(), "{body}");
        }
    }

    #[test]
    fn test_azure_factory_preserves_model_reasoning_and_compat_precedence() {
        let dir = tempfile::tempdir().expect("model fixture directory");
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            r#"{
                "providers": {
                    "azure-openai": {
                        "api": "azure-openai",
                        "baseUrl": "https://contoso.openai.azure.com",
                        "compat": {
                            "supportsReasoningEffort": true,
                            "maxTokensField": "max_tokens",
                            "thinkingLevelMap": {"xhigh": "medium"}
                        },
                        "models": [
                            {
                                "id": "production",
                                "reasoning": true,
                                "compat": {"maxTokensField": "max_completion_tokens"},
                                "thinkingLevelMap": {"xhigh": "high"}
                            },
                            {"id": "plain", "reasoning": false},
                            {
                                "id": "effort-disabled",
                                "reasoning": true,
                                "compat": {"supportsReasoningEffort": false}
                            }
                        ]
                    }
                }
            }"#,
        )
        .expect("write models fixture");
        let registry =
            crate::models::ModelRegistry::load_with_credential_resolver(Some(path), |_| None);
        assert!(registry.error().is_none(), "{:?}", registry.error());

        for (model, expected_effort, expected_field) in [
            ("production", Some("high"), "max_completion_tokens"),
            ("plain", None, "max_tokens"),
            ("effort-disabled", None, "max_tokens"),
        ] {
            let entry = registry.find("azure-openai", model).expect("fixture model");
            let provider = crate::providers::create_provider(&entry, None).expect("Azure factory");
            let (tx, rx) = mpsc::channel();
            let options = StreamOptions {
                api_key: Some("azure-test-key".to_string()),
                max_tokens: Some(2048),
                thinking_level: Some(ThinkingLevel::XHigh),
                before_provider_request: Some(crate::provider::BeforeProviderRequestHook::new(
                    move |event| {
                        tx.send(event).expect("capture factory request");
                        // Hold before dispatch, so this exercises the real
                        // factory without opening an Azure network connection.
                        Box::pin(std::future::pending())
                    },
                )),
                ..Default::default()
            };
            let context = Context::default();
            assert!(
                futures::FutureExt::now_or_never(provider.stream(&context, &options)).is_none()
            );
            let captured = rx.try_recv().expect("factory reached request hook");
            assert_eq!(captured.api, "azure-openai");
            assert_eq!(captured.payload[expected_field], 2048);
            assert_eq!(
                captured
                    .payload
                    .get("reasoning_effort")
                    .and_then(Value::as_str),
                expected_effort,
                "{model}: {}",
                captured.payload
            );
        }
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

        let mut tool_images = Vec::new();
        let azure_message = convert_message_to_azure(&message, &mut tool_images);
        assert_eq!(azure_message.role, "user");
        assert!(tool_images.is_empty());
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
    fn test_stream_sends_azure_reasoning_and_capability_overrides() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = AzureOpenAIProvider::new("contoso", "production")
            .with_endpoint_url(base_url)
            .with_reasoning(true)
            .with_compat(Some(CompatConfig {
                supports_reasoning_effort: Some(true),
                supports_tools: Some(false),
                supports_usage_in_streaming: Some(false),
                system_role_name: Some("developer".to_string()),
                thinking_level_map: Some(HashMap::from([(
                    "xhigh".to_string(),
                    Some("high".to_string()),
                )])),
                ..CompatConfig::default()
            }));
        let context = Context::owned(
            Some("Be concise.".to_string()),
            vec![Message::User(UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            vec![ToolDef {
                name: "lookup".to_string(),
                description: "Look up a fact".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            }],
        );
        let options = StreamOptions {
            api_key: Some("azure-test-key".to_string()),
            max_tokens: Some(8192),
            thinking_level: Some(ThinkingLevel::XHigh),
            ..Default::default()
        };
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider.stream(&context, &options).await.expect("stream");
            let mut done = false;
            while let Some(event) = stream.next().await {
                done |= matches!(event.expect("stream event"), StreamEvent::Done { .. });
            }
            assert!(done, "Azure response should complete");
        });

        let captured = rx.recv_timeout(Duration::from_secs(2)).expect("captured");
        let body: Value = serde_json::from_str(&captured.body).expect("body JSON");
        assert_eq!(body["max_completion_tokens"], 8192);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["messages"][0]["role"], "developer");
        assert_eq!(body["messages"][1]["content"], "ping");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], false);
        assert!(body.get("tools").is_none());
        assert_eq!(
            captured.headers.get("api-key").map(String::as_str),
            Some("azure-test-key")
        );
    }

    #[test]
    fn test_stream_cache_discount_controls_shared_spend_admission() {
        for (cached, input, expected_cost, exhausted) in [
            (Some(1500), 500, 0.002_15, false),
            (None, 2000, 0.004_4, true),
        ] {
            let mut usage = json!({
                "prompt_tokens": 2000,
                "completion_tokens": 100,
                "total_tokens": 2100,
            });
            if let Some(cached) = cached {
                usage["prompt_tokens_details"] = json!({"cached_tokens": cached});
            }
            let response = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"answer\"}},\"finish_reason\":\"stop\"}}]}}\n\n\
                 data: {{\"choices\":[],\"usage\":{usage}}}\n\n\
                 data: [DONE]\n\n"
            );
            let (url, captured) = spawn_test_server(200, "text/event-stream", &response);
            let priced = std::sync::Arc::new(crate::providers::PricedProvider {
                inner: std::sync::Arc::new(
                    AzureOpenAIProvider::new("contoso", "billing-fixture").with_endpoint_url(url),
                ),
                cost: crate::provider::ModelCost {
                    input: 2.0,
                    output: 4.0,
                    cache_read: 0.5,
                    ..crate::provider::ModelCost::default()
                },
            });
            let budget = crate::cost::CostBudget::new(None, Some(0.003), 0.0).unwrap();
            let provider = crate::provider::guard_with_cost_budget(priced, budget.clone());
            let runtime = RuntimeBuilder::current_thread().build().unwrap();
            runtime.block_on(async {
                let context = Context::default();
                let options = StreamOptions {
                    api_key: Some("azure-test-key".to_string()),
                    ..Default::default()
                };
                let mut events = provider.stream(&context, &options).await.unwrap();
                let mut done = 0;
                while let Some(event) = events.next().await {
                    if let StreamEvent::Done { message, .. } = event.expect("stream event") {
                        done += 1;
                        assert_eq!(message.usage.input, input);
                        assert_eq!(message.usage.cache_read, cached.unwrap_or(0));
                        assert_eq!(message.usage.output, 100);
                        assert_eq!(message.usage.total_tokens, 2100);
                        assert!((message.usage.cost.total - expected_cost).abs() < 1e-12);
                    }
                }
                assert_eq!(done, 1);
                if exhausted {
                    assert!(matches!(
                        provider.stream(&context, &options).await,
                        Err(Error::CostCap { .. })
                    ));
                }
            });
            assert!((budget.spent() - expected_cost).abs() < 1e-12);
            assert_eq!(budget.snapshot().pending, 0.0);
            assert_eq!(budget.admit().is_err(), exhausted);
            assert!(captured.recv_timeout(Duration::from_secs(2)).is_ok());
        }
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

    #[test]
    fn stream_rejects_transport_eof_before_done_sentinel() {
        let body = [
            r#"data: {"choices":[{"delta":{"content":"partial"}}]}"#,
            "",
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
            "",
        ]
        .join("\n");

        let out = collect_stream_items_from_body(&body);
        assert!(
            out.iter()
                .any(|item| matches!(item, Ok(StreamEvent::TextDelta { delta, .. }) if delta == "partial")),
            "partial content should be emitted before the terminal error"
        );
        assert!(
            !out.iter()
                .any(|item| matches!(item, Ok(StreamEvent::Done { .. }))),
            "premature EOF must never be reported as Done"
        );
        let error = out
            .last()
            .expect("terminal stream item")
            .as_ref()
            .expect_err("premature EOF must be a stream error")
            .to_string();
        assert!(error.contains("[DONE]"), "{error}");
        assert!(error.contains("unexpected EOF"), "{error}");
        assert!(crate::error::is_retryable_error(&error, None, None));
    }

    #[test]
    fn stream_emits_exactly_one_done_when_transport_ends_after_done_sentinel() {
        let body = [
            r#"data: {"choices":[{"delta":{"content":"whole"}}]}"#,
            "",
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
            "",
            "data: [DONE]",
            "",
        ]
        .join("\n");

        let out = collect_stream_items_from_body(&body);
        assert!(out.iter().all(Result::is_ok), "complete stream: {out:?}");
        let done = out
            .iter()
            .filter(|item| matches!(item, Ok(StreamEvent::Done { .. })))
            .count();
        assert_eq!(done, 1, "a complete stream finishes exactly once: {out:?}");
        assert!(
            matches!(out.last(), Some(Ok(StreamEvent::Done { .. }))),
            "Done must be the terminal item: {out:?}"
        );
    }

    fn collect_stream_items_from_body(body: &str) -> Vec<Result<StreamEvent>> {
        let (base_url, _rx) = spawn_test_server(200, "text/event-stream", body);
        let provider = AzureOpenAIProvider::new("contoso", "gpt-test").with_endpoint_url(base_url);
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
            api_key: Some("azure-test-key".to_string()),
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

    fn request_messages(messages: Vec<Message>) -> Vec<AzureMessage> {
        let provider = AzureOpenAIProvider::new("contoso", "gpt-4o");
        let context = Context {
            system_prompt: None,
            messages: messages.into(),
            tools: Vec::new().into(),
        };
        provider
            .build_request(&context, &StreamOptions::default())
            .messages
    }

    fn tool_call_message(ids: &[&str]) -> Message {
        Message::assistant(AssistantMessage {
            content: ids
                .iter()
                .map(|id| {
                    ContentBlock::ToolCall(ToolCall {
                        id: (*id).to_string(),
                        name: "test_tool".to_string(),
                        arguments: json!({"path": id}),
                        thought_signature: None,
                    })
                })
                .collect(),
            stop_reason: StopReason::ToolUse,
            ..AssistantMessage::default()
        })
    }

    fn image_block(data: &str, mime_type: &str) -> ContentBlock {
        ContentBlock::Image(ImageContent {
            data: data.to_string(),
            mime_type: mime_type.to_string(),
        })
    }

    fn make_tool_result(id: &str, content: Vec<ContentBlock>) -> Message {
        Message::tool_result(ToolResultMessage {
            tool_call_id: id.to_string(),
            tool_name: "test_tool".to_string(),
            content,
            details: None,
            is_error: false,
            timestamp: 0,
        })
    }

    #[test]
    fn tool_result_text_only_produces_single_tool_message() {
        let msg = make_tool_result(
            "call_123",
            vec![ContentBlock::Text(TextContent {
                text: "result text".to_string(),
                text_signature: None,
            })],
        );
        let azure_msgs = request_messages(vec![msg]);
        assert_eq!(azure_msgs.len(), 1);
        assert_eq!(azure_msgs[0].role, "tool");
        assert_eq!(azure_msgs[0].tool_call_id.as_deref(), Some("call_123"));
        let json = serde_json::to_value(&azure_msgs[0]).expect("serialize");
        assert_eq!(json["content"], "result text");
    }

    #[test]
    fn tool_result_image_only_produces_tool_plus_user_message() {
        let msg = make_tool_result(
            "call_123",
            vec![ContentBlock::Image(ImageContent {
                data: "aW1hZ2U=".to_string(),
                mime_type: "image/png".to_string(),
            })],
        );
        let azure_msgs = request_messages(vec![msg]);
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
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(
            parts[1]["text"],
            "Images from test_tool (tool call call_123):"
        );
        assert_eq!(parts[2]["type"], "image_url");
        assert!(
            parts[2]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
    }

    #[test]
    fn tool_result_mixed_text_and_image_splits_correctly() {
        let msg = make_tool_result(
            "call_123",
            vec![
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
            ],
        );
        let azure_msgs = request_messages(vec![msg]);
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
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(
            parts[1]["text"],
            "Images from test_tool (tool call call_123):"
        );
        assert_eq!(parts[2]["type"], "image_url");
    }

    #[test]
    fn tool_result_empty_content_produces_single_tool_message_with_no_content() {
        let msg = make_tool_result("call_123", vec![]);
        let azure_msgs = request_messages(vec![msg]);
        assert_eq!(azure_msgs.len(), 1);
        assert_eq!(azure_msgs[0].role, "tool");
        let json = serde_json::to_value(&azure_msgs[0]).expect("serialize");
        assert!(
            json["content"].is_null(),
            "empty tool result should have null content"
        );
    }

    #[test]
    fn parallel_tool_results_precede_one_ordered_image_bundle() {
        let wire = serde_json::to_value(request_messages(vec![
            tool_call_message(&["call_a", "call_b", "call_c", "call_empty"]),
            make_tool_result(
                "call_a",
                vec![
                    ContentBlock::Text(TextContent::new("first result")),
                    image_block("AQ==", "image/png"),
                    image_block("Ag==", "image/jpeg"),
                ],
            ),
            Message::tool_result(ToolResultMessage {
                tool_call_id: "call_b".to_string(),
                tool_name: "test_tool".to_string(),
                content: vec![ContentBlock::Text(TextContent::new("read failed"))],
                details: None,
                is_error: true,
                timestamp: 0,
            }),
            make_tool_result("call_c", vec![image_block("Aw==", "image/webp")]),
            make_tool_result("call_empty", Vec::new()),
        ]))
        .expect("serialize request messages");
        let messages = wire.as_array().expect("messages");
        assert_eq!(
            messages
                .iter()
                .map(|m| m["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["assistant", "tool", "tool", "tool", "tool", "user"]
        );
        for (index, id) in ["call_a", "call_b", "call_c", "call_empty"]
            .iter()
            .enumerate()
        {
            assert_eq!(messages[0]["tool_calls"][index]["id"], *id);
            assert_eq!(messages[index + 1]["tool_call_id"], *id);
        }
        assert_eq!(messages[1]["content"], "first result");
        assert_eq!(messages[2]["content"], "read failed");
        assert_eq!(messages[3]["content"], "(see attached image)");
        assert!(messages[4]["content"].is_null());
        assert_eq!(
            messages[5]["content"],
            json!([
                {"type": "text", "text": "Attached image(s) from tool results:"},
                {"type": "text", "text": "Images from test_tool (tool call call_a):"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AQ=="}},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,Ag=="}},
                {"type": "text", "text": "Images from test_tool (tool call call_c):"},
                {"type": "image_url", "image_url": {"url": "data:image/webp;base64,Aw=="}}
            ])
        );
    }

    #[test]
    fn tool_image_bundles_stay_with_their_run_and_preserve_user_images() {
        let wire = serde_json::to_value(request_messages(vec![
            Message::User(UserMessage {
                content: UserContent::Blocks(vec![image_block("AQ==", "image/png")]),
                timestamp: 0,
            }),
            tool_call_message(&["call_a"]),
            make_tool_result("call_a", vec![image_block("Ag==", "image/jpeg")]),
            tool_call_message(&["call_b"]),
            make_tool_result("call_b", vec![image_block("Aw==", "image/webp")]),
            Message::User(UserMessage {
                content: UserContent::Blocks(vec![image_block("BA==", "image/png")]),
                timestamp: 0,
            }),
        ]))
        .expect("serialize request messages");
        let messages = wire.as_array().expect("messages");
        assert_eq!(
            messages
                .iter()
                .map(|m| m["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "user",
                "assistant",
                "tool",
                "user",
                "assistant",
                "tool",
                "user",
                "user"
            ]
        );
        for (message_index, part_index, expected) in [
            (0, 0, "data:image/png;base64,AQ=="),
            (3, 2, "data:image/jpeg;base64,Ag=="),
            (6, 2, "data:image/webp;base64,Aw=="),
            (7, 0, "data:image/png;base64,BA=="),
        ] {
            assert_eq!(
                messages[message_index]["content"][part_index]["image_url"]["url"],
                expected
            );
        }
        assert_eq!(messages[3]["content"].as_array().unwrap().len(), 3);
        assert_eq!(messages[6]["content"].as_array().unwrap().len(), 3);
        assert_eq!(
            messages[3]["content"][1]["text"],
            "Images from test_tool (tool call call_a):"
        );
        assert_eq!(
            messages[6]["content"][1]["text"],
            "Images from test_tool (tool call call_b):"
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
