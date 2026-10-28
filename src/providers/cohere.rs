//! Cohere Chat API provider implementation.
//!
//! Native v2/chat image inputs, thinking controls and incremental tool calls.
//! Completion and indexed content lifecycles are validated by `streaming`.

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{ContentBlock, Message, StreamEvent, UserContent};
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use async_trait::async_trait;
use futures::stream::Stream;
use serde::Serialize;
use std::pin::Pin;

mod request_options;
mod streaming;
#[cfg(any(test, feature = "fuzzing"))]
use streaming::StreamState;

const COHERE_CHAT_API_URL: &str = "https://api.cohere.com/v2/chat";
const DEFAULT_MAX_TOKENS: u32 = 4096;
const MAX_ERROR_BYTES: usize = 8 * 1024;

/// Cohere `v2/chat` streaming provider.
pub struct CohereProvider {
    client: Client,
    model: String,
    base_url: String,
    provider: String,
    compat: Option<CompatConfig>,
}

impl CohereProvider {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            base_url: COHERE_CHAT_API_URL.to_string(),
            provider: "cohere".to_string(),
            compat: None,
        }
    }

    #[must_use]
    pub fn with_provider_name(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

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

    /// Build the native request. `stream` validates output caps and the final
    /// image payload before dispatch, including changes made by request hooks.
    pub fn build_request(&self, context: &Context<'_>, options: &StreamOptions) -> CohereRequest {
        let messages = build_cohere_messages(context);
        let tools = if context.tools.is_empty() {
            None
        } else {
            Some(context.tools.iter().map(convert_tool_to_cohere).collect())
        };
        CohereRequest {
            model: self.model.clone(),
            messages,
            max_tokens: options.max_tokens.or(Some(DEFAULT_MAX_TOKENS)),
            temperature: options.temperature,
            tools,
            stream: true,
            thinking: request_options::thinking(&self.model, options),
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

fn response_secrets(
    options: &StreamOptions,
    compat: Option<&CompatConfig>,
    fallback: Option<&str>,
) -> Vec<String> {
    let mut secrets = fallback.into_iter().map(str::to_string).collect::<Vec<_>>();
    let headers = options.headers.iter().chain(
        compat
            .and_then(|compat| compat.custom_headers.as_ref())
            .into_iter()
            .flatten(),
    );
    for (name, value) in headers {
        secrets.push(value.clone());
        if name.eq_ignore_ascii_case("authorization")
            && let Some((_, token)) = value.split_once(char::is_whitespace)
            && !token.trim().is_empty()
        {
            secrets.push(token.trim().to_string());
        }
    }
    secrets
}

#[async_trait]
#[allow(clippy::too_many_lines)]
impl Provider for CohereProvider {
    fn name(&self) -> &str {
        &self.provider
    }
    fn api(&self) -> &'static str {
        "cohere-chat"
    }
    fn model_id(&self) -> &str {
        &self.model
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        request_options::validate_options(&self.model, options)?;
        let auth_value = if authorization_override(options, self.compat.as_ref()).is_some() {
            None
        } else {
            Some(options.api_key.clone()
                .or_else(|| std::env::var("COHERE_API_KEY").ok())
                .ok_or_else(|| Error::provider("cohere", "Missing API key for provider. Configure credentials with /login <provider> or set the provider's API key env var."))?)
        };
        let secrets = response_secrets(options, self.compat.as_ref(), auth_value.as_deref());
        let request_body = self.build_request(context, options);
        let mut request = self
            .client
            .post(&self.base_url)
            .header("Accept", "text/event-stream");
        if let Some(auth_value) = auth_value {
            request = request.header("Authorization", format!("Bearer {auth_value}"));
        }
        if let Some(compat) = &self.compat
            && let Some(custom_headers) = &compat.custom_headers
        {
            request = super::apply_headers_ignoring_blank_auth_overrides(
                request,
                custom_headers,
                &["authorization"],
            );
        }
        request = super::apply_headers_ignoring_blank_auth_overrides(
            request,
            &options.headers,
            &["authorization"],
        );
        let rewritten_body = super::offer_before_provider_request(
            options,
            self.name(),
            self.api(),
            self.model_id(),
            &self.base_url,
            &request_body,
            |value| {
                super::validate_streamed_json_rewrite(
                    value,
                    &["model"],
                    &["messages"],
                    &[("stream", serde_json::Value::Bool(true))],
                )
            },
        )
        .await;
        let body = match rewritten_body {
            Some(body) => body,
            None => serde_json::to_value(&request_body)?,
        };
        drop(request_body);
        // Validate only the payload that will actually be sent: a hook can
        // remove attachments without paying for their decode or network transfer.
        request_options::validate_images(&body)?;
        let request = request.json(&body)?;
        let response = Box::pin(request.send()).await?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let body = response
                .text_limited(MAX_ERROR_BYTES)
                .await
                .unwrap_or_else(|_| "<failed to read bounded error response>".to_string());
            let secrets = secrets.iter().map(String::as_str).collect::<Vec<_>>();
            let body = crate::auth::redact_known_secrets_bounded(&body, &secrets, MAX_ERROR_BYTES);
            return Err(Error::provider(
                "cohere",
                format!("Cohere API error (HTTP {status}): {body}"),
            ));
        }
        let is_sse = response
            .headers()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .is_some_and(|(_, value)| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
            });
        if !is_sse {
            return Err(Error::api(format!(
                "Cohere API protocol error (HTTP {status}): expected Content-Type text/event-stream"
            )));
        }
        Ok(streaming::response_stream(
            response.bytes_stream(),
            self.model.clone(),
            self.api().to_string(),
            self.name().to_string(),
            options.usage_observer.clone(),
        ))
    }
}

// ============================================================================
// Native request types and conversation conversion
// ============================================================================

#[derive(Debug, Serialize)]
pub struct CohereRequest {
    model: String,
    messages: Vec<CohereMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<CohereTool>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<request_options::Thinking>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum CohereMessage {
    System {
        content: String,
    },
    User {
        content: request_options::InputContent,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<CohereToolCallRef>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_plan: Option<String>,
    },
    Tool {
        content: String,
        tool_call_id: String,
    },
}

#[derive(Debug, Serialize)]
struct CohereToolCallRef {
    id: String,
    #[serde(rename = "type")]
    r#type: &'static str,
    function: CohereFunctionRef,
}

#[derive(Debug, Serialize)]
struct CohereFunctionRef {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct CohereTool {
    #[serde(rename = "type")]
    r#type: &'static str,
    function: CohereFunction,
}

#[derive(Debug, Serialize)]
struct CohereFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: serde_json::Value,
}

fn convert_tool_to_cohere(tool: &ToolDef) -> CohereTool {
    CohereTool {
        r#type: "function",
        function: CohereFunction {
            name: tool.name.clone(),
            description: if tool.description.trim().is_empty() {
                None
            } else {
                Some(tool.description.clone())
            },
            parameters: tool.parameters.clone(),
        },
    }
}

fn flush_tool_images(out: &mut Vec<CohereMessage>, images: &mut Vec<serde_json::Value>) {
    if !images.is_empty() {
        out.push(CohereMessage::User {
            content: request_options::InputContent::Parts(std::mem::take(images)),
        });
    }
}

fn build_cohere_messages(context: &Context<'_>) -> Vec<CohereMessage> {
    let mut out = Vec::new();
    let mut pending_images = Vec::new();
    if let Some(system) = &context.system_prompt {
        out.push(CohereMessage::System {
            content: system.to_string(),
        });
    }
    for message in context.messages.iter() {
        if !matches!(message, Message::ToolResult(_)) {
            // Keep all parallel function responses adjacent. Adding an image
            // user turn between those responses would break tool replay.
            flush_tool_images(&mut out, &mut pending_images);
        }
        match message {
            Message::User(user) => out.push(CohereMessage::User {
                content: request_options::user_content(&user.content),
            }),
            Message::Custom(custom) => out.push(CohereMessage::User {
                content: request_options::InputContent::Text(custom.content.clone()),
            }),
            Message::Assistant(assistant) => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text(t) => text.push_str(&t.text),
                        ContentBlock::ToolCall(tc) => tool_calls.push(CohereToolCallRef {
                            id: tc.id.clone(),
                            r#type: "function",
                            function: CohereFunctionRef {
                                name: tc.name.clone(),
                                arguments: tc.arguments.to_string(),
                            },
                        }),
                        _ => {}
                    }
                }
                out.push(CohereMessage::Assistant {
                    content: if text.is_empty() { None } else { Some(text) },
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_plan: None,
                });
            }
            Message::ToolResult(result) => {
                let images = request_options::tool_images(&result.content);
                let mut content = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        ContentBlock::Media(media) => Some(media.placeholder()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !images.is_empty() {
                    if content.is_empty() {
                        content = "(see attached tool result images)".to_string();
                    }
                    pending_images.push(serde_json::json!({
                        "type":"text",
                        "text":format!("Images from tool {} (call {}):", result.tool_name, result.tool_call_id)
                    }));
                    pending_images.extend(images);
                }
                out.push(CohereMessage::Tool {
                    content,
                    tool_call_id: result.tool_call_id.clone(),
                });
            }
        }
    }
    flush_tool_images(&mut out, &mut pending_images);
    out
}

/// Text-only projection for callers that explicitly need a textual summary.
/// Native image requests use `request_options::user_content`, not this fallback.
fn extract_text_user_content(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => {
            let mut out = String::new();
            for block in blocks {
                match block {
                    ContentBlock::Text(t) => out.push_str(&t.text),
                    ContentBlock::Image(img) => {
                        use std::fmt::Write as _;
                        let _ =
                            write!(out, "[Image: {} ({} bytes)]", img.mime_type, img.data.len());
                    }
                    ContentBlock::Media(media) => out.push_str(&media.placeholder()),
                    _ => {}
                }
            }
            out
        }
    }
}

// ============================================================================
// Existing provider contracts: exercise the native decoder and request builder.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantMessage, StopReason, TextContent, ToolCall, Usage};
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

    fn load_fixture(file_name: &str) -> ProviderFixture {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/provider_responses")
            .join(file_name);
        let raw = std::fs::read_to_string(path).expect("fixture read");
        serde_json::from_str(&raw).expect("fixture parse")
    }

    fn summarize_event(event: &StreamEvent) -> EventSummary {
        let mut summary = EventSummary {
            kind: "other".to_string(),
            content_index: None,
            delta: None,
            content: None,
            reason: None,
        };
        match event {
            StreamEvent::Start { .. } => summary.kind = "start".to_string(),
            StreamEvent::TextStart { content_index } => {
                summary.kind = "text_start".to_string();
                summary.content_index = Some(*content_index);
            }
            StreamEvent::TextDelta {
                content_index,
                delta,
            } => {
                summary.kind = "text_delta".to_string();
                summary.content_index = Some(*content_index);
                summary.delta = Some(delta.clone());
            }
            StreamEvent::TextEnd {
                content_index,
                content,
            } => {
                summary.kind = "text_end".to_string();
                summary.content_index = Some(*content_index);
                summary.content = Some(content.clone());
            }
            StreamEvent::Done { reason, .. } => {
                summary.kind = "done".to_string();
                summary.reason = Some(reason_to_string(*reason));
            }
            StreamEvent::Error { reason, .. } => {
                summary.kind = "error".to_string();
                summary.reason = Some(reason_to_string(*reason));
            }
            _ => {}
        }
        summary
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

    #[test]
    fn test_stream_fixtures() {
        let fixture = load_fixture("cohere_stream.json");
        for case in fixture.cases {
            let events = collect_events(&case.events);
            let summaries: Vec<EventSummary> = events.iter().map(summarize_event).collect();
            assert_eq!(summaries, case.expected, "case {}", case.name);
        }
    }

    #[test]
    fn test_provider_info() {
        let provider = CohereProvider::new("command-r");
        assert_eq!(provider.name(), "cohere");
        assert_eq!(provider.api(), "cohere-chat");
    }

    #[test]
    fn test_build_request_includes_system_tools_and_v2_shape() {
        let provider = CohereProvider::new("command-r");
        let context = Context::owned(
            Some("You are concise.".to_string()),
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })],
            vec![ToolDef {
                name: "search".to_string(),
                description: "Search docs".to_string(),
                parameters: json!({"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}),
            }],
        );
        let options = StreamOptions {
            temperature: Some(0.2),
            max_tokens: Some(123),
            ..Default::default()
        };
        let value = serde_json::to_value(provider.build_request(&context, &options)).unwrap();
        assert_eq!(value["model"], "command-r");
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["messages"][0]["content"], "You are concise.");
        assert_eq!(value["messages"][1]["role"], "user");
        assert_eq!(value["messages"][1]["content"], "Ping");
        assert_eq!(value["stream"], true);
        assert_eq!(value["max_tokens"], 123);
        assert!((value["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
        assert_eq!(value["tools"][0]["type"], "function");
        assert_eq!(value["tools"][0]["function"]["name"], "search");
        assert_eq!(value["tools"][0]["function"]["description"], "Search docs");
        assert_eq!(
            value["tools"][0]["function"]["parameters"],
            json!({"type":"object","properties":{"q":{"type":"string"}},"required":["q"]})
        );
    }

    #[test]
    fn test_convert_tool_to_cohere_omits_empty_description() {
        let tool = ToolDef {
            name: "echo".to_string(),
            description: "   ".to_string(),
            parameters: json!({"type":"object","properties":{"text":{"type":"string"}}}),
        };
        let value = serde_json::to_value(convert_tool_to_cohere(&tool)).unwrap();
        assert_eq!(value["type"], "function");
        assert_eq!(value["function"]["name"], "echo");
        assert!(value["function"].get("description").is_none());
    }

    #[test]
    fn test_stream_parses_text_and_tool_call() {
        let events = vec![
            json!({"type":"message-start","id":"msg_1"}),
            json!({"type":"content-start","index":0,"delta":{"message":{"content":{"type":"text","text":"Hello"}}}}),
            json!({"type":"content-delta","index":0,"delta":{"message":{"content":{"text":" world"}}}}),
            json!({"type":"content-end","index":0}),
            json!({"type":"tool-call-start","delta":{"message":{"tool_calls":{"id":"call_1","type":"function","function":{"name":"echo","arguments":"{\"text\":\"hi\"}"}}}}}),
            json!({"type":"tool-call-end"}),
            json!({"type":"message-end","delta":{"finish_reason":"TOOL_CALL","usage":{"tokens":{"input_tokens":1,"output_tokens":2}}}}),
        ];
        let out = collect_events(&events);
        assert!(matches!(out.first(), Some(StreamEvent::Start { .. })));
        assert!(
            out.iter().any(
                |e| matches!(e, StreamEvent::TextDelta { delta, .. } if delta.contains("Hello"))
            )
        );
        assert!(out.iter().any(
            |e| matches!(e, StreamEvent::ToolCallEnd { tool_call, .. } if tool_call.name == "echo")
        ));
        assert!(out.iter().any(|e| matches!(
            e,
            StreamEvent::Done {
                reason: StopReason::ToolUse,
                ..
            }
        )));
    }

    #[test]
    fn test_stream_parses_thinking_and_max_tokens_stop_reason() {
        let events = vec![
            json!({"type":"message-start","id":"msg_1"}),
            json!({"type":"content-start","index":0,"delta":{"message":{"content":{"type":"thinking","thinking":"Plan"}}}}),
            json!({"type":"content-delta","index":0,"delta":{"message":{"content":{"thinking":" more"}}}}),
            json!({"type":"content-end","index":0}),
            json!({"type":"message-end","delta":{"finish_reason":"MAX_TOKENS","usage":{"tokens":{"input_tokens":2,"output_tokens":3}}}}),
        ];
        let out = collect_events(&events);
        assert!(
            out.iter()
                .any(|e| matches!(e, StreamEvent::ThinkingStart { .. }))
        );
        assert!(out.iter().any(
            |e| matches!(e, StreamEvent::ThinkingDelta { delta, .. } if delta.contains("Plan"))
        ));
        assert!(out.iter().any(|e| matches!(e, StreamEvent::ThinkingEnd { content, .. } if content.contains("Plan more"))));
        assert!(out.iter().any(|e| matches!(
            e,
            StreamEvent::Done {
                reason: StopReason::Length,
                ..
            }
        )));
    }

    #[test]
    fn test_stream_sets_bearer_auth_header() {
        let captured = run_stream_and_capture_headers(Some("test-cohere-key"), HashMap::new())
            .expect("captured request");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer test-cohere-key")
        );
        assert_eq!(
            captured.headers.get("accept").map(String::as_str),
            Some("text/event-stream")
        );
        let body: Value = serde_json::from_str(&captured.body).unwrap();
        assert_eq!(body["model"], "command-r");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn test_stream_uses_existing_authorization_header_without_api_key() {
        let mut headers = HashMap::new();
        headers.insert(
            "Authorization".to_string(),
            "Bearer from-custom-header".to_string(),
        );
        headers.insert("X-Test".to_string(), "1".to_string());
        let captured = run_stream_and_capture_headers(None, headers).expect("captured request");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer from-custom-header")
        );
        assert_eq!(
            captured.headers.get("x-test").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn test_stream_compat_authorization_header_overrides_api_key_without_duplicate() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let mut custom_headers = HashMap::new();
        custom_headers.insert(
            "Authorization".to_string(),
            "Bearer compat-header".to_string(),
        );
        let provider = CohereProvider::new("command-r")
            .with_base_url(base_url)
            .with_compat(Some(CompatConfig {
                custom_headers: Some(custom_headers),
                ..Default::default()
            }));
        let context = Context::owned(
            Some("system".to_string()),
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let options = StreamOptions {
            api_key: Some("test-cohere-key".to_string()),
            ..Default::default()
        };
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let mut stream = provider.stream(&context, &options).await.unwrap();
            while let Some(event) = stream.next().await {
                if matches!(event.unwrap(), StreamEvent::Done { .. }) {
                    break;
                }
            }
        });
        let captured = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer compat-header")
        );
        assert_eq!(captured.header_count("authorization"), 1);
    }

    #[test]
    fn test_stream_compat_authorization_header_works_without_api_key() {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let mut custom_headers = HashMap::new();
        custom_headers.insert(
            "Authorization".to_string(),
            "Bearer compat-header".to_string(),
        );
        let provider = CohereProvider::new("command-r")
            .with_base_url(base_url)
            .with_compat(Some(CompatConfig {
                custom_headers: Some(custom_headers),
                ..Default::default()
            }));
        let context = Context::owned(
            Some("system".to_string()),
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let mut stream = provider
                .stream(&context, &StreamOptions::default())
                .await
                .unwrap();
            while let Some(event) = stream.next().await {
                if matches!(event.unwrap(), StreamEvent::Done { .. }) {
                    break;
                }
            }
        });
        let captured = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer compat-header")
        );
        assert_eq!(captured.header_count("authorization"), 1);
    }

    fn collect_events(events: &[Value]) -> Vec<StreamEvent> {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let byte_stream = stream::iter(
                events
                    .iter()
                    .map(|event| format!("data: {}\n\n", serde_json::to_string(event).unwrap()))
                    .map(|s| Ok(s.into_bytes())),
            );
            let event_source = crate::sse::SseStream::new(Box::pin(byte_stream));
            let mut state = StreamState::new(
                event_source,
                "command-r".to_string(),
                "cohere-chat".to_string(),
                "cohere".to_string(),
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
            out
        })
    }

    fn billed_test_provider(
        url: String,
        budget: crate::cost::CostBudget,
    ) -> std::sync::Arc<dyn Provider> {
        let entry = crate::models::ModelEntry {
            model: crate::provider::Model {
                id: "command-r-test".to_string(),
                name: "Cohere billing fixture".to_string(),
                api: "cohere-chat".to_string(),
                provider: "cohere".to_string(),
                base_url: url,
                reasoning: false,
                input: vec![crate::provider::InputType::Text],
                cost: crate::provider::ModelCost {
                    input: 2.0,
                    output: 4.0,
                    ..Default::default()
                },
                context_window: 128_000,
                max_tokens: 4096,
                headers: HashMap::new(),
            },
            api_key: None,
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        };
        crate::provider::guard_with_cost_budget(
            crate::providers::create_provider(&entry, None).unwrap(),
            budget,
        )
    }

    #[test]
    fn native_billed_units_drive_cost_caps_without_shrinking_context_usage() {
        for (name, billed, limit, expected, exhausted) in [
            (
                "discounted",
                Some(json!({"input_tokens":100,"output_tokens":10})),
                0.001,
                Some(0.000_24),
                false,
            ),
            (
                "free",
                Some(json!({"input_tokens":0.0,"output_tokens":0.0})),
                0.001,
                Some(0.0),
                false,
            ),
            (
                "billed-cap",
                Some(json!({"input_tokens":100,"output_tokens":10})),
                0.000_2,
                Some(0.000_24),
                true,
            ),
            ("legacy", None, 0.001, Some(0.002_4), true),
            (
                "incomplete",
                Some(json!({"input_tokens":100})),
                0.001,
                None,
                true,
            ),
        ] {
            let mut usage = json!({"tokens":{"input_tokens":1000,"output_tokens":100}});
            if let Some(billed) = billed {
                usage["billed_units"] = billed;
            }
            let terminal =
                json!({"type":"message-end","delta":{"finish_reason":"COMPLETE","usage":usage}});
            let response = format!("data: {{\"type\":\"message-start\"}}\n\ndata: {terminal}\n\n");
            let (url, captured) = spawn_test_server(200, "text/event-stream", &response);
            let budget = crate::cost::CostBudget::new(None, Some(limit), 0.0).unwrap();
            let provider = billed_test_provider(url, budget.clone());
            let runtime = RuntimeBuilder::current_thread().build().unwrap();
            runtime.block_on(async {
                let context = Context::default();
                let options = StreamOptions {
                    api_key: Some("fixture-key".to_string()),
                    ..Default::default()
                };
                let mut events = provider.stream(&context, &options).await.unwrap();
                let mut completed = false;
                let mut failed = false;
                while let Some(event) = events.next().await {
                    match event {
                        Ok(StreamEvent::Done { message, .. }) => {
                            assert_eq!(
                                (
                                    message.usage.input,
                                    message.usage.output,
                                    message.usage.total_tokens
                                ),
                                (1000, 100, 1100),
                                "{name}"
                            );
                            assert!(
                                (message.usage.cost.total - expected.unwrap()).abs() < 1e-12,
                                "{name}"
                            );
                            assert_eq!(message.usage.billing.is_some(), name != "legacy");
                            completed = true;
                        }
                        Err(_) => failed = true,
                        _ => {}
                    }
                }
                assert_eq!(completed, expected.is_some(), "{name}");
                assert_eq!(failed, expected.is_none(), "{name}");
                if exhausted {
                    match (expected, provider.stream(&context, &options).await) {
                        (Some(_), Err(Error::CostCap { .. })) | (None, Err(Error::Session(_))) => {}
                        _ => panic!("{name}: follow-up was not stopped by the accounting boundary"),
                    }
                }
            });
            assert!(
                (budget.spent() - expected.unwrap_or(0.0)).abs() < 1e-12,
                "{name}"
            );
            assert_eq!(budget.admit().is_err(), exhausted, "{name}");
            assert!(captured.recv_timeout(Duration::from_secs(2)).is_ok());
        }
    }

    #[test]
    fn native_billing_is_kept_before_queued_done_or_a_terminal_protocol_error() {
        for (finish, billed) in [("COMPLETE", 100), ("COMPLETE", 0), ("INVALID_FINISH", 100)] {
            let terminal = json!({"type":"message-end","delta":{"finish_reason":finish,"usage":{
                "tokens":{"input_tokens":1000,"output_tokens":100},
                "billed_units":{"input_tokens":billed,"output_tokens":0}
            }}});
            let response = format!(
                "data: {{\"type\":\"message-start\"}}\n\ndata: {{\"type\":\"tool-plan-delta\",\"delta\":{{\"message\":{{\"tool_plan\":\"acknowledged plan\"}}}}}}\n\ndata: {terminal}\n\n"
            );
            let (url, captured) = spawn_test_server(200, "text/event-stream", &response);
            let budget = crate::cost::CostBudget::new(None, Some(0.001), 0.0).unwrap();
            let provider = billed_test_provider(url, budget.clone());
            let runtime = RuntimeBuilder::current_thread().build().unwrap();
            runtime.block_on(async {
                let mut events = provider
                    .stream(
                        &Context::default(),
                        &StreamOptions {
                            api_key: Some("fixture-key".to_string()),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                let mut stopped = false;
                while let Some(event) = events.next().await {
                    match event {
                        Ok(StreamEvent::TextEnd { .. }) => {
                            assert_eq!(finish, "COMPLETE");
                            stopped = true;
                            break;
                        }
                        Err(_) => {
                            assert_eq!(finish, "INVALID_FINISH");
                            stopped = true;
                            break;
                        }
                        Ok(StreamEvent::Done { .. }) => {
                            panic!("the queued plan end must precede Done")
                        }
                        _ => {}
                    }
                }
                assert!(stopped);
                drop(events);
            });
            let expected = f64::from(billed) * 2.0 / 1_000_000.0;
            assert!((budget.spent() - expected).abs() < 1e-12);
            assert_eq!(budget.snapshot().pending, 0.0);
            budget.admit().unwrap();
            assert!(captured.recv_timeout(Duration::from_secs(2)).is_ok());
        }
    }

    #[derive(Debug)]
    struct CapturedRequest {
        headers: HashMap<String, String>,
        header_lines: Vec<(String, String)>,
        body: String,
    }

    impl CapturedRequest {
        fn header_count(&self, name: &str) -> usize {
            self.header_lines
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                .count()
        }
    }

    fn run_stream_and_capture_headers(
        api_key: Option<&str>,
        extra_headers: HashMap<String, String>,
    ) -> Option<CapturedRequest> {
        let (base_url, rx) = spawn_test_server(200, "text/event-stream", &success_sse_body());
        let provider = CohereProvider::new("command-r").with_base_url(base_url);
        let context = Context::owned(
            Some("system".to_string()),
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("ping".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let options = StreamOptions {
            api_key: api_key.map(str::to_string),
            headers: extra_headers,
            ..Default::default()
        };
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider.stream(&context, &options).await.unwrap();
            while let Some(event) = stream.next().await {
                if matches!(event.unwrap(), StreamEvent::Done { .. }) {
                    break;
                }
            }
        });
        rx.recv_timeout(Duration::from_secs(2)).ok()
    }

    fn success_sse_body() -> String {
        [
            r#"data: {"type":"message-start","id":"msg_1"}"#,
            "",
            r#"data: {"type":"message-end","delta":{"finish_reason":"COMPLETE","usage":{"tokens":{"input_tokens":1,"output_tokens":1}}}}"#,
            "",
        ].join("\n")
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
            let (headers, header_lines) = parse_headers(&header_text);
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
                header_lines,
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

    fn parse_headers(header_text: &str) -> (HashMap<String, String>, Vec<(String, String)>) {
        let mut headers = HashMap::new();
        let mut header_lines = Vec::new();
        for line in header_text.lines().skip(1) {
            if let Some((name, value)) = line.split_once(':') {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim().to_string();
                header_lines.push((name.clone(), value.clone()));
                headers.insert(name, value);
            }
        }
        (headers, header_lines)
    }

    #[test]
    fn test_build_request_no_system_prompt() {
        let provider = CohereProvider::new("command-r-plus");
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("Hi".to_string()),
                timestamp: 0,
            })],
            vec![],
        );
        let value =
            serde_json::to_value(provider.build_request(&context, &StreamOptions::default()))
                .unwrap();
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(value["messages"][0]["content"], "Hi");
        assert!(
            !value["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "system")
        );
    }

    #[test]
    fn test_build_request_default_max_tokens() {
        let provider = CohereProvider::new("command-r");
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("test".to_string()),
                timestamp: 0,
            })],
            vec![],
        );
        let value =
            serde_json::to_value(provider.build_request(&context, &StreamOptions::default()))
                .unwrap();
        assert_eq!(value["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn test_build_request_no_tools_omits_tools_field() {
        let provider = CohereProvider::new("command-r");
        let context = Context::owned(
            None,
            vec![Message::User(crate::model::UserMessage {
                content: UserContent::Text("test".to_string()),
                timestamp: 0,
            })],
            vec![],
        );
        let value =
            serde_json::to_value(provider.build_request(&context, &StreamOptions::default()))
                .unwrap();
        assert!(value.get("tools").is_none() || value["tools"].is_null());
    }

    #[test]
    fn test_build_request_full_conversation_with_tool_call_and_result() {
        let provider = CohereProvider::new("command-r");
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
                        arguments: json!({"path":"/tmp/a.txt"}),
                        thought_signature: None,
                    })],
                    api: "cohere-chat".to_string(),
                    provider: "cohere".to_string(),
                    model: "command-r".to_string(),
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
                parameters: json!({"type":"object"}),
            }],
        );
        let value =
            serde_json::to_value(provider.build_request(&context, &StreamOptions::default()))
                .unwrap();
        let msgs = value["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[3]["role"], "tool");
        assert!(msgs[2].get("content").is_none() || msgs[2]["content"].is_null());
        let calls = msgs[2]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "read");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[3]["content"], "file contents");
    }

    #[test]
    fn test_build_request_assistant_text_preserved_alongside_tool_calls() {
        let provider = CohereProvider::new("command-r");
        let context = Context::owned(
            None,
            vec![Message::assistant(AssistantMessage {
                content: vec![
                    ContentBlock::Text(TextContent::new("Let me read that file.")),
                    ContentBlock::ToolCall(ToolCall {
                        id: "call_1".to_string(),
                        name: "read".to_string(),
                        arguments: json!({"path":"/tmp/a.txt"}),
                        thought_signature: None,
                    }),
                ],
                api: "cohere-chat".to_string(),
                provider: "cohere".to_string(),
                model: "command-r".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            })],
            vec![],
        );
        let value =
            serde_json::to_value(provider.build_request(&context, &StreamOptions::default()))
                .unwrap();
        assert_eq!(value["messages"][0]["role"], "assistant");
        assert_eq!(
            value["messages"][0]["content"].as_str(),
            Some("Let me read that file.")
        );
        assert_eq!(
            value["messages"][0]["tool_calls"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn test_convert_custom_message_to_cohere() {
        let context = Context::owned(
            None,
            vec![Message::Custom(crate::model::CustomMessage {
                custom_type: "extension_note".to_string(),
                content: "Important context.".to_string(),
                display: false,
                details: None,
                timestamp: 0,
            })],
            vec![],
        );
        let msgs = build_cohere_messages(&context);
        assert_eq!(msgs.len(), 1);
        let value = serde_json::to_value(&msgs[0]).unwrap();
        assert_eq!(value["role"], "user");
        assert_eq!(value["content"], "Important context.");
    }

    #[test]
    fn test_convert_user_blocks_extracts_text_only() {
        let content = UserContent::Blocks(vec![
            ContentBlock::Text(TextContent::new("part 1")),
            ContentBlock::Image(crate::model::ImageContent {
                data: "aGVsbG8=".to_string(),
                mime_type: "image/png".to_string(),
            }),
            ContentBlock::Text(TextContent::new("part 2")),
        ]);
        assert_eq!(
            extract_text_user_content(&content),
            "part 1[Image: image/png (8 bytes)]part 2"
        );
        // The explicit text projection is still available, but native requests
        // must not use that projection to replace an actual image attachment.
        let native = serde_json::to_value(request_options::user_content(&content)).unwrap();
        assert_eq!(
            native[1]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
    }

    #[test]
    fn test_custom_provider_name() {
        let provider = CohereProvider::new("command-r").with_provider_name("my-proxy");
        assert_eq!(provider.name(), "my-proxy");
        assert_eq!(provider.api(), "cohere-chat");
    }

    #[test]
    fn test_custom_base_url() {
        let provider =
            CohereProvider::new("command-r").with_base_url("https://proxy.example.com/v2/chat");
        assert_eq!(provider.base_url, "https://proxy.example.com/v2/chat");
    }

    #[test]
    fn test_stream_complete_finish_reason_maps_to_stop() {
        let out = collect_events(&[
            json!({"type":"message-start","id":"msg_1"}),
            json!({"type":"message-end","delta":{"finish_reason":"COMPLETE","usage":{"tokens":{"input_tokens":5,"output_tokens":10}}}}),
        ]);
        assert!(out.iter().any(|e| matches!(e, StreamEvent::Done { reason: StopReason::Stop, message, .. } if message.usage.input == 5 && message.usage.output == 10)));
    }

    #[test]
    fn test_stream_error_finish_reason_maps_to_error() {
        let out = collect_events(&[
            json!({"type":"message-start","id":"msg_1"}),
            json!({"type":"message-end","delta":{"finish_reason":"ERROR","usage":{"tokens":{"input_tokens":1,"output_tokens":0}}}}),
        ]);
        assert!(out.iter().any(|e| matches!(
            e,
            StreamEvent::Done {
                reason: StopReason::Error,
                ..
            }
        )));
    }

    #[test]
    fn test_stream_tool_call_with_streamed_arguments() {
        let out = collect_events(&[
            json!({"type":"message-start","id":"msg_1"}),
            json!({"type":"tool-call-start","delta":{"message":{"tool_calls":{"id":"call_42","type":"function","function":{"name":"bash","arguments":"{\"co"}}}}}),
            json!({"type":"tool-call-delta","delta":{"message":{"tool_calls":{"function":{"arguments":"mmand\""}}}}}),
            json!({"type":"tool-call-delta","delta":{"message":{"tool_calls":{"function":{"arguments":": \"ls -la\"}"}}}}}),
            json!({"type":"tool-call-end"}),
            json!({"type":"message-end","delta":{"finish_reason":"TOOL_CALL","usage":{"tokens":{"input_tokens":10,"output_tokens":20}}}}),
        ]);
        let call = out
            .iter()
            .find_map(|e| match e {
                StreamEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
                _ => None,
            })
            .expect("ToolCallEnd");
        assert_eq!(call.name, "bash");
        assert_eq!(call.id, "call_42");
        assert_eq!(call.arguments["command"], "ls -la");
    }

    #[test]
    fn test_stream_unknown_event_type_ignored() {
        let out = collect_events(&[
            json!({"type":"message-start","id":"msg_1"}),
            json!({"type":"some-future-event","data":"ignored"}),
            json!({"type":"content-start","index":0,"delta":{"message":{"content":{"type":"text","text":"OK"}}}}),
            json!({"type":"content-end","index":0}),
            json!({"type":"message-end","delta":{"finish_reason":"COMPLETE","usage":{"tokens":{"input_tokens":1,"output_tokens":1}}}}),
        ]);
        assert!(out.iter().any(|e| matches!(e, StreamEvent::Done { .. })));
        assert!(out.iter().any(|e| matches!(
            e,
            StreamEvent::TextStart {
                content_index: 0,
                ..
            }
        )));
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
    /// Opaque wrapper around the Cohere stream processor state.
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
                "cohere-fuzz".into(),
                "cohere".into(),
                "cohere".into(),
            ))
        }
        /// Feed one SSE data payload and return any emitted `StreamEvent`s.
        pub fn process_event(&mut self, data: &str) -> crate::error::Result<Vec<StreamEvent>> {
            self.0.process_event(data)?;
            Ok(self.0.pending_events.drain(..).collect())
        }
    }
}
