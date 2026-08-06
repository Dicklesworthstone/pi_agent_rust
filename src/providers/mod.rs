//! Provider implementations.
//!
//! This module contains concrete implementations of the Provider trait
//! for various LLM APIs.

use crate::error::{Error, Result};
use crate::extensions::{ExtensionManager, ExtensionRuntimeHandle};
use crate::http::client::{Client, RequestBuilder};
use crate::model::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, StopReason, TextContent, Usage,
};
use crate::models::ModelEntry;
use crate::provider::{Context, Provider, StreamEvent, StreamOptions};
use crate::provider_metadata::{
    PROVIDER_METADATA, canonical_provider_id, provider_routing_defaults,
};
use crate::vcr::{VCR_ENV_MODE, VcrRecorder};
use async_trait::async_trait;
use chrono::Utc;
use futures::stream;
use futures::stream::Stream;
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::pin::Pin;
use std::sync::Arc;
use url::Url;

pub mod anthropic;
pub mod azure;
pub mod bedrock;
pub mod cohere;
pub mod copilot;
pub mod cursor;
pub mod gemini;
pub mod gitlab;
pub mod model_fetch;
pub mod openai;
pub mod openai_responses;
pub mod vertex;

pub use model_fetch::{
    DISABLE_CACHE_ENV, MODEL_CACHE_TTL, ModelCatalogSource, ProviderModelCatalog,
    ProviderModelCatalogFetchPlan, fetch_provider_model_catalog, fetch_provider_models,
    persist_provider_model_catalog, prepare_provider_model_catalog_fetch,
    refresh_provider_model_catalog, refresh_provider_models, static_registry_models,
};

pub(super) fn first_non_empty_header_value_case_insensitive(
    headers: &HashMap<String, String>,
    names: &[&str],
) -> Option<String> {
    headers.iter().find_map(|(key, value)| {
        names
            .iter()
            .any(|name| key.eq_ignore_ascii_case(name))
            .then_some(value.trim())
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

pub(super) fn apply_headers_ignoring_blank_auth_overrides<'a>(
    mut request: RequestBuilder<'a>,
    headers: &HashMap<String, String>,
    auth_names: &[&str],
) -> RequestBuilder<'a> {
    for (key, value) in headers {
        let is_blank_auth_override =
            auth_names.iter().any(|name| key.eq_ignore_ascii_case(name)) && value.trim().is_empty();
        if is_blank_auth_override {
            continue;
        }
        request = request.header(key, value);
    }
    request
}

fn base_url_targets_loopback(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    match url.host() {
        Some(url::Host::Domain("localhost")) => true,
        Some(url::Host::Ipv4(addr)) => addr.is_loopback(),
        Some(url::Host::Ipv6(addr)) => addr.is_loopback(),
        _ => false,
    }
}

fn vcr_client_if_enabled(base_url: &str) -> Result<Option<Client>> {
    if env::var(VCR_ENV_MODE).is_err() {
        return Ok(None);
    }

    if base_url_targets_loopback(base_url) && env::var("PI_VCR_ALLOW_LOOPBACK").is_err() {
        return Ok(None);
    }

    let test_name = env::var("PI_VCR_TEST_NAME").unwrap_or_else(|_| "pi_runtime".to_string());
    let recorder = VcrRecorder::new(&test_name)?;
    Ok(Some(Client::new().with_vcr(recorder)))
}

struct ExtensionStreamSimpleProvider {
    model: crate::provider::Model,
    runtime: ExtensionRuntimeHandle,
}

struct ExtensionStreamSimpleState {
    runtime: ExtensionRuntimeHandle,
    stream_id: Option<String>,
    model_id: String,
    provider: String,
    api: String,
    accumulated_text: String,
    last_message: Option<AssistantMessage>,
    /// Whether `StreamEvent::Start` + `TextStart` have been emitted for string-chunk mode.
    string_chunk_started: bool,
    /// Buffered events to drain before polling the next JS chunk.
    pending_events: std::collections::VecDeque<StreamEvent>,
}

impl Drop for ExtensionStreamSimpleState {
    fn drop(&mut self) {
        if let Some(stream_id) = self.stream_id.take() {
            self.runtime
                .provider_stream_simple_cancel_best_effort(stream_id);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderRouteKind {
    NativeAnthropic,
    NativeOpenAICompletions,
    NativeOpenAIResponses,
    NativeOpenAICodexResponses,
    NativeCohere,
    NativeCursor,
    NativeGoogle,
    NativeGoogleGeminiCli,
    NativeGoogleVertex,
    NativeBedrock,
    NativeAzure,
    NativeCopilot,
    NativeGitlab,
    ApiAnthropicMessages,
    ApiOpenAICompletions,
    ApiOpenAIResponses,
    ApiOpenAICodexResponses,
    ApiCohereChat,
    ApiGoogleGenerativeAi,
    ApiGoogleGeminiCli,
}

impl ProviderRouteKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NativeAnthropic => "native:anthropic",
            Self::NativeOpenAICompletions => "native:openai-completions",
            Self::NativeOpenAIResponses => "native:openai-responses",
            Self::NativeOpenAICodexResponses => "native:openai-codex-responses",
            Self::NativeCohere => "native:cohere",
            Self::NativeCursor => "native:cursor",
            Self::NativeGoogle => "native:google",
            Self::NativeGoogleGeminiCli => "native:google-gemini-cli",
            Self::NativeGoogleVertex => "native:google-vertex",
            Self::NativeBedrock => "native:amazon-bedrock",
            Self::NativeAzure => "native:azure-openai",
            Self::NativeCopilot => "native:github-copilot",
            Self::NativeGitlab => "native:gitlab",
            Self::ApiAnthropicMessages => "api:anthropic-messages",
            Self::ApiOpenAICompletions => "api:openai-completions",
            Self::ApiOpenAIResponses => "api:openai-responses",
            Self::ApiOpenAICodexResponses => "api:openai-codex-responses",
            Self::ApiCohereChat => "api:cohere-chat",
            Self::ApiGoogleGenerativeAi => "api:google-generative-ai",
            Self::ApiGoogleGeminiCli => "api:google-gemini-cli",
        }
    }
}

fn resolve_provider_route(entry: &ModelEntry) -> Result<(ProviderRouteKind, String, String)> {
    let canonical_provider =
        canonical_provider_id(&entry.model.provider).unwrap_or(entry.model.provider.as_str());
    let schema_api = provider_routing_defaults(&entry.model.provider).map(|defaults| defaults.api);
    let effective_api = if entry.model.api.is_empty() {
        schema_api.unwrap_or_default().to_string()
    } else {
        entry.model.api.clone()
    };

    let route = match canonical_provider {
        "anthropic" => ProviderRouteKind::NativeAnthropic,
        "openai" => {
            if effective_api == "openai-completions" {
                ProviderRouteKind::NativeOpenAICompletions
            } else {
                ProviderRouteKind::NativeOpenAIResponses
            }
        }
        "openai-codex" => ProviderRouteKind::NativeOpenAICodexResponses,
        "cohere" => ProviderRouteKind::NativeCohere,
        "cursor" => ProviderRouteKind::NativeCursor,
        "google" => ProviderRouteKind::NativeGoogle,
        "google-gemini-cli" | "google-antigravity" => ProviderRouteKind::NativeGoogleGeminiCli,
        "google-vertex" | "vertexai" => ProviderRouteKind::NativeGoogleVertex,
        "amazon-bedrock" | "bedrock" => ProviderRouteKind::NativeBedrock,
        "azure-openai" | "azure" | "azure-cognitive-services" | "azure-openai-responses" => {
            ProviderRouteKind::NativeAzure
        }
        "github-copilot" | "copilot" => ProviderRouteKind::NativeCopilot,
        "gitlab" | "gitlab-duo" => ProviderRouteKind::NativeGitlab,
        _ => match effective_api.as_str() {
            "anthropic-messages" => ProviderRouteKind::ApiAnthropicMessages,
            "openai-completions" => ProviderRouteKind::ApiOpenAICompletions,
            "openai-responses" => ProviderRouteKind::ApiOpenAIResponses,
            "openai-codex-responses" => ProviderRouteKind::ApiOpenAICodexResponses,
            "cohere-chat" => ProviderRouteKind::ApiCohereChat,
            "google-generative-ai" => ProviderRouteKind::ApiGoogleGenerativeAi,
            "google-gemini-cli" => ProviderRouteKind::ApiGoogleGeminiCli,
            "google-vertex" => ProviderRouteKind::NativeGoogleVertex,
            "bedrock-converse-stream" => ProviderRouteKind::NativeBedrock,
            "azure-openai-responses" => ProviderRouteKind::NativeAzure,
            _ => {
                let suggestions = suggest_similar_providers(&entry.model.provider);
                let msg = if suggestions.is_empty() {
                    format!("Provider not implemented (api: {effective_api})")
                } else {
                    format!(
                        "Provider not implemented (api: {effective_api}). Did you mean: {}?",
                        suggestions.join(", ")
                    )
                };
                return Err(Error::provider(&entry.model.provider, msg));
            }
        },
    };

    Ok((route, canonical_provider.to_string(), effective_api))
}

/// Levenshtein edit distance between two byte slices. Uses a single-row
/// buffer so memory is O(min(a,b)).
fn edit_distance(a: &[u8], b: &[u8]) -> usize {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut row: Vec<usize> = (0..=short.len()).collect();
    for (i, &lb) in long.iter().enumerate() {
        let mut prev = i;
        row[0] = i + 1;
        for (j, &sb) in short.iter().enumerate() {
            let cost = usize::from(lb != sb);
            let val = (row[j + 1] + 1).min(row[j] + 1).min(prev + cost);
            prev = row[j + 1];
            row[j + 1] = val;
        }
    }
    row[short.len()]
}

/// Maximum edit distance allowed for a fuzzy suggestion, scaled by the
/// length of the input so very short inputs don't produce false positives.
const fn max_edit_distance(input_len: usize) -> usize {
    match input_len {
        0..=2 => 0,
        3..=5 => 1,
        6..=9 => 2,
        _ => 3,
    }
}

/// Suggest provider names similar to `input` by checking prefix matching,
/// substring containment, and Levenshtein edit distance against all
/// canonical IDs and aliases.
fn suggest_similar_providers(input: &str) -> Vec<String> {
    let needle = input.to_lowercase();
    let needle_bytes = needle.as_bytes();
    let threshold = max_edit_distance(needle.len());
    let mut matches: Vec<(usize, String)> = Vec::new();

    for meta in PROVIDER_METADATA {
        let names: Vec<&str> = std::iter::once(meta.canonical_id)
            .chain(meta.aliases.iter().copied())
            .collect();
        let mut matched = false;
        for name in &names {
            let haystack = name.to_lowercase();
            // Tier 0: exact prefix match (highest quality)
            if haystack.starts_with(&needle) || needle.starts_with(&haystack) {
                matches.push((0, meta.canonical_id.to_string()));
                matched = true;
                break;
            }
            // Tier 1: substring containment
            if haystack.contains(&needle) || needle.contains(&haystack) {
                matches.push((1, meta.canonical_id.to_string()));
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        // Tier 2: edit distance (typo correction)
        if threshold > 0 {
            let mut best_dist = usize::MAX;
            for name in &names {
                let haystack = name.to_lowercase();
                let dist = edit_distance(needle_bytes, haystack.as_bytes());
                best_dist = best_dist.min(dist);
            }
            if best_dist <= threshold {
                // Encode distance in the sort key so closer matches rank higher
                matches.push((
                    2_usize.wrapping_add(best_dist),
                    meta.canonical_id.to_string(),
                ));
            }
        }
    }

    matches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    matches.dedup_by(|a, b| a.1 == b.1);
    matches.truncate(3);
    matches.into_iter().map(|(_, name)| name).collect()
}

const AZURE_OPENAI_RESOURCE_ENV: &str = "AZURE_OPENAI_RESOURCE";
const AZURE_OPENAI_DEPLOYMENT_ENV: &str = "AZURE_OPENAI_DEPLOYMENT";
const AZURE_OPENAI_API_VERSION_ENV: &str = "AZURE_OPENAI_API_VERSION";

#[derive(Debug, Clone, PartialEq, Eq)]
struct AzureProviderRuntime {
    resource: String,
    deployment: String,
    api_version: String,
    endpoint_url: String,
}

fn trim_non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parse_azure_resource_from_host(host: &str) -> Option<String> {
    host.strip_suffix(".openai.azure.com")
        .or_else(|| host.strip_suffix(".cognitiveservices.azure.com"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn parse_azure_base_url_details(
    base_url: &str,
) -> Result<(String, Option<String>, Option<String>)> {
    let url = Url::parse(base_url)
        .map_err(|err| Error::config(format!("Invalid Azure base_url '{base_url}': {err}")))?;
    let host = url.host_str().map(ToString::to_string).ok_or_else(|| {
        Error::config(format!(
            "Azure base_url is missing host information: '{base_url}'"
        ))
    })?;

    let mut deployment = None;
    if let Some(segments) = url.path_segments() {
        let mut iter = segments;
        while let Some(segment) = iter.next() {
            if segment == "deployments" {
                deployment = iter
                    .next()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string);
                break;
            }
        }
    }

    let api_version = url
        .query_pairs()
        .find(|(key, _)| key == "api-version")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.trim().is_empty());

    Ok((host, deployment, api_version))
}

fn resolve_azure_provider_runtime(entry: &ModelEntry) -> Result<AzureProviderRuntime> {
    resolve_azure_provider_runtime_with_env(entry, |name| env::var(name).ok())
}

fn resolve_azure_provider_runtime_with_env<F>(
    entry: &ModelEntry,
    mut env_lookup: F,
) -> Result<AzureProviderRuntime>
where
    F: FnMut(&str) -> Option<String>,
{
    let base_url = entry.model.base_url.trim();
    if base_url.is_empty() {
        return Err(Error::config(format!(
            "Missing Azure base_url for provider '{}'; expected https://<resource>.openai.azure.com or https://<resource>.cognitiveservices.azure.com",
            entry.model.provider
        )));
    }

    let (host, base_deployment, base_api_version) = parse_azure_base_url_details(base_url)?;
    let host_resource = parse_azure_resource_from_host(&host);
    let env_resource = trim_non_empty(env_lookup(AZURE_OPENAI_RESOURCE_ENV));
    let resource = env_resource.or(host_resource).ok_or_else(|| {
        Error::config(format!(
            "Unable to resolve Azure resource for provider '{}'; set {AZURE_OPENAI_RESOURCE_ENV} or use an Azure host in base_url ('{base_url}')",
            entry.model.provider
        ))
    })?;

    let env_deployment = trim_non_empty(env_lookup(AZURE_OPENAI_DEPLOYMENT_ENV));
    let model_deployment = {
        let model_id = entry.model.id.trim();
        (!model_id.is_empty()).then(|| model_id.to_string())
    };
    let deployment = env_deployment
        .or(base_deployment)
        .or(model_deployment)
        .ok_or_else(|| {
            Error::config(format!(
                "Unable to resolve Azure deployment for provider '{}'; set {AZURE_OPENAI_DEPLOYMENT_ENV}, provide a non-empty model id, or include '/deployments/<name>' in base_url ('{base_url}')",
                entry.model.provider
            ))
        })?;

    let api_version = trim_non_empty(env_lookup(AZURE_OPENAI_API_VERSION_ENV))
        .or(base_api_version)
        .unwrap_or_else(azure::azure_api_version);

    let endpoint_host = if parse_azure_resource_from_host(&host).is_some() {
        host
    } else {
        format!("{resource}.openai.azure.com")
    };
    let endpoint_url = format!(
        "https://{endpoint_host}/openai/deployments/{deployment}/chat/completions?api-version={api_version}"
    );

    Ok(AzureProviderRuntime {
        resource,
        deployment,
        api_version,
        endpoint_url,
    })
}

fn resolve_copilot_token(entry: &ModelEntry) -> Result<String> {
    resolve_copilot_token_with_env(entry, |name| env::var(name).ok())
}

fn resolve_copilot_token_with_env<F>(entry: &ModelEntry, mut env_lookup: F) -> Result<String>
where
    F: FnMut(&str) -> Option<String>,
{
    let inline = entry
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let from_env = || {
        env_lookup("GITHUB_COPILOT_API_KEY")
            .or_else(|| env_lookup("GITHUB_TOKEN"))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };

    inline.or_else(from_env).ok_or_else(|| {
        Error::auth(
            "GitHub Copilot requires login credentials or GITHUB_COPILOT_API_KEY/GITHUB_TOKEN",
        )
    })
}

impl ExtensionStreamSimpleProvider {
    const NEXT_TIMEOUT_MS: u64 = 600_000;

    const fn new(model: crate::provider::Model, runtime: ExtensionRuntimeHandle) -> Self {
        Self { model, runtime }
    }

    fn build_js_model(model: &crate::provider::Model) -> Value {
        serde_json::json!({
            "id": &model.id,
            "name": &model.name,
            "api": &model.api,
            "provider": &model.provider,
            "baseUrl": &model.base_url,
            "reasoning": model.reasoning,
            "input": &model.input,
            "cost": &model.cost,
            "contextWindow": model.context_window,
            "maxTokens": model.max_tokens,
            "headers": &model.headers,
        })
    }

    fn build_js_context(context: &Context<'_>) -> Value {
        let mut map = serde_json::Map::new();
        if let Some(system_prompt) = &context.system_prompt {
            map.insert(
                "systemPrompt".to_string(),
                Value::String(system_prompt.to_string()),
            );
        }
        map.insert(
            "messages".to_string(),
            serde_json::to_value(&context.messages).unwrap_or(Value::Array(Vec::new())),
        );
        if !context.tools.is_empty() {
            let tools = context
                .tools
                .iter()
                .map(|tool| {
                    serde_json::json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    })
                })
                .collect::<Vec<_>>();
            map.insert("tools".to_string(), Value::Array(tools));
        }
        Value::Object(map)
    }

    fn build_js_options(options: &StreamOptions) -> Value {
        let mut map = serde_json::Map::new();
        if let Some(temp) = options.temperature {
            map.insert("temperature".to_string(), serde_json::json!(temp));
        }
        if let Some(max_tokens) = options.max_tokens {
            map.insert("maxTokens".to_string(), serde_json::json!(max_tokens));
        }
        if let Some(api_key) = &options.api_key {
            map.insert("apiKey".to_string(), Value::String(api_key.clone()));
        }
        if let Some(session_id) = &options.session_id {
            map.insert("sessionId".to_string(), Value::String(session_id.clone()));
        }
        if !options.headers.is_empty() {
            map.insert(
                "headers".to_string(),
                serde_json::to_value(&options.headers)
                    .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
            );
        }
        let cache_retention = match options.cache_retention {
            crate::provider::CacheRetention::None => "none",
            crate::provider::CacheRetention::Short => "short",
            crate::provider::CacheRetention::Long => "long",
        };
        map.insert(
            "cacheRetention".to_string(),
            Value::String(cache_retention.to_string()),
        );
        if let Some(level) = options.thinking_level
            && level != crate::model::ThinkingLevel::Off
        {
            map.insert("reasoning".to_string(), Value::String(level.to_string()));
        }
        if let Some(budgets) = &options.thinking_budgets {
            map.insert(
                "thinkingBudgets".to_string(),
                serde_json::json!({
                    "minimal": budgets.minimal,
                    "low": budgets.low,
                    "medium": budgets.medium,
                    "high": budgets.high,
                    "xhigh": budgets.xhigh,
                    "max": budgets.max,
                }),
            );
        }
        Value::Object(map)
    }

    fn assistant_event_to_stream_event(event: AssistantMessageEvent) -> StreamEvent {
        match event {
            AssistantMessageEvent::Start { partial } => StreamEvent::Start {
                partial: partial.as_ref().clone(),
            },
            AssistantMessageEvent::TextStart { content_index, .. } => {
                StreamEvent::TextStart { content_index }
            }
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                ..
            } => StreamEvent::TextDelta {
                content_index,
                delta,
            },
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                ..
            } => StreamEvent::TextEnd {
                content_index,
                content,
            },
            AssistantMessageEvent::ThinkingStart { content_index, .. } => {
                StreamEvent::ThinkingStart { content_index }
            }
            AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                ..
            } => StreamEvent::ThinkingDelta {
                content_index,
                delta,
            },
            AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                ..
            } => StreamEvent::ThinkingEnd {
                content_index,
                content,
            },
            AssistantMessageEvent::ToolCallStart {
                content_index,
                partial,
            } => {
                // #129: recover the tool-call id/name from the partial so the
                // reconstructed stream stays correlatable from the start.
                let (id, name) = match partial.content.get(content_index) {
                    Some(crate::model::ContentBlock::ToolCall(tc)) => {
                        (tc.id.clone(), tc.name.clone())
                    }
                    _ => (String::new(), String::new()),
                };
                StreamEvent::ToolCallStart {
                    content_index,
                    id,
                    name,
                }
            }
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                ..
            } => StreamEvent::ToolCallDelta {
                content_index,
                delta,
            },
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                ..
            } => StreamEvent::ToolCallEnd {
                content_index,
                tool_call,
            },
            AssistantMessageEvent::Done { reason, message } => StreamEvent::Done {
                reason,
                message: message.as_ref().clone(),
            },
            AssistantMessageEvent::Error { reason, error } => StreamEvent::Error {
                reason,
                error: error.as_ref().clone(),
            },
        }
    }

    fn make_partial(model_id: &str, provider: &str, api: &str, text: &str) -> AssistantMessage {
        AssistantMessage {
            model: model_id.to_string(),
            api: api.to_string(),
            provider: provider.to_string(),
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            stop_reason: StopReason::default(),
            usage: Usage::default(),
            stop_details: None,
            error_message: None,
            timestamp: Utc::now().timestamp_millis(),
        }
    }
}

#[allow(clippy::too_many_lines)]
#[async_trait]
impl Provider for ExtensionStreamSimpleProvider {
    #[allow(clippy::misnamed_getters)]
    fn name(&self) -> &str {
        &self.model.provider
    }

    fn api(&self) -> &str {
        &self.model.api
    }

    fn model_id(&self) -> &str {
        &self.model.id
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let model = Self::build_js_model(&self.model);
        let ctx = Self::build_js_context(context);
        let opts = Self::build_js_options(options);

        let stream_id = self
            .runtime
            .provider_stream_simple_start(
                self.model.provider.clone(),
                model,
                ctx,
                opts,
                Self::NEXT_TIMEOUT_MS,
            )
            .await?;

        let state = ExtensionStreamSimpleState {
            runtime: self.runtime.clone(),
            stream_id: Some(stream_id),
            model_id: self.model.id.clone(),
            provider: self.model.provider.clone(),
            api: self.model.api.clone(),
            accumulated_text: String::new(),
            last_message: None,
            string_chunk_started: false,
            pending_events: std::collections::VecDeque::new(),
        };

        let stream = stream::unfold(state, |mut state| async move {
            // Drain any buffered events before polling JS.
            if let Some(event) = state.pending_events.pop_front() {
                return Some((Ok(event), state));
            }

            let stream_id = state.stream_id.clone()?;
            let stream_id_for_cancel = stream_id.clone();

            match state
                .runtime
                .provider_stream_simple_next(stream_id, Self::NEXT_TIMEOUT_MS)
                .await
            {
                Ok(Some(value)) => {
                    if let Some(chunk) = value.as_str() {
                        let chunk = chunk.to_string();
                        state.accumulated_text.push_str(&chunk);
                        // Update last_message in-place: mutate existing text
                        // content instead of rebuilding the entire
                        // AssistantMessage (avoids 3 String + Vec allocs per
                        // chunk).
                        match &mut state.last_message {
                            Some(msg) => {
                                if let Some(ContentBlock::Text(t)) = msg.content.first_mut() {
                                    t.text.clone_from(&state.accumulated_text);
                                }
                            }
                            None => {
                                state.last_message = Some(Self::make_partial(
                                    &state.model_id,
                                    &state.provider,
                                    &state.api,
                                    &state.accumulated_text,
                                ));
                            }
                        }

                        // Emit Start + TextStart before first string-chunk TextDelta.
                        if !state.string_chunk_started {
                            state.string_chunk_started = true;
                            state
                                .pending_events
                                .push_back(StreamEvent::TextStart { content_index: 0 });
                            state.pending_events.push_back(StreamEvent::TextDelta {
                                content_index: 0,
                                delta: chunk,
                            });
                            // Raw string mode still streams deltas chunk-by-chunk, so the
                            // synthetic Start event must begin empty. Otherwise the agent
                            // seeds the partial with the first chunk and then appends that
                            // same first delta again.
                            return Some((
                                Ok(StreamEvent::Start {
                                    partial: Self::make_partial(
                                        &state.model_id,
                                        &state.provider,
                                        &state.api,
                                        "",
                                    ),
                                }),
                                state,
                            ));
                        }
                        return Some((
                            Ok(StreamEvent::TextDelta {
                                content_index: 0,
                                delta: chunk,
                            }),
                            state,
                        ));
                    }

                    let event: AssistantMessageEvent = match serde_json::from_value(value) {
                        Ok(event) => event,
                        Err(err) => {
                            state
                                .runtime
                                .provider_stream_simple_cancel_best_effort(stream_id_for_cancel);
                            state.stream_id = None;
                            return Some((
                                Err(Error::extension(format!(
                                    "streamSimple yielded invalid event: {err}"
                                ))),
                                state,
                            ));
                        }
                    };

                    match &event {
                        AssistantMessageEvent::Start { partial }
                        | AssistantMessageEvent::TextStart { partial, .. }
                        | AssistantMessageEvent::TextDelta { partial, .. }
                        | AssistantMessageEvent::TextEnd { partial, .. }
                        | AssistantMessageEvent::ThinkingStart { partial, .. }
                        | AssistantMessageEvent::ThinkingDelta { partial, .. }
                        | AssistantMessageEvent::ThinkingEnd { partial, .. }
                        | AssistantMessageEvent::ToolCallStart { partial, .. }
                        | AssistantMessageEvent::ToolCallDelta { partial, .. }
                        | AssistantMessageEvent::ToolCallEnd { partial, .. } => {
                            state.last_message = Some(partial.as_ref().clone());
                        }
                        AssistantMessageEvent::Done { message, .. } => {
                            state.last_message = Some(message.as_ref().clone());
                        }
                        AssistantMessageEvent::Error { error, .. } => {
                            state.last_message = Some(error.as_ref().clone());
                        }
                    }

                    let stream_event = Self::assistant_event_to_stream_event(event);
                    if matches!(
                        stream_event,
                        StreamEvent::Done { .. } | StreamEvent::Error { .. }
                    ) {
                        state
                            .runtime
                            .provider_stream_simple_cancel_best_effort(stream_id_for_cancel);
                        state.stream_id = None;
                    }
                    Some((Ok(stream_event), state))
                }
                Ok(None) => {
                    // Stream ended — emit TextEnd (if string chunks were used) then Done.
                    state.stream_id = None;
                    let message = state.last_message.clone().unwrap_or_else(|| {
                        Self::make_partial(
                            &state.model_id,
                            &state.provider,
                            &state.api,
                            &state.accumulated_text,
                        )
                    });

                    if state.string_chunk_started {
                        // Emit TextEnd before Done.
                        state.pending_events.push_back(StreamEvent::Done {
                            reason: StopReason::Stop,
                            message,
                        });
                        Some((
                            Ok(StreamEvent::TextEnd {
                                content_index: 0,
                                content: state.accumulated_text.clone(),
                            }),
                            state,
                        ))
                    } else {
                        Some((
                            Ok(StreamEvent::Done {
                                reason: StopReason::Stop,
                                message,
                            }),
                            state,
                        ))
                    }
                }
                Err(err) => {
                    state
                        .runtime
                        .provider_stream_simple_cancel_best_effort(stream_id_for_cancel);
                    state.stream_id = None;
                    Some((Err(err), state))
                }
            }
        });

        Ok(Box::pin(stream))
    }
}

#[allow(clippy::too_many_lines)]
pub fn create_provider(
    entry: &ModelEntry,
    extensions: Option<&ExtensionManager>,
) -> Result<Arc<dyn Provider>> {
    if let Some(manager) = extensions
        && manager.provider_has_stream_simple(&entry.model.provider)
    {
        let runtime = manager.runtime().ok_or_else(|| {
            Error::provider(
                &entry.model.provider,
                "Extension runtime not configured for streamSimple provider",
            )
        })?;
        return Ok(Arc::new(ExtensionStreamSimpleProvider::new(
            entry.model.clone(),
            runtime,
        )));
    }

    let (route, canonical_provider, effective_api) = resolve_provider_route(entry)?;
    let vcr_client = vcr_client_if_enabled(&entry.model.base_url)?;
    let client = vcr_client.unwrap_or_else(Client::new);
    tracing::debug!(
        event = "pi.provider.factory.select",
        provider = %entry.model.provider,
        canonical_provider = %canonical_provider,
        api = %effective_api,
        base_url = %entry.model.base_url,
        route = %route.as_str(),
        "Selecting provider implementation"
    );

    match route {
        ProviderRouteKind::NativeAnthropic | ProviderRouteKind::ApiAnthropicMessages => {
            Ok(Arc::new(
                anthropic::AnthropicProvider::new(entry.model.id.clone())
                    .with_provider_name(entry.model.provider.clone())
                    .with_base_url(normalize_anthropic_base(&entry.model.base_url))
                    .with_compat(entry.compat.clone())
                    .with_client(client),
            ))
        }
        ProviderRouteKind::NativeOpenAICompletions | ProviderRouteKind::ApiOpenAICompletions => {
            Ok(Arc::new(
                openai::OpenAIProvider::new(entry.model.id.clone())
                    .with_provider_name(entry.model.provider.clone())
                    .with_base_url(normalize_openai_base(&entry.model.base_url))
                    .with_compat(entry.compat.clone())
                    .with_auth_header(entry.auth_header)
                    .with_reasoning(entry.model.reasoning)
                    .with_client(client),
            ))
        }
        ProviderRouteKind::NativeOpenAIResponses | ProviderRouteKind::ApiOpenAIResponses => {
            Ok(Arc::new(
                openai_responses::OpenAIResponsesProvider::new(entry.model.id.clone())
                    .with_provider_name(entry.model.provider.clone())
                    .with_base_url(normalize_openai_responses_base(&entry.model.base_url))
                    .with_compat(entry.compat.clone())
                    .with_auth_header(entry.auth_header)
                    .with_client(client),
            ))
        }
        ProviderRouteKind::NativeOpenAICodexResponses
        | ProviderRouteKind::ApiOpenAICodexResponses => Ok(Arc::new(
            openai_responses::OpenAIResponsesProvider::new(entry.model.id.clone())
                .with_provider_name(entry.model.provider.clone())
                .with_api_name("openai-codex-responses")
                .with_codex_mode(true)
                .with_base_url(normalize_openai_codex_responses_base(&entry.model.base_url))
                .with_compat(entry.compat.clone())
                .with_auth_header(entry.auth_header)
                .with_client(client),
        )),
        ProviderRouteKind::NativeCursor => Ok(Arc::new(
            cursor::CursorProvider::new(entry.model.id.clone())
                .with_provider_name(entry.model.provider.clone())
                .with_base_url(normalize_cursor_base(&entry.model.base_url))
                .with_client(client),
        )),
        ProviderRouteKind::NativeCohere | ProviderRouteKind::ApiCohereChat => Ok(Arc::new(
            cohere::CohereProvider::new(entry.model.id.clone())
                .with_provider_name(entry.model.provider.clone())
                .with_base_url(normalize_cohere_base(&entry.model.base_url))
                .with_compat(entry.compat.clone())
                .with_client(client),
        )),
        ProviderRouteKind::NativeGoogle | ProviderRouteKind::ApiGoogleGenerativeAi => Ok(Arc::new(
            gemini::GeminiProvider::new(entry.model.id.clone())
                .with_provider_name(entry.model.provider.clone())
                .with_api_name("google-generative-ai")
                .with_base_url(entry.model.base_url.clone())
                .with_compat(entry.compat.clone())
                .with_client(client),
        )),
        ProviderRouteKind::NativeGoogleGeminiCli | ProviderRouteKind::ApiGoogleGeminiCli => {
            Ok(Arc::new(
                gemini::GeminiProvider::new(entry.model.id.clone())
                    .with_provider_name(entry.model.provider.clone())
                    .with_api_name("google-gemini-cli")
                    .with_google_cli_mode(true)
                    .with_base_url(entry.model.base_url.clone())
                    .with_compat(entry.compat.clone())
                    .with_client(client),
            ))
        }
        ProviderRouteKind::NativeGoogleVertex => {
            let runtime = vertex::resolve_vertex_provider_runtime(entry)?;
            Ok(Arc::new(
                vertex::VertexProvider::new(runtime.model)
                    .with_project(runtime.project)
                    .with_location(runtime.location)
                    .with_publisher(runtime.publisher)
                    .with_compat(entry.compat.clone())
                    .with_client(client),
            ))
        }
        ProviderRouteKind::NativeBedrock => Ok(Arc::new(
            bedrock::BedrockProvider::new(&entry.model.id)
                .with_provider_name(&entry.model.provider)
                .with_base_url(&entry.model.base_url)
                .with_compat(entry.compat.clone())
                .with_client(client),
        )),
        ProviderRouteKind::NativeAzure => {
            let runtime = resolve_azure_provider_runtime(entry)?;
            Ok(Arc::new(
                azure::AzureOpenAIProvider::new(runtime.resource, runtime.deployment)
                    .with_provider_name(&entry.model.provider)
                    .with_api_version(runtime.api_version)
                    .with_endpoint_url(runtime.endpoint_url)
                    .with_compat(entry.compat.clone())
                    .with_client(client),
            ))
        }
        ProviderRouteKind::NativeCopilot => {
            let github_token = resolve_copilot_token(entry)?;
            let mut provider = copilot::CopilotProvider::new(&entry.model.id, github_token)
                .with_provider_name(&entry.model.provider)
                .with_compat(entry.compat.clone())
                .with_client(client);
            if !entry.model.base_url.is_empty() {
                provider = provider.with_github_api_base(&entry.model.base_url);
            }
            Ok(Arc::new(provider))
        }
        ProviderRouteKind::NativeGitlab => Ok(Arc::new(
            gitlab::GitLabProvider::new(&entry.model.id)
                .with_provider_name(&entry.model.provider)
                .with_base_url(&entry.model.base_url)
                .with_compat(entry.compat.clone())
                .with_client(client),
        )),
    }
}

pub fn normalize_anthropic_base(base_url: &str) -> String {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return "https://api.anthropic.com/v1/messages".to_string();
    }

    let mut base_for_fallback = trimmed.trim_end_matches('/').to_string();

    if let Ok(url) = Url::parse(trimmed) {
        if url.cannot_be_a_base() {
            base_for_fallback = url.as_str().trim_end_matches('/').to_string();
        } else {
            if trimmed_url_path(&url).ends_with("/v1/messages") {
                return canonicalize_url_path(&url);
            }
            return append_url_path(&url, "v1/messages");
        }
    }

    let base_url = base_for_fallback;
    if base_url.ends_with("/v1/messages") {
        return base_url;
    }
    format!("{base_url}/v1/messages")
}

fn trimmed_url_path(url: &Url) -> &str {
    match url.path().trim_end_matches('/') {
        "" => "/",
        trimmed => trimmed,
    }
}

fn canonicalize_url_path(url: &Url) -> String {
    let mut canonical = url.clone();
    canonical.set_path(trimmed_url_path(url));
    canonical.to_string()
}

fn replace_url_path(url: &Url, path: &str) -> String {
    let mut updated = url.clone();
    updated.set_path(path);
    updated.to_string()
}

fn append_url_path(url: &Url, suffix: &str) -> String {
    let base_path = trimmed_url_path(url);
    let path = if base_path == "/" {
        format!("/{suffix}")
    } else {
        format!("{base_path}/{suffix}")
    };
    replace_url_path(url, &path)
}

fn strip_url_path_suffix(url: &Url, suffix: &str) -> Option<Url> {
    let base_path = trimmed_url_path(url);
    let prefix = base_path.strip_suffix(suffix)?;
    let mut stripped = url.clone();
    stripped.set_path(if prefix.is_empty() { "/" } else { prefix });
    Some(stripped)
}

fn is_official_https_origin(url: &Url, host: &str, default_port: u16) -> bool {
    url.scheme().eq_ignore_ascii_case("https")
        && url
            .host_str()
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(host))
        && url.port_or_known_default() == Some(default_port)
        && trimmed_url_path(url) == "/"
}

pub fn normalize_openai_base(base_url: &str) -> String {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return "https://api.openai.com/v1/chat/completions".to_string();
    }

    let mut base_for_fallback = trimmed.trim_end_matches('/').to_string();

    if let Ok(url) = Url::parse(trimmed) {
        if url.cannot_be_a_base() {
            base_for_fallback = url.as_str().trim_end_matches('/').to_string();
        } else {
            if trimmed_url_path(&url).ends_with("/chat/completions") {
                return canonicalize_url_path(&url);
            }
            let url = strip_url_path_suffix(&url, "/responses").unwrap_or(url);
            if is_official_https_origin(&url, "api.openai.com", 443) {
                return replace_url_path(&url, "/v1/chat/completions");
            }
            return append_url_path(&url, "chat/completions");
        }
    }

    let base_url = base_for_fallback;
    if base_url.ends_with("/chat/completions") {
        return base_url;
    }
    let base_url = base_url
        .strip_suffix("/responses")
        .unwrap_or(base_url.as_str());
    format!("{base_url}/chat/completions")
}

pub fn normalize_openai_responses_base(base_url: &str) -> String {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return "https://api.openai.com/v1/responses".to_string();
    }

    let mut base_for_fallback = trimmed.trim_end_matches('/').to_string();

    if let Ok(url) = Url::parse(trimmed) {
        if url.cannot_be_a_base() {
            base_for_fallback = url.as_str().trim_end_matches('/').to_string();
        } else {
            if trimmed_url_path(&url).ends_with("/responses") {
                return canonicalize_url_path(&url);
            }
            let url = strip_url_path_suffix(&url, "/chat/completions").unwrap_or(url);
            if is_official_https_origin(&url, "api.openai.com", 443) {
                return replace_url_path(&url, "/v1/responses");
            }
            return append_url_path(&url, "responses");
        }
    }

    let base_url = base_for_fallback;
    if base_url.ends_with("/responses") {
        return base_url;
    }
    let base_url = base_url
        .strip_suffix("/chat/completions")
        .unwrap_or(base_url.as_str());
    format!("{base_url}/responses")
}

pub fn normalize_openai_codex_responses_base(base_url: &str) -> String {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return openai_responses::CODEX_RESPONSES_API_URL.to_string();
    }

    let mut base_for_fallback = trimmed.trim_end_matches('/').to_string();

    if let Ok(url) = Url::parse(trimmed) {
        if url.cannot_be_a_base() {
            base_for_fallback = url.as_str().trim_end_matches('/').to_string();
        } else {
            let path = trimmed_url_path(&url);
            if path.ends_with("/backend-api/codex/responses") || path.ends_with("/responses") {
                return canonicalize_url_path(&url);
            }
            if path.ends_with("/backend-api") {
                return append_url_path(&url, "codex/responses");
            }
            return append_url_path(&url, "backend-api/codex/responses");
        }
    }

    let base = base_for_fallback;
    if base.ends_with("/backend-api/codex/responses") {
        return base;
    }
    // Some registries (including legacy Pi) store the ChatGPT base as
    // `https://chatgpt.com/backend-api`. In that case we only want to append
    // `/codex/responses`, not `/backend-api/codex/responses` again.
    if base.ends_with("/backend-api") {
        return format!("{base}/codex/responses");
    }
    if base.ends_with("/responses") {
        return base;
    }
    format!("{base}/backend-api/codex/responses")
}

pub fn normalize_cohere_base(base_url: &str) -> String {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return "https://api.cohere.com/v2/chat".to_string();
    }

    let mut base_for_fallback = trimmed.trim_end_matches('/').to_string();

    if let Ok(url) = Url::parse(trimmed) {
        if url.cannot_be_a_base() {
            base_for_fallback = url.as_str().trim_end_matches('/').to_string();
        } else {
            if trimmed_url_path(&url).ends_with("/chat") {
                return canonicalize_url_path(&url);
            }
            if is_official_https_origin(&url, "api.cohere.com", 443) {
                return replace_url_path(&url, "/v2/chat");
            }
            return append_url_path(&url, "chat");
        }
    }

    let base_url = base_for_fallback;
    if base_url.ends_with("/chat") {
        return base_url;
    }
    format!("{base_url}/chat")
}

/// Normalize a Cursor base URL into the full Connect RPC endpoint.
///
/// Accepts an empty string (uses the default), a bare origin such as
/// `https://api2.cursor.sh` (appends the RPC path), or a full path already
/// ending in `/agent.v1.AgentService/Run`.
pub fn normalize_cursor_base(base_url: &str) -> String {
    const RPC_PATH: &str = "agent.v1.AgentService/Run";
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return cursor::CURSOR_API_URL.to_string();
    }

    if let Ok(url) = Url::parse(trimmed)
        && !url.cannot_be_a_base()
    {
        if trimmed_url_path(&url).ends_with(&format!("/{RPC_PATH}")) {
            return canonicalize_url_path(&url);
        }
        return append_url_path(&url, RPC_PATH);
    }

    let base = trimmed.trim_end_matches('/');
    if base.ends_with(RPC_PATH) {
        return base.to_string();
    }
    format!("{base}/{RPC_PATH}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
    use crate::extensions_js::PiJsRuntimeConfig;
    use crate::model::{ContentBlock, Message, UserContent, UserMessage};
    use crate::tools::ToolRegistry;
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::time::{sleep, wall_now};
    use futures::StreamExt;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn vcr_loopback_detection_covers_provider_factory_mock_hosts() {
        assert!(base_url_targets_loopback("http://127.0.0.1:45713/v1"));
        assert!(base_url_targets_loopback("http://localhost:45713/v1"));
        assert!(base_url_targets_loopback("http://[::1]:45713/v1"));
        assert!(base_url_targets_loopback("http://127.12.34.56:45713/v1"));
        assert!(!base_url_targets_loopback("https://api.openai.com/v1"));
        assert!(!base_url_targets_loopback("http://127.example.com/v1"));
        assert!(!base_url_targets_loopback("not a url"));
    }

    const STREAM_SIMPLE_EXTENSION: &str = r#"
export default function init(pi) {
  pi.registerProvider("stream-provider", {
    baseUrl: "https://api.example.test",
    apiKey: "EXAMPLE_KEY",
    api: "custom-api",
    models: [
      { id: "stream-model", name: "Stream Model", contextWindow: 100, maxTokens: 10, input: ["text"] }
    ],
    streamSimple: async function* (model, context, options) {
      if (!model || !model.baseUrl || !model.maxTokens || !model.contextWindow) {
        throw new Error("bad model shape");
      }
      if (!context || !Array.isArray(context.messages)) {
        throw new Error("bad context shape");
      }
      if (!options || !options.signal) {
        throw new Error("missing abort signal");
      }

      const partial = {
        role: "assistant",
        content: [{ type: "text", text: "" }],
        api: model.api,
        provider: model.provider,
        model: model.id,
        usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
        stopReason: "stop",
        timestamp: 0
      };

      yield { type: "start", partial };
      yield { type: "text_start", contentIndex: 0, partial };
      partial.content[0].text += "hi";
      yield { type: "text_delta", contentIndex: 0, delta: "hi", partial };
      yield { type: "done", reason: "stop", message: partial };
    }
  });
}
"#;

    const STREAM_SIMPLE_CANCEL_EXTENSION: &str = r#"
export default function init(pi) {
  pi.registerProvider("cancel-provider", {
    baseUrl: "https://api.example.test",
    apiKey: "EXAMPLE_KEY",
    api: "custom-api",
    models: [
      { id: "cancel-model", name: "Cancel Model", contextWindow: 100, maxTokens: 10, input: ["text"] }
    ],
    streamSimple: async function* (model, context, options) {
      const partial = {
        role: "assistant",
        content: [{ type: "text", text: "" }],
        api: model.api,
        provider: model.provider,
        model: model.id,
        usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
        stopReason: "stop",
        timestamp: 0
      };

      try {
        yield { type: "start", partial };
        await new Promise((resolve) => {
          if (options && options.signal && options.signal.aborted) return resolve();
          if (options && options.signal && typeof options.signal.addEventListener === "function") {
            options.signal.addEventListener("abort", () => resolve());
          }
        });
      } finally {
        await pi.tool("write", { path: "cancelled.txt", content: "ok" });
      }
    }
  });
}
"#;

    const API_REGISTERED_PROVIDER_EXTENSION: &str = r#"
import { createAssistantMessageEventStream, registerApiProvider } from "@earendil-works/pi-ai/compat";

registerApiProvider({
  api: "extension-fixture-api",
  stream: (model, context, options) => {
    const events = createAssistantMessageEventStream();
    events.push({ type: "done", message: { role: "assistant", content: [], api: model.api, provider: model.provider, model: model.id, stopReason: "stop" } });
    return events;
  },
  streamSimple: (model, context, options) => {
    const partial = {
      role: "assistant",
      content: [{ type: "text", text: "" }],
      api: model.api,
      provider: model.provider,
      model: model.id,
      usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
      stopReason: "stop",
      timestamp: 0
    };
    const events = createAssistantMessageEventStream();
    events.push({ type: "start", partial });
    events.push({ type: "text_start", contentIndex: 0, partial });
    partial.content[0].text = "via-api-registry";
    events.push({ type: "text_delta", contentIndex: 0, delta: "via-api-registry", partial });
    events.push({ type: "done", reason: "stop", message: partial });
    return events;
  },
}, "api-registration-fixture");

export default function init(pi) {
  pi.registerProvider("api-registered-provider", {
    api: "extension-fixture-api",
    models: [{ id: "api-registered-model", name: "API Registered Model", contextWindow: 100, maxTokens: 10, input: ["text"] }]
  });
}
"#;

    async fn load_extension(
        source: &str,
        allow_write: bool,
    ) -> (tempfile::TempDir, ExtensionManager) {
        let dir = tempdir().expect("tempdir");
        let entry_path = dir.path().join("ext.mjs");
        std::fs::write(&entry_path, source).expect("write extension");

        let manager = ExtensionManager::new();
        let tools = if allow_write {
            Arc::new(ToolRegistry::new(&["write"], dir.path(), None))
        } else {
            Arc::new(ToolRegistry::new(&[], dir.path(), None))
        };

        let js_runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: dir.path().display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await
        .expect("start js runtime");
        manager.set_js_runtime(js_runtime);

        let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("load spec");
        manager
            .load_js_extensions(vec![spec])
            .await
            .expect("load extension");

        (dir, manager)
    }

    fn basic_context() -> Context<'static> {
        Context {
            system_prompt: Some("system".to_string().into()),
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        }
    }

    fn basic_options() -> StreamOptions {
        StreamOptions {
            api_key: Some("sk-test".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn extension_stream_simple_provider_emits_assistant_events() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let (_dir, manager) = load_extension(STREAM_SIMPLE_EXTENSION, false).await;
            let entries = manager.extension_model_entries();
            assert_eq!(entries.len(), 1);
            let entry = entries
                .iter()
                .find(|e| e.model.provider == "stream-provider")
                .expect("stream-provider entry");

            let provider = create_provider(entry, Some(&manager)).expect("create provider");
            assert_eq!(provider.name(), "stream-provider");

            let ctx = basic_context();
            let opts = basic_options();
            let mut stream = provider.stream(&ctx, &opts).await.expect("stream");

            let mut saw_start = false;
            let mut saw_text_delta = false;
            while let Some(item) = stream.next().await {
                let event = item.expect("stream event");
                match event {
                    StreamEvent::Start { .. } => {
                        saw_start = true;
                    }
                    StreamEvent::TextDelta { delta, .. } => {
                        assert_eq!(delta, "hi");
                        saw_text_delta = true;
                    }
                    StreamEvent::Done { reason, message } => {
                        assert_eq!(reason, StopReason::Stop);
                        let text = match &message.content[0] {
                            ContentBlock::Text(text) => text,
                            other => unreachable!("expected text content block, got {other:?}"),
                        };
                        assert_eq!(text.text, "hi");
                        break;
                    }
                    _ => {}
                }
            }

            assert!(saw_start, "expected a Start event");
            assert!(saw_text_delta, "expected a TextDelta event");
        });
    }

    #[test]
    fn extension_api_provider_registration_routes_models_through_registered_handler() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let (_dir, manager) = load_extension(API_REGISTERED_PROVIDER_EXTENSION, false).await;
            assert!(manager.provider_has_stream_simple("api-registered-provider"));

            let entries = manager.extension_model_entries();
            assert_eq!(entries.len(), 1);
            let entry = entries
                .iter()
                .find(|entry| entry.model.provider == "api-registered-provider")
                .expect("registered provider model entry");
            assert_eq!(entry.model.api, "extension-fixture-api");

            let provider = create_provider(entry, Some(&manager)).expect("create provider");
            let context = basic_context();
            let options = basic_options();
            let mut stream = provider.stream(&context, &options).await.expect("stream");

            let mut text = String::new();
            while let Some(event) = stream.next().await {
                if let StreamEvent::TextDelta { delta, .. } = event.expect("stream event") {
                    text.push_str(&delta);
                }
            }
            assert_eq!(text, "via-api-registry");
        });
    }

    #[test]
    fn extension_stream_simple_provider_drop_cancels_js_stream() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let (dir, manager) = load_extension(STREAM_SIMPLE_CANCEL_EXTENSION, true).await;
            let entries = manager.extension_model_entries();
            assert_eq!(entries.len(), 1);
            let entry = entries
                .iter()
                .find(|e| e.model.provider == "cancel-provider")
                .expect("cancel-provider entry");

            let provider = create_provider(entry, Some(&manager)).expect("create provider");
            let ctx = basic_context();
            let opts = basic_options();
            let mut stream = provider.stream(&ctx, &opts).await.expect("stream");

            let first = stream.next().await.expect("first event");
            let _ = first.expect("first event ok");
            drop(stream);

            let out_path = dir.path().join("cancelled.txt");
            for _ in 0..200 {
                if out_path.exists() {
                    let contents = std::fs::read_to_string(&out_path).expect("read cancelled.txt");
                    assert_eq!(contents, "ok");
                    return;
                }
                sleep(wall_now(), Duration::from_millis(5)).await;
            }

            assert!(
                out_path.exists(),
                "expected cancelled.txt to be created after stream drop/cancel"
            );
        });
    }

    // ========================================================================
    // Additional tests for bd-izzp
    // ========================================================================

    const STREAM_SIMPLE_MULTI_CHUNK: &str = r#"
export default function init(pi) {
  pi.registerProvider("multi-chunk-provider", {
    baseUrl: "https://api.example.test",
    apiKey: "EXAMPLE_KEY",
    api: "custom-api",
    models: [
      { id: "multi-model", name: "Multi Model", contextWindow: 100, maxTokens: 10, input: ["text"] }
    ],
    streamSimple: async function* (model, context, options) {
      const partial = {
        role: "assistant",
        content: [{ type: "text", text: "" }],
        api: model.api,
        provider: model.provider,
        model: model.id,
        usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
        stopReason: "stop",
        timestamp: 0
      };

      yield { type: "start", partial };
      yield { type: "text_start", contentIndex: 0, partial };

      const chunks = ["Hello", ", ", "world", "!"];
      for (const chunk of chunks) {
        partial.content[0].text += chunk;
        yield { type: "text_delta", contentIndex: 0, delta: chunk, partial };
      }

      yield { type: "text_end", contentIndex: 0, content: partial.content[0].text, partial };
      yield { type: "done", reason: "stop", message: partial };
    }
  });
}
"#;

    const STREAM_SIMPLE_ERROR: &str = r#"
export default function init(pi) {
  pi.registerProvider("error-provider", {
    baseUrl: "https://api.example.test",
    apiKey: "EXAMPLE_KEY",
    api: "custom-api",
    models: [
      { id: "error-model", name: "Error Model", contextWindow: 100, maxTokens: 10, input: ["text"] }
    ],
    streamSimple: async function* (model, context, options) {
      const partial = {
        role: "assistant",
        content: [{ type: "text", text: "" }],
        api: model.api,
        provider: model.provider,
        model: model.id,
        usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
        stopReason: "stop",
        timestamp: 0
      };

      yield { type: "start", partial };
      throw new Error("simulated JS error during streaming");
    }
  });
}
"#;

    const STREAM_SIMPLE_UNICODE: &str = r#"
export default function init(pi) {
  pi.registerProvider("unicode-provider", {
    baseUrl: "https://api.example.test",
    apiKey: "EXAMPLE_KEY",
    api: "custom-api",
    models: [
      { id: "unicode-model", name: "Unicode Model", contextWindow: 100, maxTokens: 10, input: ["text"] }
    ],
    streamSimple: async function* (model, context, options) {
      const partial = {
        role: "assistant",
        content: [{ type: "text", text: "" }],
        api: model.api,
        provider: model.provider,
        model: model.id,
        usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
        stopReason: "stop",
        timestamp: 0
      };

      yield { type: "start", partial };
      yield { type: "text_start", contentIndex: 0, partial };
      partial.content[0].text = "日本語テスト 🦀";
      yield { type: "text_delta", contentIndex: 0, delta: "日本語テスト 🦀", partial };
      yield { type: "done", reason: "stop", message: partial };
    }
  });
}
"#;

    #[test]
    fn extension_stream_simple_multiple_chunks_in_order() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let (_dir, manager) = load_extension(STREAM_SIMPLE_MULTI_CHUNK, false).await;
            let entries = manager.extension_model_entries();
            let entry = entries
                .iter()
                .find(|e| e.model.provider == "multi-chunk-provider")
                .expect("multi-chunk-provider entry");

            let provider = create_provider(entry, Some(&manager)).expect("create provider");
            let ctx = basic_context();
            let opts = basic_options();
            let mut stream = provider.stream(&ctx, &opts).await.expect("stream");

            let mut deltas = Vec::new();
            let mut final_text = String::new();
            while let Some(item) = stream.next().await {
                let event = item.expect("stream event");
                match event {
                    StreamEvent::TextDelta { delta, .. } => {
                        deltas.push(delta);
                    }
                    StreamEvent::Done { message, .. } => {
                        let text = match &message.content[0] {
                            ContentBlock::Text(text) => text,
                            other => unreachable!("expected text content block, got {other:?}"),
                        };
                        final_text = text.text.clone();
                        break;
                    }
                    _ => {}
                }
            }

            assert_eq!(deltas, vec!["Hello", ", ", "world", "!"]);
            assert_eq!(final_text, "Hello, world!");
        });
    }

    #[test]
    fn extension_stream_simple_js_error_propagates() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let (_dir, manager) = load_extension(STREAM_SIMPLE_ERROR, false).await;
            let entries = manager.extension_model_entries();
            let entry = entries
                .iter()
                .find(|e| e.model.provider == "error-provider")
                .expect("error-provider entry");

            let provider = create_provider(entry, Some(&manager)).expect("create provider");
            let ctx = basic_context();
            let opts = basic_options();
            let mut stream = provider.stream(&ctx, &opts).await.expect("stream");

            let mut saw_start = false;
            let mut saw_error = false;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(StreamEvent::Start { .. }) => {
                        saw_start = true;
                    }
                    Err(err) => {
                        // JS error should propagate as an extension error.
                        let msg = err.to_string();
                        assert!(
                            msg.contains("simulated JS error") || msg.contains("error"),
                            "expected JS error message, got: {msg}"
                        );
                        saw_error = true;
                        break;
                    }
                    Ok(StreamEvent::Error { .. }) => {
                        saw_error = true;
                        break;
                    }
                    _ => {}
                }
            }

            assert!(saw_start, "expected a Start event before error");
            assert!(saw_error, "expected JS error to propagate");
        });
    }

    #[test]
    fn extension_stream_simple_unicode_content() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let (_dir, manager) = load_extension(STREAM_SIMPLE_UNICODE, false).await;
            let entries = manager.extension_model_entries();
            let entry = entries
                .iter()
                .find(|e| e.model.provider == "unicode-provider")
                .expect("unicode-provider entry");

            let provider = create_provider(entry, Some(&manager)).expect("create provider");
            let ctx = basic_context();
            let opts = basic_options();
            let mut stream = provider.stream(&ctx, &opts).await.expect("stream");

            let mut saw_unicode = false;
            while let Some(item) = stream.next().await {
                let event = item.expect("stream event");
                match event {
                    StreamEvent::TextDelta { delta, .. } => {
                        assert_eq!(delta, "日本語テスト 🦀");
                        saw_unicode = true;
                    }
                    StreamEvent::Done { .. } => break,
                    _ => {}
                }
            }

            assert!(saw_unicode, "expected unicode text delta");
        });
    }

    #[test]
    fn extension_stream_simple_provider_name_and_model() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let (_dir, manager) = load_extension(STREAM_SIMPLE_EXTENSION, false).await;
            let entries = manager.extension_model_entries();
            let entry = entries
                .iter()
                .find(|e| e.model.provider == "stream-provider")
                .expect("stream-provider entry");

            let provider = create_provider(entry, Some(&manager)).expect("create provider");
            assert_eq!(provider.name(), "stream-provider");
            assert_eq!(provider.model_id(), "stream-model");
            assert_eq!(provider.api(), "custom-api");
        });
    }

    #[test]
    fn create_provider_returns_extension_provider_for_stream_simple() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let (_dir, manager) = load_extension(STREAM_SIMPLE_EXTENSION, false).await;
            let entries = manager.extension_model_entries();
            let entry = entries
                .iter()
                .find(|e| e.model.provider == "stream-provider")
                .expect("stream-provider entry");

            // With extensions, should create ExtensionStreamSimpleProvider.
            let provider = create_provider(entry, Some(&manager));
            assert!(provider.is_ok());

            // Without extensions, should fail (unknown provider).
            let provider_no_ext = create_provider(entry, None);
            assert!(provider_no_ext.is_err());
        });
    }

    // ========================================================================
    // bd-g1nx: Provider factory + URL normalization tests
    // ========================================================================

    use crate::models::ModelEntry;
    use crate::provider::{InputType, Model, ModelCost};
    use std::collections::HashMap;

    fn model_entry(provider: &str, api: &str, model_id: &str, base_url: &str) -> ModelEntry {
        ModelEntry {
            model: Model {
                id: model_id.to_string(),
                name: model_id.to_string(),
                api: api.to_string(),
                provider: provider.to_string(),
                base_url: base_url.to_string(),
                reasoning: false,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 3.0,
                    output: 15.0,
                    cache_read: 0.3,
                    cache_write: 3.75,
                },
                context_window: 200_000,
                max_tokens: 8192,
                headers: HashMap::new(),
            },
            api_key: Some("sk-test-key".to_string()),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }
    }

    #[test]
    fn resolve_provider_route_uses_metadata_for_alias_provider() {
        let entry = model_entry(
            "kimi",
            "openai-completions",
            "kimi-k2-instruct",
            "https://api.moonshot.ai/v1",
        );
        let (route, canonical_provider, effective_api) =
            resolve_provider_route(&entry).expect("resolve alias route");
        assert_eq!(route, ProviderRouteKind::ApiOpenAICompletions);
        assert_eq!(canonical_provider, "moonshotai");
        assert_eq!(effective_api, "openai-completions");
    }

    #[test]
    fn resolve_provider_route_openai_unknown_api_defaults_to_native_responses() {
        let entry = model_entry("openai", "openai", "gpt-4o", "https://api.openai.com/v1");
        let (route, canonical_provider, effective_api) =
            resolve_provider_route(&entry).expect("resolve openai route");
        assert_eq!(route, ProviderRouteKind::NativeOpenAIResponses);
        assert_eq!(canonical_provider, "openai");
        assert_eq!(effective_api, "openai");
    }

    #[test]
    fn resolve_provider_route_cloudflare_workers_defaults_to_openai_completions() {
        let entry = model_entry(
            "cloudflare-workers-ai",
            "",
            "@cf/meta/llama-3.1-8b-instruct",
            "https://api.cloudflare.com/client/v4/accounts/test-account/ai/v1",
        );
        let (route, canonical_provider, effective_api) =
            resolve_provider_route(&entry).expect("resolve cloudflare workers route");
        assert_eq!(route, ProviderRouteKind::ApiOpenAICompletions);
        assert_eq!(canonical_provider, "cloudflare-workers-ai");
        assert_eq!(effective_api, "openai-completions");
    }

    #[test]
    fn resolve_provider_route_cloudflare_gateway_defaults_to_openai_completions() {
        let entry = model_entry(
            "cloudflare-ai-gateway",
            "",
            "gpt-4o-mini",
            "https://gateway.ai.cloudflare.com/v1/account-id/gateway-id/openai",
        );
        let (route, canonical_provider, effective_api) =
            resolve_provider_route(&entry).expect("resolve cloudflare gateway route");
        assert_eq!(route, ProviderRouteKind::ApiOpenAICompletions);
        assert_eq!(canonical_provider, "cloudflare-ai-gateway");
        assert_eq!(effective_api, "openai-completions");
    }

    #[test]
    fn resolve_provider_route_uses_native_azure_route_for_cognitive_alias() {
        let entry = model_entry(
            "azure-cognitive-services",
            "openai-completions",
            "gpt-4o-mini",
            "https://myresource.cognitiveservices.azure.com",
        );
        let (route, canonical_provider, effective_api) =
            resolve_provider_route(&entry).expect("resolve azure cognitive route");
        assert_eq!(route, ProviderRouteKind::NativeAzure);
        assert_eq!(canonical_provider, "azure-openai");
        assert_eq!(effective_api, "openai-completions");
    }

    #[test]
    fn resolve_provider_route_uses_native_azure_route_for_legacy_provider_alias() {
        let entry = model_entry(
            "azure-openai-responses",
            "azure-openai-responses",
            "gpt-4o-mini",
            "https://myresource.openai.azure.com",
        );
        let (route, canonical_provider, effective_api) =
            resolve_provider_route(&entry).expect("resolve azure legacy alias route");
        assert_eq!(route, ProviderRouteKind::NativeAzure);
        assert_eq!(canonical_provider, "azure-openai");
        assert_eq!(effective_api, "azure-openai-responses");
    }

    #[test]
    fn resolve_provider_route_accepts_azure_legacy_api_for_custom_provider_id() {
        let entry = model_entry(
            "my-azure",
            "azure-openai-responses",
            "gpt-4o-mini",
            "https://example.invalid",
        );
        let (route, canonical_provider, effective_api) =
            resolve_provider_route(&entry).expect("resolve azure legacy api fallback");
        assert_eq!(route, ProviderRouteKind::NativeAzure);
        assert_eq!(canonical_provider, "my-azure");
        assert_eq!(effective_api, "azure-openai-responses");
    }

    #[test]
    fn resolve_copilot_token_prefers_inline_model_api_key() {
        let mut entry = model_entry("github-copilot", "", "gpt-4o", "");
        entry.api_key = Some("inline-copilot-token".to_string());

        let token = resolve_copilot_token_with_env(&entry, |_| None)
            .expect("inline token should be accepted");
        assert_eq!(token, "inline-copilot-token");
    }

    #[test]
    fn resolve_copilot_token_falls_back_to_env() {
        let mut entry = model_entry("github-copilot", "", "gpt-4o", "");
        entry.api_key = None;

        let token = resolve_copilot_token_with_env(&entry, |name| match name {
            "GITHUB_COPILOT_API_KEY" => Some("env-copilot-token".to_string()),
            _ => None,
        })
        .expect("env token should be accepted");
        assert_eq!(token, "env-copilot-token");
    }

    #[test]
    fn resolve_copilot_token_errors_when_missing_everywhere() {
        let mut entry = model_entry("github-copilot", "", "gpt-4o", "");
        entry.api_key = None;

        let err = resolve_copilot_token_with_env(&entry, |_| None).expect_err("expected error");
        assert!(
            err.to_string().contains("GitHub Copilot requires"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn suggest_similar_providers_finds_prefix_match() {
        let suggestions = suggest_similar_providers("deep");
        assert!(
            suggestions.contains(&"deepinfra".to_string())
                || suggestions.contains(&"deepseek".to_string()),
            "expected deepinfra or deepseek in suggestions: {suggestions:?}"
        );
    }

    #[test]
    fn suggest_similar_providers_finds_substring_match() {
        let suggestions = suggest_similar_providers("flow");
        assert!(
            suggestions.contains(&"siliconflow".to_string()),
            "expected siliconflow in suggestions: {suggestions:?}"
        );
    }

    #[test]
    fn suggest_similar_providers_returns_empty_for_gibberish() {
        let suggestions = suggest_similar_providers("xyzzzabc123");
        assert!(
            suggestions.is_empty(),
            "expected no suggestions for gibberish: {suggestions:?}"
        );
    }

    #[test]
    fn suggest_similar_providers_caps_at_three() {
        let suggestions = suggest_similar_providers("a");
        assert!(
            suggestions.len() <= 3,
            "expected at most 3 suggestions: {suggestions:?}"
        );
    }

    #[test]
    fn edit_distance_basic_cases() {
        assert_eq!(edit_distance(b"", b""), 0);
        assert_eq!(edit_distance(b"abc", b"abc"), 0);
        assert_eq!(edit_distance(b"abc", b"ab"), 1);
        assert_eq!(edit_distance(b"abc", b"axc"), 1);
        assert_eq!(edit_distance(b"abc", b"abcd"), 1);
        assert_eq!(edit_distance(b"kitten", b"sitting"), 3);
        assert_eq!(edit_distance(b"", b"hello"), 5);
    }

    #[test]
    fn suggest_similar_providers_finds_typo_with_edit_distance() {
        // "anthropick" is edit distance 1 from "anthropic"
        let suggestions = suggest_similar_providers("anthropick");
        assert!(
            suggestions.contains(&"anthropic".to_string()),
            "expected anthropic for typo 'anthropick': {suggestions:?}"
        );
    }

    #[test]
    fn suggest_similar_providers_finds_typo_missing_char() {
        // "openai" with missing letter: "opnai" → edit distance 1
        let suggestions = suggest_similar_providers("opnai");
        assert!(
            suggestions.contains(&"openai".to_string()),
            "expected openai for typo 'opnai': {suggestions:?}"
        );
    }

    #[test]
    fn suggest_similar_providers_finds_transposed_chars() {
        // "gogle" → "google" edit distance 1 (missing 'o')
        let suggestions = suggest_similar_providers("gogle");
        assert!(
            suggestions.contains(&"google".to_string()),
            "expected google for typo 'gogle': {suggestions:?}"
        );
    }

    #[test]
    fn suggest_similar_providers_no_false_positives_for_short_input() {
        // Very short input should not match via edit distance (threshold=0)
        let suggestions = suggest_similar_providers("xy");
        assert!(
            suggestions.is_empty(),
            "expected no suggestions for 'xy': {suggestions:?}"
        );
    }

    #[test]
    fn resolve_azure_provider_runtime_supports_openai_host() {
        let entry = model_entry(
            "azure-openai",
            "openai-completions",
            "gpt-4o",
            "https://myresource.openai.azure.com",
        );
        let runtime =
            resolve_azure_provider_runtime_with_env(&entry, |_| None).expect("resolve runtime");
        assert_eq!(runtime.resource, "myresource");
        assert_eq!(runtime.deployment, "gpt-4o");
        assert_eq!(runtime.api_version, "2024-12-01-preview");
        assert_eq!(
            runtime.endpoint_url,
            "https://myresource.openai.azure.com/openai/deployments/gpt-4o/chat/completions?api-version=2024-12-01-preview"
        );
    }

    #[test]
    fn resolve_azure_provider_runtime_supports_cognitive_services_host() {
        let entry = model_entry(
            "azure-cognitive-services",
            "openai-completions",
            "gpt-4o-mini",
            "https://myresource.cognitiveservices.azure.com/openai/deployments/custom/chat/completions?api-version=2024-10-21",
        );
        let runtime =
            resolve_azure_provider_runtime_with_env(&entry, |_| None).expect("resolve runtime");
        assert_eq!(runtime.resource, "myresource");
        assert_eq!(runtime.deployment, "custom");
        assert_eq!(runtime.api_version, "2024-10-21");
        assert_eq!(
            runtime.endpoint_url,
            "https://myresource.cognitiveservices.azure.com/openai/deployments/custom/chat/completions?api-version=2024-10-21"
        );
    }

    #[test]
    fn resolve_azure_provider_runtime_prefers_base_url_deployment_over_model_id() {
        let entry = model_entry(
            "azure-openai",
            "openai-completions",
            "model-fallback",
            "https://myresource.openai.azure.com/openai/deployments/base-deploy/chat/completions?api-version=2024-10-21",
        );
        let runtime =
            resolve_azure_provider_runtime_with_env(&entry, |_| None).expect("resolve runtime");
        assert_eq!(runtime.resource, "myresource");
        assert_eq!(runtime.deployment, "base-deploy");
        assert_eq!(runtime.api_version, "2024-10-21");
        assert_eq!(
            runtime.endpoint_url,
            "https://myresource.openai.azure.com/openai/deployments/base-deploy/chat/completions?api-version=2024-10-21"
        );
    }

    #[test]
    fn resolve_azure_provider_runtime_env_deployment_overrides_base_url_and_model_id() {
        let entry = model_entry(
            "azure-openai",
            "openai-completions",
            "model-fallback",
            "https://myresource.openai.azure.com/openai/deployments/base-deploy/chat/completions?api-version=2024-10-21",
        );
        let runtime = resolve_azure_provider_runtime_with_env(&entry, |name| match name {
            AZURE_OPENAI_DEPLOYMENT_ENV => Some("env-deploy".to_string()),
            _ => None,
        })
        .expect("resolve runtime");
        assert_eq!(runtime.resource, "myresource");
        assert_eq!(runtime.deployment, "env-deploy");
        assert_eq!(runtime.api_version, "2024-10-21");
        assert_eq!(
            runtime.endpoint_url,
            "https://myresource.openai.azure.com/openai/deployments/env-deploy/chat/completions?api-version=2024-10-21"
        );
    }

    // ── create_provider: built-in provider selection ─────────────────

    #[test]
    fn create_provider_anthropic_by_name() {
        let entry = model_entry(
            "anthropic",
            "anthropic-messages",
            "claude-sonnet-4-5",
            "https://api.anthropic.com",
        );
        let provider = create_provider(&entry, None).expect("anthropic provider");
        assert_eq!(provider.name(), "anthropic");
        assert_eq!(provider.model_id(), "claude-sonnet-4-5");
        assert_eq!(provider.api(), "anthropic-messages");
    }

    #[test]
    fn create_provider_openai_completions_by_name() {
        let entry = model_entry(
            "openai",
            "openai-completions",
            "gpt-4o",
            "https://api.openai.com/v1",
        );
        let provider = create_provider(&entry, None).expect("openai completions provider");
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.model_id(), "gpt-4o");
    }

    #[test]
    fn create_provider_openai_responses_by_name() {
        let entry = model_entry(
            "openai",
            "openai-responses",
            "gpt-4o",
            "https://api.openai.com/v1",
        );
        let provider = create_provider(&entry, None).expect("openai responses provider");
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.model_id(), "gpt-4o");
    }

    #[test]
    fn create_provider_openai_defaults_to_responses() {
        // When api is not "openai-completions", OpenAI defaults to Responses API
        let entry = model_entry("openai", "openai", "gpt-4o", "https://api.openai.com/v1");
        let provider = create_provider(&entry, None).expect("openai default responses provider");
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn create_provider_google_by_name() {
        let entry = model_entry(
            "google",
            "google-generative-ai",
            "gemini-2.0-flash",
            "https://generativelanguage.googleapis.com",
        );
        let provider = create_provider(&entry, None).expect("google provider");
        assert_eq!(provider.name(), "google");
        assert_eq!(provider.model_id(), "gemini-2.0-flash");
    }

    #[test]
    fn create_provider_cohere_by_name() {
        let entry = model_entry(
            "cohere",
            "cohere-chat",
            "command-r-plus",
            "https://api.cohere.com/v2",
        );
        let provider = create_provider(&entry, None).expect("cohere provider");
        assert_eq!(provider.name(), "cohere");
        assert_eq!(provider.model_id(), "command-r-plus");
    }

    #[test]
    fn create_provider_azure_openai_by_name() {
        let entry = model_entry(
            "azure-openai",
            "openai-completions",
            "gpt-4o",
            "https://myresource.openai.azure.com",
        );
        let provider = create_provider(&entry, None).expect("azure provider");
        assert_eq!(provider.name(), "azure-openai");
        assert_eq!(provider.api(), "azure-openai");
        assert!(!provider.model_id().is_empty());
    }

    #[test]
    fn create_provider_azure_cognitive_services_alias_by_name() {
        let entry = model_entry(
            "azure-cognitive-services",
            "openai-completions",
            "gpt-4o-mini",
            "https://myresource.cognitiveservices.azure.com",
        );
        let provider = create_provider(&entry, None).expect("azure cognitive provider");
        assert_eq!(provider.name(), "azure-cognitive-services");
        assert_eq!(provider.api(), "azure-openai");
        assert!(!provider.model_id().is_empty());
    }

    #[test]
    fn create_provider_cloudflare_workers_ai_by_name() {
        let entry = model_entry(
            "cloudflare-workers-ai",
            "",
            "@cf/meta/llama-3.1-8b-instruct",
            "https://api.cloudflare.com/client/v4/accounts/test-account/ai/v1",
        );
        let provider = create_provider(&entry, None).expect("cloudflare workers provider");
        assert_eq!(provider.name(), "cloudflare-workers-ai");
        assert_eq!(provider.api(), "openai-completions");
        assert_eq!(provider.model_id(), "@cf/meta/llama-3.1-8b-instruct");
    }

    #[test]
    fn create_provider_cloudflare_ai_gateway_by_name() {
        let entry = model_entry(
            "cloudflare-ai-gateway",
            "",
            "gpt-4o-mini",
            "https://gateway.ai.cloudflare.com/v1/account-id/gateway-id/openai",
        );
        let provider = create_provider(&entry, None).expect("cloudflare gateway provider");
        assert_eq!(provider.name(), "cloudflare-ai-gateway");
        assert_eq!(provider.api(), "openai-completions");
        assert_eq!(provider.model_id(), "gpt-4o-mini");
    }

    // ── create_provider: API fallback path ──────────────────────────

    #[test]
    fn create_provider_falls_back_to_api_anthropic_messages() {
        let entry = model_entry(
            "custom-anthropic",
            "anthropic-messages",
            "my-model",
            "https://custom.api.com",
        );
        let provider = create_provider(&entry, None).expect("fallback anthropic provider");
        // Anthropic fallback uses the standard anthropic provider
        assert_eq!(provider.model_id(), "my-model");
    }

    #[test]
    fn create_provider_falls_back_to_api_openai_completions() {
        let entry = model_entry(
            "my-openai-compat",
            "openai-completions",
            "local-model",
            "http://localhost:8080/v1",
        );
        let provider = create_provider(&entry, None).expect("fallback openai completions");
        assert_eq!(provider.model_id(), "local-model");
    }

    #[test]
    fn create_provider_falls_back_to_api_openai_responses() {
        let entry = model_entry(
            "my-openai-compat",
            "openai-responses",
            "local-model",
            "http://localhost:8080/v1",
        );
        let provider = create_provider(&entry, None).expect("fallback openai responses");
        assert_eq!(provider.model_id(), "local-model");
    }

    #[test]
    fn create_provider_falls_back_to_api_cohere_chat() {
        let entry = model_entry(
            "custom-cohere",
            "cohere-chat",
            "custom-r",
            "https://custom-cohere.api.com/v2",
        );
        let provider = create_provider(&entry, None).expect("fallback cohere provider");
        assert_eq!(provider.model_id(), "custom-r");
    }

    #[test]
    fn create_provider_falls_back_to_api_google() {
        let entry = model_entry(
            "custom-google",
            "google-generative-ai",
            "custom-gemini",
            "https://custom.google.com",
        );
        let provider = create_provider(&entry, None).expect("fallback google provider");
        assert_eq!(provider.model_id(), "custom-gemini");
    }

    #[test]
    fn resolve_provider_route_copilot_routes_correctly() {
        let entry = model_entry("github-copilot", "", "gpt-4o", "");
        let (route, canonical, _api) = resolve_provider_route(&entry).expect("copilot route");
        assert_eq!(route, ProviderRouteKind::NativeCopilot);
        assert_eq!(canonical, "github-copilot");
    }

    #[test]
    fn resolve_provider_route_copilot_alias_routes_correctly() {
        let entry = model_entry("copilot", "", "gpt-4o", "");
        let (route, canonical, _api) = resolve_provider_route(&entry).expect("copilot alias route");
        assert_eq!(route, ProviderRouteKind::NativeCopilot);
        assert_eq!(canonical, "github-copilot");
    }

    #[test]
    fn create_provider_unknown_provider_and_api_returns_error() {
        let entry = model_entry(
            "totally-unknown",
            "unknown-api",
            "some-model",
            "https://example.com",
        );
        let Err(err) = create_provider(&entry, None) else {
            panic!();
        };
        let msg = err.to_string();
        assert!(
            msg.contains("not implemented"),
            "expected 'not implemented' message, got: {msg}"
        );
    }

    // ── normalize_anthropic_base ───────────────────────────────────

    #[test]
    fn normalize_anthropic_base_appends_v1_messages() {
        assert_eq!(
            normalize_anthropic_base("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn normalize_anthropic_base_keeps_existing_v1_messages() {
        assert_eq!(
            normalize_anthropic_base("https://api.anthropic.com/v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn normalize_anthropic_base_strips_trailing_slash() {
        assert_eq!(
            normalize_anthropic_base("https://api.anthropic.com/"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn normalize_anthropic_base_empty_uses_default() {
        assert_eq!(
            normalize_anthropic_base("   "),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn normalize_anthropic_base_preserves_query_and_fragment() {
        assert_eq!(
            normalize_anthropic_base("https://api.anthropic.com/?via=proxy#frag"),
            "https://api.anthropic.com/v1/messages?via=proxy#frag"
        );
    }

    #[test]
    fn normalize_anthropic_base_handles_opaque_url_fallback() {
        assert_eq!(
            normalize_anthropic_base("data:text/plain,hello"),
            "data:text/plain,hello/v1/messages"
        );
    }

    // ── normalize_openai_base ───────────────────────────────────────

    #[test]
    fn normalize_openai_base_appends_chat_completions_to_v1() {
        assert_eq!(
            normalize_openai_base("https://api.openai.com/v1"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn normalize_openai_base_keeps_existing_chat_completions() {
        assert_eq!(
            normalize_openai_base("https://api.openai.com/v1/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn normalize_openai_base_strips_trailing_slash() {
        assert_eq!(
            normalize_openai_base("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn normalize_openai_base_strips_responses_suffix() {
        assert_eq!(
            normalize_openai_base("https://api.openai.com/v1/responses"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn normalize_openai_base_official_bare_url_gets_v1_chat_completions() {
        assert_eq!(
            normalize_openai_base("https://api.openai.com"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn normalize_openai_base_official_default_port_gets_v1_chat_completions() {
        assert_eq!(
            normalize_openai_base("https://api.openai.com:443"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn normalize_openai_base_strips_non_v1_official_responses_suffix() {
        assert_eq!(
            normalize_openai_base("https://api.openai.com/responses"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn normalize_openai_base_custom_bare_url_gets_chat_completions() {
        assert_eq!(
            normalize_openai_base("https://my-llm-proxy.com"),
            "https://my-llm-proxy.com/chat/completions"
        );
    }

    #[test]
    fn normalize_openai_base_preserves_query_and_fragment_on_official_origin() {
        assert_eq!(
            normalize_openai_base("https://api.openai.com:443/?via=proxy#frag"),
            "https://api.openai.com/v1/chat/completions?via=proxy#frag"
        );
    }

    #[test]
    fn normalize_openai_base_empty_uses_default() {
        assert_eq!(
            normalize_openai_base(""),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn normalize_openai_base_handles_opaque_url_fallback() {
        assert_eq!(
            normalize_openai_base("data:text/plain,hello"),
            "data:text/plain,hello/chat/completions"
        );
    }

    // ── normalize_openai_responses_base ─────────────────────────────

    #[test]
    fn normalize_responses_appends_responses_to_v1() {
        assert_eq!(
            normalize_openai_responses_base("https://api.openai.com/v1"),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn normalize_responses_keeps_existing_responses() {
        assert_eq!(
            normalize_openai_responses_base("https://api.openai.com/v1/responses"),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn normalize_responses_strips_trailing_slash() {
        assert_eq!(
            normalize_openai_responses_base("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn normalize_responses_strips_chat_completions_suffix() {
        assert_eq!(
            normalize_openai_responses_base("https://api.openai.com/v1/chat/completions"),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn normalize_responses_official_bare_url_gets_v1_responses() {
        assert_eq!(
            normalize_openai_responses_base("https://api.openai.com"),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn normalize_responses_official_default_port_gets_v1_responses() {
        assert_eq!(
            normalize_openai_responses_base("https://api.openai.com:443"),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn normalize_responses_strips_non_v1_official_chat_completions_suffix() {
        assert_eq!(
            normalize_openai_responses_base("https://api.openai.com/chat/completions"),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn normalize_responses_custom_bare_url_gets_responses() {
        assert_eq!(
            normalize_openai_responses_base("https://my-llm-proxy.com"),
            "https://my-llm-proxy.com/responses"
        );
    }

    #[test]
    fn normalize_responses_preserves_query_and_fragment() {
        assert_eq!(
            normalize_openai_responses_base("https://my-llm-proxy.com/api?via=proxy#frag"),
            "https://my-llm-proxy.com/api/responses?via=proxy#frag"
        );
    }

    #[test]
    fn normalize_responses_preserves_query_and_fragment_on_official_origin() {
        assert_eq!(
            normalize_openai_responses_base("https://api.openai.com:443/?via=proxy#frag"),
            "https://api.openai.com/v1/responses?via=proxy#frag"
        );
    }

    #[test]
    fn normalize_responses_base_empty_uses_default() {
        assert_eq!(
            normalize_openai_responses_base("  "),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn normalize_responses_base_handles_opaque_url_fallback() {
        assert_eq!(
            normalize_openai_responses_base("data:text/plain,hello"),
            "data:text/plain,hello/responses"
        );
    }

    // ── normalize_openai_codex_responses_base ──────────────────────

    #[test]
    fn normalize_codex_responses_base_empty_uses_default() {
        assert_eq!(
            normalize_openai_codex_responses_base(""),
            openai_responses::CODEX_RESPONSES_API_URL
        );
    }

    #[test]
    fn normalize_codex_responses_base_keeps_existing_suffix() {
        assert_eq!(
            normalize_openai_codex_responses_base(
                "https://chatgpt.com/backend-api/codex/responses"
            ),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn normalize_codex_responses_base_appends_suffix_from_backend_api() {
        assert_eq!(
            normalize_openai_codex_responses_base("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn normalize_codex_responses_base_preserves_query_and_fragment() {
        assert_eq!(
            normalize_openai_codex_responses_base("https://chatgpt.com/backend-api?via=proxy#frag"),
            "https://chatgpt.com/backend-api/codex/responses?via=proxy#frag"
        );
    }

    #[test]
    fn normalize_codex_responses_base_handles_opaque_url_fallback() {
        assert_eq!(
            normalize_openai_codex_responses_base("data:text/plain,hello"),
            "data:text/plain,hello/backend-api/codex/responses"
        );
    }

    // ── normalize_cohere_base ───────────────────────────────────────

    #[test]
    fn normalize_cohere_appends_chat_to_v2() {
        assert_eq!(
            normalize_cohere_base("https://api.cohere.com/v2"),
            "https://api.cohere.com/v2/chat"
        );
    }

    #[test]
    fn normalize_cohere_keeps_existing_chat() {
        assert_eq!(
            normalize_cohere_base("https://api.cohere.com/v2/chat"),
            "https://api.cohere.com/v2/chat"
        );
    }

    #[test]
    fn normalize_cohere_strips_trailing_slash() {
        assert_eq!(
            normalize_cohere_base("https://api.cohere.com/v2/"),
            "https://api.cohere.com/v2/chat"
        );
    }

    #[test]
    fn normalize_cohere_official_bare_url_gets_v2_chat() {
        assert_eq!(
            normalize_cohere_base("https://api.cohere.com"),
            "https://api.cohere.com/v2/chat"
        );
    }

    #[test]
    fn normalize_cohere_official_default_port_gets_v2_chat() {
        assert_eq!(
            normalize_cohere_base("https://api.cohere.com:443"),
            "https://api.cohere.com/v2/chat"
        );
    }

    #[test]
    fn normalize_cohere_custom_bare_url_gets_chat() {
        assert_eq!(
            normalize_cohere_base("https://custom-cohere.example.com"),
            "https://custom-cohere.example.com/chat"
        );
    }

    #[test]
    fn normalize_cohere_preserves_query_and_fragment() {
        assert_eq!(
            normalize_cohere_base("https://custom-cohere.example.com/v2?tenant=test#frag"),
            "https://custom-cohere.example.com/v2/chat?tenant=test#frag"
        );
    }

    #[test]
    fn normalize_cohere_preserves_query_and_fragment_on_official_origin() {
        assert_eq!(
            normalize_cohere_base("https://api.cohere.com:443/?tenant=test#frag"),
            "https://api.cohere.com/v2/chat?tenant=test#frag"
        );
    }

    #[test]
    fn normalize_cohere_base_empty_uses_default() {
        assert_eq!(normalize_cohere_base(""), "https://api.cohere.com/v2/chat");
    }

    #[test]
    fn normalize_cohere_base_handles_opaque_url_fallback() {
        assert_eq!(
            normalize_cohere_base("data:text/plain,hello"),
            "data:text/plain,hello/chat"
        );
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn normalize_anthropic_base_is_idempotent_and_targets_v1_messages(
                base in "[A-Za-z0-9:/._-]{1,96}"
            ) {
                let normalized = normalize_anthropic_base(&base);
                prop_assert!(normalized.ends_with("/v1/messages"));
                prop_assert_eq!(normalize_anthropic_base(&normalized), normalized);
            }

            #[test]
            fn normalize_openai_base_is_idempotent_and_targets_chat_completions(
                base in "[A-Za-z0-9:/._-]{1,96}"
            ) {
                let normalized = normalize_openai_base(&base);
                prop_assert!(normalized.ends_with("/chat/completions"));
                prop_assert_eq!(normalize_openai_base(&normalized), normalized);
            }

            #[test]
            fn normalize_openai_responses_base_is_idempotent_and_targets_responses(
                base in "[A-Za-z0-9:/._-]{1,96}"
            ) {
                let normalized = normalize_openai_responses_base(&base);
                prop_assert!(normalized.ends_with("/responses"));
                prop_assert_eq!(normalize_openai_responses_base(&normalized), normalized);
            }

            #[test]
            fn normalize_cohere_base_is_idempotent_and_targets_chat(
                base in "[A-Za-z0-9:/._-]{1,96}"
            ) {
                let normalized = normalize_cohere_base(&base);
                prop_assert!(normalized.ends_with("/chat"));
                prop_assert_eq!(normalize_cohere_base(&normalized), normalized);
            }

            #[test]
            fn normalize_openai_base_rewrites_responses_suffix(
                host in "[a-z0-9-]{1,32}",
                trailing_slashes in 0usize..4
            ) {
                let base = format!(
                    "https://{host}.example/v1/responses{}",
                    "/".repeat(trailing_slashes)
                );
                prop_assert_eq!(
                    normalize_openai_base(&base),
                    format!("https://{host}.example/v1/chat/completions")
                );
            }

            #[test]
            fn normalize_openai_responses_base_rewrites_chat_completions_suffix(
                host in "[a-z0-9-]{1,32}",
                trailing_slashes in 0usize..4
            ) {
                let base = format!(
                    "https://{host}.example/v1/chat/completions{}",
                    "/".repeat(trailing_slashes)
                );
                prop_assert_eq!(
                    normalize_openai_responses_base(&base),
                    format!("https://{host}.example/v1/responses")
                );
            }
        }
    }

    // ── bd-3uqg.2.4: Compat override propagation ─────────────────────

    use crate::models::CompatConfig;

    fn compat_with_custom_headers() -> CompatConfig {
        let mut custom = HashMap::new();
        custom.insert("X-Custom-Header".to_string(), "test-value".to_string());
        custom.insert("X-Provider-Tag".to_string(), "override".to_string());
        CompatConfig {
            custom_headers: Some(custom),
            ..Default::default()
        }
    }

    fn model_entry_with_compat(
        provider: &str,
        api: &str,
        model_id: &str,
        base_url: &str,
        compat: CompatConfig,
    ) -> ModelEntry {
        let mut entry = model_entry(provider, api, model_id, base_url);
        entry.compat = Some(compat);
        entry
    }

    #[test]
    fn create_provider_anthropic_accepts_compat_config() {
        let entry = model_entry_with_compat(
            "anthropic",
            "anthropic-messages",
            "claude-sonnet-4-5",
            "https://api.anthropic.com",
            compat_with_custom_headers(),
        );
        let provider = create_provider(&entry, None).expect("anthropic with compat");
        assert_eq!(provider.name(), "anthropic");
    }

    #[test]
    fn create_provider_openai_completions_accepts_compat_config() {
        let entry = model_entry_with_compat(
            "openai",
            "openai-completions",
            "gpt-4o",
            "https://api.openai.com/v1",
            CompatConfig {
                max_tokens_field: Some("max_completion_tokens".to_string()),
                system_role_name: Some("developer".to_string()),
                supports_tools: Some(false),
                ..Default::default()
            },
        );
        let provider = create_provider(&entry, None).expect("openai completions with compat");
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn create_provider_openai_responses_accepts_compat_config() {
        let entry = model_entry_with_compat(
            "openai",
            "openai-responses",
            "gpt-4o",
            "https://api.openai.com/v1",
            compat_with_custom_headers(),
        );
        let provider = create_provider(&entry, None).expect("openai responses with compat");
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn create_provider_cohere_accepts_compat_config() {
        let entry = model_entry_with_compat(
            "cohere",
            "cohere-chat",
            "command-r-plus",
            "https://api.cohere.com/v2",
            compat_with_custom_headers(),
        );
        let provider = create_provider(&entry, None).expect("cohere with compat");
        assert_eq!(provider.name(), "cohere");
    }

    #[test]
    fn create_provider_google_accepts_compat_config() {
        let entry = model_entry_with_compat(
            "google",
            "google-generative-ai",
            "gemini-2.0-flash",
            "https://generativelanguage.googleapis.com",
            compat_with_custom_headers(),
        );
        let provider = create_provider(&entry, None).expect("google with compat");
        assert_eq!(provider.name(), "google");
    }

    #[test]
    fn create_provider_fallback_api_routes_accept_compat_config() {
        // Custom provider using anthropic-messages API fallback
        let entry = model_entry_with_compat(
            "custom-anthropic",
            "anthropic-messages",
            "my-model",
            "https://custom.api.com",
            compat_with_custom_headers(),
        );
        let provider = create_provider(&entry, None).expect("fallback anthropic with compat");
        assert_eq!(provider.model_id(), "my-model");

        // Custom provider using openai-completions API fallback
        let entry = model_entry_with_compat(
            "my-groq-clone",
            "openai-completions",
            "llama-3.1",
            "http://localhost:8080/v1",
            compat_with_custom_headers(),
        );
        let provider = create_provider(&entry, None).expect("fallback openai with compat");
        assert_eq!(provider.model_id(), "llama-3.1");

        // Custom provider using cohere-chat API fallback
        let entry = model_entry_with_compat(
            "custom-cohere",
            "cohere-chat",
            "custom-r",
            "https://custom-cohere.api.com/v2",
            compat_with_custom_headers(),
        );
        let provider = create_provider(&entry, None).expect("fallback cohere with compat");
        assert_eq!(provider.model_id(), "custom-r");

        // Custom provider using google-generative-ai API fallback
        let entry = model_entry_with_compat(
            "custom-google",
            "google-generative-ai",
            "custom-gemini",
            "https://custom.google.com",
            compat_with_custom_headers(),
        );
        let provider = create_provider(&entry, None).expect("fallback google with compat");
        assert_eq!(provider.model_id(), "custom-gemini");
    }

    // ── bd-3uqg.3.1: Google Vertex AI provider routing ──────────────

    #[test]
    fn resolve_provider_route_google_vertex_routes_to_native() {
        let entry = model_entry(
            "google-vertex",
            "google-vertex",
            "gemini-2.0-flash",
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-2.0-flash",
        );
        let (route, canonical_provider, effective_api) =
            resolve_provider_route(&entry).expect("resolve google-vertex route");
        assert_eq!(route, ProviderRouteKind::NativeGoogleVertex);
        assert_eq!(canonical_provider, "google-vertex");
        assert_eq!(effective_api, "google-vertex");
    }

    #[test]
    fn resolve_provider_route_vertexai_alias_routes_to_native() {
        let entry = model_entry(
            "vertexai",
            "google-vertex",
            "gemini-2.0-flash",
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-2.0-flash",
        );
        let (route, canonical_provider, effective_api) =
            resolve_provider_route(&entry).expect("resolve vertexai alias route");
        assert_eq!(route, ProviderRouteKind::NativeGoogleVertex);
        assert_eq!(canonical_provider, "google-vertex");
        assert_eq!(effective_api, "google-vertex");
    }

    #[test]
    fn resolve_provider_route_google_vertex_api_fallback() {
        // Unknown provider but google-vertex API should still route correctly
        let entry = model_entry(
            "custom-vertex",
            "google-vertex",
            "gemini-2.0-flash",
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-2.0-flash",
        );
        let (route, _canonical_provider, effective_api) =
            resolve_provider_route(&entry).expect("resolve google-vertex fallback");
        assert_eq!(route, ProviderRouteKind::NativeGoogleVertex);
        assert_eq!(effective_api, "google-vertex");
    }

    #[test]
    fn create_provider_google_vertex_from_full_url() {
        let entry = model_entry(
            "google-vertex",
            "google-vertex",
            "gemini-2.0-flash",
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-2.0-flash",
        );
        let provider = create_provider(&entry, None).expect("google-vertex from full URL");
        assert_eq!(provider.name(), "google-vertex");
        assert_eq!(provider.api(), "google-vertex");
        assert_eq!(provider.model_id(), "gemini-2.0-flash");
    }

    #[test]
    fn create_provider_google_vertex_anthropic_publisher() {
        let entry = model_entry(
            "google-vertex",
            "google-vertex",
            "claude-sonnet-4-5",
            "https://us-east5-aiplatform.googleapis.com/v1/projects/my-project/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-5",
        );
        let provider =
            create_provider(&entry, None).expect("google-vertex with anthropic publisher");
        assert_eq!(provider.name(), "google-vertex");
        assert_eq!(provider.model_id(), "claude-sonnet-4-5");
    }

    #[test]
    fn create_provider_google_vertex_accepts_compat_config() {
        let entry = model_entry_with_compat(
            "google-vertex",
            "google-vertex",
            "gemini-2.0-flash",
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-2.0-flash",
            compat_with_custom_headers(),
        );
        let provider = create_provider(&entry, None).expect("google-vertex with compat");
        assert_eq!(provider.name(), "google-vertex");
    }

    #[test]
    fn create_provider_compat_none_accepted_by_all_routes() {
        // Verify None compat doesn't break anything (regression guard)
        let routes = [
            (
                "anthropic",
                "anthropic-messages",
                "https://api.anthropic.com",
            ),
            ("openai", "openai-completions", "https://api.openai.com/v1"),
            ("openai", "openai-responses", "https://api.openai.com/v1"),
            ("cohere", "cohere-chat", "https://api.cohere.com/v2"),
            (
                "google",
                "google-generative-ai",
                "https://generativelanguage.googleapis.com",
            ),
            (
                "google-vertex",
                "google-vertex",
                "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google/models/test-model",
            ),
        ];
        for (provider, api, base_url) in routes {
            let entry = model_entry(provider, api, "test-model", base_url);
            assert!(
                entry.compat.is_none(),
                "expected None compat for {provider}"
            );
            let result = create_provider(&entry, None);
            assert!(
                result.is_ok(),
                "create_provider failed for {provider} with None compat: {:?}",
                result.err()
            );
        }
    }
}
