//! Amazon Bedrock Converse provider implementation.
//!
//! This provider targets the Bedrock Converse API and maps its non-streaming
//! JSON response into Pi stream events.

use crate::auth::{
    AUTH_RESOLUTION_LOCK_TIMEOUT, AuthStorage, AuthStorageLoadFailure, AwsResolvedCredentials,
    resolve_ambient_aws_credentials_async, resolve_aws_credentials_async,
};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::http::client::{Client, RequestBuilder};
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ToolCall,
    ToolResultMessage, Usage, UserContent,
};
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::Stream;
use futures::stream;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use url::Url;

const DEFAULT_REGION: &str = "us-east-1";
const BEDROCK_SERVICE: &str = "bedrock";
const MAX_BEDROCK_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_BEDROCK_ERROR_SNIPPET_BYTES: usize = 8 * 1024;
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
        AuthStorage::load_with_lock_timeout_classified(
            self.auth_path(),
            AUTH_RESOLUTION_LOCK_TIMEOUT,
        )
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
            segments.push("model");
            segments.push(&self.model);
            segments.push("converse");
        }
        Ok(url)
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

        let mut messages = Vec::new();
        for message in context.messages.iter() {
            if let Some(converted) = convert_message(message) {
                messages.push(converted);
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

        let stop_reason = map_stop_reason(response.stop_reason.as_deref());
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
                        content.push(ContentBlock::ToolCall(ToolCall {
                            id: tool_use.tool_use_id,
                            name: tool_use.name,
                            arguments: tool_use.input,
                            thought_signature: None,
                        }));
                    }
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
            error_message: None,
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
                ContentBlock::ToolCall(tool_call) => {
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

        events.push(Ok(StreamEvent::Done {
            reason: message.stop_reason,
            message: message.clone(),
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
        let mut request = self
            .client
            .post(url.as_str())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json");
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
        let request_body = Self::build_request(context, options);
        let body = serde_json::to_vec(&request_body).map_err(|err| {
            Error::provider(
                "amazon-bedrock",
                format!("Failed to serialize request body: {err}"),
            )
        })?;

        let auth_context = self.resolve_auth_context(options).await?;
        // Reject caller-owned signing headers before endpoint parsing or an inference request.
        // Credential discovery necessarily happens first so bearer mode can retain its custom
        // Authorization semantics, but a malformed endpoint override must not mask a SigV4
        // ownership violation.
        self.validate_auth_header_ownership(&auth_context, options)?;
        let url = self.converse_url(&auth_context.region)?;
        let (request, response_secrets) =
            self.authenticated_request(&url, &body, auth_context, options)?;

        let response = request.body(body).send().await?;
        let status = response.status();
        let response_text = response
            .text_limited(MAX_BEDROCK_RESPONSE_BYTES)
            .await
            .unwrap_or_else(|err| format!("<failed to read body: {err}>"));

        if !(200..300).contains(&status) {
            let response_snippet = bedrock_error_snippet(&response_text, &response_secrets);
            return Err(Error::provider(
                "amazon-bedrock",
                format!("Bedrock Converse API error (HTTP {status}): {response_snippet}"),
            ));
        }

        let parsed: BedrockConverseResponse =
            serde_json::from_str(&response_text).map_err(|err| {
                let response_snippet = bedrock_error_snippet(&response_text, &response_secrets);
                Error::provider(
                    "amazon-bedrock",
                    format!("Failed to parse Bedrock response: {err}\nData: {response_snippet}"),
                )
            })?;

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
        Message::Custom(_) => None,
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
    #[allow(dead_code)]
    role: Option<String>,
    #[serde(default)]
    content: Vec<BedrockResponseContent>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BedrockResponseContent {
    Text {
        text: String,
    },
    ToolUse {
        #[serde(rename = "toolUse")]
        tool_use: BedrockResponseToolUse,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BedrockResponseToolUse {
    tool_use_id: String,
    name: String,
    #[serde(default)]
    input: Value,
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
    total_tokens: u64,
}

fn convert_usage(usage: &BedrockUsage) -> Usage {
    let total = if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input_tokens + usage.output_tokens
    };

    Usage {
        input: usage.input_tokens,
        output: usage.output_tokens,
        total_tokens: total,
        ..Usage::default()
    }
}

fn map_stop_reason(stop_reason: Option<&str>) -> StopReason {
    match stop_reason.unwrap_or("end_turn") {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::Length,
        "guardrail_intervened" | "content_filtered" => StopReason::Error,
        _ => StopReason::Stop,
    }
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
    use serde_json::json;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    #[derive(Debug)]
    struct CapturedBedrockRequest {
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    fn spawn_bedrock_test_server() -> (String, mpsc::Receiver<CapturedBedrockRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind Bedrock test server");
        let address = listener.local_addr().expect("Bedrock test server address");
        let (sender, receiver) = mpsc::channel();

        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept Bedrock request");
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set Bedrock request read timeout");

            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            let header_end = loop {
                let read = socket.read(&mut chunk).expect("read Bedrock request");
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
                let read = socket.read(&mut chunk).expect("read Bedrock request body");
                assert!(read > 0, "Bedrock request ended before its body");
                request.extend_from_slice(&chunk[..read]);
            }
            sender
                .send(CapturedBedrockRequest {
                    headers,
                    body: request[body_start..body_start + content_length].to_vec(),
                })
                .expect("capture Bedrock request");

            let response_body = serde_json::to_string(&json!({
                "output": {
                    "message": {
                        "role": "assistant",
                        "content": [{"text": "ok"}]
                    }
                },
                "stopReason": "end_turn",
                "usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}
            }))
            .expect("serialize Bedrock response");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            socket
                .write_all(response.as_bytes())
                .expect("write Bedrock response");
        });

        (format!("http://{address}"), receiver)
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
    fn converse_url_appends_model_path_and_encodes_model_id() {
        let provider = BedrockProvider::new("anthropic.claude-3-5-sonnet-20240620-v1:0")
            .with_base_url("https://bedrock-runtime.us-east-1.amazonaws.com");
        let url = provider
            .converse_url("us-east-1")
            .expect("build converse URL");
        assert_eq!(
            url.path(),
            "/model/anthropic.claude-3-5-sonnet-20240620-v1:0/converse"
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
        let request_url =
            Url::parse(&format!("{base_url}/model/model/converse")).expect("Bedrock request URL");
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
}
