//! Amazon Bedrock Converse provider implementation.
//!
//! Uses ConverseStream for incremental text, tool calls, and reasoning. Explicit
//! Converse endpoints and JSON-speaking gateways retain the bounded JSON path.

use crate::auth::{
    AUTH_RESOLUTION_LOCK_TIMEOUT, AuthStorage, AuthStorageLoadFailure, AwsResolvedCredentials,
    resolve_ambient_aws_credentials_async, resolve_aws_credentials_async,
};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::http::client::{Client, RequestBuilder};
use crate::model::{
    AssistantMessage, ContentBlock, Message, RedactedThinkingContent, StopReason, StreamEvent,
    TextContent, ThinkingContent, ToolCall, ToolResultMessage, Usage, UserContent,
};
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::Stream;
use futures::stream;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use url::Url;

mod request_options;
mod streaming;

const DEFAULT_REGION: &str = "us-east-1";
const BEDROCK_SERVICE: &str = "bedrock";
const MAX_BEDROCK_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_BEDROCK_ERROR_SNIPPET_BYTES: usize = 8 * 1024;
const MAX_BEDROCK_CONTENT_BLOCKS: usize = 1024;
const SIGV4_OWNED_HEADERS: [&str; 6] = [
    "authorization",
    "host",
    "content-type",
    "x-amz-date",
    "x-amz-content-sha256",
    "x-amz-security-token",
];

type HmacSha256 = Hmac<Sha256>;

fn bedrock_region_with_env<F>(mut env: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    env("AWS_REGION")
        .and_then(|region| {
            let region = region.trim();
            (!region.is_empty()).then(|| region.to_string())
        })
        .or_else(|| {
            env("AWS_DEFAULT_REGION").and_then(|region| {
                let region = region.trim();
                (!region.is_empty()).then(|| region.to_string())
            })
        })
        .unwrap_or_else(|| DEFAULT_REGION.to_string())
}

fn bedrock_error_snippet(response: &str, secrets: &[String]) -> String {
    let secret_refs = secrets.iter().map(String::as_str).collect::<Vec<_>>();
    crate::auth::redact_known_secrets_bounded(
        response,
        &secret_refs,
        MAX_BEDROCK_ERROR_SNIPPET_BYTES,
    )
}

fn apply_bedrock_headers<'a>(
    mut request: RequestBuilder<'a>,
    headers: &HashMap<String, String>,
    response_secrets: &mut Vec<String>,
) -> Result<RequestBuilder<'a>> {
    for (name, value) in headers {
        response_secrets.push(value.clone());
        if name.eq_ignore_ascii_case("Authorization")
            && let Some(separator) = value.find(char::is_whitespace)
            && !value[separator..].trim().is_empty()
        {
            response_secrets.push(value[separator..].trim().to_string());
        }
        request = request.try_header(name, value)?;
    }
    Ok(request)
}

fn validate_sigv4_header_ownership(headers: &HashMap<String, String>, source: &str) -> Result<()> {
    let mut collisions = headers
        .keys()
        .filter(|name| {
            let normalized = name.to_ascii_lowercase();
            SIGV4_OWNED_HEADERS
                .iter()
                .any(|owned| name.eq_ignore_ascii_case(owned))
                // AWS requires every outbound x-amz-* header to appear in the
                // canonical and signed-header sets. This signer deliberately
                // owns that namespace so custom headers cannot be sent unsigned.
                || normalized.starts_with("x-amz-")
        })
        .map(|name| name.to_ascii_lowercase())
        .collect::<Vec<_>>();
    collisions.sort_unstable();
    collisions.dedup();

    if collisions.is_empty() {
        return Ok(());
    }

    Err(Error::provider(
        "amazon-bedrock",
        format!(
            "{source} must not set SigV4-owned Bedrock header(s): {}. Authorization, Host, Content-Type, and the entire x-amz-* namespace are generated from the exact signed request; use explicit bearer authentication if a proxy requires custom Authorization semantics",
            collisions.join(", ")
        ),
    ))
}

#[derive(Debug, Clone)]
enum BedrockAuth {
    Sigv4 {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
    Bearer {
        token: String,
    },
}

#[derive(Debug, Clone)]
struct BedrockAuthContext {
    auth: BedrockAuth,
    region: String,
}

#[derive(Debug, Clone)]
struct Sigv4Headers {
    authorization: String,
    amz_date: String,
    payload_hash: String,
    security_token: Option<String>,
}

/// Amazon Bedrock provider.
pub struct BedrockProvider {
    client: Client,
    model: String,
    provider_name: String,
    base_url_override: Option<String>,
    compat: Option<CompatConfig>,
    auth_path_override: Option<PathBuf>,
    auth_command_working_directory: Option<PathBuf>,
}

impl BedrockProvider {
    /// Create a Bedrock provider for the given model ID.
    pub fn new(model: impl Into<String>) -> Self {
        let raw_model = model.into();
        let normalized_model = normalize_model_id(&raw_model)
            .ok()
            .unwrap_or_else(|| raw_model.trim().to_string());
        Self {
            client: Client::new(),
            model: normalized_model,
            provider_name: "amazon-bedrock".to_string(),
            base_url_override: None,
            compat: None,
            auth_path_override: None,
            auth_command_working_directory: None,
        }
    }

    /// Set provider name for event attribution.
    #[must_use]
    pub fn with_provider_name(mut self, provider_name: impl Into<String>) -> Self {
        self.provider_name = provider_name.into();
        self
    }

    /// Override Bedrock base URL (useful for tests/proxies).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl AsRef<str>) -> Self {
        let trimmed = base_url.as_ref().trim();
        if !trimmed.is_empty() {
            self.base_url_override = Some(trimmed.to_string());
        }
        self
    }

    /// Attach provider compatibility overrides.
    #[must_use]
    pub fn with_compat(mut self, compat: Option<CompatConfig>) -> Self {
        self.compat = compat;
        self
    }

    /// Inject a custom HTTP client.
    #[must_use]
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Bind request-time credential reloads to this runtime's auth source.
    ///
    /// Only the file path and command workspace are retained. Each request
    /// reloads the file so login, logout, and credential rotation stay visible
    /// even when the provider is shared with auxiliary completion clients.
    /// Relative auth paths are anchored when this binding is created.
    ///
    /// # Errors
    /// Returns an error if a relative auth path cannot be anchored.
    pub fn with_auth_storage(mut self, auth: &AuthStorage) -> Result<Self> {
        self.auth_path_override = Some(if auth.path().is_absolute() {
            auth.path().to_path_buf()
        } else {
            std::env::current_dir()?.join(auth.path())
        });
        self.auth_command_working_directory =
            auth.command_working_directory().map(Path::to_path_buf);
        Ok(self)
    }

    #[cfg(test)]
    #[must_use]
    fn with_auth_path(mut self, path: impl AsRef<Path>) -> Self {
        self.auth_path_override = Some(path.as_ref().to_path_buf());
        self
    }

    fn auth_path(&self) -> PathBuf {
        self.auth_path_override
            .clone()
            .unwrap_or_else(Config::auth_path)
    }

    fn load_auth_storage(&self) -> std::result::Result<AuthStorage, AuthStorageLoadFailure> {
        let mut auth = AuthStorage::load_with_lock_timeout_classified(
            self.auth_path(),
            AUTH_RESOLUTION_LOCK_TIMEOUT,
        )?;
        if let Some(cwd) = &self.auth_command_working_directory {
            auth.set_command_working_directory(cwd)
                .map_err(AuthStorageLoadFailure::Other)?;
        }
        Ok(auth)
    }

    async fn resolve_auth_context(&self, options: &StreamOptions) -> Result<BedrockAuthContext> {
        // `StreamOptions.api_key` is the explicit direct-bearer override contract for Bedrock.
        // Check it before auto-detected AWS credentials so CLI/SDK callers can deterministically
        // override a profile or ambient SigV4 chain. Generic auth resolution deliberately never
        // puts AWS_REGION, AWS_PROFILE, session tokens, or access-key IDs into this lane.
        if let Some(token) = options
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            return Ok(BedrockAuthContext {
                auth: BedrockAuth::Bearer {
                    token: token.to_string(),
                },
                region: bedrock_region_with_env(|name| std::env::var(name).ok()),
            });
        }

        let resolved = match self.load_auth_storage() {
            Ok(auth_storage) => resolve_aws_credentials_async(&auth_storage, &self.client).await?,
            Err(failure @ AuthStorageLoadFailure::LockTimeout(_)) => {
                return Err(failure.into_error());
            }
            Err(AuthStorageLoadFailure::Other(load_error)) => {
                let ambient = resolve_ambient_aws_credentials_async(&self.client).await?;
                if ambient.is_some() {
                    tracing::warn!(
                        error = %load_error,
                        "stored Bedrock credentials are unavailable; using ambient AWS credentials"
                    );
                    ambient
                } else {
                    return Err(load_error);
                }
            }
        };
        if let Some(resolved) = resolved {
            return Ok(match resolved {
                AwsResolvedCredentials::Sigv4 {
                    access_key_id,
                    secret_access_key,
                    session_token,
                    region,
                } => BedrockAuthContext {
                    auth: BedrockAuth::Sigv4 {
                        access_key_id,
                        secret_access_key,
                        session_token,
                    },
                    region,
                },
                AwsResolvedCredentials::Bearer { token, region } => BedrockAuthContext {
                    auth: BedrockAuth::Bearer { token },
                    region,
                },
            });
        }

        Err(Error::auth(
            "Amazon Bedrock requires AWS credentials. Set AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, AWS_BEARER_TOKEN_BEDROCK, AWS_PROFILE (static or SSO), or store amazon-bedrock credentials in auth.json. \
             For SSO profiles, run: aws sso login --profile <name>. An explicit direct bearer token may be passed via --api-key or StreamOptions.api_key.",
        ))
    }

    fn converse_url(&self, region: &str) -> Result<Url> {
        let base = self
            .base_url_override
            .clone()
            .unwrap_or_else(|| format!("https://bedrock-runtime.{region}.amazonaws.com"));
        let mut url = Url::parse(&base)
            .map_err(|err| Error::provider("amazon-bedrock", format!("Invalid base URL: {err}")))?;

        if self.model.trim().is_empty() {
            return Err(Error::provider(
                "amazon-bedrock",
                "Bedrock model id cannot be empty",
            ));
        }

        // An explicit Converse endpoint is the opt-out for models or gateways
        // without streaming support. Never retry a failed stream as a second
        // non-streaming inference request.
        if url.path().ends_with("/converse") || url.path().ends_with("/converse-stream") {
            return Ok(url);
        }

        {
            let mut segments = url.path_segments_mut().map_err(|()| {
                Error::provider(
                    "amazon-bedrock",
                    "Bedrock base URL does not support path segments",
                )
            })?;
            segments.pop_if_empty();
            segments.push("model");
            segments.push(&self.model);
            segments.push("converse-stream");
        }
        Ok(url)
    }

    /// Prepare the actual destination-aware request before hooks or signing.
    /// Bedrock reasoning is model-owned replay data, including opaque encrypted
    /// blocks. A shared Converse API name does not make another model or
    /// configured provider able to validate it. Keep ordinary history and tool
    /// pairs, but omit foreign reasoning without changing the stored transcript.
    fn prepare_request(&self, context: &Context<'_>, options: &StreamOptions) -> Result<Value> {
        let mut messages = Cow::Borrowed(context.messages.as_ref());
        for (index, message) in context.messages.iter().enumerate() {
            let Message::Assistant(assistant) = message else {
                continue;
            };
            let same_origin = assistant.api == self.api()
                && assistant.provider == self.name()
                && assistant.model == self.model_id();
            if same_origin
                || !assistant.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Thinking(_) | ContentBlock::RedactedThinking(_)
                    )
                })
            {
                continue;
            }
            if let Message::Assistant(projected) = &mut messages.to_mut()[index] {
                Arc::make_mut(projected).content.retain(|block| {
                    !matches!(
                        block,
                        ContentBlock::Thinking(_) | ContentBlock::RedactedThinking(_)
                    )
                });
            }
        }
        let projected = Context {
            system_prompt: context.system_prompt.as_deref().map(Cow::Borrowed),
            messages,
            tools: Cow::Borrowed(context.tools.as_ref()),
        };
        request_options::prepare(&self.model, self.compat.as_ref(), &projected, options)
    }

    pub fn build_request(context: &Context<'_>, options: &StreamOptions) -> BedrockConverseRequest {
        let mut system = Vec::new();
        if let Some(system_prompt) = context
            .system_prompt
            .as_deref()
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
        {
            system.push(BedrockSystemContent {
                text: system_prompt.to_string(),
            });
        }

        let mut messages: Vec<BedrockMessage> = Vec::new();
        for message in context.messages.iter() {
            if let Some(converted) = convert_message(message) {
                // Converse requires alternating roles, with every toolResult
                // for one assistant turn in the next user message. Parallel
                // tool results, steering and custom context arrive as
                // separate messages, so merge consecutive same-role ones.
                match messages.last_mut() {
                    Some(last) if last.role == converted.role => {
                        last.content.extend(converted.content);
                    }
                    _ => messages.push(converted),
                }
            }
        }

        if messages.is_empty() {
            messages.push(BedrockMessage {
                role: "user",
                content: vec![BedrockContent::Text {
                    text: "Hello".to_string(),
                }],
            });
        }

        let inference_config = if options.max_tokens.is_some() || options.temperature.is_some() {
            Some(BedrockInferenceConfig {
                max_tokens: options.max_tokens,
                temperature: options.temperature,
            })
        } else {
            None
        };

        let tool_config = if context.tools.is_empty() {
            None
        } else {
            Some(BedrockToolConfig {
                tools: context.tools.iter().map(convert_tool).collect(),
            })
        };

        BedrockConverseRequest {
            system,
            messages,
            inference_config,
            tool_config,
        }
    }

    fn response_to_message(&self, response: BedrockConverseResponse) -> AssistantMessage {
        let usage = response
            .usage
            .as_ref()
            .map_or_else(Usage::default, convert_usage);

        // JSON gateways return the complete response at once. Validate the
        // entire batch before mapping any block to an executable local call.
        let has_tools = response.output.as_ref().is_some_and(|output| {
            output
                .message
                .content
                .iter()
                .any(|block| matches!(block, BedrockResponseContent::ToolUse { .. }))
        });
        let (mut stop_reason, failure) = response_stop_outcome(
            response.stop_reason.as_deref().unwrap_or_default(),
            has_tools,
        );
        let failure = failure.or_else(|| validate_json_output(response.output.as_ref()));
        if failure.is_some() {
            stop_reason = StopReason::Error;
        }
        let mut content = Vec::new();

        if let Some(output) = response.output {
            for block in output.message.content {
                match block {
                    BedrockResponseContent::Text { text } => {
                        if !text.is_empty() {
                            content.push(ContentBlock::Text(TextContent {
                                text,
                                text_signature: None,
                            }));
                        }
                    }
                    BedrockResponseContent::ToolUse { tool_use } => {
                        if stop_reason == StopReason::ToolUse {
                            content.push(ContentBlock::ToolCall(ToolCall {
                                id: tool_use.tool_use_id,
                                name: tool_use.name,
                                arguments: tool_use.input,
                                thought_signature: None,
                            }));
                        }
                    }
                    BedrockResponseContent::ReasoningContent { reasoning_content } => {
                        content.push(match reasoning_content {
                            BedrockReasoningContent::Text { reasoning_text } => {
                                ContentBlock::Thinking(ThinkingContent {
                                    thinking: reasoning_text.text,
                                    thinking_signature: reasoning_text.signature,
                                })
                            }
                            BedrockReasoningContent::Redacted { redacted_content } => {
                                ContentBlock::RedactedThinking(RedactedThinkingContent {
                                    data: redacted_content,
                                })
                            }
                        });
                    }
                    BedrockResponseContent::Invalid(_) => {}
                }
            }
        }

        AssistantMessage {
            content,
            api: "bedrock-converse-stream".to_string(),
            provider: self.provider_name.clone(),
            model: self.model.clone(),
            usage,
            stop_reason,
            stop_details: None,
            error_message: failure.map(str::to_string),
            timestamp: Utc::now().timestamp_millis(),
        }
    }

    fn message_events(message: &AssistantMessage) -> Vec<Result<StreamEvent>> {
        let mut events = Vec::new();
        events.push(Ok(StreamEvent::Start {
            partial: message.clone(),
        }));
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
                ContentBlock::Thinking(thinking) => {
                    events.push(Ok(StreamEvent::ThinkingStart { content_index }));
                    events.push(Ok(StreamEvent::ThinkingDelta {
                        content_index,
                        delta: thinking.thinking.clone(),
                    }));
                    events.push(Ok(StreamEvent::ThinkingEnd {
                        content_index,
                        content: thinking.thinking.clone(),
                    }));
                }
                ContentBlock::ToolCall(tool_call) if message.stop_reason == StopReason::ToolUse => {
                    let delta = serde_json::to_string(&tool_call.arguments)
                        .unwrap_or_else(|_| "{}".to_string());
                    events.push(Ok(StreamEvent::ToolCallStart {
                        content_index,
                        id: tool_call.id.clone(),
                        name: tool_call.name.clone(),
                    }));
                    events.push(Ok(StreamEvent::ToolCallDelta {
                        content_index,
                        delta,
                    }));
                    events.push(Ok(StreamEvent::ToolCallEnd {
                        content_index,
                        tool_call: tool_call.clone(),
                    }));
                }
                _ => {}
            }
        }

        events.push(Ok(if message.stop_reason == StopReason::Error {
            StreamEvent::Error {
                reason: StopReason::Error,
                error: message.clone(),
            }
        } else {
            StreamEvent::Done {
                reason: message.stop_reason,
                message: message.clone(),
            }
        }));
        events
    }

    fn validate_auth_header_ownership(
        &self,
        auth_context: &BedrockAuthContext,
        options: &StreamOptions,
    ) -> Result<()> {
        if matches!(&auth_context.auth, BedrockAuth::Sigv4 { .. }) {
            if let Some(custom_headers) = self
                .compat
                .as_ref()
                .and_then(|compat| compat.custom_headers.as_ref())
            {
                validate_sigv4_header_ownership(custom_headers, "compat.customHeaders")?;
            }
            validate_sigv4_header_ownership(&options.headers, "StreamOptions.headers")?;
        }
        Ok(())
    }

    fn authenticated_request<'a>(
        &'a self,
        url: &Url,
        body: &[u8],
        auth_context: BedrockAuthContext,
        options: &StreamOptions,
    ) -> Result<(RequestBuilder<'a>, Vec<String>)> {
        let accept = if url.path().ends_with("/converse-stream") {
            streaming::CONTENT_TYPE
        } else {
            "application/json"
        };
        let mut request = self
            .client
            .post(url.as_str())
            .header("Content-Type", "application/json")
            .header("Accept", accept);
        let mut response_secrets = url
            .query_pairs()
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if !url.username().is_empty() {
            response_secrets.push(url.username().to_string());
        }
        if let Some(password) = url.password().filter(|password| !password.is_empty()) {
            response_secrets.push(password.to_string());
        }

        match auth_context.auth {
            BedrockAuth::Bearer { token } => {
                response_secrets.push(token.clone());
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            BedrockAuth::Sigv4 {
                access_key_id,
                secret_access_key,
                session_token,
            } => {
                let signing_headers = build_sigv4_headers(
                    url,
                    body,
                    &access_key_id,
                    &secret_access_key,
                    session_token.as_deref(),
                    &auth_context.region,
                    Utc::now(),
                )?;
                response_secrets.push(access_key_id);
                response_secrets.push(secret_access_key);
                response_secrets.push(signing_headers.authorization.clone());
                if let Some((_, signature)) =
                    signing_headers.authorization.rsplit_once("Signature=")
                    && !signature.trim().is_empty()
                {
                    response_secrets.push(signature.trim().to_string());
                }
                if let Some(token) = session_token {
                    response_secrets.push(token);
                }
                request = request
                    .header("Authorization", signing_headers.authorization)
                    .header("x-amz-date", signing_headers.amz_date)
                    .header("x-amz-content-sha256", signing_headers.payload_hash);
                if let Some(token) = signing_headers.security_token {
                    response_secrets.push(token.clone());
                    request = request.header("x-amz-security-token", token);
                }
            }
        }

        if let Some(compat) = &self.compat
            && let Some(custom_headers) = &compat.custom_headers
        {
            request = apply_bedrock_headers(request, custom_headers, &mut response_secrets)?;
        }
        request = apply_bedrock_headers(request, &options.headers, &mut response_secrets)?;
        Ok((request, response_secrets))
    }
}

#[async_trait]
impl Provider for BedrockProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn api(&self) -> &'static str {
        "bedrock-converse-stream"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        // Apply model controls before hooks, auth discovery, and body signing.
        let request_body = self.prepare_request(context, options)?;

        let auth_context = self.resolve_auth_context(options).await?;
        // Reject caller-owned signing headers before endpoint parsing or an inference request.
        // Credential discovery necessarily happens first so bearer mode can retain its custom
        // Authorization semantics, but a malformed endpoint override must not mask a SigV4
        // ownership violation.
        self.validate_auth_header_ownership(&auth_context, options)?;
        let url = self.converse_url(&auth_context.region)?;

        // The rewrite hook must run before serialization: SigV4 signs the
        // final body bytes, so the chosen body (rewritten or original) is
        // serialized once, below, and signed as-is. Model lives in the URL,
        // so only the `messages` array is structurally required.
        let rewritten_body = super::offer_before_provider_request(
            options,
            self.name(),
            self.api(),
            self.model_id(),
            url.as_str(),
            &request_body,
            |value| super::validate_streamed_json_rewrite(value, &[], &["messages"], &[]),
        )
        .await;
        let body = rewritten_body
            .as_ref()
            .map_or_else(|| serde_json::to_vec(&request_body), serde_json::to_vec)
            .map_err(|err| {
                Error::provider(
                    "amazon-bedrock",
                    format!("Failed to serialize request body: {err}"),
                )
            })?;
        let (request, response_secrets) =
            self.authenticated_request(&url, &body, auth_context, options)?;

        let response = request.body(body).send().await?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let response_text = response
                .text_limited(MAX_BEDROCK_RESPONSE_BYTES)
                .await
                .unwrap_or_else(|err| format!("<failed to read body: {err}>"));
            let response_snippet = bedrock_error_snippet(&response_text, &response_secrets);
            return Err(Error::provider(
                self.name(),
                format!("Bedrock Converse API error (HTTP {status}): {response_snippet}"),
            ));
        }

        let is_event_stream = response.headers().iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("content-type")
                && value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .eq_ignore_ascii_case(streaming::CONTENT_TYPE)
        });
        if is_event_stream {
            // Return before reading the response body. The owned decoder emits
            // deltas as frames arrive and releases the socket on drop or error.
            return Ok(streaming::from_bytes(
                response.bytes_stream(),
                self.model.clone(),
                self.provider_name.clone(),
                response_secrets,
                options.usage_observer.clone(),
            ));
        }

        // Explicit /converse endpoints and some gateways respond with JSON.
        // This consumes the same response, never a second inference request.
        let response_text = response
            .text_limited(MAX_BEDROCK_RESPONSE_BYTES)
            .await
            .map_err(|err| {
                let detail = bedrock_error_snippet(&err.to_string(), &response_secrets);
                Error::provider(
                    self.name(),
                    format!("Failed to read Bedrock response: {detail}"),
                )
            })?;
        let parse_error = |err| {
            let response_snippet = bedrock_error_snippet(&response_text, &response_secrets);
            Error::provider(
                self.name(),
                format!("Failed to parse Bedrock response: {err}\nData: {response_snippet}"),
            )
        };
        // A valid billing envelope survives an invalid output shape as well
        // as a caller dropping the returned stream before its first poll.
        // Keep typed decoding: a Value round-trip would silently discard
        // duplicate billing fields before the response could reject them.
        if let Some(observer) = &options.usage_observer {
            let envelope: BedrockUsageEnvelope =
                serde_json::from_str(&response_text).map_err(parse_error)?;
            if let Some(usage) = &envelope.usage {
                observer.observe(&convert_usage(usage))?;
            }
        }
        let parsed: BedrockConverseResponse =
            serde_json::from_str(&response_text).map_err(parse_error)?;
        if parsed.output.is_none() || parsed.stop_reason.is_none() {
            return Err(Error::provider(
                self.name(),
                "Bedrock JSON response is missing output or stopReason",
            ));
        }

        let message = self.response_to_message(parsed);
        Ok(Box::pin(stream::iter(Self::message_events(&message))))
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BedrockConverseRequest {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<BedrockSystemContent>,
    messages: Vec<BedrockMessage>,
    #[serde(rename = "inferenceConfig", skip_serializing_if = "Option::is_none")]
    inference_config: Option<BedrockInferenceConfig>,
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    tool_config: Option<BedrockToolConfig>,
}

#[derive(Debug, Serialize)]
struct BedrockSystemContent {
    text: String,
}

#[derive(Debug, Serialize)]
struct BedrockMessage {
    role: &'static str,
    content: Vec<BedrockContent>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum BedrockContent {
    Text {
        text: String,
    },
    Image {
        image: BedrockImageBlock,
    },
    ToolUse {
        #[serde(rename = "toolUse")]
        tool_use: BedrockToolUse,
    },
    ToolResult {
        #[serde(rename = "toolResult")]
        tool_result: BedrockToolResult,
    },
    ReasoningContent {
        #[serde(rename = "reasoningContent")]
        reasoning_content: BedrockReasoningContent,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum BedrockReasoningContent {
    Text {
        #[serde(rename = "reasoningText")]
        reasoning_text: BedrockReasoningText,
    },
    Redacted {
        #[serde(rename = "redactedContent")]
        redacted_content: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct BedrockReasoningText {
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

#[derive(Debug, Serialize)]
struct BedrockImageBlock {
    format: String,
    source: BedrockImageSource,
}

#[derive(Debug, Serialize)]
struct BedrockImageSource {
    bytes: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockToolUse {
    tool_use_id: String,
    name: String,
    input: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockToolResult {
    tool_use_id: String,
    content: Vec<BedrockToolResultContent>,
    status: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum BedrockToolResultContent {
    Text { text: String },
    Image { image: BedrockImageBlock },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockInferenceConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Debug, Serialize)]
struct BedrockToolConfig {
    tools: Vec<BedrockToolDef>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockToolDef {
    tool_spec: BedrockToolSpec,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockToolSpec {
    name: String,
    description: String,
    input_schema: BedrockInputSchema,
}

#[derive(Debug, Serialize)]
struct BedrockInputSchema {
    json: Value,
}

fn convert_message(message: &Message) -> Option<BedrockMessage> {
    match message {
        Message::User(user_message) => convert_user_message(user_message),
        Message::Assistant(assistant_message) => convert_assistant_message(assistant_message),
        Message::ToolResult(tool_result_message) => {
            Some(convert_tool_result_message(tool_result_message))
        }
        // Custom context (a handoff import's brief, extension messages) is
        // user text, as on every other provider.
        Message::Custom(custom) => (!custom.content.trim().is_empty()).then(|| BedrockMessage {
            role: "user",
            content: vec![BedrockContent::Text {
                text: custom.content.clone(),
            }],
        }),
    }
}

fn convert_user_message(message: &crate::model::UserMessage) -> Option<BedrockMessage> {
    let mut content = Vec::new();
    match &message.content {
        UserContent::Text(text) => {
            if !text.trim().is_empty() {
                content.push(BedrockContent::Text { text: text.clone() });
            }
        }
        UserContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    ContentBlock::Text(text) if !text.text.trim().is_empty() => {
                        content.push(BedrockContent::Text {
                            text: text.text.clone(),
                        });
                    }
                    ContentBlock::Image(img) => {
                        let format = img
                            .mime_type
                            .rsplit('/')
                            .next()
                            .unwrap_or("png")
                            .to_string();
                        content.push(BedrockContent::Image {
                            image: BedrockImageBlock {
                                format,
                                source: BedrockImageSource {
                                    bytes: img.data.clone(),
                                },
                            },
                        });
                    }
                    // Converse has a `video` block for Amazon's own models
                    // only; the Claude models pi routes here take text, so
                    // degrade to the placeholder (gh #212).
                    ContentBlock::Media(media) => {
                        content.push(BedrockContent::Text {
                            text: media.placeholder(),
                        });
                    }
                    _ => {}
                }
            }
        }
    }

    if content.is_empty() {
        None
    } else {
        Some(BedrockMessage {
            role: "user",
            content,
        })
    }
}

fn convert_assistant_message(message: &AssistantMessage) -> Option<BedrockMessage> {
    let mut content = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text(text) if !text.text.trim().is_empty() => {
                content.push(BedrockContent::Text {
                    text: text.text.clone(),
                });
            }
            ContentBlock::Thinking(thinking) if message.api == "bedrock-converse-stream" => {
                content.push(BedrockContent::ReasoningContent {
                    reasoning_content: BedrockReasoningContent::Text {
                        reasoning_text: BedrockReasoningText {
                            text: thinking.thinking.clone(),
                            signature: thinking.thinking_signature.clone(),
                        },
                    },
                });
            }
            ContentBlock::RedactedThinking(redacted)
                if message.api == "bedrock-converse-stream" =>
            {
                content.push(BedrockContent::ReasoningContent {
                    reasoning_content: BedrockReasoningContent::Redacted {
                        redacted_content: redacted.data.clone(),
                    },
                });
            }
            ContentBlock::ToolCall(tool_call) => {
                content.push(BedrockContent::ToolUse {
                    tool_use: BedrockToolUse {
                        tool_use_id: tool_call.id.clone(),
                        name: tool_call.name.clone(),
                        input: tool_call.arguments.clone(),
                    },
                });
            }
            _ => {}
        }
    }

    if content.is_empty() {
        None
    } else {
        Some(BedrockMessage {
            role: "assistant",
            content,
        })
    }
}

fn convert_tool_result_message(message: &ToolResultMessage) -> BedrockMessage {
    let mut contents = Vec::new();

    for block in &message.content {
        match block {
            ContentBlock::Text(text) if !text.text.is_empty() => {
                contents.push(BedrockToolResultContent::Text {
                    text: text.text.clone(),
                });
            }
            ContentBlock::Image(img) => {
                let format = img
                    .mime_type
                    .rsplit('/')
                    .next()
                    .unwrap_or("png")
                    .to_string();
                contents.push(BedrockToolResultContent::Image {
                    image: BedrockImageBlock {
                        format,
                        source: BedrockImageSource {
                            bytes: img.data.clone(),
                        },
                    },
                });
            }
            ContentBlock::Media(media) => {
                contents.push(BedrockToolResultContent::Text {
                    text: media.placeholder(),
                });
            }
            _ => {}
        }
    }

    if contents.is_empty() {
        contents.push(BedrockToolResultContent::Text {
            text: "{}".to_string(),
        });
    }

    BedrockMessage {
        role: "user",
        content: vec![BedrockContent::ToolResult {
            tool_result: BedrockToolResult {
                tool_use_id: message.tool_call_id.clone(),
                content: contents,
                status: if message.is_error {
                    "error".to_string()
                } else {
                    "success".to_string()
                },
            },
        }],
    }
}

fn convert_tool(tool: &ToolDef) -> BedrockToolDef {
    BedrockToolDef {
        tool_spec: BedrockToolSpec {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: BedrockInputSchema {
                json: tool.parameters.clone(),
            },
        },
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BedrockUsageEnvelope {
    #[serde(default)]
    usage: Option<BedrockUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BedrockConverseResponse {
    #[serde(default)]
    output: Option<BedrockResponseOutput>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<BedrockUsage>,
}

#[derive(Debug, Deserialize)]
struct BedrockResponseOutput {
    message: BedrockResponseMessage,
}

#[derive(Debug, Deserialize)]
struct BedrockResponseMessage {
    role: Option<String>,
    #[serde(default)]
    content: Vec<BedrockResponseContent>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum BedrockResponseContent {
    Text {
        text: String,
    },
    ToolUse {
        #[serde(rename = "toolUse")]
        tool_use: BedrockResponseToolUse,
    },
    ReasoningContent {
        #[serde(rename = "reasoningContent")]
        reasoning_content: BedrockReasoningContent,
    },
    // Reject unsupported/ambiguous blocks during whole-message validation,
    // after retaining the valid envelope's billing and other readable text.
    Invalid(Value),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BedrockResponseToolUse {
    tool_use_id: String,
    name: String,
    #[serde(default)]
    input: Value,
    // Keep presence, including null, so a server-managed or unknown `type`
    // cannot disappear during deserialization and become a local tool call.
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

fn validate_json_output(output: Option<&BedrockResponseOutput>) -> Option<&'static str> {
    let Some(output) = output else {
        return Some("Bedrock JSON response is missing output; local tool calls withheld");
    };
    if output.message.role.as_deref() != Some("assistant") {
        return Some("Bedrock JSON response has no assistant role; local tool calls withheld");
    }
    if output.message.content.len() > MAX_BEDROCK_CONTENT_BLOCKS {
        return Some(
            "Bedrock JSON response exceeds the content-block limit; local tool calls withheld",
        );
    }
    let mut tool_ids = HashSet::new();
    for block in &output.message.content {
        if let BedrockResponseContent::Invalid(value) = block {
            return Some(if value.get("toolUse").is_some() {
                "Bedrock JSON response has malformed or ambiguous tool content; local tool calls withheld"
            } else {
                "Bedrock JSON response has unsupported or ambiguous content; local tool calls withheld"
            });
        }
        let BedrockResponseContent::ToolUse { tool_use } = block else {
            continue;
        };
        if tool_use.extra.contains_key("type") {
            return Some(
                "Bedrock server-managed or unknown tool type is not a local tool request; local tool calls withheld",
            );
        }
        if !valid_tool_identity(&tool_use.tool_use_id, true)
            || !valid_tool_identity(&tool_use.name, false)
        {
            return Some(
                "Bedrock JSON response has an invalid tool-call identity; local tool calls withheld",
            );
        }
        if !tool_ids.insert(tool_use.tool_use_id.as_str()) {
            return Some(
                "Bedrock JSON response has duplicate tool-call ids; local tool calls withheld",
            );
        }
        if !tool_use.input.is_object() {
            return Some(
                "Bedrock JSON tool input must be a complete JSON object; local tool calls withheld",
            );
        }
    }
    None
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_field_names)]
struct BedrockUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_write_input_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

fn convert_usage(usage: &BedrockUsage) -> Usage {
    let total = if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage
            .input_tokens
            .saturating_add(usage.output_tokens)
            .saturating_add(usage.cache_read_input_tokens)
            .saturating_add(usage.cache_write_input_tokens)
    };

    Usage {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_input_tokens,
        cache_write: usage.cache_write_input_tokens,
        total_tokens: total,
        ..Usage::default()
    }
}

/// Shared by JSON Converse replies and binary ConverseStream messageStop.
/// A complete tool input alone is not authorization to execute it: the agent
/// also admits tools on Stop/Length, so those outcomes must withhold calls.
fn response_stop_outcome(wire_reason: &str, has_tools: bool) -> (StopReason, Option<&'static str>) {
    let (reason, failure) = match wire_reason {
        "end_turn" | "stop_sequence" => (StopReason::Stop, None),
        "tool_use" if has_tools => (StopReason::ToolUse, None),
        "tool_use" => (
            StopReason::Error,
            Some("Bedrock tool_use stop reason has no local tool calls"),
        ),
        "max_tokens" | "model_context_window_exceeded" => (StopReason::Length, None),
        "guardrail_intervened" => (
            StopReason::Error,
            Some("Bedrock guardrail_intervened: response blocked; local tool calls withheld"),
        ),
        "content_filtered" => (
            StopReason::Error,
            Some("Bedrock content_filtered: response refused; local tool calls withheld"),
        ),
        "malformed_tool_use" => (
            StopReason::Error,
            Some("Bedrock malformed_tool_use: invalid tool request; local tool calls withheld"),
        ),
        "malformed_model_output" => (
            StopReason::Error,
            Some(
                "Bedrock malformed_model_output: invalid model response; local tool calls withheld",
            ),
        ),
        // Do not echo unknown wire strings, which may contain secrets or
        // terminal controls. These are provider failures, never normal stops.
        _ => (
            StopReason::Error,
            Some("Bedrock returned an unsupported stop reason; local tool calls withheld"),
        ),
    };
    let failure = failure.or_else(|| {
        if !has_tools || reason == StopReason::ToolUse {
            None
        } else if reason == StopReason::Length {
            Some("Bedrock response was truncated before tool_use authorization; local tool calls withheld")
        } else {
            Some("Bedrock ended without tool_use authorization; local tool calls withheld")
        }
    });
    (
        if failure.is_some() {
            StopReason::Error
        } else {
            reason
        },
        failure,
    )
}

// Bedrock's ToolUseBlock and ToolUseBlockStart share this identity grammar.
// IDs allow '.', ':' in addition to the tool-name alphabet; both max 64 bytes.
fn valid_tool_identity(value: &str, is_id: bool) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-')
                || (is_id && matches!(byte, b'.' | b':'))
        })
}

fn normalize_model_id(model_id: &str) -> Result<String> {
    let mut normalized = model_id.trim().trim_matches('/');
    if normalized.is_empty() {
        return Err(Error::provider(
            "amazon-bedrock",
            "Bedrock model id cannot be empty",
        ));
    }

    for prefix in ["amazon-bedrock/", "bedrock/", "model/"] {
        if let Some(stripped) = normalized.strip_prefix(prefix) {
            normalized = stripped;
            break;
        }
    }

    if let Some((_, stripped)) = normalized.split_once("/model/") {
        normalized = stripped;
    }

    for suffix in ["/converse-stream", "/converse"] {
        if let Some(stripped) = normalized.strip_suffix(suffix) {
            normalized = stripped;
            break;
        }
    }

    let final_id = normalized.trim_matches('/');
    if final_id.is_empty() {
        return Err(Error::provider(
            "amazon-bedrock",
            "Bedrock model id cannot be empty",
        ));
    }

    Ok(final_id.to_string())
}

fn build_sigv4_headers(
    url: &Url,
    payload: &[u8],
    access_key_id: &str,
    secret_access_key: &str,
    session_token: Option<&str>,
    region: &str,
    now: DateTime<Utc>,
) -> Result<Sigv4Headers> {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = now.format("%Y%m%d").to_string();
    let payload_hash = sha256_hex(payload);
    let host = canonical_host(url)?;
    let canonical_uri = canonical_uri(url);
    let canonical_query = canonical_query(url);

    let mut canonical_headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("host".to_string(), host),
        ("x-amz-content-sha256".to_string(), payload_hash.clone()),
        ("x-amz-date".to_string(), amz_date.clone()),
    ];
    if let Some(token) = session_token {
        canonical_headers.push(("x-amz-security-token".to_string(), token.to_string()));
    }
    canonical_headers.sort_by(|left, right| left.0.cmp(&right.0));

    let signed_headers = canonical_headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let mut canonical_headers_block = String::new();
    for (name, value) in &canonical_headers {
        let trimmed = value.trim();
        writeln!(&mut canonical_headers_block, "{name}:{trimmed}")
            .map_err(|err| Error::api(format!("Failed to build canonical headers: {err}")))?;
    }

    let canonical_request = format!(
        "POST\n{canonical_uri}\n{canonical_query}\n{canonical_headers_block}\n{signed_headers}\n{payload_hash}"
    );
    let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
    let credential_scope = format!("{date_stamp}/{region}/{BEDROCK_SERVICE}/aws4_request");
    let string_to_sign =
        format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{canonical_request_hash}");
    let signature = hex_encode(&signing_key(
        secret_access_key,
        &date_stamp,
        region,
        &string_to_sign,
    )?);

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key_id}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );

    Ok(Sigv4Headers {
        authorization,
        amz_date,
        payload_hash,
        security_token: session_token.map(ToString::to_string),
    })
}

fn canonical_host(url: &Url) -> Result<String> {
    let host = url.host_str().ok_or_else(|| {
        Error::provider("amazon-bedrock", "Bedrock endpoint URL is missing a host")
    })?;
    Ok(url
        .port()
        .map_or_else(|| host.to_string(), |port| format!("{host}:{port}")))
}

fn canonical_uri(url: &Url) -> String {
    let segments = url
        .path_segments()
        .map(|parts| parts.map(aws_percent_encode).collect::<Vec<_>>())
        .unwrap_or_default();

    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

fn canonical_query(url: &Url) -> String {
    let mut pairs = url
        .query_pairs()
        .map(|(key, value)| (aws_percent_encode(&key), aws_percent_encode(&value)))
        .collect::<Vec<_>>();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn aws_percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(nibble_to_hex(byte >> 4));
            encoded.push(nibble_to_hex(byte & 0x0f));
        }
    }
    encoded
}

fn nibble_to_hex(nibble: u8) -> char {
    match nibble {
        0..=9 => char::from(b'0' + nibble),
        10..=15 => char::from(b'A' + nibble - 10),
        _ => '0',
    }
}

fn signing_key(
    secret_access_key: &str,
    date_stamp: &str,
    region: &str,
    string_to_sign: &str,
) -> Result<Vec<u8>> {
    let key_date = hmac_sha256(
        format!("AWS4{secret_access_key}").as_bytes(),
        date_stamp.as_bytes(),
    )?;
    let key_region = hmac_sha256(&key_date, region.as_bytes())?;
    let key_service = hmac_sha256(&key_region, BEDROCK_SERVICE.as_bytes())?;
    let key_signing = hmac_sha256(&key_service, b"aws4_request")?;
    hmac_sha256(&key_signing, string_to_sign.as_bytes())
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|err| Error::api(format!("Failed to initialize HMAC: {err}")))?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthCredential;
    use chrono::TimeZone as _;
    use futures::StreamExt as _;
    use serde_json::json;
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
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
    /// client had merely not been scheduled in time. Two seconds is nothing
    /// under a parallel lib suite; the same shape took seven tests out of the
    /// Gemini and Vertex fixtures before it was fixed there (bd-eg6ng).
    fn read_with_deadline(
        socket: &mut TcpStream,
        chunk: &mut [u8],
        deadline: Instant,
        what: &str,
    ) -> usize {
        loop {
            match socket.read(chunk) {
                Ok(count) => return count,
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
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

    #[derive(Debug)]
    struct CapturedBedrockRequest {
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    fn spawn_bedrock_test_server() -> (String, mpsc::Receiver<CapturedBedrockRequest>) {
        spawn_bedrock_json_server(vec![json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{"text": "ok"}]
                }
            },
            "stopReason": "end_turn",
            "usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}
        })])
    }

    fn spawn_bedrock_json_server(
        responses: Vec<Value>,
    ) -> (String, mpsc::Receiver<CapturedBedrockRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind Bedrock test server");
        let address = listener.local_addr().expect("Bedrock test server address");
        let (sender, receiver) = mpsc::channel();

        std::thread::spawn(move || {
            for response_body in responses {
                let (socket, _) = listener.accept().expect("accept Bedrock request");
                serve_bedrock_json_reply(socket, &sender, &response_body);
            }
        });

        (format!("http://{address}"), receiver)
    }

    fn serve_bedrock_json_reply(
        mut socket: TcpStream,
        sender: &mpsc::Sender<CapturedBedrockRequest>,
        response_body: &Value,
    ) {
        socket
            .set_read_timeout(Some(FIXTURE_POLL))
            .expect("set Bedrock request read timeout");
        socket
            .set_write_timeout(Some(FIXTURE_BUDGET))
            .expect("set Bedrock response write timeout");
        let deadline = Instant::now() + FIXTURE_BUDGET;

        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            let read =
                read_with_deadline(&mut socket, &mut chunk, deadline, "read Bedrock request");
            assert!(read > 0, "Bedrock request ended before its headers");
            request.extend_from_slice(&chunk[..read]);
            if let Some(position) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break position;
            }
        };

        let header_text = String::from_utf8_lossy(&request[..header_end]);
        let headers = header_text
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect::<HashMap<_, _>>();
        let content_length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .expect("Bedrock request content-length");
        let body_start = header_end + 4;
        while request.len().saturating_sub(body_start) < content_length {
            let read = read_with_deadline(
                &mut socket,
                &mut chunk,
                deadline,
                "read Bedrock request body",
            );
            assert!(read > 0, "Bedrock request ended before its body");
            request.extend_from_slice(&chunk[..read]);
        }
        sender
            .send(CapturedBedrockRequest {
                headers,
                body: request[body_start..body_start + content_length].to_vec(),
            })
            .expect("capture Bedrock request");

        let response_body =
            serde_json::to_string(response_body).expect("serialize Bedrock response");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        socket
            .write_all(response.as_bytes())
            .expect("write Bedrock response");
    }

    fn write_test_sigv4_auth(path: &Path) {
        let mut stored = AuthStorage::load(path.to_path_buf()).expect("load Bedrock test auth");
        stored.set(
            "amazon-bedrock",
            AuthCredential::AwsCredentials {
                access_key_id: "AKIDEXAMPLE".to_string(),
                // ubs:ignore test fixture credential, not a live secret.
                secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
                session_token: Some("session-token".to_string()),
                region: Some("us-west-2".to_string()),
            },
        );
        stored.save().expect("save Bedrock test auth");
    }

    fn test_context_with_tools() -> Context<'static> {
        Context {
            system_prompt: Some("You are concise.".to_string().into()),
            messages: vec![
                Message::User(crate::model::UserMessage {
                    content: UserContent::Text("Ping".to_string()),
                    timestamp: 0,
                }),
                Message::assistant(AssistantMessage {
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "tool_1".to_string(),
                        name: "search".to_string(),
                        arguments: json!({ "q": "rust" }),
                        thought_signature: None,
                    })],
                    api: "bedrock-converse-stream".to_string(),
                    provider: "amazon-bedrock".to_string(),
                    model: "m".to_string(),
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    stop_details: None,
                    error_message: None,
                    timestamp: 0,
                }),
                Message::tool_result(ToolResultMessage {
                    tool_call_id: "tool_1".to_string(),
                    tool_name: "search".to_string(),
                    content: vec![ContentBlock::Text(TextContent {
                        text: "result".to_string(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: false,
                    timestamp: 0,
                }),
            ]
            .into(),
            tools: vec![ToolDef {
                name: "search".to_string(),
                description: "Search docs".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {"q": {"type": "string"}},
                    "required": ["q"]
                }),
            }]
            .into(),
        }
    }

    #[test]
    fn error_snippet_redacts_credentials_and_bounds_untrusted_body() {
        let secret = "bedrock-session-token".to_string();
        let response = format!(
            "upstream echoed x-amz-security-token={secret} {}",
            "x".repeat(MAX_BEDROCK_ERROR_SNIPPET_BYTES)
        );
        let snippet = bedrock_error_snippet(&response, std::slice::from_ref(&secret));
        assert!(snippet.contains("[REDACTED]"), "{snippet}");
        assert!(!snippet.contains(&secret), "credential leaked: {snippet}");
        assert!(snippet.ends_with("...[truncated]"), "{snippet}");
        assert!(
            snippet.len() <= MAX_BEDROCK_ERROR_SNIPPET_BYTES,
            "untrusted error snippet exceeded its hard bound"
        );

        let expanding = bedrock_error_snippet(
            &"x".repeat(MAX_BEDROCK_ERROR_SNIPPET_BYTES),
            &["x".to_string()],
        );
        assert!(
            expanding.len() <= MAX_BEDROCK_ERROR_SNIPPET_BYTES,
            "redaction expansion exceeded the final output bound"
        );

        let boundary_secret = "LEAK_BOUNDARY_CREDENTIAL".to_string();
        let boundary_body = format!(
            "{}{}",
            "x".repeat(MAX_BEDROCK_ERROR_SNIPPET_BYTES - 8),
            boundary_secret
        );
        let boundary =
            bedrock_error_snippet(&boundary_body, std::slice::from_ref(&boundary_secret));
        assert!(
            !boundary.contains("LEAK"),
            "credential prefix leaked: {boundary}"
        );
        assert!(boundary.len() <= MAX_BEDROCK_ERROR_SNIPPET_BYTES);

        let escaped_secret = "bedrock-\"quoted\\credential".to_string();
        let escaped_body = format!(
            "upstream error: {}",
            serde_json::json!({"echo": escaped_secret})
        );
        let escaped = bedrock_error_snippet(&escaped_body, std::slice::from_ref(&escaped_secret));
        assert!(
            !escaped.contains("bedrock-"),
            "credential leaked: {escaped}"
        );
        assert!(escaped.contains("[REDACTED]"), "{escaped}");
    }

    #[test]
    fn build_request_includes_system_messages_and_tools() {
        let request = BedrockProvider::build_request(
            &test_context_with_tools(),
            &StreamOptions {
                max_tokens: Some(321),
                temperature: Some(0.2),
                ..StreamOptions::default()
            },
        );

        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["system"][0]["text"], "You are concise.");
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(
            value["messages"][1]["content"][0]["toolUse"]["name"],
            "search"
        );
        assert_eq!(
            value["messages"][2]["content"][0]["toolResult"]["status"],
            "success"
        );
        assert_eq!(value["inferenceConfig"]["maxTokens"], 321);
        assert_eq!(
            value["toolConfig"]["tools"][0]["toolSpec"]["name"],
            "search"
        );
    }

    #[test]
    fn build_request_answers_parallel_tool_calls_in_one_user_message() {
        let mut context = test_context_with_tools();
        let messages = context.messages.to_mut();
        if let Message::Assistant(assistant) = &mut messages[1] {
            Arc::make_mut(assistant)
                .content
                .push(ContentBlock::ToolCall(ToolCall {
                    id: "tool_2".to_string(),
                    name: "search".to_string(),
                    arguments: json!({ "q": "bedrock" }),
                    thought_signature: None,
                }));
        }
        messages.push(Message::tool_result(ToolResultMessage {
            tool_call_id: "tool_2".to_string(),
            tool_name: "search".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("second result"))],
            details: None,
            is_error: false,
            timestamp: 0,
        }));
        for content in ["Imported brief", "  "] {
            messages.push(Message::Custom(crate::model::CustomMessage {
                content: content.to_string(),
                custom_type: "handoff".to_string(),
                display: false,
                details: None,
                timestamp: 0,
            }));
        }
        messages.push(Message::User(crate::model::UserMessage {
            content: UserContent::Text("Also check the docs".to_string()),
            timestamp: 0,
        }));

        let request = BedrockProvider::build_request(&context, &StreamOptions::default());
        let value = serde_json::to_value(&request).expect("serialize request");
        let roles: Vec<_> = value["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .map(|message| message["role"].clone())
            .collect();
        assert_eq!(roles, [json!("user"), json!("assistant"), json!("user")]);
        let answer = &value["messages"][2]["content"];
        assert_eq!(answer.as_array().map(Vec::len), Some(4));
        assert_eq!(answer[0]["toolResult"]["toolUseId"], "tool_1");
        assert_eq!(answer[1]["toolResult"]["toolUseId"], "tool_2");
        assert_eq!(answer[2]["text"], "Imported brief");
        assert_eq!(answer[3]["text"], "Also check the docs");
    }

    fn replay_context(api: &str, provider: &str, model: &str) -> Context<'static> {
        let mut context = test_context_with_tools();
        let Message::Assistant(assistant) = &mut context.messages.to_mut()[1] else {
            panic!("fixture must contain an assistant tool call");
        };
        let assistant = Arc::make_mut(assistant);
        assistant.api = api.to_string();
        assistant.provider = provider.to_string();
        assistant.model = model.to_string();
        drop(assistant.content.splice(
            0..0,
            [
                ContentBlock::Thinking(ThinkingContent {
                    thinking: "model-owned reasoning".to_string(),
                    thinking_signature: Some("signed-reasoning".to_string()),
                }),
                ContentBlock::RedactedThinking(RedactedThinkingContent {
                    data: "ZW5jcnlwdGVkLXJlYXNvbmluZw==".to_string(),
                }),
                ContentBlock::Text(TextContent::new("Searching the docs.")),
            ],
        ));
        context
    }

    #[test]
    fn prepared_request_replays_reasoning_only_to_its_exact_origin() {
        let destination = BedrockProvider::new("m").with_provider_name("configured-bedrock");
        for (api, provider, model, same_origin) in [
            ("bedrock-converse-stream", "configured-bedrock", "m", true),
            (
                "bedrock-converse-stream",
                "configured-bedrock",
                "other",
                false,
            ),
            ("bedrock-converse-stream", "amazon-bedrock", "m", false),
            ("anthropic-messages", "configured-bedrock", "m", false),
            ("", "configured-bedrock", "m", false),
            ("bedrock-converse-stream", "", "m", false),
            ("bedrock-converse-stream", "configured-bedrock", "", false),
            ("", "", "", false),
        ] {
            let context = replay_context(api, provider, model);
            let original = serde_json::to_value(context.messages.as_ref()).unwrap();
            let body = destination
                .prepare_request(&context, &StreamOptions::default())
                .expect("prepare replay request");
            let mut expected = Vec::new();
            if same_origin {
                expected.extend([
                    json!({"reasoningContent": {"reasoningText": {
                        "text": "model-owned reasoning", "signature": "signed-reasoning"
                    }}}),
                    json!({"reasoningContent": {
                        "redactedContent": "ZW5jcnlwdGVkLXJlYXNvbmluZw=="
                    }}),
                ]);
            }
            expected.extend([
                json!({"text": "Searching the docs."}),
                json!({"toolUse": {
                    "toolUseId": "tool_1", "name": "search", "input": {"q": "rust"}
                }}),
            ]);
            assert_eq!(
                body["messages"][1]["content"],
                json!(expected),
                "source {api}/{provider}/{model}"
            );
            assert_eq!(body["messages"][0]["content"], json!([{"text": "Ping"}]));
            assert_eq!(
                body["messages"][2]["content"],
                json!([{"toolResult": {
                    "toolUseId": "tool_1", "content": [{"text": "result"}], "status": "success"
                }}])
            );
            assert_eq!(
                serde_json::to_value(context.messages.as_ref()).unwrap(),
                original,
                "replay must not erase reasoning from the saved conversation"
            );
        }
    }

    #[test]
    fn prepared_request_omits_foreign_reasoning_only_messages() {
        let mut context = replay_context("bedrock-converse-stream", "amazon-bedrock", "other");
        let Message::Assistant(assistant) = &mut context.messages.to_mut()[1] else {
            panic!("fixture must contain an assistant message");
        };
        Arc::make_mut(assistant).content.truncate(2);
        // This fixture has no tool call once only reasoning remains.
        context.messages.to_mut().truncate(2);
        let body = BedrockProvider::new("m")
            .prepare_request(&context, &StreamOptions::default())
            .expect("prepare reasoning-only foreign history");
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": [{"text": "Ping"}]}])
        );
        let Message::Assistant(original) = &context.messages[1] else {
            panic!("original assistant message must remain");
        };
        assert_eq!(original.content.len(), 2);
    }

    #[test]
    fn stream_sends_foreign_history_without_model_owned_reasoning() {
        let (base_url, captured_request) = spawn_bedrock_test_server();
        let provider = BedrockProvider::new("m").with_base_url(format!("{base_url}/converse"));
        let context = replay_context("bedrock-converse-stream", "amazon-bedrock", "other");
        let options = StreamOptions {
            api_key: Some("fixture-bearer-token".to_string()),
            ..StreamOptions::default()
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let mut stream = provider.stream(&context, &options).await.unwrap();
            let mut completed = false;
            while let Some(event) = stream.next().await {
                if let StreamEvent::Done { reason, message } = event.unwrap() {
                    assert_eq!(reason, StopReason::Stop);
                    assert_eq!(message.model, "m");
                    completed = true;
                }
            }
            assert!(
                completed,
                "foreign-history turn must reach a terminal response"
            );
        });
        let captured = captured_request
            .recv_timeout(FIXTURE_BUDGET)
            .expect("capture destination-aware Bedrock request");
        let body: Value = serde_json::from_slice(&captured.body).unwrap();
        assert_eq!(
            body["messages"][1]["content"],
            json!([
                {"text": "Searching the docs."},
                {"toolUse": {"toolUseId": "tool_1", "name": "search", "input": {"q": "rust"}}}
            ])
        );
        assert_eq!(
            body["messages"][2]["content"][0]["toolResult"]["toolUseId"],
            "tool_1"
        );
    }

    #[test]
    fn converse_url_appends_model_path_and_encodes_model_id() {
        let provider = BedrockProvider::new("anthropic.claude-3-5-sonnet-20240620-v1:0")
            .with_base_url("https://bedrock-runtime.us-east-1.amazonaws.com");
        let url = provider
            .converse_url("us-east-1")
            .expect("build converse URL");
        assert_eq!(
            url.path(),
            "/model/anthropic.claude-3-5-sonnet-20240620-v1:0/converse-stream"
        );
    }

    #[test]
    fn normalize_model_id_accepts_prefixed_variants() {
        assert_eq!(
            normalize_model_id("bedrock/us.anthropic.claude-3-7-sonnet-20250219-v1:0")
                .expect("normalize regional prefix"),
            "us.anthropic.claude-3-7-sonnet-20250219-v1:0"
        );
        assert_eq!(
            normalize_model_id("model/anthropic.claude-3-5-sonnet-20240620-v1:0/converse")
                .expect("normalize model path"),
            "anthropic.claude-3-5-sonnet-20240620-v1:0"
        );
    }

    #[test]
    fn sigv4_headers_include_expected_scope_and_token() {
        let url =
            Url::parse("https://bedrock-runtime.us-west-2.amazonaws.com/model/m.converse/converse")
                .expect("url");
        let now = Utc
            .with_ymd_and_hms(2026, 2, 10, 8, 0, 0)
            .single()
            .expect("datetime");
        let headers = build_sigv4_headers(
            &url,
            br#"{"messages":[{"role":"user","content":[{"text":"Ping"}]}]}"#,
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            Some("session-token"),
            "us-west-2",
            now,
        )
        .expect("sign headers");

        assert!(
            headers
                .authorization
                .contains("Credential=AKIDEXAMPLE/20260210/us-west-2/bedrock/aws4_request")
        );
        assert!(headers.authorization.contains(
            "SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token"
        ));
        assert_eq!(headers.security_token.as_deref(), Some("session-token"));
        assert_eq!(headers.amz_date, "20260210T080000Z");
        assert_eq!(headers.payload_hash.len(), 64);
    }

    #[test]
    fn sigv4_header_ownership_rejects_every_mixed_case_collision() {
        for (name, canonical_name) in [
            ("aUtHoRiZaTiOn", "authorization"),
            ("hOsT", "host"),
            ("cOnTeNt-TyPe", "content-type"),
            ("X-AmZ-DaTe", "x-amz-date"),
            ("x-AmZ-CoNtEnT-ShA256", "x-amz-content-sha256"),
            ("X-aMz-SeCuRiTy-ToKeN", "x-amz-security-token"),
            ("X-AmZ-CuStOm-MeTaDaTa", "x-amz-custom-metadata"),
        ] {
            let headers = HashMap::from([(name.to_string(), "attacker-value".to_string())]);
            let error = validate_sigv4_header_ownership(&headers, "test headers")
                .expect_err("mixed-case SigV4-owned header must be rejected");
            let message = error.to_string();
            assert!(message.contains(canonical_name), "{message}");
            assert!(message.contains("SigV4-owned"), "{message}");
        }
    }

    #[test]
    fn sigv4_header_collision_fails_before_endpoint_construction_or_inference_request() {
        let temp_dir = tempfile::tempdir().expect("Bedrock collision auth directory");
        let auth_path = temp_dir.path().join("auth.json");
        write_test_sigv4_auth(&auth_path);
        let provider = BedrockProvider::new("model")
            .with_auth_path(auth_path)
            .with_base_url("not a valid endpoint URL");
        let context = test_context_with_tools();
        let options = StreamOptions {
            headers: HashMap::from([(
                "aUtHoRiZaTiOn".to_string(),
                "Bearer must-not-replace-sigv4".to_string(),
            )]),
            ..StreamOptions::default()
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let error = runtime
            .block_on(async { provider.stream(&context, &options).await })
            .err()
            .expect("header collision must fail");
        let message = error.to_string();
        assert!(message.contains("StreamOptions.headers"), "{message}");
        assert!(message.contains("authorization"), "{message}");
        assert!(
            !message.contains("Invalid base URL"),
            "ownership validation must run before endpoint construction: {message}"
        );
    }

    #[test]
    fn sigv4_benign_compat_header_survives_without_changing_signature() {
        let (base_url, captured_request) = spawn_bedrock_test_server();
        let temp_dir = tempfile::tempdir().expect("Bedrock benign-header auth directory");
        let auth_path = temp_dir.path().join("auth.json");
        write_test_sigv4_auth(&auth_path);
        let provider = BedrockProvider::new("model")
            .with_auth_path(auth_path)
            .with_base_url(&base_url)
            .with_compat(Some(CompatConfig {
                custom_headers: Some(HashMap::from([(
                    "X-Bedrock-Trace".to_string(),
                    "trace-value".to_string(),
                )])),
                ..CompatConfig::default()
            }));
        let context = test_context_with_tools();
        let options = StreamOptions::default();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let result = runtime.block_on(async { provider.stream(&context, &options).await });
        assert!(result.is_ok(), "Bedrock request failed: {:?}", result.err());

        let captured = captured_request
            .recv_timeout(Duration::from_secs(2))
            .expect("capture Bedrock request");
        assert_eq!(
            captured.headers.get("x-bedrock-trace").map(String::as_str),
            Some("trace-value")
        );
        assert_eq!(
            captured.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(
            captured
                .headers
                .get("x-amz-security-token")
                .map(String::as_str),
            Some("session-token")
        );

        let amz_date = captured
            .headers
            .get("x-amz-date")
            .expect("signed x-amz-date");
        let signing_time = chrono::NaiveDateTime::parse_from_str(amz_date, "%Y%m%dT%H%M%SZ")
            .expect("parse x-amz-date")
            .and_utc();
        let request_url = Url::parse(&format!("{base_url}/model/model/converse-stream"))
            .expect("Bedrock request URL");
        let expected = build_sigv4_headers(
            &request_url,
            &captured.body,
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            Some("session-token"),
            "us-west-2",
            signing_time,
        )
        .expect("rebuild observed signature");
        assert_eq!(
            captured.headers.get("authorization"),
            Some(&expected.authorization)
        );
        assert_eq!(
            captured.headers.get("x-amz-content-sha256"),
            Some(&expected.payload_hash)
        );
        assert!(
            !expected.authorization.contains("x-bedrock-trace"),
            "benign custom headers remain outside the fixed SigV4 signed-header set"
        );
    }

    #[test]
    fn explicit_bearer_mode_keeps_custom_authorization_semantics() {
        let (base_url, captured_request) = spawn_bedrock_test_server();
        let provider = BedrockProvider::new("model")
            .with_base_url(base_url)
            .with_compat(Some(CompatConfig {
                custom_headers: Some(HashMap::from([(
                    "aUtHoRiZaTiOn".to_string(),
                    "ProxyScheme proxy-credential".to_string(),
                )])),
                ..CompatConfig::default()
            }));
        let context = test_context_with_tools();
        let options = StreamOptions {
            api_key: Some("direct-bearer-token".to_string()),
            ..StreamOptions::default()
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let result = runtime.block_on(async { provider.stream(&context, &options).await });
        assert!(result.is_ok(), "Bedrock request failed: {:?}", result.err());

        let captured = captured_request
            .recv_timeout(Duration::from_secs(2))
            .expect("capture Bedrock bearer request");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("ProxyScheme proxy-credential"),
            "explicit bearer mode retains custom proxy-auth replacement semantics"
        );
        assert!(
            !captured.headers.contains_key("x-amz-date"),
            "bearer mode must remain separate from SigV4 header ownership"
        );
    }

    #[test]
    fn response_to_message_maps_tool_use_and_usage() {
        let provider = BedrockProvider::new("anthropic.claude-3-5-sonnet-20240620-v1:0");
        let response: BedrockConverseResponse = serde_json::from_value(json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"text": "I can help."},
                        {"toolUse": {"toolUseId": "call_1", "name": "search", "input": {"q": "rust"}}}
                    ]
                }
            },
            "stopReason": "tool_use",
            "usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15}
        }))
        .expect("parse response");

        let message = provider.response_to_message(response);
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(message.usage.input, 10);
        assert_eq!(message.usage.output, 5);
        assert_eq!(message.usage.total_tokens, 15);
        assert!(matches!(message.content[0], ContentBlock::Text(_)));
        assert!(matches!(message.content[1], ContentBlock::ToolCall(_)));
    }

    fn json_tool(id: &str, name: &str, input: Value) -> Value {
        json!({"toolUse": {"toolUseId": id, "name": name, "input": input}})
    }

    fn json_reply(content: Vec<Value>, stop_reason: &str) -> Value {
        json!({
            "output": {"message": {"role": "assistant", "content": content}},
            "stopReason": stop_reason,
            "usage": {
                "inputTokens": 7, "outputTokens": 3,
                "cacheReadInputTokens": 20, "cacheWriteInputTokens": 5,
                "totalTokens": 35
            }
        })
    }

    fn collect_json_reply(response: Value) -> Vec<Result<StreamEvent>> {
        let (base_url, requests) = spawn_bedrock_json_server(vec![response]);
        let provider =
            BedrockProvider::new("model-a").with_base_url(format!("{base_url}/converse"));
        let options = StreamOptions {
            api_key: Some("fixture-bearer-token".to_string()),
            ..StreamOptions::default()
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let events = runtime.block_on(async {
            provider
                .stream(&Context::default(), &options)
                .await
                .expect("JSON response stream")
                .collect()
                .await
        });
        requests
            .recv_timeout(FIXTURE_BUDGET)
            .expect("real HTTP request");
        events
    }

    fn guarded_json_provider(base_url: &str) -> (Arc<dyn Provider>, crate::cost::CostBudget) {
        let provider = Arc::new(crate::providers::PricedProvider {
            inner: Arc::new(
                BedrockProvider::new("model-a").with_base_url(format!("{base_url}/converse")),
            ),
            cost: crate::provider::ModelCost {
                audio: None,
                input: 1_000_000.0,
                output: 1_000_000.0,
                cache_read: 1_000_000.0,
                cache_write: 1_000_000.0,
            },
        });
        let budget = crate::cost::CostBudget::new(None, Some(34.0), 0.0).unwrap();
        (
            crate::provider::guard_with_cost_budget(provider, budget.clone()),
            budget,
        )
    }

    #[test]
    fn json_usage_survives_unpolled_streams_and_invalid_output_shapes() {
        for outcome in ["drop", "refused", "missing_output", "invalid_output"] {
            let reason = if outcome == "refused" {
                "content_filtered"
            } else {
                "end_turn"
            };
            let mut response = json_reply(vec![json!({"text": "answer"})], reason);
            match outcome {
                "missing_output" => response["output"] = Value::Null,
                "invalid_output" => response["output"] = json!({"message": false}),
                _ => {}
            }
            let (base_url, requests) = spawn_bedrock_json_server(vec![response]);
            let (provider, budget) = guarded_json_provider(&base_url);
            let options = StreamOptions {
                api_key: Some("fixture-bearer-token".to_string()),
                ..StreamOptions::default()
            };
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime build");
            runtime.block_on(async {
                let result = provider.stream(&Context::default(), &options).await;
                if matches!(outcome, "drop" | "refused") {
                    let output = result.expect("JSON response stream");
                    assert_eq!(budget.snapshot().pending, 35.0);
                    assert_eq!(budget.spent(), 0.0);
                    assert!(matches!(
                        provider.stream(&Context::default(), &options).await,
                        Err(Error::CostCap { .. })
                    ));
                    drop(output);
                } else {
                    assert!(result.is_err(), "invalid JSON output must remain rejected");
                }
                assert_eq!(budget.spent(), 35.0);
                assert_eq!(budget.snapshot().pending, 0.0);
            });
            requests
                .recv_timeout(FIXTURE_BUDGET)
                .expect("real HTTP request");
        }
    }

    #[test]
    fn json_setup_usage_and_public_events_settle_one_bill() {
        let response = json_reply(vec![json!({"text": "answer"})], "end_turn");
        let (base_url, requests) = spawn_bedrock_json_server(vec![response]);
        let (provider, budget) = guarded_json_provider(&base_url);
        let options = StreamOptions {
            api_key: Some("fixture-bearer-token".to_string()),
            ..StreamOptions::default()
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let events: Vec<_> = provider
                .stream(&Context::default(), &options)
                .await
                .unwrap()
                .collect()
                .await;
            let Some(Ok(StreamEvent::Done { message, .. })) = events.last() else {
                panic!("expected Done: {events:?}");
            };
            assert_eq!(message.usage.cost.total, 35.0);
            assert_eq!(budget.spent(), 35.0);
            assert_eq!(budget.snapshot().pending, 0.0);
        });
        requests
            .recv_timeout(FIXTURE_BUDGET)
            .expect("real HTTP request");
    }

    #[test]
    fn json_usage_envelope_rejects_ambiguous_billing_fields() {
        for response in [
            r#"{"usage":{"inputTokens":7,"inputTokens":0}}"#,
            r#"{"usage":{"inputTokens":7},"usage":null}"#,
        ] {
            assert!(serde_json::from_str::<BedrockUsageEnvelope>(response).is_err());
            assert!(serde_json::from_str::<BedrockConverseResponse>(response).is_err());
        }
    }

    fn assert_json_failure(events: &[Result<StreamEvent>]) -> &AssistantMessage {
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        assert!(
            !events.iter().any(|event| matches!(
                event,
                Ok(StreamEvent::ToolCallStart { .. }
                    | StreamEvent::ToolCallDelta { .. }
                    | StreamEvent::ToolCallEnd { .. }
                    | StreamEvent::Done { .. })
            )),
            "invalid whole response must not publish local calls: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(StreamEvent::Error { .. })))
                .count(),
            1
        );
        let Some(Ok(StreamEvent::Error { reason, error })) = events.last() else {
            panic!("expected one terminal Error: {events:?}");
        };
        assert_eq!(*reason, StopReason::Error);
        assert_eq!(error.stop_reason, StopReason::Error);
        assert!(
            !error
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolCall(_)))
        );
        assert_eq!(error.usage.input, 7);
        assert_eq!(error.usage.output, 3);
        assert_eq!(error.usage.cache_read, 20);
        assert_eq!(error.usage.cache_write, 5);
        assert_eq!(error.usage.total_tokens, 35);
        error
    }

    #[test]
    fn json_http_tools_require_terminal_tool_use_authorization() {
        for reason in [
            "end_turn",
            "stop_sequence",
            "max_tokens",
            "model_context_window_exceeded",
            "guardrail_intervened",
            "content_filtered",
            "malformed_tool_use",
            "malformed_model_output",
            "future_status secret-canary\u{1b}[31m\n",
        ] {
            let events = collect_json_reply(json_reply(
                vec![
                    json!({"text": "retained explanation"}),
                    json_tool(
                        "call-a",
                        "write",
                        json!({"path": "kept.txt", "content": "x"}),
                    ),
                ],
                reason,
            ));
            let error = assert_json_failure(&events);
            assert!(matches!(
                &error.content[0], ContentBlock::Text(text) if text.text == "retained explanation"
            ));
            assert_eq!(error.model, "model-a");
            assert_eq!(error.provider, "amazon-bedrock");
            let diagnostic = error.error_message.as_deref().expect("fixed diagnostic");
            assert!(
                diagnostic.contains("local tool calls withheld"),
                "{diagnostic}"
            );
            assert!(!diagnostic.contains("secret-canary"), "{diagnostic}");
        }
    }

    #[test]
    fn json_http_text_stops_preserve_normal_truncated_and_refused_outcomes() {
        for (reason, expected) in [
            ("end_turn", StopReason::Stop),
            ("stop_sequence", StopReason::Stop),
            ("max_tokens", StopReason::Length),
            ("model_context_window_exceeded", StopReason::Length),
            ("guardrail_intervened", StopReason::Error),
            ("content_filtered", StopReason::Error),
        ] {
            let events = collect_json_reply(json_reply(vec![json!({"text": "héllo"})], reason));
            let message = if expected == StopReason::Error {
                assert_json_failure(&events)
            } else {
                let Some(Ok(StreamEvent::Done { reason, message })) = events.last() else {
                    panic!("expected Done for {reason}: {events:?}");
                };
                assert_eq!(*reason, expected);
                assert!(message.error_message.is_none());
                message
            };
            assert_eq!(message.stop_reason, expected);
            assert_eq!(message.usage.total_tokens, 35);
            assert!(matches!(
                &message.content[0], ContentBlock::Text(text) if text.text == "héllo"
            ));
        }
    }

    #[test]
    fn json_invalid_parallel_batch_withholds_every_call() {
        let valid = json_tool("call-a", "write", json!({"path": "a", "content": "x"}));
        let mut invalid_calls = vec![
            json_tool("call-a", "read", json!({})),
            json_tool("", "read", json!({})),
            json_tool("call-b", "read/file", json!({})),
            json_tool("call-b", "é", json!({})),
            json_tool(&"x".repeat(65), "read", json!({})),
            json_tool("call-b", &"x".repeat(65), json!({})),
            json_tool("call-b", "read", Value::Null),
            json_tool("call-b", "read", json!([])),
            json_tool("call-b", "read", json!(42)),
            json_tool("call-b", "read", json!("{}")),
            json!({"toolUse": {"toolUseId": "call-b", "name": "read"}}),
            json!({"toolUse": {"name": "read", "input": {}}}),
            json!({"toolUse": {"toolUseId": "call-b", "input": {}}}),
            json!({"toolUse": {"toolUseId": 42, "name": "read", "input": {}}}),
            json!({"toolUse": {"toolUseId": "call-b", "name": false, "input": {}}}),
        ];
        for kind in [
            json!("server_tool_use"),
            json!("future_type"),
            Value::Null,
            json!(false),
        ] {
            let mut tool = json_tool("call-b", "read", json!({}));
            tool["toolUse"]["type"] = kind;
            invalid_calls.push(tool);
        }
        for invalid in invalid_calls {
            let response: BedrockConverseResponse = serde_json::from_value(json_reply(
                vec![json!({"text": "before calls"}), valid.clone(), invalid],
                "tool_use",
            ))
            .expect("valid JSON shape");
            let message = BedrockProvider::new("m").response_to_message(response);
            let events = BedrockProvider::message_events(&message);
            assert_json_failure(&events);
        }
    }

    #[test]
    fn json_missing_role_empty_tool_stop_and_block_limit_fail_closed() {
        let call = json_tool("call-a", "read", json!({}));
        // A valid unique tool after otherwise valid text blocks isolates the
        // size guard from identity/argument errors in this regression.
        let mut too_many = vec![json!({"text": "x"}); MAX_BEDROCK_CONTENT_BLOCKS];
        too_many.push(call.clone());
        let mut replies = vec![
            json_reply(vec![json!({"text": "no calls"})], "tool_use"),
            json_reply(too_many, "tool_use"),
        ];
        for role in [json!("user"), json!("system"), Value::Null] {
            let mut reply = json_reply(vec![call.clone()], "tool_use");
            reply["output"]["message"]["role"] = role;
            replies.push(reply);
        }
        let mut missing_role = json_reply(vec![call], "tool_use");
        missing_role["output"]["message"]
            .as_object_mut()
            .unwrap()
            .remove("role");
        replies.push(missing_role);
        for reply in replies {
            let response: BedrockConverseResponse = serde_json::from_value(reply).unwrap();
            let message = BedrockProvider::new("m").response_to_message(response);
            assert_json_failure(&BedrockProvider::message_events(&message));
        }
    }

    #[test]
    fn json_content_union_never_hides_a_second_tool_member() {
        let mut ambiguous = json_tool("call-b", "write", json!({"path": "a", "content": "x"}));
        ambiguous["text"] = json!("cannot conceal the tool");
        let events = collect_json_reply(json_reply(
            vec![
                json!({"text": "retained explanation"}),
                json_tool("call-a", "read", json!({})),
                ambiguous,
            ],
            "tool_use",
        ));
        let error = assert_json_failure(&events);
        assert!(matches!(
            &error.content[0], ContentBlock::Text(text) if text.text == "retained explanation"
        ));
        assert!(
            error
                .error_message
                .as_deref()
                .unwrap()
                .contains("ambiguous tool content")
        );
    }

    #[test]
    fn json_http_authorized_parallel_calls_preserve_ids_inputs_and_order() {
        let events = collect_json_reply(json_reply(
            vec![
                json!({"text": "working"}),
                json_tool("provider:call.1_a-b", "read", json!({"path": "a"})),
                json_tool("call-b", "current_time", json!({})),
            ],
            "tool_use",
        ));
        let calls = events
            .iter()
            .filter_map(|event| match event {
                Ok(StreamEvent::ToolCallEnd { tool_call, .. }) => Some(tool_call),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "provider:call.1_a-b");
        assert_eq!(calls[0].arguments, json!({"path": "a"}));
        assert_eq!(calls[1].id, "call-b");
        assert_eq!(calls[1].arguments, json!({}));
        let Some(Ok(StreamEvent::Done { reason, message })) = events.last() else {
            panic!("authorized batch must end successfully: {events:?}");
        };
        assert_eq!(*reason, StopReason::ToolUse);
        assert_eq!(message.usage.total_tokens, 35);
    }

    #[test]
    fn json_http_invalid_calls_cannot_write_files_through_the_agent() {
        use crate::agent::{Agent, AgentConfig, AgentEvent};
        use crate::tools::ToolRegistry;
        use std::sync::atomic::{AtomicUsize, Ordering};

        for case in [
            "authorized",
            "malformed",
            "truncated",
            "server",
            "invalid-batch",
        ] {
            let directory = tempfile::tempdir().expect("isolated write target");
            let marker = directory.path().join("marker.txt");
            let mut call = json_tool(
                "call-a",
                "write",
                json!({"path": marker.to_string_lossy(), "content": "authorized write"}),
            );
            if case == "server" {
                call["toolUse"]["type"] = json!("server_tool_use");
            }
            let mut content = vec![json!({"text": "write decision"}), call];
            if case == "invalid-batch" {
                content.push(json_tool("call-b", "read", Value::Null));
            }
            let reason = match case {
                "malformed" => "malformed_tool_use",
                "truncated" => "max_tokens",
                _ => "tool_use",
            };
            let authorized = case == "authorized";
            let mut replies = vec![json_reply(content, reason)];
            if authorized {
                replies.push(json_reply(vec![json!({"text": "Finished."})], "end_turn"));
            }
            let (url, requests) = spawn_bedrock_json_server(replies);
            let provider = BedrockProvider::new("m").with_base_url(format!("{url}/converse"));
            let config = AgentConfig {
                max_tool_iterations: 1,
                stream_options: StreamOptions {
                    api_key: Some("fixture-bearer-token".to_string()),
                    ..StreamOptions::default()
                },
                turn_recovery: crate::turn_recovery::TurnRecoveryMode::Off,
                ..AgentConfig::default()
            };
            let tools = ToolRegistry::new(&["write", "read"], directory.path(), None);
            let mut agent = Agent::new(Arc::new(provider), tools, config);
            let calls = Arc::new(AtomicUsize::new(0));
            let observed_calls = Arc::clone(&calls);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime build");
            let result = runtime
                .block_on(async {
                    agent
                        .run(
                            "Write the marker when the provider authorizes it.",
                            move |event| {
                                if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                                    observed_calls.fetch_add(1, Ordering::SeqCst);
                                }
                            },
                        )
                        .await
                })
                .expect("agent response");
            assert_eq!(
                calls.load(Ordering::SeqCst),
                usize::from(authorized),
                "{case}"
            );
            assert_eq!(
                marker.exists(),
                authorized,
                "{case} must respect tool authorization"
            );
            if authorized {
                assert_eq!(
                    std::fs::read_to_string(&marker).unwrap(),
                    "authorized write"
                );
                assert_eq!(result.stop_reason, StopReason::Stop);
            } else {
                assert_eq!(result.stop_reason, StopReason::Error, "{case}");
                assert_eq!(result.usage.total_tokens, 35);
            }
            let expected_requests = if authorized { 2 } else { 1 };
            for _ in 0..expected_requests {
                requests
                    .recv_timeout(FIXTURE_BUDGET)
                    .expect("captured inference request");
            }
            assert!(requests.try_recv().is_err(), "{case} retried unexpectedly");
        }
    }

    #[test]
    fn resolve_auth_context_uses_stream_option_api_key_override() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let auth_path = temp_dir.path().join("auth.json");
        let mut stored = AuthStorage::load(auth_path.clone()).expect("load auth");
        stored.set(
            "amazon-bedrock",
            AuthCredential::AwsCredentials {
                access_key_id: "AKIA_STORED".to_string(),
                // ubs:ignore test fixture credential, not live secret.
                secret_access_key: "stored-secret".to_string(),
                session_token: None,
                region: Some("us-west-2".to_string()),
            },
        );
        stored.save().expect("save auth");

        let provider = BedrockProvider::new("model").with_auth_path(auth_path);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let auth = runtime.block_on(async {
            provider
                .resolve_auth_context(&StreamOptions {
                    api_key: Some("bedrock-bearer".to_string()),
                    ..StreamOptions::default()
                })
                .await
                .expect("resolve auth context")
        });
        assert!(matches!(
            auth.auth,
            BedrockAuth::Bearer { ref token } if token == "bedrock-bearer"
        ));
    }

    #[test]
    fn explicit_bearer_region_trims_and_falls_back_across_region_env_vars() {
        let region = bedrock_region_with_env(|name| match name {
            "AWS_REGION" => Some("   ".to_string()),
            "AWS_DEFAULT_REGION" => Some("  eu-west-1  ".to_string()),
            _ => None,
        });
        assert_eq!(region, "eu-west-1");

        let default_region = bedrock_region_with_env(|_| Some("  ".to_string()));
        assert_eq!(default_region, DEFAULT_REGION);
    }

    fn make_bedrock_tool_result(content: Vec<ContentBlock>, is_error: bool) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: "tool_42".to_string(),
            tool_name: "test_tool".to_string(),
            content,
            details: None,
            is_error,
            timestamp: 0,
        }
    }

    #[test]
    fn tool_result_text_only_serializes_correctly() {
        let msg = make_bedrock_tool_result(
            vec![ContentBlock::Text(TextContent {
                text: "found it".to_string(),
                text_signature: None,
            })],
            false,
        );
        let bedrock_msg = convert_tool_result_message(&msg);
        let value = serde_json::to_value(&bedrock_msg).expect("serialize");
        assert_eq!(value["role"], "user");
        let tool_result = &value["content"][0]["toolResult"];
        assert_eq!(tool_result["toolUseId"], "tool_42");
        assert_eq!(tool_result["status"], "success");
        assert_eq!(tool_result["content"][0]["text"], "found it");
        assert_eq!(tool_result["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn tool_result_image_only_uses_native_image_format() {
        let msg = make_bedrock_tool_result(
            vec![ContentBlock::Image(crate::model::ImageContent {
                data: "aW1hZ2U=".to_string(),
                mime_type: "image/png".to_string(),
            })],
            false,
        );
        let bedrock_msg = convert_tool_result_message(&msg);
        let value = serde_json::to_value(&bedrock_msg).expect("serialize");
        let tool_result = &value["content"][0]["toolResult"];
        let content = &tool_result["content"][0];
        assert_eq!(content["image"]["format"], "png");
        assert_eq!(content["image"]["source"]["bytes"], "aW1hZ2U=");
    }

    #[test]
    fn tool_result_mixed_text_and_image_preserves_both() {
        let msg = make_bedrock_tool_result(
            vec![
                ContentBlock::Text(TextContent {
                    text: "description".to_string(),
                    text_signature: None,
                }),
                ContentBlock::Image(crate::model::ImageContent {
                    data: "anBlZw==".to_string(),
                    mime_type: "image/jpeg".to_string(),
                }),
            ],
            false,
        );
        let bedrock_msg = convert_tool_result_message(&msg);
        let value = serde_json::to_value(&bedrock_msg).expect("serialize");
        let contents = value["content"][0]["toolResult"]["content"]
            .as_array()
            .expect("content array");
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["text"], "description");
        assert_eq!(contents[1]["image"]["format"], "jpeg");
    }

    #[test]
    fn tool_result_empty_content_falls_back_to_empty_json() {
        let msg = make_bedrock_tool_result(vec![], false);
        let bedrock_msg = convert_tool_result_message(&msg);
        let value = serde_json::to_value(&bedrock_msg).expect("serialize");
        let contents = value["content"][0]["toolResult"]["content"]
            .as_array()
            .expect("content array");
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["text"], "{}");
    }

    #[test]
    fn tool_result_error_sets_error_status() {
        let msg = make_bedrock_tool_result(
            vec![ContentBlock::Text(TextContent {
                text: "not found".to_string(),
                text_signature: None,
            })],
            true,
        );
        let bedrock_msg = convert_tool_result_message(&msg);
        let value = serde_json::to_value(&bedrock_msg).expect("serialize");
        assert_eq!(value["content"][0]["toolResult"]["status"], "error");
    }

    #[test]
    fn tool_result_image_mime_extracts_format() {
        let msg = make_bedrock_tool_result(
            vec![ContentBlock::Image(crate::model::ImageContent {
                data: "data".to_string(),
                mime_type: "image/webp".to_string(),
            })],
            false,
        );
        let bedrock_msg = convert_tool_result_message(&msg);
        let value = serde_json::to_value(&bedrock_msg).expect("serialize");
        assert_eq!(
            value["content"][0]["toolResult"]["content"][0]["image"]["format"],
            "webp"
        );
    }

    #[test]
    fn model_controls_reach_hook_and_signed_http_body() {
        let (base_url, captured_request) = spawn_bedrock_test_server();
        let temp_dir = tempfile::tempdir().expect("auth directory");
        let auth_path = temp_dir.path().join("auth.json");
        write_test_sigv4_auth(&auth_path);
        let provider = BedrockProvider::new("us.anthropic.claude-opus-4-6-v1")
            .with_auth_path(auth_path)
            .with_base_url(&base_url);
        let context = test_context_with_tools();
        let original = serde_json::to_value(context.messages.as_ref()).unwrap();
        let options = StreamOptions {
            thinking_level: Some(crate::model::ThinkingLevel::High),
            max_tokens: Some(8192),
            temperature: Some(0.2),
            cache_retention: crate::provider::CacheRetention::Long,
            before_provider_request: Some(crate::provider::BeforeProviderRequestHook::new(
                |mut event| {
                    assert_eq!(
                        event.payload["additionalModelRequestFields"]["thinking"]["type"],
                        "adaptive"
                    );
                    assert_eq!(event.payload["system"][1]["cachePoint"]["ttl"], "1h");
                    assert!(
                        event.payload["inferenceConfig"]
                            .get("temperature")
                            .is_none()
                    );
                    event.payload["additionalModelRequestFields"]["output_config"]["effort"] =
                        json!("low");
                    event.payload["messages"][0]["content"][0]["text"] = json!("Hook rewrite");
                    Box::pin(futures::future::ready(Some(event.payload)))
                },
            )),
            ..StreamOptions::default()
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let events = runtime.block_on(async {
            provider
                .stream(&context, &options)
                .await
                .expect("provider stream")
                .collect::<Vec<_>>()
                .await
        });
        assert!(events.iter().all(Result::is_ok));
        assert!(matches!(events.last(), Some(Ok(StreamEvent::Done { .. }))));
        assert_eq!(
            serde_json::to_value(context.messages.as_ref()).unwrap(),
            original
        );
        let captured = captured_request
            .recv_timeout(Duration::from_secs(2))
            .expect("captured request");
        let body: Value = serde_json::from_slice(&captured.body).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["text"], "Hook rewrite");
        assert_eq!(
            body["additionalModelRequestFields"]["output_config"]["effort"],
            "low"
        );
        assert_eq!(body["inferenceConfig"]["maxTokens"], 8192);
        assert_eq!(body["toolConfig"]["tools"][1]["cachePoint"]["ttl"], "1h");
        assert_eq!(body["messages"][2]["content"][1]["cachePoint"]["ttl"], "1h");
        let signing_time = chrono::NaiveDateTime::parse_from_str(
            &captured.headers["x-amz-date"],
            "%Y%m%dT%H%M%SZ",
        )
        .unwrap()
        .and_utc();
        let expected = build_sigv4_headers(
            &provider.converse_url("us-west-2").unwrap(),
            &captured.body,
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            Some("session-token"),
            "us-west-2",
            signing_time,
        )
        .unwrap();
        assert_eq!(captured.headers["authorization"], expected.authorization);
        assert_eq!(
            captured.headers["x-amz-content-sha256"],
            expected.payload_hash
        );
    }

    #[test]
    fn rejected_request_hook_preserves_native_controls_on_the_wire() {
        let (base_url, captured_request) = spawn_bedrock_test_server();
        let provider = BedrockProvider::new("anthropic.claude-3-7-sonnet").with_base_url(base_url);
        let context = test_context_with_tools();
        let options = StreamOptions {
            api_key: Some("test-bearer".to_string()),
            thinking_level: Some(crate::model::ThinkingLevel::Low),
            max_tokens: Some(4096),
            cache_retention: crate::provider::CacheRetention::Short,
            before_provider_request: Some(crate::provider::BeforeProviderRequestHook::new(|_| {
                Box::pin(futures::future::ready(Some(json!({"invalid": true}))))
            })),
            ..StreamOptions::default()
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let result = runtime.block_on(async { provider.stream(&context, &options).await });
        assert!(result.is_ok(), "{:?}", result.err());
        let captured = captured_request
            .recv_timeout(Duration::from_secs(2))
            .expect("captured request");
        let body: Value = serde_json::from_slice(&captured.body).unwrap();
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["type"],
            "enabled"
        );
        assert_eq!(body["inferenceConfig"]["maxTokens"], 4096);
        assert_eq!(
            body["system"][1],
            json!({"cachePoint": {"type": "default"}})
        );
        assert_eq!(body["messages"][0]["content"][0]["text"], "Ping");
    }

    #[test]
    fn invalid_thinking_budget_fails_before_endpoint_or_hook() {
        let provider = BedrockProvider::new("anthropic.claude-sonnet-4-5")
            .with_base_url("not a valid endpoint URL");
        let options = StreamOptions {
            api_key: Some("test-bearer".to_string()),
            thinking_level: Some(crate::model::ThinkingLevel::High),
            max_tokens: Some(1024),
            before_provider_request: Some(crate::provider::BeforeProviderRequestHook::new(|_| {
                panic!("invalid budget must not reach the request hook")
            })),
            ..StreamOptions::default()
        };
        let context = test_context_with_tools();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(async { provider.stream(&context, &options).await })
            .err()
            .expect("invalid budget");
        assert!(error.to_string().contains("max_tokens greater than 1024"));
    }

    #[test]
    fn json_usage_keeps_cache_counters_and_saturates_total_fallback() {
        let mut wire = json!({
            "inputTokens": 10,
            "outputTokens": 5,
            "cacheReadInputTokens": 20,
            "cacheWriteInputTokens": 3
        });
        let usage: BedrockUsage = serde_json::from_value(wire.clone()).unwrap();
        let converted = convert_usage(&usage);
        assert_eq!(converted.input, 10);
        assert_eq!(converted.cache_read, 20);
        assert_eq!(converted.cache_write, 3);
        assert_eq!(converted.total_tokens, 38);
        wire["totalTokens"] = json!(50);
        let usage: BedrockUsage = serde_json::from_value(wire).unwrap();
        assert_eq!(convert_usage(&usage).total_tokens, 50);
        let huge: BedrockUsage = serde_json::from_value(json!({
            "inputTokens": u64::MAX, "outputTokens": 1,
            "cacheReadInputTokens": 2, "cacheWriteInputTokens": 3
        }))
        .unwrap();
        assert_eq!(convert_usage(&huge).total_tokens, u64::MAX);
    }
}
