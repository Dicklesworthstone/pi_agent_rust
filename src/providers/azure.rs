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

const DEFAULT_API_VERSION: &str = "2024-02-15-preview";
const DEFAULT_MAX_TOKENS: u32 = 4096;

// ============================================================================
// Azure OpenAI Provider
// ============================================================================

/// Azure OpenAI Chat Completions API provider.
pub struct AzureOpenAIProvider {
    client: Client,
    /// The deployment name (model deployment in Azure)
    deployment: String,
    /// Azure resource name (part of the URL)
    resource: String,
    /// API version string
    api_version: String,
    /// Optional override for the full endpoint URL (primarily for deterministic tests).
    endpoint_url_override: Option<String>,
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
            deployment: deployment.into(),
            resource: resource.into(),
            api_version: DEFAULT_API_VERSION.to_string(),
            endpoint_url_override: None,
        }
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
    fn build_request(&self, context: &Context, options: &StreamOptions) -> AzureRequest {
        let messages = Self::build_messages(context);

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
    fn build_messages(context: &Context) -> Vec<AzureMessage> {
        let mut messages = Vec::new();

        // Add system prompt as first message
        if let Some(system) = &context.system_prompt {
            messages.push(AzureMessage {
                role: "system".to_string(),
                content: Some(AzureContent::Text(system.clone())),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        // Convert conversation messages
        for message in &context.messages {
            messages.extend(convert_message_to_azure(message));
        }

        messages
    }
}

#[async_trait]
impl Provider for AzureOpenAIProvider {
    fn name(&self) -> &'static str {
        "azure"
    }

    fn api(&self) -> &'static str {
        "azure-openai"
    }

    fn model_id(&self) -> &str {
        &self.deployment
    }

    async fn stream(
        &self,
        context: &Context,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let auth_value = options
            .api_key
            .clone()
            .or_else(|| std::env::var("AZURE_OPENAI_API_KEY").ok())
            .ok_or_else(|| Error::config("Missing Azure OpenAI API key"))?;

        let request_body = self.build_request(context, options);

        let endpoint_url = self.endpoint_url();

        // Build request with Azure-specific headers (Content-Type set by .json() below)
        let mut request = self
            .client
            .post(&endpoint_url)
            .header("Accept", "text/event-stream")
            .header("api-key", &auth_value); // Azure uses api-key header, not Authorization

        for (key, value) in &options.headers {
            request = request.header(key, value);
        }

        let request = request.json(&request_body)?;

        let response = Box::pin(request.send()).await?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::api(format!(
                "Azure OpenAI API error (HTTP {status}): {body}"
            )));
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
                loop {
                    if let Some(event) = state.pending_events.pop_front() {
                        return Some((Ok(event), state));
                    }

                    match state.event_source.next().await {
                        Some(Ok(msg)) => {
                            // Azure also sends "[DONE]" as final message
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

                            if let Err(e) = state.process_event(&msg.data) {
                                return Some((Err(e), state));
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
    pending_events: VecDeque<StreamEvent>,
    started: bool,
}

struct ToolCallState {
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
                error_message: None,
                timestamp: chrono::Utc::now().timestamp_millis(),
            },
            current_text: String::new(),
            tool_calls: Vec::new(),
            pending_events: VecDeque::new(),
            started: false,
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
            self.partial
                .content
                .push(ContentBlock::Text(crate::model::TextContent::new("")));
        }
        let content_index = self.partial.content.len() - 1;

        if let Some(ContentBlock::Text(t)) = self.partial.content.get_mut(content_index) {
            t.text.push_str(&text);
        }

        StreamEvent::TextDelta {
            content_index,
            delta: text,
            partial: self.partial.clone(),
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
        let chunk: AzureStreamChunk =
            serde_json::from_str(data).map_err(|e| Error::api(format!("JSON parse error: {e}")))?;

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

        // Process choices
        for choice in choices {
            // Handle finish reason
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

                // Emit ToolCallEnd for each accumulated tool call
                for tc in &self.tool_calls {
                    if let Some(ContentBlock::ToolCall(tool_call)) =
                        self.partial.content.get(tc.content_index)
                    {
                        self.pending_events.push_back(StreamEvent::ToolCallEnd {
                            content_index: tc.content_index,
                            tool_call: tool_call.clone(),
                            partial: self.partial.clone(),
                        });
                    }
                }
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

                    // Ensure we have a slot for this tool call
                    if self.tool_calls.len() <= idx {
                        let content_index = self.partial.content.len();
                        self.tool_calls.push(ToolCallState {
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

                        // Emit ToolCallStart
                        self.pending_events.push_back(StreamEvent::ToolCallStart {
                            content_index,
                            partial: self.partial.clone(),
                        });
                    }

                    let tc_state = &mut self.tool_calls[idx];
                    let content_index = tc_state.content_index;

                    // Update the tool call state
                    if let Some(id) = tc.id {
                        tc_state.id.clone_from(&id);
                        if let Some(ContentBlock::ToolCall(block)) =
                            self.partial.content.get_mut(content_index)
                        {
                            block.id = id;
                        }
                    }
                    if let Some(func) = tc.function {
                        if let Some(name) = func.name {
                            tc_state.name.clone_from(&name);
                            if let Some(ContentBlock::ToolCall(block)) =
                                self.partial.content.get_mut(content_index)
                            {
                                block.name = name;
                            }
                        }
                        if let Some(args) = func.arguments {
                            tc_state.arguments.push_str(&args);
                            // Note: we don't update partial arguments here as they need to be valid JSON.
                            // We do that at the end.

                            self.pending_events.push_back(StreamEvent::ToolCallDelta {
                                content_index,
                                delta: args,
                                partial: self.partial.clone(),
                            });
                        }
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
struct AzureRequest {
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
    r#type: String,
    function: AzureFunctionRef,
}

#[derive(Debug, Serialize)]
struct AzureFunctionRef {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct AzureTool {
    r#type: String,
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
                .join("");

            // Collect tool calls
            let tool_calls: Vec<AzureToolCallRef> = assistant
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall(tc) => Some(AzureToolCallRef {
                        id: tc.id.clone(),
                        r#type: "function".to_string(),
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
            let content = result
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");

            vec![AzureMessage {
                role: "tool".to_string(),
                content: Some(AzureContent::Text(content)),
                tool_calls: None,
                tool_call_id: Some(result.tool_call_id.clone()),
            }]
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
        r#type: "function".to_string(),
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
    use crate::model::{TextContent, ToolCall, UserMessage};
    use crate::provider::ToolDef;
    use asupersync::runtime::RuntimeBuilder;
    use futures::{StreamExt, stream};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use std::path::PathBuf;

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
            "https://contoso.openai.azure.com/openai/deployments/gpt-4o/chat/completions?api-version=2024-02-15-preview"
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
            system_prompt: Some("You are deterministic.".to_string()),
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Hello".to_string()),
                    timestamp: 0,
                }),
                Message::Assistant(AssistantMessage {
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
                    error_message: None,
                    timestamp: 0,
                }),
            ],
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
            }],
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
            })],
            tools: Vec::new(),
        };
        let options = StreamOptions::default();

        let request = provider.build_request(&context, &options);
        let request_json = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(request_json["max_tokens"], json!(DEFAULT_MAX_TOKENS));
        assert_eq!(request_json["stream"], json!(true));
        assert!(request_json.get("tools").is_none());
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
                        message: state.partial.clone(),
                    });
                    break;
                }
                state.process_event(&msg.data).expect("process_event");
                out.extend(state.pending_events.drain(..));
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
