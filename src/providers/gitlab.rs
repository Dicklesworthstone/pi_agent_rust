//! GitLab Duo provider implementation.
//!
//! GitLab Duo uses the `/api/v4/chat/completions` endpoint with a proprietary
//! request/response format (NOT OpenAI-compatible).
//!
//! Authentication is via a GitLab Personal Access Token (PAT) or OAuth token
//! passed as `Authorization: Bearer <token>`.
//!
//! Self-hosted GitLab instances are supported via a configurable base URL
//! (defaults to `https://gitlab.com`).
//!
//! bd-3uqg.3.5

use crate::error::{Error, Result};
use crate::http::client::{Client, effective_default_request_timeout};
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, Usage,
    UserContent,
};
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamOptions};
use async_trait::async_trait;
use futures::Stream;
use futures::stream;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

// ── Constants ────────────────────────────────────────────────────

/// Default GitLab instance base URL.
const DEFAULT_GITLAB_BASE: &str = "https://gitlab.com";

/// Chat completions API path.
const CHAT_API_PATH: &str = "/api/v4/chat/completions";

/// GitLab Chat returns one finite JSON string rather than an SSE stream. Keep
/// both success and error collection far below the generic 50 MiB HTTP-client
/// ceiling so a misbehaving endpoint cannot turn diagnostics into an allocation
/// sink.
const MAX_GITLAB_SUCCESS_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_GITLAB_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_GITLAB_ERROR_DIAGNOSTIC_BYTES: usize = 4 * 1024;
const MAX_GITLAB_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_GITLAB_REQUEST_TEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_GITLAB_CONTEXT_ITEMS: usize = 4096;

// ── Request types ────────────────────────────────────────────────

/// GitLab Duo Chat request body.
#[derive(Debug, Serialize)]
pub struct GitLabChatRequest {
    /// The user's question/prompt.
    content: String,
    /// Reset GitLab's authenticated server-side chat history around this
    /// request so independent Pi sessions cannot contaminate each other.
    with_clean_history: bool,
    /// Additional context items (files, MRs, issues).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    additional_context: Vec<GitLabContextItem>,
}

/// A context item attached to a GitLab Chat request.
#[derive(Debug, Serialize)]
struct GitLabContextItem {
    /// Category: "file", "merge_request", "issue", "snippet".
    category: String,
    /// Identifier for the context item.
    id: String,
    /// Content of the context item.
    content: String,
}

#[derive(Debug, Deserialize, Default)]
struct GitLabReportedUsage {
    #[serde(default, alias = "prompt_tokens")]
    input_tokens: u64,
    #[serde(default, alias = "completion_tokens")]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct GitLabReportedToolCall {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default = "empty_tool_arguments")]
    arguments: serde_json::Value,
}

fn empty_tool_arguments() -> serde_json::Value {
    serde_json::json!({})
}

/// GitLab Chat response wrapper used by some self-hosted / AI Gateway versions.
#[derive(Debug, Deserialize)]
struct GitLabChatResponse {
    /// The generated response text.
    #[serde(default)]
    response: String,
    /// Alternative: some GitLab versions return content directly.
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<GitLabReportedToolCall>,
    #[serde(default)]
    usage: Option<GitLabReportedUsage>,
}

struct ParsedGitLabPayload {
    text: String,
    reported_tool_calls: Vec<crate::model::ToolCall>,
    reported_usage: Option<Usage>,
}

fn parse_gitlab_payload(
    text: &str,
    allowed_tools: &std::collections::HashSet<String>,
) -> Result<ParsedGitLabPayload> {
    // Check the original bytes before Value can discard a repeated field.
    serde_json::from_str::<super::openai::UniqueToolArgumentKeys>(text).map_err(|err| {
        Error::provider(
            "gitlab",
            format!("Invalid GitLab Chat JSON success response: {err}"),
        )
    })?;
    let value = serde_json::from_str::<serde_json::Value>(text).map_err(|err| {
        Error::provider(
            "gitlab",
            format!("Invalid GitLab Chat JSON success response: {err}"),
        )
    })?;
    match value {
        serde_json::Value::String(response) => {
            if response.trim().is_empty() {
                return Err(Error::provider(
                    "gitlab",
                    "GitLab Chat returned an empty success response",
                ));
            }
            Ok(ParsedGitLabPayload {
                text: response,
                reported_tool_calls: Vec::new(),
                reported_usage: None,
            })
        }
        value @ serde_json::Value::Object(_) => {
            let parsed: GitLabChatResponse = serde_json::from_value(value).map_err(|err| {
                Error::provider(
                    "gitlab",
                    format!("Unsupported GitLab Chat response object: {err}"),
                )
            })?;
            let response_text = if parsed.response.trim().is_empty() {
                parsed.content
            } else {
                parsed.response
            };
            let mut reported_tool_calls = Vec::new();
            for (idx, tc) in parsed.tool_calls.into_iter().enumerate() {
                let trimmed_name = tc.name.trim();
                if trimmed_name.is_empty() || !allowed_tools.contains(trimmed_name) {
                    continue;
                }
                let invalid_arguments = || {
                    Error::provider(
                        "gitlab",
                        format!(
                            "PI_TOOL_CALL_PROTOCOL: invalid GitLab tool arguments at index {idx}: arguments must be a JSON object; local tool batch withheld"
                        ),
                    )
                };
                let args = match tc.arguments {
                    val @ serde_json::Value::Object(_) => val,
                    serde_json::Value::String(raw) if !raw.trim().is_empty() => {
                        super::openai::parse_final_tool_arguments(&raw)
                            .map_err(|_| invalid_arguments())?
                    }
                    _ => return Err(invalid_arguments()),
                };
                reported_tool_calls.push(crate::model::ToolCall {
                    id: tc
                        .id
                        .map(|id| id.trim().to_string())
                        .filter(|id| !id.is_empty())
                        .unwrap_or_else(|| format!("gitlab_call_{}", idx + 1)),
                    name: trimmed_name.to_string(),
                    arguments: args,
                    thought_signature: None,
                });
            }
            if response_text.trim().is_empty() && reported_tool_calls.is_empty() {
                return Err(Error::provider(
                    "gitlab",
                    "GitLab Chat returned an empty success response",
                ));
            }
            let reported_usage = parsed.usage.map(|u| {
                let total = if u.total_tokens > 0 {
                    u.total_tokens
                } else {
                    u.input_tokens.saturating_add(u.output_tokens)
                };
                Usage {
                    input: u.input_tokens,
                    output: u.output_tokens,
                    total_tokens: total,
                    ..Usage::default()
                }
            });
            Ok(ParsedGitLabPayload {
                text: response_text,
                reported_tool_calls,
                reported_usage,
            })
        }
        _ => Err(Error::provider(
            "gitlab",
            "Unsupported GitLab Chat success response shape",
        )),
    }
}

/// Some gateways repeat native calls inside the textual bridge. One call ID
/// names one invocation: accept an exact repeat once, but never guess which
/// payload a conflicting ID meant. Validate the whole batch before emitting
/// any events so a later malformed peer cannot authorize earlier tool work.
fn merge_tool_calls(
    reported: Vec<crate::model::ToolCall>,
    prompt: Vec<crate::model::ToolCall>,
) -> Result<Vec<crate::model::ToolCall>> {
    let mut calls: Vec<crate::model::ToolCall> = Vec::new();
    let mut call_indices: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (index, call) in reported.into_iter().chain(prompt).enumerate() {
        match call_indices.entry(call.id.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => {
                let previous = &calls[*entry.get()];
                if previous.name != call.name || previous.arguments != call.arguments {
                    return Err(Error::provider(
                        "gitlab",
                        format!(
                            "PI_TOOL_CALL_PROTOCOL: conflicting GitLab tool call ID at index {index}; local tool batch withheld"
                        ),
                    ));
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(calls.len());
                calls.push(call);
            }
        }
    }
    Ok(calls)
}

#[cfg(test)]
fn parse_gitlab_response(text: &str) -> Result<String> {
    parse_gitlab_payload(text, &std::collections::HashSet::new()).map(|p| p.text)
}

fn validate_request_rewrite(
    value: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let value = super::validate_streamed_json_rewrite(
        value,
        &["content"],
        &[],
        &[("with_clean_history", serde_json::Value::Bool(true))],
    )?;
    if value
        .get("content")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|content| content.trim().is_empty())
    {
        return Err("GitLab request content must contain non-whitespace text".to_string());
    }
    Ok(value)
}

fn push_error_secret(secrets: &mut Vec<String>, value: &str, authorization: bool) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    secrets.push(value.to_string());
    if authorization && let Some(separator) = value.find(char::is_whitespace) {
        let credential = value[separator..].trim();
        if !credential.is_empty() {
            secrets.push(credential.to_string());
        }
    }
}

const fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn escape_untrusted_diagnostic(text: &str, max_bytes: usize) -> String {
    const TRUNCATION_MARKER: &str = "...[truncated]";
    let mut escaped = String::with_capacity(text.len().min(max_bytes));
    for character in text.chars() {
        let rendered = if character.is_control()
            || is_bidi_control(character)
            || matches!(character, '\u{2028}' | '\u{2029}')
        {
            character.escape_unicode().to_string()
        } else {
            character.to_string()
        };
        if rendered.len() > max_bytes.saturating_sub(escaped.len()) {
            let available = max_bytes.saturating_sub(TRUNCATION_MARKER.len());
            while escaped.len() > available {
                escaped.pop();
            }
            escaped.push_str(&TRUNCATION_MARKER[..TRUNCATION_MARKER.len().min(max_bytes)]);
            break;
        }
        escaped.push_str(&rendered);
    }
    escaped
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
    exceeded: bool,
}

impl BoundedJsonWriter {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes.min(8192)),
            max_bytes,
            exceeded: false,
        }
    }
}

impl std::io::Write for BoundedJsonWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer.len() > self.max_bytes.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "GitLab request body exceeds configured limit",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialize_json_bounded<T: Serialize + ?Sized>(value: &T, max_bytes: usize) -> Result<Vec<u8>> {
    let mut writer = BoundedJsonWriter::new(max_bytes);
    if let Err(err) = serde_json::to_writer(&mut writer, value) {
        if writer.exceeded {
            return Err(Error::provider(
                "gitlab",
                format!("GitLab Chat request body exceeds the {max_bytes}-byte limit"),
            ));
        }
        return Err(Error::provider(
            "gitlab",
            format!("Failed to serialize request: {err}"),
        ));
    }
    Ok(writer.bytes)
}

fn serialize_request_bounded<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    serialize_json_bounded(value, MAX_GITLAB_REQUEST_BODY_BYTES)
}

fn redacted_gitlab_error_body(
    body: &str,
    api_key: &str,
    headers: &std::collections::HashMap<String, String>,
) -> String {
    let mut secrets = Vec::with_capacity(headers.len().saturating_add(1));
    push_error_secret(&mut secrets, api_key, false);
    for (name, value) in headers {
        push_error_secret(
            &mut secrets,
            value,
            name.eq_ignore_ascii_case("authorization"),
        );
    }
    let secret_refs = secrets.iter().map(String::as_str).collect::<Vec<_>>();
    let redacted = crate::auth::redact_known_secrets_bounded(
        body,
        &secret_refs,
        MAX_GITLAB_ERROR_DIAGNOSTIC_BYTES,
    );
    escape_untrusted_diagnostic(&redacted, MAX_GITLAB_ERROR_DIAGNOSTIC_BYTES)
}

async fn apply_overall_deadline<T, F>(operation: F, timeout: Option<Duration>) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let Some(timeout) = timeout else {
        return operation.await;
    };
    asupersync::time::timeout(asupersync::time::wall_now(), timeout, Box::pin(operation))
        .await
        .map_err(|_| {
            Error::provider(
                "gitlab",
                format!(
                    "GitLab Chat request timed out after the configured {timeout:?} overall deadline"
                ),
            )
        })?
}

fn user_content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                ContentBlock::Media(media) => Some(media.placeholder()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn text_block_bytes<'a>(blocks: impl Iterator<Item = &'a ContentBlock>) -> usize {
    let mut total = 0usize;
    let mut text_blocks = 0usize;
    for block in blocks {
        let len = match block {
            ContentBlock::Text(text) => text.text.len(),
            ContentBlock::Media(media) => media.placeholder().len(),
            _ => continue,
        };
        if text_blocks > 0 {
            total = total.saturating_add(1);
        }
        total = total.saturating_add(len);
        text_blocks = text_blocks.saturating_add(1);
    }
    total
}

fn user_content_text_bytes(content: &UserContent) -> usize {
    match content {
        UserContent::Text(text) => text.len(),
        UserContent::Blocks(blocks) => text_block_bytes(blocks.iter()),
    }
}

fn charge_request_text(total: &mut usize, bytes: usize) -> Result<()> {
    *total = total.saturating_add(bytes);
    if *total > MAX_GITLAB_REQUEST_TEXT_BYTES {
        return Err(Error::provider(
            "gitlab",
            format!(
                "GitLab Chat request text exceeds the {MAX_GITLAB_REQUEST_TEXT_BYTES}-byte limit"
            ),
        ));
    }
    Ok(())
}

fn push_context_item(
    additional_context: &mut Vec<GitLabContextItem>,
    item: GitLabContextItem,
) -> Result<()> {
    if additional_context.len() >= MAX_GITLAB_CONTEXT_ITEMS {
        return Err(Error::provider(
            "gitlab",
            format!(
                "GitLab Chat request exceeds the {MAX_GITLAB_CONTEXT_ITEMS}-item context limit"
            ),
        ));
    }
    additional_context.push(item);
    Ok(())
}

fn batch_response_events(message: AssistantMessage) -> Vec<Result<StreamEvent>> {
    // `Start.partial` represents the state before deltas are applied. Including
    // the final content here would make consumers that accumulate deltas render
    // every GitLab response twice.
    let mut partial = message.clone();
    partial.content.clear();
    let stop_reason = message.stop_reason;

    let mut events = vec![Ok(StreamEvent::Start { partial })];
    for (content_index, block) in message.content.iter().enumerate() {
        match block {
            ContentBlock::Text(text) => {
                events.push(Ok(StreamEvent::TextStart { content_index }));
                events.push(Ok(StreamEvent::TextDelta {
                    content_index,
                    delta: text.text.clone(),
                }));
                events.push(Ok(StreamEvent::TextEnd {
                    content_index,
                    content: text.text.clone(),
                }));
            }
            ContentBlock::ToolCall(tool_call) => {
                let args_json = serde_json::to_string(&tool_call.arguments)
                    .unwrap_or_else(|_| "{}".to_string());
                events.push(Ok(StreamEvent::ToolCallStart {
                    content_index,
                    id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                }));
                events.push(Ok(StreamEvent::ToolCallDelta {
                    content_index,
                    delta: args_json,
                }));
                events.push(Ok(StreamEvent::ToolCallEnd {
                    content_index,
                    tool_call: tool_call.clone(),
                }));
            }
            _ => {}
        }
    }
    events.push(Ok(StreamEvent::Done {
        reason: stop_reason,
        message,
    }));
    events
}

// ── Provider ─────────────────────────────────────────────────────

/// GitLab Duo provider.
pub struct GitLabProvider {
    /// HTTP client.
    client: Client,
    /// Model identifier.
    model: String,
    /// GitLab instance base URL (e.g., `https://gitlab.com` or `https://gitlab.example.com`).
    base_url: String,
    /// Provider name for event attribution.
    provider_name: String,
    /// Compatibility overrides (unused for GitLab but kept for interface consistency).
    #[allow(dead_code)]
    compat: Option<CompatConfig>,
}

impl GitLabProvider {
    /// Create a new GitLab Duo provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            base_url: DEFAULT_GITLAB_BASE.to_string(),
            provider_name: "gitlab".to_string(),
            compat: None,
        }
    }

    /// Set the GitLab instance base URL (for self-hosted).
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        let url = url.into();
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            self.base_url = trimmed.to_string();
        }
        self
    }

    /// Set the provider name for event attribution.
    #[must_use]
    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.provider_name = name.into();
        self
    }

    /// Attach compatibility overrides.
    #[must_use]
    pub fn with_compat(mut self, compat: Option<CompatConfig>) -> Self {
        self.compat = compat;
        self
    }

    /// Inject a custom HTTP client (for testing / VCR).
    #[must_use]
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Build the chat completions URL.
    fn chat_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}{CHAT_API_PATH}")
    }

    /// Build a GitLab Chat request from the agent context.
    #[allow(clippy::too_many_lines)]
    pub fn build_request(context: &Context<'_>) -> Result<GitLabChatRequest> {
        let (primary_index, primary_user) = context
            .messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, message)| {
                if let Message::User(user) = message {
                    Some((index, user))
                } else {
                    None
                }
            })
            .ok_or_else(|| Error::provider("gitlab", "GitLab Chat requires a user text message"))?;
        if let UserContent::Blocks(blocks) = &primary_user.content
            && blocks
                .iter()
                .any(|block| !matches!(block, ContentBlock::Text(_)))
        {
            return Err(Error::provider(
                "gitlab",
                "GitLab Chat does not support non-text blocks in the latest user message",
            ));
        }
        let mut request_text_bytes = 0usize;
        charge_request_text(
            &mut request_text_bytes,
            user_content_text_bytes(&primary_user.content),
        )?;
        let has_trailing_tool_turns = context.messages[primary_index + 1..]
            .iter()
            .any(|msg| matches!(msg, Message::Assistant(_) | Message::ToolResult(_)));
        let user_text = user_content_text(&primary_user.content);
        if user_text.trim().is_empty() {
            return Err(Error::provider(
                "gitlab",
                "GitLab Chat requires non-empty text in the latest user message",
            ));
        }

        let mut additional_context = Vec::new();

        if let Some(system) = &context.system_prompt {
            charge_request_text(
                &mut request_text_bytes,
                "[System]: ".len().saturating_add(system.len()),
            )?;
            push_context_item(
                &mut additional_context,
                GitLabContextItem {
                    category: "file".to_string(),
                    id: "system-prompt".to_string(),
                    content: format!("[System]: {system}"),
                },
            )?;
        }

        if !context.tools.is_empty() {
            let tool_instructions =
                crate::provider::format_prompt_tool_instructions(&context.tools);
            charge_request_text(&mut request_text_bytes, tool_instructions.len())?;
            push_context_item(
                &mut additional_context,
                GitLabContextItem {
                    category: "file".to_string(),
                    id: "pi-tool-definitions".to_string(),
                    content: tool_instructions,
                },
            )?;
        }

        let history_end = if has_trailing_tool_turns {
            context.messages.len()
        } else {
            primary_index
        };

        for (i, msg) in context.messages[..history_end].iter().enumerate() {
            match msg {
                Message::User(user_msg) => {
                    let text = user_content_text(&user_msg.content);
                    if !text.trim().is_empty() {
                        let rendered = format!("[User]: {text}");
                        if i != primary_index {
                            charge_request_text(&mut request_text_bytes, rendered.len())?;
                        }
                        push_context_item(
                            &mut additional_context,
                            GitLabContextItem {
                                category: "file".to_string(),
                                id: format!("message-{i}"),
                                content: rendered,
                            },
                        )?;
                    }
                }
                Message::Assistant(asst_msg) => {
                    let mut parts = Vec::new();
                    for block in &asst_msg.content {
                        match block {
                            ContentBlock::Text(t) if !t.text.trim().is_empty() => {
                                parts.push(t.text.clone());
                            }
                            ContentBlock::ToolCall(tc) => {
                                let args = serde_json::to_string(&tc.arguments)
                                    .unwrap_or_else(|_| "{}".to_string());
                                parts.push(format!(
                                    "{}{{\"id\":{},\"name\":{},\"arguments\":{args}}}{}",
                                    crate::provider::PI_TOOL_CALL_OPEN,
                                    serde_json::to_string(&tc.id)
                                        .unwrap_or_else(|_| "\"call\"".to_string()),
                                    serde_json::to_string(&tc.name)
                                        .unwrap_or_else(|_| "\"tool\"".to_string()),
                                    crate::provider::PI_TOOL_CALL_CLOSE,
                                ));
                            }
                            _ => {}
                        }
                    }
                    let text = parts.join("\n");
                    if !text.trim().is_empty() {
                        let rendered = format!("[Assistant]: {text}");
                        charge_request_text(&mut request_text_bytes, rendered.len())?;
                        push_context_item(
                            &mut additional_context,
                            GitLabContextItem {
                                category: "file".to_string(),
                                id: format!("message-{i}"),
                                content: rendered,
                            },
                        )?;
                    }
                }
                Message::ToolResult(tool_result) => {
                    let body = tool_result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let rendered = format!(
                        "[ToolResult id={} name={} is_error={}]: {}",
                        tool_result.tool_call_id, tool_result.tool_name, tool_result.is_error, body
                    );
                    charge_request_text(&mut request_text_bytes, rendered.len())?;
                    push_context_item(
                        &mut additional_context,
                        GitLabContextItem {
                            category: "file".to_string(),
                            id: format!("message-{i}"),
                            content: rendered,
                        },
                    )?;
                }
                Message::Custom(_) => {}
            }
        }

        let content = if has_trailing_tool_turns {
            let continuation = "Continue solving the user's request using the tool execution results in additional_context. Emit <pi_tool_call> if another tool step is required, or provide the final answer.".to_string();
            charge_request_text(&mut request_text_bytes, continuation.len())?;
            continuation
        } else {
            user_text
        };

        Ok(GitLabChatRequest {
            content,
            with_clean_history: true,
            additional_context,
        })
    }

    async fn stream_with_timeout(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
        overall_timeout: Option<Duration>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        apply_overall_deadline(self.stream_inner(context, options), overall_timeout).await
    }

    async fn stream_inner(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let request_body = Self::build_request(context)?;
        let original_body_bytes = serialize_request_bounded(&request_body)?;
        let original_body_len = original_body_bytes.len();
        let url = self.chat_url();

        let api_key = options.api_key.as_deref().ok_or_else(|| {
            Error::auth(
                "GitLab API token is required. Set GITLAB_TOKEN or GITLAB_API_KEY environment variable.",
            )
        })?;

        let rewritten_body = super::offer_before_provider_request(
            options,
            self.name(),
            self.api(),
            self.model_id(),
            &url,
            &request_body,
            validate_request_rewrite,
        )
        .await;
        let body_bytes = if let Some(rewritten_body) = rewritten_body.as_ref() {
            serialize_request_bounded(rewritten_body)?
        } else {
            original_body_bytes
        };

        let mut request = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .try_header("Authorization", format!("Bearer {api_key}"))?;

        // Add any custom headers from options.
        for (key, value) in &options.headers {
            request = request.try_header(key, value)?;
        }

        let response = Box::pin(request.body(body_bytes).send())
            .await
            .map_err(|e| Error::provider("gitlab", format!("Request failed: {e}")))?;
        let status = response.status();
        let max_body_bytes = if (200..300).contains(&status) {
            MAX_GITLAB_SUCCESS_BODY_BYTES
        } else {
            MAX_GITLAB_ERROR_BODY_BYTES
        };
        let text = response.text_limited(max_body_bytes).await.map_err(|err| {
            Error::provider(
                "gitlab",
                format!("Failed to read GitLab response body (HTTP {status}): {err}"),
            )
        })?;

        if !(200..300).contains(&status) {
            let body = redacted_gitlab_error_body(&text, api_key, &options.headers);
            return Err(Error::provider(
                "gitlab",
                format!("GitLab API error (HTTP {status}): {body}"),
            ));
        }

        // GitLab documents a JSON string response. Wrapper tolerance remains
        // for known self-hosted variants, and structured `<pi_tool_call>` blocks
        // bridge local tool calls when `context.tools` is populated.
        let allowed_tools: std::collections::HashSet<String> =
            context.tools.iter().map(|t| t.name.clone()).collect();
        let parsed = parse_gitlab_payload(&text, &allowed_tools)?;
        let (cleaned_text, prompt_tool_calls) =
            crate::provider::extract_prompt_tool_calls(&parsed.text, &allowed_tools);

        let tool_calls = merge_tool_calls(parsed.reported_tool_calls, prompt_tool_calls)?;

        let mut content_blocks = Vec::new();
        if tool_calls.is_empty() {
            content_blocks.push(ContentBlock::Text(TextContent {
                text: parsed.text.clone(),
                text_signature: None,
            }));
        } else {
            let trimmed_prose = cleaned_text.trim();
            if !trimmed_prose.is_empty() {
                content_blocks.push(ContentBlock::Text(TextContent {
                    text: trimmed_prose.to_string(),
                    text_signature: None,
                }));
            }
            for call in tool_calls {
                content_blocks.push(ContentBlock::ToolCall(call));
            }
        }

        let has_tool_calls = content_blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall(_)));
        let estimated_input = u64::try_from(original_body_len)
            .unwrap_or(u64::MAX)
            .div_ceil(4)
            .max(1);
        let estimated_output = u64::try_from(parsed.text.len())
            .unwrap_or(u64::MAX)
            .div_ceil(4)
            .max(1);
        let mut usage = parsed.reported_usage.unwrap_or_else(|| Usage {
            input: estimated_input,
            output: estimated_output,
            total_tokens: estimated_input.saturating_add(estimated_output),
            ..Usage::default()
        });
        if let Some(observer) = &options.usage_observer {
            observer.observe_authoritative(&mut usage)?;
        }

        // Build the final assistant message.
        let message = AssistantMessage {
            content: content_blocks,
            api: "gitlab-chat".to_string(),
            provider: self.provider_name.clone(),
            model: self.model.clone(),
            usage,
            stop_reason: if has_tool_calls {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            },
            stop_details: None,
            error_message: None,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };

        // GitLab Chat API is non-streaming, so we emit the full event sequence.
        let events = batch_response_events(message);

        Ok(Box::pin(stream::iter(events)))
    }
}

#[async_trait]
impl Provider for GitLabProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn api(&self) -> &'static str {
        "gitlab-chat"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            multi_turn: true,
            tool_use: true,
            native_tool_wire: false,
            streaming: false,
            vision: false,
            authoritative_usage: false,
        }
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let url = self.chat_url();
        self.stream_with_timeout(context, options, effective_default_request_timeout(&url))
            .await
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ImageContent, UserMessage};
    use crate::provider::ToolDef;
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    /// Polling interval for a fixture socket read. NOT the budget — see
    /// [`read_with_deadline`].
    const FIXTURE_POLL: Duration = Duration::from_millis(250);
    /// Wall-clock budget for one fixture request.
    const FIXTURE_BUDGET: Duration = Duration::from_secs(30);

    /// Read into `chunk`, treating a socket read timeout as "keep waiting"
    /// until `deadline` rather than as a hard error.
    ///
    /// macOS surfaces a read timeout as EAGAIN/`WouldBlock` (errno 35), not
    /// `TimedOut`, so `read().expect(..)` failed the test outright whenever a
    /// client had merely not been scheduled in time. These fixtures allowed two
    /// seconds, which is nothing under a parallel lib suite: the same shape
    /// took seven tests out of the Gemini and Vertex fixtures before it was
    /// fixed there (bd-eg6ng). The accept loops above have always been patient
    /// this way; the reads were not.
    fn read_with_deadline(
        socket: &mut TcpStream,
        chunk: &mut [u8],
        deadline: Instant,
        what: &str,
    ) -> usize {
        loop {
            match socket.read(chunk) {
                Ok(count) => return count,
                Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    assert!(
                        Instant::now() < deadline,
                        "fixture timed out waiting for {what}"
                    );
                }
                // ubs:ignore an unexpected socket error in a fixture is an assertion failure
                Err(err) => panic!("{what}: {err}"),
            }
        }
    }

    // Ownership is part of the call contract here: consuming the listener
    // closes it after the single accepted connection.
    #[allow(clippy::needless_pass_by_value)]
    fn accept_test_request(listener: TcpListener) -> std::net::TcpStream {
        listener
            .set_nonblocking(true)
            .expect("set nonblocking listener");
        let deadline = Instant::now() + FIXTURE_BUDGET;
        let (mut socket, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "timed out accepting request");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("accept request: {err}"),
            }
        };
        socket
            .set_read_timeout(Some(FIXTURE_POLL))
            .expect("set request read timeout");
        socket
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("set response write timeout");
        let deadline = Instant::now() + FIXTURE_BUDGET;
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = read_with_deadline(&mut socket, &mut chunk, deadline, "read request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        // Consume the body before closing the connection after our response.
        // Otherwise a split request can leave unread bytes and reset the
        // socket, hiding the provider behavior this fixture should exercise.
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("complete request headers");
        let body_len = String::from_utf8_lossy(&request[..header_end])
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .unwrap_or_default();
        while request.len() < header_end + 4 + body_len {
            let read = read_with_deadline(&mut socket, &mut chunk, deadline, "request body");
            assert!(read > 0, "request body ended early");
            request.extend_from_slice(&chunk[..read]);
        }
        socket
    }

    fn stream_tool_response(
        response: serde_json::Value,
        tools: Vec<ToolDef>,
    ) -> Result<Vec<StreamEvent>> {
        use futures::TryStreamExt;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind GitLab fixture");
        let address = listener.local_addr().expect("fixture address");
        let body = response.to_string();
        let server = std::thread::spawn(move || {
            let mut socket = accept_test_request(listener);
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write GitLab response");
        });
        let provider =
            GitLabProvider::new("gitlab-duo-chat").with_base_url(format!("http://{address}"));
        let context = Context::owned(
            None,
            vec![Message::User(UserMessage {
                content: UserContent::Text("Use the available tools.".to_string()),
                timestamp: 1,
            })],
            tools,
        );
        let options = StreamOptions {
            api_key: Some("glpat-fixture".to_string()),
            ..StreamOptions::default()
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("fixture runtime");
        let result = runtime.block_on(async {
            provider
                .stream(&context, &options)
                .await?
                .try_collect()
                .await
        });
        server.join().expect("GitLab fixture completed");
        result
    }

    fn tool_definition(name: &str) -> ToolDef {
        ToolDef {
            name: name.to_string(),
            description: "Test tool".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn run_stream_bounded(
        provider: GitLabProvider,
        context: Context<'static>,
        options: StreamOptions,
        overall_timeout: Option<Duration>,
    ) -> std::result::Result<(), String> {
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime");
            let result = runtime
                .block_on(provider.stream_with_timeout(&context, &options, overall_timeout))
                .map(|_| ())
                .map_err(|err| err.to_string());
            let _ = result_tx.send(result);
        });
        let result = result_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("GitLab provider worker must not hang");
        worker.join().expect("provider worker");
        result
    }

    #[test]
    fn test_gitlab_provider_defaults() {
        let p = GitLabProvider::new("gitlab-duo-chat");
        assert_eq!(p.name(), "gitlab");
        assert_eq!(p.api(), "gitlab-chat");
        assert_eq!(p.model_id(), "gitlab-duo-chat");
        assert_eq!(p.base_url, DEFAULT_GITLAB_BASE);
    }

    #[test]
    fn test_gitlab_provider_builder() {
        let p = GitLabProvider::new("gitlab-duo-chat")
            .with_provider_name("gitlab-duo")
            .with_base_url("https://gitlab.example.com");

        assert_eq!(p.name(), "gitlab-duo");
        assert_eq!(p.base_url, "https://gitlab.example.com");
    }

    #[test]
    fn test_gitlab_chat_url_construction() {
        let p = GitLabProvider::new("model");
        assert_eq!(p.chat_url(), "https://gitlab.com/api/v4/chat/completions");

        let p = GitLabProvider::new("model").with_base_url("https://gitlab.example.com/");
        assert_eq!(
            p.chat_url(),
            "https://gitlab.example.com/api/v4/chat/completions"
        );
    }

    #[test]
    fn test_build_request_simple() {
        let context = Context::owned(
            Some("Be helpful.".to_string()),
            vec![Message::User(UserMessage {
                content: UserContent::Text("How do I define a class?".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );

        let req = GitLabProvider::build_request(&context).expect("valid request");
        assert_eq!(req.content, "How do I define a class?");
        assert!(req.with_clean_history);
        assert_eq!(req.additional_context.len(), 1); // system prompt
        assert_eq!(req.additional_context[0].id, "system-prompt");
    }

    #[test]
    fn test_build_request_multi_turn() {
        let context = Context::owned(
            None,
            vec![
                Message::User(UserMessage {
                    content: UserContent::Text("What is Rust?".to_string()),
                    timestamp: 0,
                }),
                Message::assistant(AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent {
                        text: "Rust is a systems language.".to_string(),
                        text_signature: None,
                    })],
                    api: String::new(),
                    provider: String::new(),
                    model: String::new(),
                    usage: Usage::default(),
                    stop_reason: StopReason::default(),
                    stop_details: None,
                    error_message: None,
                    timestamp: 0,
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("Tell me more.".to_string()),
                    timestamp: 0,
                }),
            ],
            Vec::new(),
        );

        let req = GitLabProvider::build_request(&context).expect("valid request");
        assert_eq!(req.content, "Tell me more.");
        // Should have 2 context items: first user msg + assistant response.
        assert_eq!(req.additional_context.len(), 2);
    }

    #[test]
    fn test_build_request_rejects_missing_user_message() {
        let context = Context::owned(None, Vec::new(), Vec::new());

        let err = GitLabProvider::build_request(&context).expect_err("missing user must fail");
        assert!(err.to_string().contains("requires a user text message"));
    }

    #[test]
    fn test_build_request_does_not_replay_older_text_for_image_only_latest_turn() {
        let context = Context::owned(
            None,
            vec![
                Message::User(UserMessage {
                    content: UserContent::Text("older prompt must not be resent".to_string()),
                    timestamp: 0,
                }),
                Message::User(UserMessage {
                    content: UserContent::Blocks(vec![ContentBlock::Image(ImageContent {
                        data: "AA==".to_string(),
                        mime_type: "image/png".to_string(),
                    })]),
                    timestamp: 1,
                }),
            ],
            Vec::new(),
        );

        let err = GitLabProvider::build_request(&context)
            .expect_err("an image-only GitLab turn must be rejected");
        assert!(err.to_string().contains("latest user message"));
        assert!(!err.to_string().contains("older prompt"));
    }

    #[test]
    fn test_build_request_rejects_mixed_text_and_image_latest_turn() {
        let context = Context::owned(
            None,
            vec![Message::User(UserMessage {
                content: UserContent::Blocks(vec![
                    ContentBlock::Text(TextContent::new("describe this image")),
                    ContentBlock::Image(ImageContent {
                        data: "AA==".to_string(),
                        mime_type: "image/png".to_string(),
                    }),
                ]),
                timestamp: 0,
            })],
            Vec::new(),
        );

        let err = GitLabProvider::build_request(&context)
            .expect_err("mixed text/image GitLab turns must not silently lose the image");
        assert!(err.to_string().contains("does not support non-text blocks"));
    }

    #[test]
    fn test_build_request_omits_standard_tool_call_schema() {
        let context = Context::owned(
            Some("Use tools when available.".to_string()),
            vec![Message::User(UserMessage {
                content: UserContent::Text("Call echo with hello.".to_string()),
                timestamp: 0,
            })],
            vec![ToolDef {
                name: "echo".to_string(),
                description: "Echo text back to the caller.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"}
                    },
                    "required": ["text"]
                }),
            }],
        );

        let req = GitLabProvider::build_request(&context).expect("valid request");
        let encoded = serde_json::to_value(&req).expect("serialize gitlab request");

        assert_eq!(req.content, "Call echo with hello.");
        assert_eq!(encoded["with_clean_history"], serde_json::Value::Bool(true));
        assert_eq!(context.tools.len(), 1);
        assert!(encoded.get("tools").is_none());
        assert!(encoded.get("tool_choice").is_none());
        assert!(encoded.get("functions").is_none());
    }

    #[test]
    fn test_gitlab_documented_json_string_response() {
        let json = r#""line one\nline \"two\" \\ path — 🦀""#;
        assert_eq!(
            parse_gitlab_response(json).expect("documented response"),
            "line one\nline \"two\" \\ path — 🦀"
        );
    }

    #[test]
    fn test_gitlab_wrapper_response_tolerance() {
        let json = r#"{"response": "Here is how you define a class in Ruby..."}"#;
        assert_eq!(
            parse_gitlab_response(json).expect("wrapper response"),
            "Here is how you define a class in Ruby..."
        );
        let whitespace_response = r#"{"response": " \n\t ", "content": "fallback"}"#;
        assert_eq!(
            parse_gitlab_response(whitespace_response).expect("nonblank fallback content"),
            "fallback"
        );
    }

    #[test]
    fn test_gitlab_content_wrapper_tolerance_and_non_json_rejection() {
        let json = r#"{"content": "Alternative response format"}"#;
        assert_eq!(
            parse_gitlab_response(json).expect("content wrapper"),
            "Alternative response format"
        );
        for body in [
            "",
            "   \n\t",
            "plain response",
            "<html>proxy error</html>",
            "{\"response\":",
        ] {
            let error = parse_gitlab_response(body).expect_err("non-JSON success must fail");
            assert!(error.to_string().contains("Invalid GitLab Chat JSON"));
        }
    }

    #[test]
    fn test_gitlab_valid_unsupported_json_is_a_protocol_error() {
        for body in [
            "{}",
            "null",
            "[]",
            r#"{"response":""}"#,
            r#"{"response":" \n\t ","content":" "}"#,
            r#"" \n\t ""#,
        ] {
            let err = parse_gitlab_response(body).expect_err("unsupported JSON must fail");
            assert!(
                err.to_string().contains("GitLab Chat")
                    && (err.to_string().contains("Unsupported")
                        || err.to_string().contains("empty success")),
                "{body}: {err}"
            );
        }
    }

    #[test]
    fn test_gitlab_batch_events_do_not_duplicate_final_text_in_start() {
        let message = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("answer"))],
            api: "gitlab-chat".to_string(),
            provider: "gitlab".to_string(),
            model: "model".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        };
        let events = batch_response_events(message)
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .expect("event sequence");

        let StreamEvent::Start { partial } = &events[0] else {
            panic!("first event must be Start");
        };
        assert!(partial.content.is_empty());
        assert!(matches!(
            &events[2],
            StreamEvent::TextDelta { delta, .. } if delta == "answer"
        ));
        let StreamEvent::Done { message, .. } = &events[4] else {
            panic!("last event must be Done");
        };
        assert!(matches!(
            message.content.as_slice(),
            [ContentBlock::Text(text)] if text.text == "answer"
        ));
    }

    #[test]
    fn test_gitlab_rewrite_cannot_disable_clean_history() {
        let rewritten = validate_request_rewrite(serde_json::json!({
            "content": "hello",
            "with_clean_history": false
        }))
        .expect("valid rewrite");
        assert_eq!(
            rewritten["with_clean_history"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn test_gitlab_rewrite_rejects_whitespace_only_content() {
        let error = validate_request_rewrite(serde_json::json!({
            "content": " \n\t ",
            "with_clean_history": true
        }))
        .expect_err("rewritten content must preserve the nonblank request invariant");
        assert!(error.contains("non-whitespace"), "{error}");
    }

    #[test]
    fn test_gitlab_error_diagnostic_is_bounded_redacted_and_control_safe() {
        let headers = std::collections::HashMap::from([(
            "Authorization".to_string(),
            "Bearer custom-secret".to_string(),
        )]);
        let body = format!(
            "token=test-token custom=custom-secret esc=\u{1b}[31m bidi=\u{202e} lines=\u{2028}\u{2029} {}",
            "x".repeat(MAX_GITLAB_ERROR_DIAGNOSTIC_BYTES * 2)
        );
        let diagnostic = redacted_gitlab_error_body(&body, "test-token", &headers);

        assert!(diagnostic.len() <= MAX_GITLAB_ERROR_DIAGNOSTIC_BYTES);
        assert!(!diagnostic.contains("test-token"));
        assert!(!diagnostic.contains("custom-secret"));
        assert!(diagnostic.contains("[REDACTED]"));
        assert!(!diagnostic.contains('\u{1b}'));
        assert!(!diagnostic.contains('\u{202e}'));
        assert!(!diagnostic.contains('\u{2028}'));
        assert!(!diagnostic.contains('\u{2029}'));
        assert!(diagnostic.contains(r"\u{1b}"));
        assert!(diagnostic.contains(r"\u{202e}"));
        assert!(diagnostic.contains(r"\u{2028}"));
        assert!(diagnostic.contains(r"\u{2029}"));
    }

    #[test]
    fn test_gitlab_request_serialization_is_bounded() {
        let request = serde_json::json!({"content": "payload"});
        let error = serialize_json_bounded(&request, 8)
            .expect_err("serialized request above the limit must fail");
        assert!(error.to_string().contains("8-byte limit"));
    }

    #[test]
    fn test_gitlab_request_text_and_context_item_limits_are_exact() {
        let mut text_bytes = MAX_GITLAB_REQUEST_TEXT_BYTES;
        charge_request_text(&mut text_bytes, 0).expect("exact text limit");
        let error = charge_request_text(&mut text_bytes, 1)
            .expect_err("text beyond the exact limit must fail");
        assert!(error.to_string().contains("request text exceeds"));

        let mut context = (0..MAX_GITLAB_CONTEXT_ITEMS - 1)
            .map(|index| GitLabContextItem {
                category: "file".to_string(),
                id: format!("context-{index}"),
                content: String::new(),
            })
            .collect::<Vec<_>>();
        push_context_item(
            &mut context,
            GitLabContextItem {
                category: "file".to_string(),
                id: "exact-limit".to_string(),
                content: String::new(),
            },
        )
        .expect("the exact context-item limit must be accepted");
        assert_eq!(context.len(), MAX_GITLAB_CONTEXT_ITEMS);
        let error = push_context_item(
            &mut context,
            GitLabContextItem {
                category: "file".to_string(),
                id: "one-too-many".to_string(),
                content: String::new(),
            },
        )
        .expect_err("context beyond the exact item limit must fail");
        assert!(error.to_string().contains("item context limit"));
    }

    #[test]
    fn test_gitlab_rewrite_cannot_expand_the_serialized_body_without_bound() {
        let provider = GitLabProvider::new("model").with_base_url("http://127.0.0.1:1");
        let context = Context::owned(
            None,
            vec![Message::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let options = StreamOptions {
            api_key: Some("test-token".to_string()),
            before_provider_request: Some(crate::provider::BeforeProviderRequestHook::new(|_| {
                Box::pin(futures::future::ready(Some(serde_json::json!({
                    "content": "x".repeat(MAX_GITLAB_REQUEST_BODY_BYTES),
                    "with_clean_history": true
                }))))
            })),
            ..StreamOptions::default()
        };
        let error = run_stream_bounded(provider, context, options, None)
            .expect_err("oversized rewrite must fail before network I/O");
        assert!(error.contains("request body exceeds"), "{error}");
    }

    #[test]
    fn test_gitlab_overall_deadline_includes_rewrite_hook() {
        let provider = GitLabProvider::new("model").with_base_url("http://127.0.0.1:1");
        let context = Context::owned(
            None,
            vec![Message::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let options = StreamOptions {
            api_key: Some("test-token".to_string()),
            before_provider_request: Some(crate::provider::BeforeProviderRequestHook::new(|_| {
                Box::pin(futures::future::pending())
            })),
            ..StreamOptions::default()
        };
        let error = run_stream_bounded(provider, context, options, Some(Duration::from_millis(25)))
            .expect_err("stalled rewrite hook must hit the overall deadline");
        assert!(error.contains("overall deadline"), "{error}");
    }

    #[test]
    fn test_gitlab_error_response_uses_status_aware_body_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let body = format!(
            "test-token\u{1b}\u{202e}{}",
            "x".repeat(MAX_GITLAB_ERROR_BODY_BYTES)
        );
        let server = std::thread::spawn(move || {
            let mut socket = accept_test_request(listener);
            let _ = write!(
                socket,
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        });

        let provider = GitLabProvider::new("model").with_base_url(format!("http://{address}"));
        let context = Context::owned(
            None,
            vec![Message::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let options = StreamOptions {
            api_key: Some("test-token".to_string()),
            ..StreamOptions::default()
        };
        let error = run_stream_bounded(provider, context, options, Some(Duration::from_secs(3)))
            .expect_err("oversized error response must fail closed");
        server.join().expect("test server");
        assert!(error.contains("response body too large"), "{error}");
        assert!(!error.contains("test-token"));
        assert!(!error.contains('\u{1b}'));
        assert!(!error.contains('\u{202e}'));
    }

    #[test]
    fn test_gitlab_finite_response_has_overall_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = std::thread::spawn(move || {
            let mut socket = accept_test_request(listener);
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 20\r\nConnection: close\r\n\r\n",
                )
                .expect("write response headers");
            for _ in 0..20 {
                if socket.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });

        let provider = GitLabProvider::new("model").with_base_url(format!("http://{address}"));
        let context = Context::owned(
            None,
            vec![Message::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let options = StreamOptions {
            api_key: Some("test-token".to_string()),
            ..StreamOptions::default()
        };
        let started = Instant::now();
        let error =
            run_stream_bounded(provider, context, options, Some(Duration::from_millis(125)))
                .expect_err("slow-drip response must hit the overall deadline");
        let request_elapsed = started.elapsed();
        server.join().expect("test server");
        assert!(error.contains("overall deadline"), "{error}");
        assert!(request_elapsed < Duration::from_secs(1));
    }

    #[test]
    fn test_gitlab_api_key_cannot_inject_headers() {
        let provider = GitLabProvider::new("model").with_base_url("http://127.0.0.1:1");
        let context = Context::owned(
            None,
            vec![Message::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let options = StreamOptions {
            api_key: Some("token\r\nX-Injected: yes".to_string()),
            ..StreamOptions::default()
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(provider.stream_with_timeout(&context, &options, None))
            .map(|_| ())
            .expect_err("credential control bytes must fail before network I/O");
        let error = error.to_string();
        assert!(error.contains("header"), "{error}");
        assert!(!error.contains("token"), "{error}");
        assert!(!error.contains("X-Injected"), "{error}");
        assert!(!error.contains('\r'), "{error:?}");
        assert!(!error.contains('\n'), "{error:?}");
    }

    #[test]
    fn test_gitlab_success_body_io_failure_is_not_an_assistant_message() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener
            .set_nonblocking(true)
            .expect("set nonblocking listener");
        let address = listener.local_addr().expect("test server address");
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "timed out accepting request");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("accept request: {err}"),
                }
            };
            socket
                .set_read_timeout(Some(FIXTURE_POLL))
                .expect("set request read timeout");
            let deadline = Instant::now() + FIXTURE_BUDGET;
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = read_with_deadline(&mut socket, &mut chunk, deadline, "read request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 128\r\nConnection: close\r\n\r\n\"truncated",
                )
                .expect("write truncated response");
        });

        let provider = GitLabProvider::new("model").with_base_url(format!("http://{address}"));
        let context = Context::owned(
            None,
            vec![Message::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let options = StreamOptions {
            api_key: Some("test-token".to_string()),
            ..StreamOptions::default()
        };
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let provider_worker = std::thread::spawn(move || {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime");
            let result = runtime
                .block_on(provider.stream(&context, &options))
                .map(|_| ())
                .map_err(|err| err.to_string());
            let _ = result_tx.send(result);
        });
        let result = result_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("GitLab response-body failure path must not hang");
        provider_worker.join().expect("provider worker");
        server.join().expect("test server");
        let err = result.expect_err("truncated successful body must remain an error");
        assert!(
            err.contains("Failed to read GitLab response body (HTTP 200)"),
            "{err}"
        );
    }

    #[test]
    fn test_gitlab_empty_base_url_uses_default() {
        let p = GitLabProvider::new("model").with_base_url("");
        assert_eq!(p.base_url, DEFAULT_GITLAB_BASE);
    }

    #[test]
    fn test_gitlab_whitespace_base_url_uses_default() {
        let p = GitLabProvider::new("model").with_base_url("   \n\t  ");
        assert_eq!(p.base_url, DEFAULT_GITLAB_BASE);
    }

    #[test]
    fn test_gitlab_base_url_is_trimmed() {
        let p = GitLabProvider::new("model").with_base_url(" https://gitlab.example.com/ ");
        assert_eq!(p.base_url, "https://gitlab.example.com/");
        assert_eq!(
            p.chat_url(),
            "https://gitlab.example.com/api/v4/chat/completions"
        );
    }

    #[test]
    fn test_gitlab_native_calls_require_an_advertised_tool() {
        for tools in [Vec::new(), vec![tool_definition("other")]] {
            let events = stream_tool_response(
                serde_json::json!({
                    "response": "Ordinary answer.",
                    "tool_calls": [{
                        "id": "unrequested", "name": "echo", "arguments": {"text": "hello"}
                    }]
                }),
                tools,
            )
            .expect("text remains available without admitting an unadvertised tool");
            assert!(!events.iter().any(|event| matches!(
                event,
                StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallEnd { .. }
            )));
            assert!(matches!(
                events.last(),
                Some(StreamEvent::Done { reason: StopReason::Stop, message })
                    if matches!(message.content.as_slice(), [ContentBlock::Text(text)]
                        if text.text == "Ordinary answer.")
            ));
        }
    }

    #[test]
    fn test_gitlab_duplicate_json_keys_withhold_the_native_batch() {
        let tools = std::collections::HashSet::from(["echo".to_string()]);
        for ambiguous in [
            r#"{"name":"echo","name":"echo"}"#,
            r#"{"name":"echo","arguments":{"items":[{"text":1,"te\u0078t":2}]}}"#,
            r#"{"name":"echo","arguments":"{\"text\":1,\"text\":2}"}"#,
        ] {
            let raw = format!(r#"{{"tool_calls":[{{"name":"echo"}},{ambiguous}]}}"#);
            assert!(parse_gitlab_payload(&raw, &tools).is_err());
        }
        let ambiguous_usage = r#"{"response":"ok","usage":{"input_tokens":10,"input_tokens":1}}"#;
        assert!(parse_gitlab_payload(ambiguous_usage, &tools).is_err());
    }

    #[test]
    fn test_gitlab_malformed_native_arguments_withhold_the_entire_batch() {
        for arguments in [
            serde_json::Value::Null,
            serde_json::json!(false),
            serde_json::json!(12),
            serde_json::json!([]),
            serde_json::json!("[1, 2]"),
            serde_json::json!("null"),
            serde_json::json!("{\"text\":\"private-argument-token\""),
        ] {
            let error = stream_tool_response(
                serde_json::json!({
                    "response": "Run these tools.",
                    "tool_calls": [
                        {"id": "valid-peer", "name": "echo", "arguments": {}},
                        {"id": "private-call-token", "name": "echo", "arguments": arguments}
                    ]
                }),
                vec![tool_definition("echo")],
            )
            .expect_err("invalid peer must fail before any tool event is emitted")
            .to_string();
            assert!(error.contains("PI_TOOL_CALL_PROTOCOL"), "{error}");
            assert!(error.contains("index 1"), "{error}");
            assert!(!error.contains("private-"), "{error}");
        }
    }

    #[test]
    fn test_gitlab_native_arguments_accept_objects_and_omitted_empty_input() {
        for (arguments, expected) in [
            (None, serde_json::json!({})),
            (Some(serde_json::json!({})), serde_json::json!({})),
            (
                Some(serde_json::json!({"text": "hello"})),
                serde_json::json!({"text": "hello"}),
            ),
            (
                Some(serde_json::json!("{\"text\":\"hello\"}")),
                serde_json::json!({"text": "hello"}),
            ),
        ] {
            let mut call = serde_json::json!({"id": " call-1 ", "name": "echo"});
            if let Some(arguments) = arguments {
                call["arguments"] = arguments;
            }
            let events = stream_tool_response(
                serde_json::json!({"tool_calls": [call]}),
                vec![tool_definition("echo")],
            )
            .expect("valid native arguments");
            assert!(matches!(
                events.last(),
                Some(StreamEvent::Done { reason: StopReason::ToolUse, message })
                    if matches!(message.content.as_slice(), [ContentBlock::ToolCall(call)]
                        if call.id == "call-1" && call.arguments == expected)
            ));
        }
    }

    #[test]
    fn test_gitlab_repeated_native_and_prompt_call_is_one_invocation() {
        let call = serde_json::json!({
            "id": "call-1", "name": "echo", "arguments": {"text": "hello"}
        });
        let events = stream_tool_response(
            serde_json::json!({
                "response": format!("Checking. <pi_tool_call>{call}</pi_tool_call>"),
                "tool_calls": [call.clone(), call]
            }),
            vec![tool_definition("echo")],
        )
        .expect("matching native and textual representations identify one invocation");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::ToolCallStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
                .count(),
            1
        );
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Done { reason: StopReason::ToolUse, message })
                if matches!(message.content.as_slice(), [ContentBlock::Text(text), ContentBlock::ToolCall(call)]
                    if text.text == "Checking." && call.id == "call-1")
        ));
    }

    #[test]
    fn test_gitlab_conflicting_tool_id_withholds_native_and_prompt_batches() {
        let original = serde_json::json!({
            "id": "private-call-token", "name": "echo", "arguments": {"text": "original"}
        });
        for conflicting in [
            serde_json::json!({
                "id": "private-call-token", "name": "echo", "arguments": {"text": "private-argument-token"}
            }),
            serde_json::json!({
                "id": "private-call-token", "name": "other", "arguments": {"text": "original"}
            }),
        ] {
            for native_conflict in [false, true] {
                let response = if native_conflict {
                    serde_json::json!({"tool_calls": [original.clone(), conflicting.clone()]})
                } else {
                    serde_json::json!({
                        "response": format!("<pi_tool_call>{conflicting}</pi_tool_call>"),
                        "tool_calls": [original.clone()]
                    })
                };
                let error = stream_tool_response(
                    response,
                    vec![tool_definition("echo"), tool_definition("other")],
                )
                .expect_err("one ID cannot authorize different tool invocations")
                .to_string();
                assert!(error.contains("PI_TOOL_CALL_PROTOCOL"), "{error}");
                assert!(error.contains("conflicting"), "{error}");
                assert!(!error.contains("private-"), "{error}");
            }
        }
    }

    #[test]
    fn test_gitlab_distinct_tool_ids_preserve_repeated_intent() {
        let events = stream_tool_response(
            serde_json::json!({
                "response": "<pi_tool_call>{\"id\":\"call-2\",\"name\":\"echo\",\"arguments\":{}}</pi_tool_call>",
                "tool_calls": [{"id": "call-1", "name": "echo", "arguments": {}}]
            }),
            vec![tool_definition("echo")],
        )
        .expect("different IDs remain separate invocations");
        let ids = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ToolCallEnd { tool_call, .. } => Some(tool_call.id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, ["call-1", "call-2"]);
    }

    #[test]
    fn test_gitlab_multi_turn_tool_call_and_tool_result_round_trip() {
        use futures::StreamExt;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock GitLab");
        let addr = listener.local_addr().expect("addr");

        let server = std::thread::spawn(move || {
            for turn in 0..2 {
                let (mut socket, _) = listener.accept().expect("accept");
                socket
                    .set_read_timeout(Some(Duration::from_millis(250)))
                    .expect("read timeout");
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = match socket.read(&mut buf) {
                        Ok(n) => n,
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "fixture timed out waiting for GitLab HTTP request"
                            );
                            continue;
                        }
                        Err(e) => panic!("read: {e}"),
                    };
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    if let Some(hdr_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        let hdrs = String::from_utf8_lossy(&raw[..hdr_end]);
                        let content_len = hdrs
                            .lines()
                            .find_map(|line| {
                                let (k, v) = line.split_once(':')?;
                                k.trim()
                                    .eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if raw.len() >= hdr_end + 4 + content_len {
                            let body_slice = &raw[hdr_end + 4..hdr_end + 4 + content_len];
                            let req_json: serde_json::Value =
                                serde_json::from_slice(body_slice).expect("valid GitLab JSON");
                            let ctx_items = req_json["additional_context"]
                                .as_array()
                                .expect("additional_context array");
                            if turn == 0 {
                                assert!(
                                    ctx_items
                                        .iter()
                                        .any(|item| item["id"] == "pi-tool-definitions"),
                                    "turn 0 must attach tool definitions in additional_context"
                                );
                                let resp_body = serde_json::to_string(
                                    r#"Let me check.<pi_tool_call>{"id":"call_gl_1","name":"echo","arguments":{"text":"hello-gitlab"}}</pi_tool_call>"#,
                                )
                                .expect("json string");
                                write!(
                                    socket,
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                    resp_body.len(),
                                    resp_body
                                )
                                .expect("write turn 0");
                            } else {
                                assert!(
                                    ctx_items.iter().any(|item| {
                                        item["content"]
                                            .as_str()
                                            .is_some_and(|c| c.contains("tool-output-42"))
                                    }),
                                    "turn 1 must forward ToolResult in additional_context"
                                );
                                let resp_body = r#"{"response":"Echo returned tool-output-42.","usage":{"input_tokens":50,"output_tokens":12}}"#;
                                write!(
                                    socket,
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                    resp_body.len(),
                                    resp_body
                                )
                                .expect("write turn 1");
                            }
                            break;
                        }
                    }
                }
            }
        });

        let provider =
            GitLabProvider::new("gitlab-duo-chat").with_base_url(format!("http://{addr}"));
        let caps = provider.capabilities();
        assert!(caps.multi_turn && caps.tool_use && !caps.native_tool_wire && !caps.streaming);

        let tools = vec![ToolDef {
            name: "echo".to_string(),
            description: "Echo text".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}}),
        }];
        let options = StreamOptions {
            api_key: Some("glpat-test".to_string()),
            ..StreamOptions::default()
        };

        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            let user_msg = Message::User(UserMessage {
                content: UserContent::Text("Echo hello-gitlab".to_string()),
                timestamp: 1,
            });
            let ctx1 = Context::owned(None, vec![user_msg.clone()], tools.clone());
            let events1: Vec<StreamEvent> = provider
                .stream(&ctx1, &options)
                .await
                .expect("turn 0 stream")
                .map(|e| e.expect("event"))
                .collect()
                .await;
            assert!(events1.iter().any(|e| matches!(e, StreamEvent::ToolCallEnd { tool_call, .. } if tool_call.name == "echo" && tool_call.id == "call_gl_1")));
            let Some(StreamEvent::Done {
                reason: StopReason::ToolUse,
                message: asst1,
            }) = events1.last().cloned()
            else {
                panic!("expected ToolUse Done on turn 0, got {events1:?}");
            };
            assert!(asst1.usage.total_tokens > 0);

            let tool_res = Message::ToolResult(std::sync::Arc::new(crate::model::ToolResultMessage {
                tool_call_id: "call_gl_1".to_string(),
                tool_name: "echo".to_string(),
                content: vec![ContentBlock::Text(TextContent::new("tool-output-42"))],
                details: None,
                is_error: false,
                timestamp: 2,
            }));
            let ctx2 = Context::owned(
                None,
                vec![user_msg, Message::assistant(asst1), tool_res],
                tools,
            );
            let events2: Vec<StreamEvent> = provider
                .stream(&ctx2, &options)
                .await
                .expect("turn 1 stream")
                .map(|e| e.expect("event"))
                .collect()
                .await;
            let Some(StreamEvent::Done {
                reason: StopReason::Stop,
                message: asst2,
            }) = events2.last()
            else {
                panic!("expected Stop Done on turn 1, got {events2:?}");
            };
            assert_eq!(asst2.usage.input, 50);
            assert_eq!(asst2.usage.output, 12);
            assert_eq!(asst2.usage.total_tokens, 62);
        });
        server.join().expect("join server");
    }
}
