//! OpenAI Responses API provider implementation.
//!
//! This module implements the Provider trait for the OpenAI `responses` endpoint,
//! supporting streaming output text and function tool calls.

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ThinkingContent,
    ToolCall, Usage, UserContent,
};
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use crate::sse::SseStream;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;

// ============================================================================
// Constants
// ============================================================================

const OPENAI_RESPONSES_API_URL: &str = "https://api.openai.com/v1/responses";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4096;

// ============================================================================
// OpenAI Responses Provider
// ============================================================================

/// OpenAI Responses API provider.
pub struct OpenAIResponsesProvider {
    client: Client,
    model: String,
    base_url: String,
    provider: String,
}

impl OpenAIResponsesProvider {
    /// Create a new OpenAI Responses provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            base_url: OPENAI_RESPONSES_API_URL.to_string(),
            provider: "openai".to_string(),
        }
    }

    /// Override the provider name reported in streamed events.
    #[must_use]
    pub fn with_provider_name(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
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

    fn build_request(&self, context: &Context, options: &StreamOptions) -> OpenAIResponsesRequest {
        let input = build_openai_responses_input(context);
        let tools: Option<Vec<OpenAIResponsesTool>> = if context.tools.is_empty() {
            None
        } else {
            Some(
                context
                    .tools
                    .iter()
                    .map(convert_tool_to_openai_responses)
                    .collect(),
            )
        };

        OpenAIResponsesRequest {
            model: self.model.clone(),
            input,
            temperature: options.temperature,
            max_output_tokens: options.max_tokens.or(Some(DEFAULT_MAX_OUTPUT_TOKENS)),
            tools,
            stream: true,
        }
    }
}

#[async_trait]
impl Provider for OpenAIResponsesProvider {
    fn name(&self) -> &str {
        &self.provider
    }

    fn api(&self) -> &'static str {
        "openai-responses"
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

        let mut request = self
            .client
            .post(&self.base_url)
            .header("Content-Type", "application/json")
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

        let event_source = SseStream::new(response.bytes_stream());

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

                    // We may have marked the stream finished (e.g. after receiving
                    // response.completed) but still need to drain queued events (ToolCallEnd,
                    // Done, etc). Only stop once the queue is empty.
                    if state.finished {
                        return None;
                    }

                    match state.event_source.next().await {
                        Some(Ok(msg)) => {
                            if msg.data == "[DONE]" {
                                // Best-effort fallback: if we didn't see a completed/incomplete
                                // chunk, emit Done using current state.
                                state.finish(None);
                                continue;
                            }

                            if let Err(e) = state.process_event(&msg.data) {
                                return Some((Err(e), state));
                            }
                        }
                        Some(Err(e)) => {
                            let err = Error::api(format!("SSE error: {e}"));
                            return Some((Err(err), state));
                        }
                        None => {
                            // If the stream ends unexpectedly, surface an error. This matches the
                            // agent loop expectation that providers emit Done/Error explicitly.
                            return Some((
                                Err(Error::api("Stream ended without Done event")),
                                state,
                            ));
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TextKey {
    item_id: String,
    content_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReasoningKey {
    item_id: String,
    summary_index: u32,
}

struct ToolCallState {
    content_index: usize,
    call_id: String,
    name: String,
    arguments: String,
}

struct StreamState<S>
where
    S: Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin,
{
    event_source: SseStream<S>,
    partial: AssistantMessage,
    pending_events: VecDeque<StreamEvent>,
    started: bool,
    finished: bool,
    text_blocks: HashMap<TextKey, usize>,
    reasoning_blocks: HashMap<ReasoningKey, usize>,
    tool_calls_by_item_id: HashMap<String, ToolCallState>,
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
            pending_events: VecDeque::new(),
            started: false,
            finished: false,
            text_blocks: HashMap::new(),
            reasoning_blocks: HashMap::new(),
            tool_calls_by_item_id: HashMap::new(),
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

    fn text_block_for(&mut self, item_id: String, content_index: u32) -> usize {
        let key = TextKey {
            item_id,
            content_index,
        };
        if let Some(idx) = self.text_blocks.get(&key) {
            return *idx;
        }

        let idx = self.partial.content.len();
        self.partial
            .content
            .push(ContentBlock::Text(TextContent::new("")));
        self.text_blocks.insert(key, idx);
        self.pending_events.push_back(StreamEvent::TextStart {
            content_index: idx,
            partial: self.partial.clone(),
        });
        idx
    }

    fn reasoning_block_for(&mut self, item_id: String, summary_index: u32) -> usize {
        let key = ReasoningKey {
            item_id,
            summary_index,
        };
        if let Some(idx) = self.reasoning_blocks.get(&key) {
            return *idx;
        }

        let idx = self.partial.content.len();
        self.partial
            .content
            .push(ContentBlock::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: None,
            }));
        self.reasoning_blocks.insert(key, idx);
        self.pending_events.push_back(StreamEvent::ThinkingStart {
            content_index: idx,
            partial: self.partial.clone(),
        });
        idx
    }

    #[allow(clippy::too_many_lines)]
    fn process_event(&mut self, data: &str) -> Result<()> {
        let chunk: OpenAIResponsesChunk =
            serde_json::from_str(data).map_err(|e| Error::api(format!("JSON parse error: {e}")))?;

        match chunk {
            OpenAIResponsesChunk::OutputTextDelta {
                item_id,
                content_index,
                delta,
            } => {
                self.ensure_started();
                let idx = self.text_block_for(item_id, content_index);
                if let Some(ContentBlock::Text(t)) = self.partial.content.get_mut(idx) {
                    t.text.push_str(&delta);
                }
                self.pending_events.push_back(StreamEvent::TextDelta {
                    content_index: idx,
                    delta,
                    partial: self.partial.clone(),
                });
            }
            OpenAIResponsesChunk::ReasoningSummaryTextDelta {
                item_id,
                summary_index,
                delta,
            } => {
                self.ensure_started();
                let idx = self.reasoning_block_for(item_id, summary_index);
                if let Some(ContentBlock::Thinking(t)) = self.partial.content.get_mut(idx) {
                    t.thinking.push_str(&delta);
                }
                self.pending_events.push_back(StreamEvent::ThinkingDelta {
                    content_index: idx,
                    delta,
                    partial: self.partial.clone(),
                });
            }
            OpenAIResponsesChunk::OutputItemAdded { item } => {
                if let OpenAIResponsesOutputItem::FunctionCall {
                    id,
                    call_id,
                    name,
                    arguments,
                } = item
                {
                    self.ensure_started();

                    let content_index = self.partial.content.len();
                    self.partial.content.push(ContentBlock::ToolCall(ToolCall {
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: serde_json::Value::Null,
                        thought_signature: None,
                    }));

                    self.tool_calls_by_item_id.insert(
                        id,
                        ToolCallState {
                            content_index,
                            call_id,
                            name,
                            arguments: arguments.clone(),
                        },
                    );

                    self.pending_events.push_back(StreamEvent::ToolCallStart {
                        content_index,
                        partial: self.partial.clone(),
                    });

                    if !arguments.is_empty() {
                        self.pending_events.push_back(StreamEvent::ToolCallDelta {
                            content_index,
                            delta: arguments,
                            partial: self.partial.clone(),
                        });
                    }
                }
            }
            OpenAIResponsesChunk::FunctionCallArgumentsDelta { item_id, delta } => {
                self.ensure_started();
                if let Some(tc) = self.tool_calls_by_item_id.get_mut(&item_id) {
                    tc.arguments.push_str(&delta);
                    self.pending_events.push_back(StreamEvent::ToolCallDelta {
                        content_index: tc.content_index,
                        delta,
                        partial: self.partial.clone(),
                    });
                }
            }
            OpenAIResponsesChunk::OutputItemDone { item } => {
                if let OpenAIResponsesOutputItemDone::FunctionCall {
                    id,
                    call_id,
                    name,
                    arguments,
                } = item
                {
                    self.ensure_started();
                    self.end_tool_call(&id, &call_id, &name, &arguments);
                }
            }
            OpenAIResponsesChunk::ResponseCompleted { response }
            | OpenAIResponsesChunk::ResponseIncomplete { response } => {
                self.ensure_started();
                self.partial.usage.input = response.usage.input_tokens;
                self.partial.usage.output = response.usage.output_tokens;
                self.partial.usage.total_tokens = response
                    .usage
                    .total_tokens
                    .unwrap_or(response.usage.input_tokens + response.usage.output_tokens);

                self.finish(response.incomplete_reason());
            }
            OpenAIResponsesChunk::Error { message } => {
                self.ensure_started();
                self.partial.stop_reason = StopReason::Error;
                self.partial.error_message = Some(message);
                self.pending_events.push_back(StreamEvent::Error {
                    reason: StopReason::Error,
                    error: self.partial.clone(),
                });
                self.finished = true;
            }
            OpenAIResponsesChunk::Unknown => {}
        }

        Ok(())
    }

    fn partial_has_tool_call(&self) -> bool {
        self.partial
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall(_)))
    }

    fn end_tool_call(&mut self, item_id: &str, call_id: &str, name: &str, arguments: &str) {
        let mut tc = self
            .tool_calls_by_item_id
            .remove(item_id)
            .unwrap_or_else(|| {
                // If we missed the added event, synthesize a content slot now.
                let content_index = self.partial.content.len();
                self.partial.content.push(ContentBlock::ToolCall(ToolCall {
                    id: call_id.to_string(),
                    name: name.to_string(),
                    arguments: serde_json::Value::Null,
                    thought_signature: None,
                }));
                ToolCallState {
                    content_index,
                    call_id: call_id.to_string(),
                    name: name.to_string(),
                    arguments: String::new(),
                }
            });

        // Prefer the final arguments field when present.
        if !arguments.is_empty() {
            tc.arguments = arguments.to_string();
        }

        let parsed_args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_else(|e| {
            tracing::warn!(error = %e, raw = %tc.arguments, "Failed to parse tool arguments as JSON");
            serde_json::Value::Null
        });

        if let Some(ContentBlock::ToolCall(block)) = self.partial.content.get_mut(tc.content_index)
        {
            block.id.clone_from(&tc.call_id);
            block.name.clone_from(&tc.name);
            block.arguments = parsed_args.clone();
        }

        self.partial.stop_reason = StopReason::ToolUse;
        self.pending_events.push_back(StreamEvent::ToolCallEnd {
            content_index: tc.content_index,
            tool_call: ToolCall {
                id: tc.call_id,
                name: tc.name,
                arguments: parsed_args,
                thought_signature: None,
            },
            partial: self.partial.clone(),
        });
    }

    fn finish(&mut self, incomplete_reason: Option<String>) {
        if self.finished {
            return;
        }

        // Best-effort: close any tool calls we didn't see "done" for.
        let ids: Vec<String> = self.tool_calls_by_item_id.keys().cloned().collect();
        for id in ids {
            // Clone metadata first (end_tool_call removes the state).
            let (call_id, name, arguments) = match self.tool_calls_by_item_id.get(&id) {
                Some(tc) => (tc.call_id.clone(), tc.name.clone(), tc.arguments.clone()),
                None => continue,
            };
            self.end_tool_call(&id, &call_id, &name, &arguments);
        }

        // Infer stop reason.
        if let Some(reason) = incomplete_reason {
            let reason_lower = reason.to_ascii_lowercase();
            if reason_lower.contains("max_output") || reason_lower.contains("length") {
                self.partial.stop_reason = StopReason::Length;
            } else if reason_lower.contains("tool") {
                self.partial.stop_reason = StopReason::ToolUse;
            } else if reason_lower.contains("content_filter") || reason_lower.contains("error") {
                self.partial.stop_reason = StopReason::Error;
            }
        } else if self.partial_has_tool_call() {
            self.partial.stop_reason = StopReason::ToolUse;
        }

        let reason = self.partial.stop_reason;
        self.pending_events.push_back(StreamEvent::Done {
            reason,
            message: self.partial.clone(),
        });
        self.finished = true;
    }
}

// ============================================================================
// OpenAI Responses API Types (minimal)
// ============================================================================

#[derive(Debug, Serialize)]
struct OpenAIResponsesRequest {
    model: String,
    input: Vec<OpenAIResponsesInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAIResponsesTool>>,
    stream: bool,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum OpenAIResponsesInputItem {
    System {
        role: &'static str,
        content: String,
    },
    User {
        role: &'static str,
        content: Vec<OpenAIResponsesUserContentPart>,
    },
    Assistant {
        role: &'static str,
        content: Vec<OpenAIResponsesAssistantContentPart>,
    },
    FunctionCall {
        #[serde(rename = "type")]
        r#type: &'static str,
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        #[serde(rename = "type")]
        r#type: &'static str,
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAIResponsesUserContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "input_image")]
    InputImage { image_url: String },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAIResponsesAssistantContentPart {
    #[serde(rename = "output_text")]
    OutputText { text: String },
}

#[derive(Debug, Serialize)]
struct OpenAIResponsesTool {
    #[serde(rename = "type")]
    r#type: &'static str,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: serde_json::Value,
}

fn convert_tool_to_openai_responses(tool: &ToolDef) -> OpenAIResponsesTool {
    OpenAIResponsesTool {
        r#type: "function",
        name: tool.name.clone(),
        description: if tool.description.trim().is_empty() {
            None
        } else {
            Some(tool.description.clone())
        },
        parameters: tool.parameters.clone(),
    }
}

fn build_openai_responses_input(context: &Context) -> Vec<OpenAIResponsesInputItem> {
    let mut input = Vec::new();

    if let Some(system) = &context.system_prompt {
        input.push(OpenAIResponsesInputItem::System {
            role: "system",
            content: system.clone(),
        });
    }

    for message in &context.messages {
        match message {
            Message::User(user) => input.push(convert_user_message_to_responses(&user.content)),
            Message::Custom(custom) => input.push(OpenAIResponsesInputItem::User {
                role: "user",
                content: vec![OpenAIResponsesUserContentPart::InputText {
                    text: custom.content.clone(),
                }],
            }),
            Message::Assistant(assistant) => {
                // Preserve ordering between text and tool calls.
                let mut pending_text = String::new();

                for block in &assistant.content {
                    match block {
                        ContentBlock::Text(t) => pending_text.push_str(&t.text),
                        ContentBlock::ToolCall(tc) => {
                            if !pending_text.is_empty() {
                                input.push(OpenAIResponsesInputItem::Assistant {
                                    role: "assistant",
                                    content: vec![
                                        OpenAIResponsesAssistantContentPart::OutputText {
                                            text: std::mem::take(&mut pending_text),
                                        },
                                    ],
                                });
                            }
                            input.push(OpenAIResponsesInputItem::FunctionCall {
                                r#type: "function_call",
                                call_id: tc.id.clone(),
                                name: tc.name.clone(),
                                arguments: tc.arguments.to_string(),
                            });
                        }
                        _ => {}
                    }
                }

                if !pending_text.is_empty() {
                    input.push(OpenAIResponsesInputItem::Assistant {
                        role: "assistant",
                        content: vec![OpenAIResponsesAssistantContentPart::OutputText {
                            text: pending_text,
                        }],
                    });
                }
            }
            Message::ToolResult(result) => {
                let mut out = String::new();
                for (i, block) in result.content.iter().enumerate() {
                    if i > 0 {
                        out.push('\n');
                    }
                    if let ContentBlock::Text(t) = block {
                        out.push_str(&t.text);
                    }
                }
                input.push(OpenAIResponsesInputItem::FunctionCallOutput {
                    r#type: "function_call_output",
                    call_id: result.tool_call_id.clone(),
                    output: out,
                });
            }
        }
    }

    input
}

fn convert_user_message_to_responses(content: &UserContent) -> OpenAIResponsesInputItem {
    match content {
        UserContent::Text(text) => OpenAIResponsesInputItem::User {
            role: "user",
            content: vec![OpenAIResponsesUserContentPart::InputText { text: text.clone() }],
        },
        UserContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text(t) => {
                        parts.push(OpenAIResponsesUserContentPart::InputText {
                            text: t.text.clone(),
                        });
                    }
                    ContentBlock::Image(img) => {
                        let url = format!("data:{};base64,{}", img.mime_type, img.data);
                        parts.push(OpenAIResponsesUserContentPart::InputImage { image_url: url });
                    }
                    _ => {}
                }
            }
            if parts.is_empty() {
                parts.push(OpenAIResponsesUserContentPart::InputText {
                    text: String::new(),
                });
            }
            OpenAIResponsesInputItem::User {
                role: "user",
                content: parts,
            }
        }
    }
}

// ============================================================================
// Streaming Chunk Types (minimal, forward-compatible)
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OpenAIResponsesChunk {
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        item_id: String,
        content_index: u32,
        delta: String,
    },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded { item: OpenAIResponsesOutputItem },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone { item: OpenAIResponsesOutputItemDone },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta { item_id: String, delta: String },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta {
        item_id: String,
        summary_index: u32,
        delta: String,
    },
    #[serde(rename = "response.completed")]
    ResponseCompleted {
        response: OpenAIResponsesDonePayload,
    },
    #[serde(rename = "response.incomplete")]
    ResponseIncomplete {
        response: OpenAIResponsesDonePayload,
    },
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OpenAIResponsesOutputItem {
    #[serde(rename = "function_call")]
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        #[serde(default)]
        arguments: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OpenAIResponsesOutputItemDone {
    #[serde(rename = "function_call")]
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        #[serde(default)]
        arguments: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponsesDonePayload {
    #[serde(default)]
    incomplete_details: Option<OpenAIResponsesIncompleteDetails>,
    usage: OpenAIResponsesUsage,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponsesIncompleteDetails {
    reason: String,
}

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
struct OpenAIResponsesUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    total_tokens: Option<u64>,
}

impl OpenAIResponsesDonePayload {
    fn incomplete_reason(&self) -> Option<String> {
        self.incomplete_details.as_ref().map(|d| d.reason.clone())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use futures::stream;

    #[test]
    fn test_provider_info() {
        let provider = OpenAIResponsesProvider::new("gpt-4o");
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.api(), "openai-responses");
    }

    #[test]
    fn test_stream_parses_text_and_tool_call() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let events = vec![
                serde_json::json!({
                    "type": "response.output_text.delta",
                    "item_id": "msg_1",
                    "content_index": 0,
                    "delta": "Hello"
                }),
                serde_json::json!({
                    "type": "response.output_item.added",
                    "output_index": 1,
                    "item": {
                        "type": "function_call",
                        "id": "fc_1",
                        "call_id": "call_1",
                        "name": "echo",
                        "arguments": ""
                    }
                }),
                serde_json::json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": "fc_1",
                    "output_index": 1,
                    "delta": "{\"text\":\"hi\"}"
                }),
                serde_json::json!({
                    "type": "response.output_item.done",
                    "output_index": 1,
                    "item": {
                        "type": "function_call",
                        "id": "fc_1",
                        "call_id": "call_1",
                        "name": "echo",
                        "arguments": "{\"text\":\"hi\"}",
                        "status": "completed"
                    }
                }),
                serde_json::json!({
                    "type": "response.completed",
                    "response": {
                        "incomplete_details": null,
                        "usage": {
                            "input_tokens": 1,
                            "output_tokens": 2,
                            "total_tokens": 3
                        }
                    }
                }),
            ];

            let byte_stream = stream::iter(
                events
                    .iter()
                    .map(|event| format!("data: {}\n\n", serde_json::to_string(event).unwrap()))
                    .map(|s| Ok(s.into_bytes())),
            );

            let event_source = crate::sse::SseStream::new(Box::pin(byte_stream));
            let mut state = StreamState::new(
                event_source,
                "gpt-test".to_string(),
                "openai-responses".to_string(),
                "openai".to_string(),
            );

            let mut out = Vec::new();
            while let Some(item) = state.event_source.next().await {
                let msg = item.expect("SSE event");
                state.process_event(&msg.data).expect("process_event");
                out.extend(state.pending_events.drain(..));
                if state.finished {
                    break;
                }
            }

            assert!(matches!(out.first(), Some(StreamEvent::Start { .. })));
            assert!(out.iter().any(|e| matches!(e, StreamEvent::TextDelta { delta, .. } if delta == "Hello")));
            assert!(out.iter().any(|e| matches!(e, StreamEvent::ToolCallEnd { tool_call, .. } if tool_call.name == "echo")));
            assert!(out.iter().any(|e| matches!(e, StreamEvent::Done { reason: StopReason::ToolUse, .. })));
        });
    }
}
