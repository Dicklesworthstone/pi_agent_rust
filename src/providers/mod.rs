//! Provider implementations.
//!
//! This module contains concrete implementations of the Provider trait
//! for various LLM APIs.

use crate::error::{Error, Result};
use crate::extensions::{ExtensionManager, JsExtensionRuntimeHandle};
use crate::http::client::Client;
use crate::model::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, StopReason, TextContent, Usage,
};
use crate::models::ModelEntry;
use crate::provider::{Context, Provider, StreamEvent, StreamOptions};
use crate::vcr::{VCR_ENV_MODE, VcrRecorder};
use async_trait::async_trait;
use futures::stream;
use futures::stream::Stream;
use serde_json::Value;
use std::env;
use std::pin::Pin;
use std::sync::Arc;

pub mod anthropic;
pub mod azure;
pub mod cohere;
pub mod gemini;
pub mod openai;
pub mod openai_responses;

fn vcr_client_if_enabled() -> Result<Option<Client>> {
    if env::var(VCR_ENV_MODE).is_err() {
        return Ok(None);
    }

    let test_name = env::var("PI_VCR_TEST_NAME").unwrap_or_else(|_| "pi_runtime".to_string());
    let recorder = VcrRecorder::new(&test_name)?;
    Ok(Some(Client::new().with_vcr(recorder)))
}

struct ExtensionStreamSimpleProvider {
    model: crate::provider::Model,
    runtime: JsExtensionRuntimeHandle,
}

struct ExtensionStreamSimpleState {
    runtime: JsExtensionRuntimeHandle,
    stream_id: Option<String>,
    model_id: String,
    provider: String,
    api: String,
    accumulated_text: String,
    last_message: Option<AssistantMessage>,
}

impl Drop for ExtensionStreamSimpleState {
    fn drop(&mut self) {
        if let Some(stream_id) = self.stream_id.take() {
            self.runtime
                .provider_stream_simple_cancel_best_effort(stream_id);
        }
    }
}

impl ExtensionStreamSimpleProvider {
    const NEXT_TIMEOUT_MS: u64 = 600_000;

    const fn new(model: crate::provider::Model, runtime: JsExtensionRuntimeHandle) -> Self {
        Self { model, runtime }
    }

    fn build_js_model(model: &crate::provider::Model) -> Value {
        serde_json::json!({
            "id": model.id.clone(),
            "name": model.name.clone(),
            "api": model.api.clone(),
            "provider": model.provider.clone(),
            "baseUrl": model.base_url.clone(),
            "reasoning": model.reasoning,
            "input": model.input.clone(),
            "cost": model.cost.clone(),
            "contextWindow": model.context_window,
            "maxTokens": model.max_tokens,
            "headers": model.headers.clone(),
        })
    }

    fn build_js_context(context: &Context) -> Value {
        let mut map = serde_json::Map::new();
        if let Some(system_prompt) = &context.system_prompt {
            map.insert(
                "systemPrompt".to_string(),
                Value::String(system_prompt.clone()),
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
        if let Some(level) = options.thinking_level {
            if level != crate::model::ThinkingLevel::Off {
                map.insert("reasoning".to_string(), Value::String(level.to_string()));
            }
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
                }),
            );
        }
        Value::Object(map)
    }

    fn assistant_event_to_stream_event(event: AssistantMessageEvent) -> StreamEvent {
        match event {
            AssistantMessageEvent::Start { partial } => StreamEvent::Start { partial },
            AssistantMessageEvent::TextStart {
                content_index,
                partial,
            } => StreamEvent::TextStart {
                content_index,
                partial,
            },
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                partial,
            } => StreamEvent::TextDelta {
                content_index,
                delta,
                partial,
            },
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                partial,
            } => StreamEvent::TextEnd {
                content_index,
                content,
                partial,
            },
            AssistantMessageEvent::ThinkingStart {
                content_index,
                partial,
            } => StreamEvent::ThinkingStart {
                content_index,
                partial,
            },
            AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                partial,
            } => StreamEvent::ThinkingDelta {
                content_index,
                delta,
                partial,
            },
            AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                partial,
            } => StreamEvent::ThinkingEnd {
                content_index,
                content,
                partial,
            },
            AssistantMessageEvent::ToolCallStart {
                content_index,
                partial,
            } => StreamEvent::ToolCallStart {
                content_index,
                partial,
            },
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                partial,
            } => StreamEvent::ToolCallDelta {
                content_index,
                delta,
                partial,
            },
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                partial,
            } => StreamEvent::ToolCallEnd {
                content_index,
                tool_call,
                partial,
            },
            AssistantMessageEvent::Done { reason, message } => {
                StreamEvent::Done { reason, message }
            }
            AssistantMessageEvent::Error { reason, error } => StreamEvent::Error { reason, error },
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
            error_message: None,
            timestamp: 0,
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
        context: &Context,
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
        };

        let stream = stream::unfold(state, |mut state| async move {
            let stream_id = state.stream_id.clone()?;
            let stream_id_for_cancel = stream_id.clone();

            match state
                .runtime
                .provider_stream_simple_next(stream_id, Self::NEXT_TIMEOUT_MS)
                .await
            {
                Ok(Some(value)) => {
                    if let Some(chunk) = value.as_str() {
                        // Minimal compatibility: if streamSimple yields string chunks, map them to TextDelta events.
                        let chunk = chunk.to_string();
                        state.accumulated_text.push_str(&chunk);
                        let partial = Self::make_partial(
                            &state.model_id,
                            &state.provider,
                            &state.api,
                            &state.accumulated_text,
                        );
                        state.last_message = Some(partial.clone());
                        return Some((
                            Ok(StreamEvent::TextDelta {
                                content_index: 0,
                                delta: chunk,
                                partial,
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
                            state.last_message = Some(partial.clone());
                        }
                        AssistantMessageEvent::Done { message, .. } => {
                            state.last_message = Some(message.clone());
                        }
                        AssistantMessageEvent::Error { error, .. } => {
                            state.last_message = Some(error.clone());
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
                    // Stream ended — emit Done.
                    state.stream_id = None;
                    let message = state.last_message.clone().unwrap_or_else(|| {
                        Self::make_partial(
                            &state.model_id,
                            &state.provider,
                            &state.api,
                            &state.accumulated_text,
                        )
                    });
                    Some((
                        Ok(StreamEvent::Done {
                            reason: StopReason::Stop,
                            message,
                        }),
                        state,
                    ))
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

pub fn create_provider(
    entry: &ModelEntry,
    extensions: Option<&ExtensionManager>,
) -> Result<Arc<dyn Provider>> {
    if let Some(manager) = extensions {
        if manager.provider_has_stream_simple(&entry.model.provider) {
            let runtime = manager.js_runtime().ok_or_else(|| {
                Error::provider(
                    &entry.model.provider,
                    "Extension JS runtime not configured for streamSimple provider",
                )
            })?;
            return Ok(Arc::new(ExtensionStreamSimpleProvider::new(
                entry.model.clone(),
                runtime,
            )));
        }
    }

    let vcr_client = vcr_client_if_enabled()?;
    let client = vcr_client.unwrap_or_else(Client::new);
    // Try matching on known provider name first.
    match entry.model.provider.as_str() {
        "anthropic" => {
            return Ok(Arc::new(
                anthropic::AnthropicProvider::new(entry.model.id.clone())
                    .with_base_url(entry.model.base_url.clone())
                    .with_client(client),
            ));
        }
        "openai" => {
            // Built-in OpenAI provider can speak either chat completions or responses,
            // based on the configured `api` field.
            if entry.model.api == "openai-completions" {
                return Ok(Arc::new(
                    openai::OpenAIProvider::new(entry.model.id.clone())
                        .with_provider_name(entry.model.provider.clone())
                        .with_base_url(normalize_openai_base(&entry.model.base_url))
                        .with_client(client),
                ));
            }

            // Default to the newer Responses API.
            return Ok(Arc::new(
                openai_responses::OpenAIResponsesProvider::new(entry.model.id.clone())
                    .with_provider_name(entry.model.provider.clone())
                    .with_base_url(normalize_openai_responses_base(&entry.model.base_url))
                    .with_client(client),
            ));
        }
        "cohere" => {
            return Ok(Arc::new(
                cohere::CohereProvider::new(entry.model.id.clone())
                    .with_provider_name(entry.model.provider.clone())
                    .with_base_url(normalize_cohere_base(&entry.model.base_url))
                    .with_client(client),
            ));
        }
        "google" => {
            return Ok(Arc::new(
                gemini::GeminiProvider::new(entry.model.id.clone())
                    .with_base_url(entry.model.base_url.clone())
                    .with_client(client),
            ));
        }
        "azure-openai" => {
            return Err(Error::provider(
                "azure-openai",
                "Azure OpenAI provider requires resource+deployment; configure via models.json",
            ));
        }
        _ => {}
    }

    // Fall back to API type for extension-registered providers.
    match entry.model.api.as_str() {
        "anthropic-messages" => Ok(Arc::new(
            anthropic::AnthropicProvider::new(entry.model.id.clone())
                .with_base_url(entry.model.base_url.clone())
                .with_client(client),
        )),
        "openai-completions" => Ok(Arc::new(
            openai::OpenAIProvider::new(entry.model.id.clone())
                .with_provider_name(entry.model.provider.clone())
                .with_base_url(normalize_openai_base(&entry.model.base_url))
                .with_client(client),
        )),
        "openai-responses" => Ok(Arc::new(
            openai_responses::OpenAIResponsesProvider::new(entry.model.id.clone())
                .with_provider_name(entry.model.provider.clone())
                .with_base_url(normalize_openai_responses_base(&entry.model.base_url))
                .with_client(client),
        )),
        "cohere-chat" => Ok(Arc::new(
            cohere::CohereProvider::new(entry.model.id.clone())
                .with_provider_name(entry.model.provider.clone())
                .with_base_url(normalize_cohere_base(&entry.model.base_url))
                .with_client(client),
        )),
        "google-generative-ai" => Ok(Arc::new(
            gemini::GeminiProvider::new(entry.model.id.clone())
                .with_base_url(entry.model.base_url.clone())
                .with_client(client),
        )),
        _ => Err(Error::provider(
            &entry.model.provider,
            format!("Provider not implemented (api: {})", entry.model.api),
        )),
    }
}

pub fn normalize_openai_base(base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    if base_url.ends_with("/chat/completions") {
        return base_url.to_string();
    }
    let base_url = base_url.strip_suffix("/responses").unwrap_or(base_url);
    if base_url.ends_with("/v1") {
        return format!("{base_url}/chat/completions");
    }
    format!("{base_url}/chat/completions")
}

pub fn normalize_openai_responses_base(base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    if base_url.ends_with("/responses") {
        return base_url.to_string();
    }
    let base_url = base_url
        .strip_suffix("/chat/completions")
        .unwrap_or(base_url);
    if base_url.ends_with("/v1") {
        return format!("{base_url}/responses");
    }
    format!("{base_url}/responses")
}

pub fn normalize_cohere_base(base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    if base_url.ends_with("/chat") {
        return base_url.to_string();
    }
    if base_url.ends_with("/v2") {
        return format!("{base_url}/chat");
    }
    format!("{base_url}/chat")
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

    fn basic_context() -> Context {
        Context {
            system_prompt: Some("system".to_string()),
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            })],
            tools: Vec::new(),
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
                    StreamEvent::TextDelta { delta, partial, .. } => {
                        assert_eq!(delta, "hi");
                        let text = match &partial.content[0] {
                            ContentBlock::Text(text) => text,
                            other => unreachable!("expected text content block, got {other:?}"),
                        };
                        assert_eq!(text.text, "hi");
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
}
