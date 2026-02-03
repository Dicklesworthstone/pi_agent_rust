//! Azure OpenAI Chat Completions API provider implementation.
//!
//! This module implements the Provider trait for Azure OpenAI, using the same
//! streaming protocol as OpenAI but with Azure-specific authentication and endpoints.
//!
//! Azure OpenAI URL format:
//! `https://{resource}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={version}`

use crate::error::{Error, Result};
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, Usage, UserContent,
};
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use crate::sse::SseStream;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, Stream};
use reqwest::Client;
use serde::{Deserialize, Serialize};
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
        }
    }

    /// Set the API version.
    #[must_use]
    pub fn with_api_version(mut self, version: impl Into<String>) -> Self {
        self.api_version = version.into();
        self
    }

    /// Get the full endpoint URL.
    fn endpoint_url(&self) -> String {
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
        let api_key = options
            .api_key
            .clone()
            .or_else(|| std::env::var("AZURE_OPENAI_API_KEY").ok())
            .ok_or_else(|| Error::config("Missing Azure OpenAI API key"))?;

        let request_body = self.build_request(context, options);

        // Build request with Azure-specific headers
        let mut request = self
            .client
            .post(self.endpoint_url())
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("api-key", &api_key); // Azure uses api-key header, not Authorization

        for (key, value) in &options.headers {
            request = request.header(key, value);
        }

        let request = request.json(&request_body);

        let response = request
            .send()
            .await
            .map_err(|e| Error::api(format!("HTTP request failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::api(format!(
                "Azure OpenAI API error (HTTP {status}): {body}"
            )));
        }

        let byte_stream = response.bytes_stream().map(|chunk| {
            chunk
                .map(|bytes| bytes.to_vec())
                .map_err(std::io::Error::other)
        });

        // Create SSE stream for streaming responses.
        let event_source = SseStream::new(Box::pin(byte_stream));

        // Create stream state
        let model = self.deployment.clone();
        let api = self.api().to_string();
        let provider = self.name().to_string();

        let stream = stream::unfold(
            StreamState::new(event_source, model, api, provider),
            |mut state| async move {
                loop {
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
    started: bool,
}

struct ToolCallState {
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
            started: false,
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn process_event(&mut self, data: &str) -> Result<Option<StreamEvent>> {
        let chunk: AzureStreamChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => return Ok(None), // Skip malformed chunks
        };

        // Process usage if present
        if let Some(usage) = chunk.usage {
            self.partial.usage.input = usage.prompt_tokens;
            self.partial.usage.output = usage.completion_tokens.unwrap_or(0);
            self.partial.usage.total_tokens = usage.total_tokens;
        }

        // Process choices
        for choice in chunk.choices {
            // Handle finish reason
            if let Some(reason) = choice.finish_reason {
                self.partial.stop_reason = match reason.as_str() {
                    "length" => StopReason::Length,
                    "content_filter" => StopReason::Error,
                    "tool_calls" => StopReason::ToolUse,
                    // "stop" and any other reason treated as normal stop
                    _ => StopReason::Stop,
                };

                // Finalize any pending text
                if !self.current_text.is_empty() {
                    self.partial
                        .content
                        .push(ContentBlock::Text(crate::model::TextContent::new(
                            &self.current_text,
                        )));
                }

                // Finalize any pending tool calls
                for tc in &self.tool_calls {
                    let arguments: serde_json::Value = match serde_json::from_str(&tc.arguments) {
                        Ok(args) => args,
                        Err(e) => {
                            eprintln!(
                                "Warning: Failed to parse tool arguments as JSON: {e}. Raw: {}",
                                &tc.arguments
                            );
                            serde_json::Value::Null
                        }
                    };
                    self.partial
                        .content
                        .push(ContentBlock::ToolCall(crate::model::ToolCall {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            arguments,
                            thought_signature: None,
                        }));
                }
            }

            // Handle text content
            if let Some(text) = choice.delta.content {
                // Always save text first (before started check) to avoid losing content
                self.current_text.push_str(&text);

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

            // Handle tool calls
            if let Some(tool_calls) = choice.delta.tool_calls {
                for tc in tool_calls {
                    let idx = tc.index as usize;

                    // Ensure we have a slot for this tool call
                    while self.tool_calls.len() <= idx {
                        self.tool_calls.push(ToolCallState {
                            id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                        });
                    }

                    // Update the tool call state
                    if let Some(id) = tc.id {
                        self.tool_calls[idx].id = id;
                    }
                    if let Some(func) = tc.function {
                        if let Some(name) = func.name {
                            self.tool_calls[idx].name = name;
                        }
                        if let Some(args) = func.arguments {
                            self.tool_calls[idx].arguments.push_str(&args);

                            if !self.started {
                                self.started = true;
                                self.partial.stop_reason = StopReason::ToolUse;
                                return Ok(Some(StreamEvent::Start {
                                    partial: self.partial.clone(),
                                }));
                            }

                            return Ok(Some(StreamEvent::ToolCallDelta {
                                content_index: idx,
                                delta: args,
                                partial: self.partial.clone(),
                            }));
                        }
                    }
                }
            }
        }

        Ok(None)
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

    #[test]
    fn test_azure_provider_creation() {
        let provider = AzureOpenAIProvider::new("my-resource", "gpt-4");
        assert_eq!(provider.name(), "azure");
        assert_eq!(provider.api(), "azure-openai");
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
    fn test_azure_message_conversion() {
        use crate::model::UserMessage;

        let message = Message::User(UserMessage {
            content: UserContent::Text("Hello".to_string()),
            timestamp: chrono::Utc::now().timestamp_millis(),
        });

        let azure_messages = convert_message_to_azure(&message);
        assert_eq!(azure_messages.len(), 1);
        assert_eq!(azure_messages[0].role, "user");
    }
}
