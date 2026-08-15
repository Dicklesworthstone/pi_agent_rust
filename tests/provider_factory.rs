//! Provider factory and base URL normalization tests (no network).

mod common;

use common::{MockHttpResponse, TestHarness};
use futures::StreamExt;
use pi::Error;
use pi::auth::{AuthCredential, AuthStorage};
use pi::model::{Message, UserContent, UserMessage};
use pi::models::{ModelEntry, ModelRegistry};
use pi::provider::{
    Api, CacheRetention, Context, InputType, KnownProvider, Model, ModelCost, StreamEvent,
    StreamOptions, ToolDef,
};
use pi::provider_metadata::{
    canonical_provider_id, provider_auth_env_keys, provider_routing_defaults,
};
use pi::providers::{
    create_provider, normalize_cohere_base, normalize_openai_base, normalize_openai_responses_base,
};
use proptest::prelude::*;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

fn make_model_entry(provider: &str, model_id: &str, base_url: &str) -> ModelEntry {
    ModelEntry {
        model: Model {
            id: model_id.to_string(),
            name: format!("{provider} {model_id}"),
            api: "test-api".to_string(),
            provider: provider.to_string(),
            base_url: base_url.to_string(),
            reasoning: false,
            input: vec![InputType::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 8192,
            max_tokens: 4096,
            headers: HashMap::new(),
        },
        api_key: None,
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

fn make_model_with_cost(cost: ModelCost) -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: "test-api".to_string(),
        provider: "test-provider".to_string(),
        base_url: "https://example.com/v1".to_string(),
        reasoning: false,
        input: vec![InputType::Text],
        cost,
        context_window: 8192,
        max_tokens: 4096,
        headers: HashMap::new(),
    }
}

fn text_event_stream_response(body: String) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: body.into_bytes(),
    }
}

fn openai_chat_sse_body() -> String {
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

fn openai_responses_sse_body() -> String {
    [
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","content_index":0,"delta":"ok"}"#,
        "",
        r#"data: {"type":"response.completed","response":{"incomplete_details":null,"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
        "",
    ]
    .join("\n")
}

fn anthropic_messages_sse_body() -> String {
    [
        r#"data: {"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
        "",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
        "",
        r#"data: {"type":"message_stop"}"#,
        "",
    ]
    .join("\n")
}

fn request_header(headers: &[(String, String)], key: &str) -> Option<String> {
    headers
        .iter()
        .rev()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.clone())
}

fn drive_provider_stream_to_done(
    provider: Arc<dyn pi::provider::Provider>,
    context: Context<'static>,
    options: StreamOptions,
) {
    common::run_async(async move {
        let mut stream = provider
            .stream(&context, &options)
            .await
            .expect("provider stream should start");
        while let Some(event) = stream.next().await {
            if matches!(event.expect("stream event"), StreamEvent::Done { .. }) {
                return;
            }
        }
        panic!("provider stream ended before Done event");
    });
}

fn expect_provider_stream_start_error(
    provider: Arc<dyn pi::provider::Provider>,
    context: Context<'static>,
    options: StreamOptions,
) -> Error {
    common::run_async(async move {
        match provider.stream(&context, &options).await {
            Ok(mut stream) => {
                let mut event_count = 0usize;
                while stream.next().await.is_some() {
                    event_count += 1;
                    if event_count >= 3 {
                        break;
                    }
                }
                Error::api(format!(
                    "provider stream unexpectedly started and yielded {event_count} event(s)"
                ))
            }
            Err(err) => err,
        }
    })
}

const WAVE_A_PRESET_CASES: [(&str, &str); 14] = [
    ("groq", "https://api.groq.com/openai/v1"),
    ("deepinfra", "https://api.deepinfra.com/v1/openai"),
    ("cerebras", "https://api.cerebras.ai/v1"),
    ("atlascloud", "https://api.atlascloud.ai/v1"),
    ("openrouter", "https://openrouter.ai/api/v1"),
    ("mistral", "https://api.mistral.ai/v1"),
    ("moonshotai", "https://api.moonshot.ai/v1"),
    (
        "dashscope",
        "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
    ),
    ("deepseek", "https://api.deepseek.com"),
    ("fireworks", "https://api.fireworks.ai/inference/v1"),
    ("togetherai", "https://api.together.xyz/v1"),
    ("perplexity", "https://api.perplexity.ai"),
    ("xai", "https://api.x.ai/v1"),
    ("fireworks-ai", "https://api.fireworks.ai/inference/v1"),
];

const WAVE_B1_PRESET_CASES: [(&str, &str, &str, bool); 7] = [
    (
        "alibaba-cn",
        "openai-completions",
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
        true,
    ),
    (
        "alibaba-us",
        "openai-completions",
        "https://dashscope-us.aliyuncs.com/compatible-mode/v1",
        true,
    ),
    (
        "kimi-for-coding",
        "anthropic-messages",
        "https://api.kimi.com/coding/v1/messages",
        false,
    ),
    (
        "minimax",
        "anthropic-messages",
        "https://api.minimax.io/anthropic/v1/messages",
        false,
    ),
    (
        "minimax-cn",
        "anthropic-messages",
        "https://api.minimaxi.com/anthropic/v1/messages",
        false,
    ),
    (
        "minimax-coding-plan",
        "anthropic-messages",
        "https://api.minimax.io/anthropic/v1/messages",
        false,
    ),
    (
        "minimax-cn-coding-plan",
        "anthropic-messages",
        "https://api.minimaxi.com/anthropic/v1/messages",
        false,
    ),
];

const WAVE_B2_PRESET_CASES: [(&str, &str, &str, bool); 5] = [
    (
        "modelscope",
        "openai-completions",
        "https://api-inference.modelscope.cn/v1",
        true,
    ),
    (
        "moonshotai-cn",
        "openai-completions",
        "https://api.moonshot.cn/v1",
        true,
    ),
    (
        "nebius",
        "openai-completions",
        "https://api.tokenfactory.nebius.com/v1",
        true,
    ),
    (
        "ovhcloud",
        "openai-completions",
        "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
        true,
    ),
    (
        "scaleway",
        "openai-completions",
        "https://api.scaleway.ai/v1",
        true,
    ),
];

const WAVE_B3_PRESET_CASES: [(&str, &str, &str, bool); 8] = [
    (
        "siliconflow",
        "openai-completions",
        "https://api.siliconflow.com/v1",
        true,
    ),
    (
        "siliconflow-cn",
        "openai-completions",
        "https://api.siliconflow.cn/v1",
        true,
    ),
    (
        "upstage",
        "openai-completions",
        "https://api.upstage.ai/v1/solar",
        true,
    ),
    (
        "venice",
        "openai-completions",
        "https://api.venice.ai/api/v1",
        true,
    ),
    (
        "zai",
        "openai-completions",
        "https://api.z.ai/api/paas/v4",
        true,
    ),
    (
        "zai-coding-plan",
        "openai-completions",
        "https://api.z.ai/api/coding/paas/v4",
        true,
    ),
    (
        "zhipuai",
        "openai-completions",
        "https://open.bigmodel.cn/api/paas/v4",
        true,
    ),
    (
        "zhipuai-coding-plan",
        "openai-completions",
        "https://open.bigmodel.cn/api/coding/paas/v4",
        true,
    ),
];

const SPECIAL_ROUTING_CASES: [(&str, &str, &str, bool); 3] = [
    (
        "opencode",
        "openai-completions",
        "https://opencode.ai/zen/v1",
        true,
    ),
    (
        "vercel",
        "openai-completions",
        "https://ai-gateway.vercel.sh/v1",
        true,
    ),
    (
        "zenmux",
        "anthropic-messages",
        "https://zenmux.ai/api/anthropic/v1/messages",
        false,
    ),
];

fn provider_base_strategy() -> impl Strategy<Value = String> {
    (
        prop::sample::select(vec!["https", "http"]),
        "[a-z][a-z0-9]{2,12}",
        "[a-z][a-z0-9]{2,12}",
        prop::sample::select(vec![
            "",
            "/v1",
            "/v1/",
            "/v1/chat/completions",
            "/v1/chat/completions/",
            "/v1/responses",
            "/v1/responses/",
            "/v2",
            "/v2/",
            "/v2/chat",
            "/v2/chat/",
        ]),
    )
        .prop_map(|(scheme, left, right, suffix)| {
            format!("{scheme}://{left}.{right}.example{suffix}")
        })
}

#[test]
fn normalize_openai_base_appends_for_plain_host() {
    let harness = TestHarness::new("normalize_openai_base_appends_for_plain_host");
    let input = "https://api.openai.com";
    let expected = "https://api.openai.com/v1/chat/completions";
    harness.log().info_ctx("normalize", "plain host", |ctx| {
        ctx.push(("input".to_string(), input.to_string()));
        ctx.push(("expected".to_string(), expected.to_string()));
    });
    let normalized = normalize_openai_base(input);
    assert_eq!(normalized, expected);
}

#[test]
fn normalize_openai_base_appends_for_v1() {
    let harness = TestHarness::new("normalize_openai_base_appends_for_v1");
    let input = "https://api.openai.com/v1";
    let expected = "https://api.openai.com/v1/chat/completions";
    harness.log().info_ctx("normalize", "v1 host", |ctx| {
        ctx.push(("input".to_string(), input.to_string()));
        ctx.push(("expected".to_string(), expected.to_string()));
    });
    let normalized = normalize_openai_base(input);
    assert_eq!(normalized, expected);
}

#[test]
fn normalize_openai_base_trims_trailing_slash() {
    let harness = TestHarness::new("normalize_openai_base_trims_trailing_slash");
    let input = "https://api.openai.com/v1/";
    let expected = "https://api.openai.com/v1/chat/completions";
    harness
        .log()
        .info_ctx("normalize", "trailing slash", |ctx| {
            ctx.push(("input".to_string(), input.to_string()));
            ctx.push(("expected".to_string(), expected.to_string()));
        });
    let normalized = normalize_openai_base(input);
    assert_eq!(normalized, expected);
}

#[test]
fn normalize_openai_base_empty_uses_default_endpoint() {
    let harness = TestHarness::new("normalize_openai_base_empty_uses_default_endpoint");
    let input = "   ";
    let expected = "https://api.openai.com/v1/chat/completions";
    harness
        .log()
        .info_ctx("normalize", "empty uses default", |ctx| {
            ctx.push(("input".to_string(), input.to_string()));
            ctx.push(("expected".to_string(), expected.to_string()));
        });
    let normalized = normalize_openai_base(input);
    assert_eq!(normalized, expected);
}

#[test]
fn normalize_openai_base_preserves_chat_completions() {
    let harness = TestHarness::new("normalize_openai_base_preserves_chat_completions");
    let input = "https://api.openai.com/v1/chat/completions";
    let expected = "https://api.openai.com/v1/chat/completions";
    harness
        .log()
        .info_ctx("normalize", "chat completions", |ctx| {
            ctx.push(("input".to_string(), input.to_string()));
            ctx.push(("expected".to_string(), expected.to_string()));
        });
    let normalized = normalize_openai_base(input);
    assert_eq!(normalized, expected);
}

#[test]
fn normalize_openai_responses_base_preserves_responses() {
    let harness = TestHarness::new("normalize_openai_responses_base_preserves_responses");
    let input = "https://api.openai.com/v1/responses";
    let expected = "https://api.openai.com/v1/responses";
    harness
        .log()
        .info_ctx("normalize", "responses endpoint", |ctx| {
            ctx.push(("input".to_string(), input.to_string()));
            ctx.push(("expected".to_string(), expected.to_string()));
        });
    let normalized = normalize_openai_responses_base(input);
    assert_eq!(normalized, expected);
}

#[test]
fn normalize_openai_base_trims_trailing_slash_for_chat_completions() {
    let harness =
        TestHarness::new("normalize_openai_base_trims_trailing_slash_for_chat_completions");
    let input = "https://api.openai.com/v1/chat/completions/";
    let expected = "https://api.openai.com/v1/chat/completions";
    harness
        .log()
        .info_ctx("normalize", "chat completions trailing slash", |ctx| {
            ctx.push(("input".to_string(), input.to_string()));
            ctx.push(("expected".to_string(), expected.to_string()));
        });
    let normalized = normalize_openai_base(input);
    assert_eq!(normalized, expected);
}

#[test]
fn normalize_openai_responses_base_trims_trailing_slash() {
    let harness = TestHarness::new("normalize_openai_responses_base_trims_trailing_slash");
    let input = "https://api.openai.com/v1/responses/";
    let expected = "https://api.openai.com/v1/responses";
    harness
        .log()
        .info_ctx("normalize", "responses trailing slash", |ctx| {
            ctx.push(("input".to_string(), input.to_string()));
            ctx.push(("expected".to_string(), expected.to_string()));
        });
    let normalized = normalize_openai_responses_base(input);
    assert_eq!(normalized, expected);
}

#[test]
fn normalize_openai_responses_base_empty_uses_default_endpoint() {
    let harness = TestHarness::new("normalize_openai_responses_base_empty_uses_default_endpoint");
    let input = "";
    let expected = "https://api.openai.com/v1/responses";
    harness
        .log()
        .info_ctx("normalize", "empty uses default", |ctx| {
            ctx.push(("input".to_string(), input.to_string()));
            ctx.push(("expected".to_string(), expected.to_string()));
        });
    let normalized = normalize_openai_responses_base(input);
    assert_eq!(normalized, expected);
}

proptest! {
    #[test]
    fn normalize_openai_base_property_invariants(base in provider_base_strategy()) {
        let normalized = normalize_openai_base(&base);
        prop_assert!(normalized.ends_with("/chat/completions"));
        prop_assert!(!normalized.ends_with('/'));
        prop_assert_eq!(normalize_openai_base(&normalized), normalized);
    }

    #[test]
    fn normalize_openai_responses_base_property_invariants(base in provider_base_strategy()) {
        let normalized = normalize_openai_responses_base(&base);
        prop_assert!(normalized.ends_with("/responses"));
        prop_assert!(!normalized.ends_with('/'));
        prop_assert_eq!(normalize_openai_responses_base(&normalized), normalized);
    }

    #[test]
    fn normalize_cohere_base_property_invariants(base in provider_base_strategy()) {
        let normalized = normalize_cohere_base(&base);
        prop_assert!(normalized.ends_with("/chat"));
        prop_assert!(!normalized.ends_with('/'));
        prop_assert_eq!(normalize_cohere_base(&normalized), normalized);
    }
}

#[test]
fn create_provider_for_anthropic() {
    let harness = TestHarness::new("create_provider_for_anthropic");
    let entry = make_model_entry(
        "anthropic",
        "claude-test",
        "https://api.anthropic.com/v1/messages",
    );
    let provider = create_provider(&entry, None).expect("create anthropic provider");
    harness
        .log()
        .info_ctx("provider", "created provider", |ctx| {
            ctx.push(("name".to_string(), provider.name().to_string()));
            ctx.push(("api".to_string(), provider.api().to_string()));
            ctx.push(("model".to_string(), provider.model_id().to_string()));
        });

    assert_eq!(provider.name(), "anthropic");
    assert_eq!(provider.api(), "anthropic-messages");
    assert_eq!(provider.model_id(), "claude-test");
}

#[test]
fn create_provider_for_openai() {
    let harness = TestHarness::new("create_provider_for_openai");
    let entry = make_model_entry("openai", "gpt-test", "https://api.openai.com/v1");
    let provider = create_provider(&entry, None).expect("create openai provider");
    harness
        .log()
        .info_ctx("provider", "created provider", |ctx| {
            ctx.push(("name".to_string(), provider.name().to_string()));
            ctx.push(("api".to_string(), provider.api().to_string()));
            ctx.push(("model".to_string(), provider.model_id().to_string()));
        });

    assert_eq!(provider.name(), "openai");
    assert_eq!(provider.api(), "openai-responses");
    assert_eq!(provider.model_id(), "gpt-test");
}

#[test]
fn create_provider_for_openai_completions() {
    let harness = TestHarness::new("create_provider_for_openai_completions");
    let mut entry = make_model_entry("openai", "gpt-test", "https://api.openai.com/v1");
    entry.model.api = "openai-completions".to_string();

    let provider = create_provider(&entry, None).expect("create openai completions provider");
    harness
        .log()
        .info_ctx("provider", "created provider", |ctx| {
            ctx.push(("name".to_string(), provider.name().to_string()));
            ctx.push(("api".to_string(), provider.api().to_string()));
            ctx.push(("model".to_string(), provider.model_id().to_string()));
        });

    assert_eq!(provider.name(), "openai");
    assert_eq!(provider.api(), "openai-completions");
    assert_eq!(provider.model_id(), "gpt-test");
}

#[test]
fn create_provider_for_gemini() {
    let harness = TestHarness::new("create_provider_for_gemini");
    let entry = make_model_entry(
        "google",
        "gemini-test",
        "https://generativelanguage.googleapis.com/v1beta",
    );
    let provider = create_provider(&entry, None).expect("create gemini provider");
    harness
        .log()
        .info_ctx("provider", "created provider", |ctx| {
            ctx.push(("name".to_string(), provider.name().to_string()));
            ctx.push(("api".to_string(), provider.api().to_string()));
            ctx.push(("model".to_string(), provider.model_id().to_string()));
        });

    assert_eq!(provider.name(), "google");
    assert_eq!(provider.api(), "google-generative-ai");
    assert_eq!(provider.model_id(), "gemini-test");
}

#[test]
fn create_provider_for_cohere() {
    let harness = TestHarness::new("create_provider_for_cohere");
    let entry = make_model_entry("cohere", "command-r-test", "https://api.cohere.com/v2");
    let provider = create_provider(&entry, None).expect("create cohere provider");
    harness
        .log()
        .info_ctx("provider", "created provider", |ctx| {
            ctx.push(("name".to_string(), provider.name().to_string()));
            ctx.push(("api".to_string(), provider.api().to_string()));
            ctx.push(("model".to_string(), provider.model_id().to_string()));
        });

    assert_eq!(provider.name(), "cohere");
    assert_eq!(provider.api(), "cohere-chat");
    assert_eq!(provider.model_id(), "command-r-test");
}

#[test]
fn create_provider_falls_back_to_api_openai_completions() {
    let harness = TestHarness::new("create_provider_falls_back_to_api_openai_completions");
    let mut entry = make_model_entry("custom-openai", "custom-gpt", "https://api.openai.com/v1");
    entry.model.api = "openai-completions".to_string();
    let provider = create_provider(&entry, None).expect("create api-fallback openai provider");
    harness
        .log()
        .info_ctx("provider", "fallback provider", |ctx| {
            ctx.push(("name".to_string(), provider.name().to_string()));
            ctx.push(("api".to_string(), provider.api().to_string()));
            ctx.push(("model".to_string(), provider.model_id().to_string()));
        });

    assert_eq!(provider.name(), "custom-openai");
    assert_eq!(provider.api(), "openai-completions");
    assert_eq!(provider.model_id(), "custom-gpt");
}

#[test]
fn create_provider_falls_back_to_api_openai_responses() {
    let harness = TestHarness::new("create_provider_falls_back_to_api_openai_responses");
    let mut entry = make_model_entry(
        "custom-openai-responses",
        "custom-gpt",
        "https://api.openai.com/v1",
    );
    entry.model.api = "openai-responses".to_string();
    let provider =
        create_provider(&entry, None).expect("create api-fallback openai responses provider");
    harness
        .log()
        .info_ctx("provider", "fallback provider", |ctx| {
            ctx.push(("name".to_string(), provider.name().to_string()));
            ctx.push(("api".to_string(), provider.api().to_string()));
            ctx.push(("model".to_string(), provider.model_id().to_string()));
        });

    assert_eq!(provider.name(), "custom-openai-responses");
    assert_eq!(provider.api(), "openai-responses");
    assert_eq!(provider.model_id(), "custom-gpt");
}

#[test]
fn create_provider_falls_back_to_api_cohere_chat() {
    let harness = TestHarness::new("create_provider_falls_back_to_api_cohere_chat");
    let mut entry = make_model_entry(
        "custom-cohere",
        "command-r-test",
        "https://api.cohere.com/v2",
    );
    entry.model.api = "cohere-chat".to_string();
    let provider = create_provider(&entry, None).expect("create api-fallback cohere provider");
    harness
        .log()
        .info_ctx("provider", "fallback provider", |ctx| {
            ctx.push(("name".to_string(), provider.name().to_string()));
            ctx.push(("api".to_string(), provider.api().to_string()));
            ctx.push(("model".to_string(), provider.model_id().to_string()));
        });

    assert_eq!(provider.name(), "custom-cohere");
    assert_eq!(provider.api(), "cohere-chat");
    assert_eq!(provider.model_id(), "command-r-test");
}

#[test]
fn create_provider_falls_back_to_api_anthropic_messages() {
    let harness = TestHarness::new("create_provider_falls_back_to_api_anthropic_messages");
    let mut entry = make_model_entry(
        "custom-anthropic",
        "claude-test",
        "https://api.anthropic.com/v1/messages",
    );
    entry.model.api = "anthropic-messages".to_string();
    let provider = create_provider(&entry, None).expect("create api-fallback anthropic provider");
    harness
        .log()
        .info_ctx("provider", "fallback provider", |ctx| {
            ctx.push(("name".to_string(), provider.name().to_string()));
            ctx.push(("api".to_string(), provider.api().to_string()));
            ctx.push(("model".to_string(), provider.model_id().to_string()));
        });

    assert_eq!(provider.name(), "custom-anthropic");
    assert_eq!(provider.api(), "anthropic-messages");
    assert_eq!(provider.model_id(), "claude-test");
}

#[test]
fn schema_metadata_drives_alias_provider_selection_end_to_end() {
    let harness = TestHarness::new("schema_metadata_drives_alias_provider_selection_end_to_end");
    let auth_path = harness.temp_path("auth.json");
    let mut auth = AuthStorage::load(auth_path).expect("load auth storage");
    auth.set(
        "moonshotai",
        AuthCredential::ApiKey {
            key: "moonshot-schema-key".to_string(),
        },
    );
    auth.save().expect("save auth storage");

    let models_path = harness.create_file(
        "models.json",
        r#"{
  "providers": {
    "kimi": {
      "models": [{ "id": "kimi-k2-instruct" }]
    }
  }
}"#,
    );

    let registry = ModelRegistry::load(&auth, Some(models_path));
    assert!(
        registry.error().is_none(),
        "unexpected models load error: {:?}",
        registry.error()
    );

    let entry = registry
        .find("kimi", "kimi-k2-instruct")
        .expect("schema should register kimi model");
    harness
        .log()
        .info_ctx("scenario", "resolved schema-driven model", |ctx| {
            ctx.push(("provider".to_string(), entry.model.provider.clone()));
            ctx.push(("model".to_string(), entry.model.id.clone()));
            ctx.push(("api".to_string(), entry.model.api.clone()));
            ctx.push(("base_url".to_string(), entry.model.base_url.clone()));
        });
    assert_eq!(entry.model.api, "openai-completions");
    assert_eq!(entry.model.base_url, "https://api.moonshot.ai/v1");
    assert_eq!(entry.api_key.as_deref(), Some("moonshot-schema-key"));
    assert!(entry.auth_header);

    let provider = create_provider(&entry, None).expect("create provider from schema-driven entry");
    harness
        .log()
        .info_ctx("scenario", "selected provider implementation", |ctx| {
            ctx.push(("name".to_string(), provider.name().to_string()));
            ctx.push(("api".to_string(), provider.api().to_string()));
            ctx.push(("model".to_string(), provider.model_id().to_string()));
        });
    assert_eq!(provider.name(), "kimi");
    assert_eq!(provider.api(), "openai-completions");
    assert_eq!(provider.model_id(), "kimi-k2-instruct");
}

#[test]
fn schema_metadata_drives_native_anthropic_selection_end_to_end() {
    let harness = TestHarness::new("schema_metadata_drives_native_anthropic_selection_end_to_end");
    let auth_path = harness.temp_path("auth.json");
    let mut auth = AuthStorage::load(auth_path).expect("load auth storage");
    auth.set(
        "anthropic",
        AuthCredential::ApiKey {
            key: "anthropic-schema-key".to_string(),
        },
    );
    auth.save().expect("save auth storage");

    let models_path = harness.create_file(
        "models.json",
        r#"{
  "providers": {
    "anthropic": {
      "models": [{ "id": "claude-schema-default" }]
    }
  }
}"#,
    );

    let registry = ModelRegistry::load(&auth, Some(models_path));
    assert!(
        registry.error().is_none(),
        "unexpected models load error: {:?}",
        registry.error()
    );

    let entry = registry
        .find("anthropic", "claude-schema-default")
        .expect("schema should register anthropic model");
    harness
        .log()
        .info_ctx("scenario", "resolved native schema-driven model", |ctx| {
            ctx.push(("provider".to_string(), entry.model.provider.clone()));
            ctx.push(("model".to_string(), entry.model.id.clone()));
            ctx.push(("api".to_string(), entry.model.api.clone()));
            ctx.push(("base_url".to_string(), entry.model.base_url.clone()));
            ctx.push(("max_tokens".to_string(), entry.model.max_tokens.to_string()));
        });
    assert_eq!(entry.model.api, "anthropic-messages");
    assert_eq!(
        entry.model.base_url,
        "https://api.anthropic.com/v1/messages"
    );
    assert_eq!(entry.model.max_tokens, 8192);
    assert!(!entry.auth_header);

    let provider = create_provider(&entry, None).expect("create native provider from schema entry");
    harness.log().info_ctx(
        "scenario",
        "selected native provider implementation",
        |ctx| {
            ctx.push(("name".to_string(), provider.name().to_string()));
            ctx.push(("api".to_string(), provider.api().to_string()));
            ctx.push(("model".to_string(), provider.model_id().to_string()));
        },
    );
    assert_eq!(provider.name(), "anthropic");
    assert_eq!(provider.api(), "anthropic-messages");
    assert_eq!(provider.model_id(), "claude-schema-default");
}

#[test]
#[allow(clippy::too_many_lines)]
fn schema_compat_overrides_flow_through_factory_for_openai_completions() {
    let harness =
        TestHarness::new("schema_compat_overrides_flow_through_factory_for_openai_completions");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/v1/chat/completions",
        text_event_stream_response(openai_chat_sse_body()),
    );

    let auth_path = harness.temp_path("auth.json");
    let auth = AuthStorage::load(auth_path).expect("load auth storage");

    let models_path = harness.create_file(
        "models.json",
        format!(
            r#"{{
  "providers": {{
    "custom-openai": {{
      "api": "openai-completions",
      "baseUrl": "{base_url}/v1",
      "apiKey": "provider-secret-key",
      "compat": {{
        "supportsTools": false,
        "supportsUsageInStreaming": false,
        "maxTokensField": "max_completion_tokens",
        "customHeaders": {{
          "x-provider-routing": "provider-default"
        }}
      }},
      "models": [{{
        "id": "compat-chat-model",
        "compat": {{
          "systemRoleName": "developer",
          "customHeaders": {{
            "x-model-routing": "model-override"
          }}
        }}
      }}]
    }}
  }}
}}"#,
            base_url = server.base_url()
        ),
    );

    let registry = ModelRegistry::load(&auth, Some(models_path));
    assert!(
        registry.error().is_none(),
        "unexpected models load error: {:?}",
        registry.error()
    );

    let entry = registry
        .find("custom-openai", "compat-chat-model")
        .expect("schema should register custom openai model");
    let compat = entry.compat.as_ref().expect("compat should be present");
    assert_eq!(
        compat.max_tokens_field.as_deref(),
        Some("max_completion_tokens")
    );
    assert_eq!(compat.system_role_name.as_deref(), Some("developer"));
    assert_eq!(compat.supports_tools, Some(false));
    assert_eq!(compat.supports_usage_in_streaming, Some(false));

    let provider = create_provider(&entry, None).expect("create compat completions provider");
    let context = Context {
        system_prompt: Some("You are concise.".to_string().into()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text("Ping".to_string()),
            timestamp: 0,
        })]
        .into(),
        tools: vec![ToolDef {
            name: "search".to_string(),
            description: "Search docs".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "q": { "type": "string" }
                },
                "required": ["q"]
            }),
        }]
        .into(),
    };
    let options = StreamOptions {
        api_key: entry.api_key.clone(),
        headers: entry.headers.clone(),
        max_tokens: Some(321),
        ..Default::default()
    };
    drive_provider_stream_to_done(provider, context, options);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "expected exactly one request");
    let request = &requests[0];
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(
        request_header(&request.headers, "authorization").as_deref(),
        Some("Bearer provider-secret-key")
    );
    assert_eq!(
        request_header(&request.headers, "x-provider-routing").as_deref(),
        Some("provider-default")
    );
    assert_eq!(
        request_header(&request.headers, "x-model-routing").as_deref(),
        Some("model-override")
    );

    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("request body should be json");
    assert_eq!(body["messages"][0]["role"], "developer");
    assert_eq!(body["max_completion_tokens"], 321);
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["stream_options"]["include_usage"], false);
    assert!(body.get("tools").is_none());

    let dump = harness.log().dump();
    assert!(dump.contains("header.authorization = [REDACTED]"));
    assert!(!dump.contains("provider-secret-key"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn schema_compat_headers_respect_precedence_for_openai_responses() {
    let harness = TestHarness::new("schema_compat_headers_respect_precedence_for_openai_responses");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/v1/responses",
        text_event_stream_response(openai_responses_sse_body()),
    );

    let auth_path = harness.temp_path("auth.json");
    let auth = AuthStorage::load(auth_path).expect("load auth storage");

    let models_path = harness.create_file(
        "models.json",
        format!(
            r#"{{
  "providers": {{
    "custom-responses": {{
      "api": "openai-responses",
      "baseUrl": "{base_url}/v1",
      "apiKey": "responses-secret-key",
      "headers": {{
        "x-routing": "request-override"
      }},
      "compat": {{
        "customHeaders": {{
          "x-routing": "provider-compat",
          "x-compat-only": "provider-only"
        }}
      }},
      "models": [{{
        "id": "compat-responses-model",
        "headers": {{
          "x-model-header": "request-model"
        }},
        "compat": {{
          "customHeaders": {{
            "x-routing": "model-compat",
            "x-model-compat": "model-only"
          }}
        }}
      }}]
    }}
  }}
}}"#,
            base_url = server.base_url()
        ),
    );

    let registry = ModelRegistry::load(&auth, Some(models_path));
    assert!(
        registry.error().is_none(),
        "unexpected models load error: {:?}",
        registry.error()
    );

    let entry = registry
        .find("custom-responses", "compat-responses-model")
        .expect("schema should register custom responses model");
    let compat_headers = entry
        .compat
        .as_ref()
        .and_then(|compat| compat.custom_headers.as_ref())
        .expect("compat headers should be present");
    assert_eq!(
        compat_headers.get("x-routing").map(String::as_str),
        Some("model-compat")
    );
    assert_eq!(
        compat_headers.get("x-compat-only").map(String::as_str),
        Some("provider-only")
    );
    assert_eq!(
        compat_headers.get("x-model-compat").map(String::as_str),
        Some("model-only")
    );

    let provider = create_provider(&entry, None).expect("create compat responses provider");
    let context = Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text("Ping".to_string()),
            timestamp: 0,
        })]
        .into(),
        tools: Vec::new().into(),
    };
    let options = StreamOptions {
        api_key: entry.api_key.clone(),
        headers: entry.headers.clone(),
        max_tokens: Some(123),
        ..Default::default()
    };
    drive_provider_stream_to_done(provider, context, options);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "expected exactly one request");
    let request = &requests[0];
    assert_eq!(request.path, "/v1/responses");
    assert_eq!(
        request_header(&request.headers, "authorization").as_deref(),
        Some("Bearer responses-secret-key")
    );
    assert_eq!(
        request_header(&request.headers, "x-routing").as_deref(),
        Some("request-override")
    );
    assert_eq!(
        request_header(&request.headers, "x-compat-only").as_deref(),
        Some("provider-only")
    );
    assert_eq!(
        request_header(&request.headers, "x-model-compat").as_deref(),
        Some("model-only")
    );
    assert_eq!(
        request_header(&request.headers, "x-model-header").as_deref(),
        Some("request-model")
    );

    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("request body should be json");
    assert_eq!(body["max_output_tokens"], 123);

    let dump = harness.log().dump();
    assert!(dump.contains("header.authorization = [REDACTED]"));
    assert!(!dump.contains("responses-secret-key"));
}

#[test]
fn create_provider_azure_openai_routes_natively() {
    let harness = TestHarness::new("create_provider_azure_openai_routes_natively");
    let entry = make_model_entry("azure-openai", "gpt-4o", "https://example.openai.azure.com");
    let provider = create_provider(&entry, None).expect("azure-openai provider");
    harness.log().info_ctx("provider", "azure route", |ctx| {
        ctx.push(("name".to_string(), provider.name().to_string()));
        ctx.push(("api".to_string(), provider.api().to_string()));
        ctx.push(("model".to_string(), provider.model_id().to_string()));
    });
    assert_eq!(provider.name(), "azure-openai");
    assert_eq!(provider.api(), "azure-openai");
    assert!(!provider.model_id().is_empty());
}

#[test]
fn create_provider_azure_cognitive_services_alias_routes_natively() {
    let harness =
        TestHarness::new("create_provider_azure_cognitive_services_alias_routes_natively");
    let entry = make_model_entry(
        "azure-cognitive-services",
        "gpt-4o-mini",
        "https://example.cognitiveservices.azure.com",
    );
    let provider = create_provider(&entry, None).expect("azure-cognitive-services provider");
    harness
        .log()
        .info_ctx("provider", "azure cognitive route", |ctx| {
            ctx.push(("name".to_string(), provider.name().to_string()));
            ctx.push(("api".to_string(), provider.api().to_string()));
            ctx.push(("model".to_string(), provider.model_id().to_string()));
        });
    assert_eq!(provider.name(), "azure-cognitive-services");
    assert_eq!(provider.api(), "azure-openai");
    assert!(!provider.model_id().is_empty());
}

#[test]
fn create_provider_amazon_bedrock_routes_natively() {
    let harness = TestHarness::new("create_provider_amazon_bedrock_routes_natively");
    let entry = make_model_entry(
        "amazon-bedrock",
        "anthropic.claude-3-5-sonnet-20240620-v1:0",
        "https://bedrock-runtime.us-east-1.amazonaws.com",
    );
    let provider = create_provider(&entry, None).expect("amazon-bedrock provider");
    harness.log().info_ctx("provider", "bedrock route", |ctx| {
        ctx.push(("name".to_string(), provider.name().to_string()));
        ctx.push(("api".to_string(), provider.api().to_string()));
        ctx.push(("model".to_string(), provider.model_id().to_string()));
    });
    assert_eq!(provider.name(), "amazon-bedrock");
    assert_eq!(provider.api(), "bedrock-converse-stream");
    assert_eq!(
        provider.model_id(),
        "anthropic.claude-3-5-sonnet-20240620-v1:0"
    );
}

#[test]
fn bedrock_provider_uses_bearer_auth_and_converse_payload() {
    let harness = TestHarness::new("bedrock_provider_uses_bearer_auth_and_converse_payload");
    let server = harness.start_mock_http_server();
    let bedrock_model = "anthropic.claude-3-5-sonnet-20240620-v1:0";
    server.add_route(
        "POST",
        "/model/anthropic.claude-3-5-sonnet-20240620-v1:0/converse",
        MockHttpResponse::json(
            200,
            &serde_json::json!({
                "output": {
                    "message": {
                        "role": "assistant",
                        "content": [{"text": "pong"}]
                    }
                },
                "stopReason": "end_turn",
                "usage": {"inputTokens": 11, "outputTokens": 7, "totalTokens": 18}
            }),
        ),
    );

    let base_url = server.base_url();
    let mut entry = make_model_entry("amazon-bedrock", bedrock_model, &base_url);
    entry.model.api = "bedrock-converse-stream".to_string();
    let provider = create_provider(&entry, None).expect("amazon-bedrock provider");
    let context = Context {
        system_prompt: Some("Be concise.".to_string().into()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text("Ping".to_string()),
            timestamp: 0,
        })]
        .into(),
        tools: vec![ToolDef {
            name: "search".to_string(),
            description: "Search docs".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "q": {"type": "string"}
                },
                "required": ["q"]
            }),
        }]
        .into(),
    };
    let options = StreamOptions {
        api_key: Some("bedrock-test-token".to_string()),
        max_tokens: Some(128),
        temperature: Some(0.1),
        ..Default::default()
    };

    drive_provider_stream_to_done(provider, context, options);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "expected exactly one request");
    let request = &requests[0];
    assert_eq!(
        request.path,
        "/model/anthropic.claude-3-5-sonnet-20240620-v1:0/converse"
    );
    assert_eq!(
        request_header(&request.headers, "authorization").as_deref(),
        Some("Bearer bedrock-test-token")
    );
    assert_eq!(
        request_header(&request.headers, "content-type").as_deref(),
        Some("application/json")
    );

    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("request body should be json");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["text"], "Ping");
    assert_eq!(body["system"][0]["text"], "Be concise.");
    assert_eq!(body["inferenceConfig"]["maxTokens"], 128);
    assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "search");
}

#[test]
fn bedrock_provider_surfaces_access_denied_error_without_live_credentials() {
    let harness =
        TestHarness::new("bedrock_provider_surfaces_access_denied_error_without_live_credentials");
    let server = harness.start_mock_http_server();
    let bedrock_model = "anthropic.claude-3-5-sonnet-20240620-v1:0";
    server.add_route(
        "POST",
        "/model/anthropic.claude-3-5-sonnet-20240620-v1:0/converse",
        MockHttpResponse::json(
            403,
            &serde_json::json!({
                "__type": "AccessDeniedException",
                "message": "The security token included in the request is invalid."
            }),
        ),
    );

    let mut entry = make_model_entry("amazon-bedrock", bedrock_model, &server.base_url());
    entry.model.api = "bedrock-converse-stream".to_string();
    let provider = create_provider(&entry, None).expect("amazon-bedrock provider");
    let context = Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text("Ping".to_string()),
            timestamp: 0,
        })]
        .into(),
        tools: Vec::new().into(),
    };
    let options = StreamOptions {
        api_key: Some("bedrock-test-token".to_string()),
        ..Default::default()
    };

    let err = expect_provider_stream_start_error(provider, context, options);
    let msg = err.to_string();
    assert!(msg.contains("HTTP 403"), "{msg}");
    assert!(msg.contains("AccessDeniedException"), "{msg}");
    assert!(
        msg.contains("security token included in the request is invalid"),
        "{msg}"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "expected exactly one request");
    assert_eq!(
        request_header(&requests[0].headers, "authorization").as_deref(),
        Some("Bearer bedrock-test-token")
    );
}

#[test]
fn bedrock_provider_surfaces_rate_limit_error_without_live_credentials() {
    let harness =
        TestHarness::new("bedrock_provider_surfaces_rate_limit_error_without_live_credentials");
    let server = harness.start_mock_http_server();
    let bedrock_model = "anthropic.claude-3-5-sonnet-20240620-v1:0";
    server.add_route(
        "POST",
        "/model/anthropic.claude-3-5-sonnet-20240620-v1:0/converse",
        MockHttpResponse::json(
            429,
            &serde_json::json!({
                "__type": "ThrottlingException",
                "message": "Rate exceeded for Converse. Retry later."
            }),
        ),
    );

    let mut entry = make_model_entry("amazon-bedrock", bedrock_model, &server.base_url());
    entry.model.api = "bedrock-converse-stream".to_string();
    let provider = create_provider(&entry, None).expect("amazon-bedrock provider");
    let context = Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text("Ping".to_string()),
            timestamp: 0,
        })]
        .into(),
        tools: Vec::new().into(),
    };
    let options = StreamOptions {
        api_key: Some("bedrock-test-token".to_string()),
        ..Default::default()
    };

    let err = expect_provider_stream_start_error(provider, context, options);
    let msg = err.to_string();
    assert!(msg.contains("HTTP 429"), "{msg}");
    assert!(msg.contains("ThrottlingException"), "{msg}");
    assert!(msg.contains("Rate exceeded for Converse"), "{msg}");

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "expected exactly one request");
    assert_eq!(
        request_header(&requests[0].headers, "authorization").as_deref(),
        Some("Bearer bedrock-test-token")
    );
}

#[test]
fn create_provider_cloudflare_workers_ai_routes_via_openai_compat() {
    let harness =
        TestHarness::new("create_provider_cloudflare_workers_ai_routes_via_openai_compat");
    let mut entry = make_model_entry(
        "cloudflare-workers-ai",
        "@cf/meta/llama-3.1-8b-instruct",
        "https://api.cloudflare.com/client/v4/accounts/test-account/ai/v1",
    );
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("cloudflare-workers-ai provider");
    harness
        .log()
        .info_ctx("provider", "cloudflare workers route", |ctx| {
            ctx.push(("name".to_string(), provider.name().to_string()));
            ctx.push(("api".to_string(), provider.api().to_string()));
            ctx.push(("model".to_string(), provider.model_id().to_string()));
        });
    assert_eq!(provider.name(), "cloudflare-workers-ai");
    assert_eq!(provider.api(), "openai-completions");
    assert_eq!(provider.model_id(), "@cf/meta/llama-3.1-8b-instruct");
}

#[test]
fn create_provider_cloudflare_ai_gateway_routes_via_openai_compat() {
    let harness =
        TestHarness::new("create_provider_cloudflare_ai_gateway_routes_via_openai_compat");
    let mut entry = make_model_entry(
        "cloudflare-ai-gateway",
        "gpt-4o-mini",
        "https://gateway.ai.cloudflare.com/v1/account-id/gateway-id/openai",
    );
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("cloudflare-ai-gateway provider");
    harness
        .log()
        .info_ctx("provider", "cloudflare gateway route", |ctx| {
            ctx.push(("name".to_string(), provider.name().to_string()));
            ctx.push(("api".to_string(), provider.api().to_string()));
            ctx.push(("model".to_string(), provider.model_id().to_string()));
        });
    assert_eq!(provider.name(), "cloudflare-ai-gateway");
    assert_eq!(provider.api(), "openai-completions");
    assert_eq!(provider.model_id(), "gpt-4o-mini");
}

#[test]
fn wave_a_presets_resolve_openai_compat_defaults_and_factory_route() {
    let harness =
        TestHarness::new("wave_a_presets_resolve_openai_compat_defaults_and_factory_route");
    for (provider_id, expected_base_url) in WAVE_A_PRESET_CASES {
        let defaults = provider_routing_defaults(provider_id)
            .unwrap_or_else(|| panic!("missing metadata defaults for {provider_id}"));
        harness
            .log()
            .info_ctx("wave_a.defaults", "metadata defaults", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
                ctx.push(("api".to_string(), defaults.api.to_string()));
                ctx.push(("base_url".to_string(), defaults.base_url.to_string()));
                ctx.push(("auth_header".to_string(), defaults.auth_header.to_string()));
            });
        assert_eq!(defaults.api, "openai-completions");
        assert_eq!(defaults.base_url, expected_base_url);
        assert!(defaults.auth_header);

        let mut entry = make_model_entry(provider_id, "wave-a-default-model", expected_base_url);
        entry.model.api.clear();
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider should route {provider_id}: {e}"));
        harness
            .log()
            .info_ctx("wave_a.factory", "factory route", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
                ctx.push(("name".to_string(), provider.name().to_string()));
                ctx.push(("api".to_string(), provider.api().to_string()));
            });
        assert_eq!(provider.name(), provider_id);
        assert_eq!(provider.api(), "openai-completions");
        assert_eq!(provider.model_id(), "wave-a-default-model");
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn wave_a_openai_compat_streams_use_chat_completions_path_and_bearer_auth() {
    let harness =
        TestHarness::new("wave_a_openai_compat_streams_use_chat_completions_path_and_bearer_auth");
    for (index, (provider_id, _)) in WAVE_A_PRESET_CASES.into_iter().enumerate() {
        let server = harness.start_mock_http_server();
        let path_prefix = format!("/wave-a/{index}/{}", provider_id.replace('-', "_"));
        let expected_path = format!("{path_prefix}/chat/completions");
        server.add_route(
            "POST",
            &expected_path,
            text_event_stream_response(openai_chat_sse_body()),
        );

        let mut entry = make_model_entry(
            provider_id,
            "wave-a-stream-model",
            &format!("{}{}", server.base_url(), path_prefix),
        );
        entry.model.api.clear();
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider should stream-route {provider_id}: {e}"));

        let api_key = format!("wave-a-token-{index}");
        let context = Context {
            system_prompt: Some("Be concise.".to_string().into()),
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions {
            api_key: Some(api_key.clone()),
            max_tokens: Some(64),
            ..Default::default()
        };
        drive_provider_stream_to_done(provider, context, options);

        let requests = server.requests();
        assert_eq!(
            requests.len(),
            1,
            "expected exactly one request for {provider_id}"
        );
        let request = &requests[0];
        harness
            .log()
            .info_ctx("wave_a.stream", "request lock", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
                ctx.push(("path".to_string(), request.path.clone()));
                ctx.push((
                    "authorization".to_string(),
                    request_header(&request.headers, "authorization").unwrap_or_default(),
                ));
            });
        assert_eq!(request.path, expected_path);
        let expected_auth = format!("Bearer {api_key}");
        assert_eq!(
            request_header(&request.headers, "authorization").as_deref(),
            Some(expected_auth.as_str())
        );
        assert_eq!(
            request_header(&request.headers, "content-type").as_deref(),
            Some("application/json")
        );
    }
}

#[test]
fn wave_b1_presets_resolve_metadata_defaults_and_factory_route() {
    let harness = TestHarness::new("wave_b1_presets_resolve_metadata_defaults_and_factory_route");
    for (provider_id, expected_api, expected_base_url, expected_auth_header) in WAVE_B1_PRESET_CASES
    {
        let defaults = provider_routing_defaults(provider_id)
            .unwrap_or_else(|| panic!("missing metadata defaults for {provider_id}"));
        harness
            .log()
            .info_ctx("wave_b1.defaults", "metadata defaults", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
                ctx.push(("api".to_string(), defaults.api.to_string()));
                ctx.push(("base_url".to_string(), defaults.base_url.to_string()));
                ctx.push(("auth_header".to_string(), defaults.auth_header.to_string()));
            });
        assert_eq!(defaults.api, expected_api);
        assert_eq!(defaults.base_url, expected_base_url);
        assert_eq!(defaults.auth_header, expected_auth_header);
        assert_eq!(canonical_provider_id(provider_id), Some(provider_id));

        let mut entry = make_model_entry(provider_id, "wave-b1-default-model", expected_base_url);
        entry.model.api.clear();
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider should route {provider_id}: {e}"));
        harness
            .log()
            .info_ctx("wave_b1.factory", "factory route", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
                ctx.push(("name".to_string(), provider.name().to_string()));
                ctx.push(("api".to_string(), provider.api().to_string()));
            });
        assert_eq!(provider.name(), provider_id);
        assert_eq!(provider.api(), expected_api);
        assert_eq!(provider.model_id(), "wave-b1-default-model");
    }
}

#[test]
fn wave_b1_alibaba_cn_openai_compat_streams_use_chat_completions_path_and_bearer_auth() {
    let harness = TestHarness::new(
        "wave_b1_alibaba_cn_openai_compat_streams_use_chat_completions_path_and_bearer_auth",
    );
    let server = harness.start_mock_http_server();
    let path_prefix = "/wave-b1/alibaba_cn";
    let expected_path = format!("{path_prefix}/chat/completions");
    server.add_route(
        "POST",
        &expected_path,
        text_event_stream_response(openai_chat_sse_body()),
    );

    let mut entry = make_model_entry(
        "alibaba-cn",
        "wave-b1-alibaba-cn-model",
        &format!("{}{}", server.base_url(), path_prefix),
    );
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("create_provider should route alibaba-cn");
    assert_eq!(provider.api(), "openai-completions");

    let api_key = "wave-b1-alibaba-cn-token".to_string();
    let context = Context {
        system_prompt: Some("Be concise.".to_string().into()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text("Ping".to_string()),
            timestamp: 0,
        })]
        .into(),
        tools: Vec::new().into(),
    };
    let options = StreamOptions {
        api_key: Some(api_key.clone()),
        max_tokens: Some(64),
        ..Default::default()
    };
    drive_provider_stream_to_done(provider, context, options);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "expected exactly one request");
    let request = &requests[0];
    assert_eq!(request.path, expected_path);
    let expected_auth = format!("Bearer {api_key}");
    assert_eq!(
        request_header(&request.headers, "authorization").as_deref(),
        Some(expected_auth.as_str())
    );
    assert_eq!(
        request_header(&request.headers, "content-type").as_deref(),
        Some("application/json")
    );
}

#[test]
fn wave_b1_anthropic_compat_streams_use_messages_path_and_x_api_key() {
    let harness =
        TestHarness::new("wave_b1_anthropic_compat_streams_use_messages_path_and_x_api_key");
    for (index, (provider_id, expected_api, _, expected_auth_header)) in
        WAVE_B1_PRESET_CASES.into_iter().enumerate()
    {
        if expected_api != "anthropic-messages" {
            continue;
        }
        let server = harness.start_mock_http_server();
        let path_prefix = format!("/wave-b1/{index}/{}", provider_id.replace('-', "_"));
        let expected_path = format!("{path_prefix}/v1/messages");
        server.add_route(
            "POST",
            &expected_path,
            text_event_stream_response(anthropic_messages_sse_body()),
        );

        let mut entry = make_model_entry(
            provider_id,
            "wave-b1-anthropic-model",
            &format!("{}{}", server.base_url(), path_prefix),
        );
        entry.model.api.clear();
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider should route {provider_id}: {e}"));
        assert_eq!(provider.name(), provider_id);
        assert_eq!(provider.api(), "anthropic-messages");

        let api_key = format!("sk-wave-b1-anthropic-token-{index}");
        let context = Context {
            system_prompt: Some("Be concise.".to_string().into()),
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions {
            api_key: Some(api_key.clone()),
            max_tokens: Some(64),
            ..Default::default()
        };
        drive_provider_stream_to_done(provider, context, options);

        let requests = server.requests();
        assert_eq!(
            requests.len(),
            1,
            "expected exactly one request for {provider_id}"
        );
        let request = &requests[0];
        assert_eq!(request.path, expected_path);
        assert_eq!(
            request_header(&request.headers, "x-api-key").as_deref(),
            Some(api_key.as_str())
        );
        assert_eq!(
            request_header(&request.headers, "content-type").as_deref(),
            Some("application/json")
        );
        assert!(
            !expected_auth_header,
            "anthropic fallbacks should not use bearer auth"
        );
    }
}

#[test]
fn wave_b1_family_coherence_with_existing_moonshot_and_alibaba_mappings() {
    let harness =
        TestHarness::new("wave_b1_family_coherence_with_existing_moonshot_and_alibaba_mappings");
    harness
        .log()
        .info_ctx("wave_b1.coherence", "canonical mapping", |ctx| {
            ctx.push((
                "kimi_alias".to_string(),
                canonical_provider_id("kimi")
                    .unwrap_or("missing")
                    .to_string(),
            ));
            ctx.push((
                "kimi_for_coding".to_string(),
                canonical_provider_id("kimi-for-coding")
                    .unwrap_or("missing")
                    .to_string(),
            ));
            ctx.push((
                "alibaba".to_string(),
                canonical_provider_id("alibaba")
                    .unwrap_or("missing")
                    .to_string(),
            ));
            ctx.push((
                "alibaba_cn".to_string(),
                canonical_provider_id("alibaba-cn")
                    .unwrap_or("missing")
                    .to_string(),
            ));
            ctx.push((
                "alibaba_us".to_string(),
                canonical_provider_id("alibaba-us")
                    .unwrap_or("missing")
                    .to_string(),
            ));
        });
    assert_eq!(canonical_provider_id("kimi"), Some("moonshotai"));
    assert_eq!(
        canonical_provider_id("kimi-for-coding"),
        Some("kimi-for-coding")
    );
    assert_eq!(canonical_provider_id("alibaba"), Some("alibaba"));
    assert_eq!(canonical_provider_id("alibaba-cn"), Some("alibaba-cn"));
    assert_eq!(canonical_provider_id("alibaba-us"), Some("alibaba-us"));

    let alibaba = provider_routing_defaults("alibaba").expect("alibaba defaults");
    let alibaba_cn = provider_routing_defaults("alibaba-cn").expect("alibaba-cn defaults");
    let alibaba_us = provider_routing_defaults("alibaba-us").expect("alibaba-us defaults");
    assert_eq!(
        provider_auth_env_keys("alibaba"),
        &["DASHSCOPE_API_KEY", "QWEN_API_KEY"]
    );
    assert_eq!(provider_auth_env_keys("alibaba-cn"), &["DASHSCOPE_API_KEY"]);
    assert_eq!(
        provider_auth_env_keys("alibaba-us"),
        &["DASHSCOPE_API_KEY", "QWEN_API_KEY"]
    );
    assert_ne!(alibaba.base_url, alibaba_cn.base_url);
    assert_ne!(alibaba.base_url, alibaba_us.base_url);
    assert_ne!(alibaba_cn.base_url, alibaba_us.base_url);
}

#[test]
fn wave_b2_presets_resolve_metadata_defaults_and_factory_route() {
    let harness = TestHarness::new("wave_b2_presets_resolve_metadata_defaults_and_factory_route");
    for (provider_id, expected_api, expected_base_url, expected_auth_header) in WAVE_B2_PRESET_CASES
    {
        let defaults = provider_routing_defaults(provider_id)
            .unwrap_or_else(|| panic!("missing metadata defaults for {provider_id}"));
        harness
            .log()
            .info_ctx("wave_b2.defaults", "metadata defaults", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
                ctx.push(("api".to_string(), defaults.api.to_string()));
                ctx.push(("base_url".to_string(), defaults.base_url.to_string()));
                ctx.push(("auth_header".to_string(), defaults.auth_header.to_string()));
            });
        assert_eq!(defaults.api, expected_api);
        assert_eq!(defaults.base_url, expected_base_url);
        assert_eq!(defaults.auth_header, expected_auth_header);
        assert_eq!(canonical_provider_id(provider_id), Some(provider_id));

        let mut entry = make_model_entry(provider_id, "wave-b2-default-model", expected_base_url);
        entry.model.api.clear();
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider should route {provider_id}: {e}"));
        harness
            .log()
            .info_ctx("wave_b2.factory", "factory route", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
                ctx.push(("name".to_string(), provider.name().to_string()));
                ctx.push(("api".to_string(), provider.api().to_string()));
            });
        assert_eq!(provider.name(), provider_id);
        assert_eq!(provider.api(), expected_api);
        assert_eq!(provider.model_id(), "wave-b2-default-model");
    }
}

#[test]
fn wave_b2_openai_compat_streams_use_chat_completions_path_and_bearer_auth() {
    let harness =
        TestHarness::new("wave_b2_openai_compat_streams_use_chat_completions_path_and_bearer_auth");
    for (index, (provider_id, expected_api, _, expected_auth_header)) in
        WAVE_B2_PRESET_CASES.into_iter().enumerate()
    {
        let server = harness.start_mock_http_server();
        let path_prefix = format!("/wave-b2/{index}/{}", provider_id.replace('-', "_"));
        let expected_path = format!("{path_prefix}/chat/completions");
        server.add_route(
            "POST",
            &expected_path,
            text_event_stream_response(openai_chat_sse_body()),
        );

        let mut entry = make_model_entry(
            provider_id,
            "wave-b2-openai-model",
            &format!("{}{}", server.base_url(), path_prefix),
        );
        entry.model.api.clear();
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider should route {provider_id}: {e}"));
        assert_eq!(provider.api(), expected_api);

        let api_key = format!("wave-b2-openai-token-{index}");
        let context = Context {
            system_prompt: Some("Be concise.".to_string().into()),
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions {
            api_key: Some(api_key.clone()),
            max_tokens: Some(64),
            ..Default::default()
        };
        drive_provider_stream_to_done(provider, context, options);

        let requests = server.requests();
        assert_eq!(
            requests.len(),
            1,
            "expected exactly one request for {provider_id}"
        );
        let request = &requests[0];
        assert_eq!(request.path, expected_path);
        let expected_auth = format!("Bearer {api_key}");
        assert_eq!(
            request_header(&request.headers, "authorization").as_deref(),
            Some(expected_auth.as_str())
        );
        assert_eq!(
            request_header(&request.headers, "content-type").as_deref(),
            Some("application/json")
        );
        assert!(
            expected_auth_header,
            "openai-compatible B2 providers should use bearer auth"
        );
    }
}

#[test]
fn wave_b2_moonshot_cn_and_global_moonshot_mapping_are_distinct() {
    let global_defaults = provider_routing_defaults("moonshotai").expect("moonshotai defaults");
    let cn_defaults = provider_routing_defaults("moonshotai-cn").expect("moonshotai-cn defaults");

    assert_eq!(canonical_provider_id("moonshot"), Some("moonshotai"));
    assert_eq!(
        canonical_provider_id("moonshotai-cn"),
        Some("moonshotai-cn")
    );
    assert_eq!(
        provider_auth_env_keys("moonshotai"),
        &["MOONSHOT_API_KEY", "KIMI_API_KEY"]
    );
    assert_eq!(
        provider_auth_env_keys("moonshotai-cn"),
        &["MOONSHOT_API_KEY"]
    );
    assert_ne!(global_defaults.base_url, cn_defaults.base_url);
    assert_eq!(global_defaults.api, "openai-completions");
    assert_eq!(cn_defaults.api, "openai-completions");
}

#[test]
fn wave_b3_presets_resolve_metadata_defaults_and_factory_route() {
    let harness = TestHarness::new("wave_b3_presets_resolve_metadata_defaults_and_factory_route");
    for (provider_id, expected_api, expected_base_url, expected_auth_header) in WAVE_B3_PRESET_CASES
    {
        let defaults = provider_routing_defaults(provider_id)
            .unwrap_or_else(|| panic!("missing metadata defaults for {provider_id}"));
        harness
            .log()
            .info_ctx("wave_b3.defaults", "metadata defaults", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
                ctx.push(("api".to_string(), defaults.api.to_string()));
                ctx.push(("base_url".to_string(), defaults.base_url.to_string()));
                ctx.push(("auth_header".to_string(), defaults.auth_header.to_string()));
            });
        assert_eq!(defaults.api, expected_api);
        assert_eq!(defaults.base_url, expected_base_url);
        assert_eq!(defaults.auth_header, expected_auth_header);
        assert_eq!(canonical_provider_id(provider_id), Some(provider_id));

        let mut entry = make_model_entry(provider_id, "wave-b3-default-model", expected_base_url);
        entry.model.api.clear();
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider should route {provider_id}: {e}"));
        harness
            .log()
            .info_ctx("wave_b3.factory", "factory route", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
                ctx.push(("name".to_string(), provider.name().to_string()));
                ctx.push(("api".to_string(), provider.api().to_string()));
            });
        assert_eq!(provider.name(), provider_id);
        assert_eq!(provider.api(), expected_api);
        assert_eq!(provider.model_id(), "wave-b3-default-model");
    }
}

#[test]
fn wave_b3_openai_compat_streams_use_chat_completions_path_and_bearer_auth() {
    let harness =
        TestHarness::new("wave_b3_openai_compat_streams_use_chat_completions_path_and_bearer_auth");
    for (index, (provider_id, expected_api, _, expected_auth_header)) in
        WAVE_B3_PRESET_CASES.into_iter().enumerate()
    {
        let server = harness.start_mock_http_server();
        let path_prefix = format!("/wave-b3/{index}/{}", provider_id.replace('-', "_"));
        let expected_path = format!("{path_prefix}/chat/completions");
        server.add_route(
            "POST",
            &expected_path,
            text_event_stream_response(openai_chat_sse_body()),
        );

        let mut entry = make_model_entry(
            provider_id,
            "wave-b3-openai-model",
            &format!("{}{}", server.base_url(), path_prefix),
        );
        entry.model.api.clear();
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider should route {provider_id}: {e}"));
        assert_eq!(provider.api(), expected_api);

        let api_key = format!("wave-b3-openai-token-{index}");
        let context = Context {
            system_prompt: Some("Be concise.".to_string().into()),
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions {
            api_key: Some(api_key.clone()),
            max_tokens: Some(64),
            ..Default::default()
        };
        drive_provider_stream_to_done(provider, context, options);

        let requests = server.requests();
        assert_eq!(
            requests.len(),
            1,
            "expected exactly one request for {provider_id}"
        );
        let request = &requests[0];
        assert_eq!(request.path, expected_path);
        let expected_auth = format!("Bearer {api_key}");
        assert_eq!(
            request_header(&request.headers, "authorization").as_deref(),
            Some(expected_auth.as_str())
        );
        assert_eq!(
            request_header(&request.headers, "content-type").as_deref(),
            Some("application/json")
        );
        assert!(
            expected_auth_header,
            "openai-compatible B3 providers should use bearer auth"
        );
    }
}

#[test]
fn wave_b3_family_and_coding_plan_variants_are_distinct() {
    let siliconflow = provider_routing_defaults("siliconflow").expect("siliconflow defaults");
    let siliconflow_cn =
        provider_routing_defaults("siliconflow-cn").expect("siliconflow-cn defaults");
    assert_eq!(canonical_provider_id("siliconflow"), Some("siliconflow"));
    assert_eq!(
        canonical_provider_id("siliconflow-cn"),
        Some("siliconflow-cn")
    );
    assert_eq!(
        provider_auth_env_keys("siliconflow"),
        &["SILICONFLOW_API_KEY"]
    );
    assert_eq!(
        provider_auth_env_keys("siliconflow-cn"),
        &["SILICONFLOW_CN_API_KEY"]
    );
    assert_ne!(siliconflow.base_url, siliconflow_cn.base_url);

    let zai = provider_routing_defaults("zai").expect("zai defaults");
    let zai_coding = provider_routing_defaults("zai-coding-plan").expect("zai-coding defaults");
    assert_eq!(canonical_provider_id("zai"), Some("zai"));
    assert_eq!(
        canonical_provider_id("zai-coding-plan"),
        Some("zai-coding-plan")
    );
    assert_eq!(provider_auth_env_keys("zai"), &["ZHIPU_API_KEY"]);
    assert_eq!(
        provider_auth_env_keys("zai-coding-plan"),
        &["ZHIPU_API_KEY"]
    );
    assert_eq!(zai.api, "openai-completions");
    assert_eq!(zai_coding.api, "openai-completions");
    assert_ne!(zai.base_url, zai_coding.base_url);

    let zhipu = provider_routing_defaults("zhipuai").expect("zhipu defaults");
    let zhipu_coding =
        provider_routing_defaults("zhipuai-coding-plan").expect("zhipu-coding defaults");
    assert_eq!(canonical_provider_id("zhipuai"), Some("zhipuai"));
    assert_eq!(
        canonical_provider_id("zhipuai-coding-plan"),
        Some("zhipuai-coding-plan")
    );
    assert_eq!(provider_auth_env_keys("zhipuai"), &["ZHIPU_API_KEY"]);
    assert_eq!(
        provider_auth_env_keys("zhipuai-coding-plan"),
        &["ZHIPU_API_KEY"]
    );
    assert_eq!(zhipu.api, "openai-completions");
    assert_eq!(zhipu_coding.api, "openai-completions");
    assert_ne!(zhipu.base_url, zhipu_coding.base_url);
}

#[test]
fn special_routing_presets_resolve_metadata_defaults_and_factory_route() {
    let harness =
        TestHarness::new("special_routing_presets_resolve_metadata_defaults_and_factory_route");
    for (provider_id, expected_api, expected_base_url, expected_auth_header) in
        SPECIAL_ROUTING_CASES
    {
        let defaults = provider_routing_defaults(provider_id)
            .unwrap_or_else(|| panic!("missing metadata defaults for {provider_id}"));
        harness
            .log()
            .info_ctx("special.defaults", "metadata defaults", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
                ctx.push(("api".to_string(), defaults.api.to_string()));
                ctx.push(("base_url".to_string(), defaults.base_url.to_string()));
                ctx.push(("auth_header".to_string(), defaults.auth_header.to_string()));
            });
        assert_eq!(defaults.api, expected_api);
        assert_eq!(defaults.base_url, expected_base_url);
        assert_eq!(defaults.auth_header, expected_auth_header);
        assert_eq!(canonical_provider_id(provider_id), Some(provider_id));

        let mut entry = make_model_entry(
            provider_id,
            "special-routing-default-model",
            expected_base_url,
        );
        entry.model.api.clear();
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider should route {provider_id}: {e}"));

        assert_eq!(provider.name(), provider_id);
        assert_eq!(provider.api(), expected_api);
        assert_eq!(provider.model_id(), "special-routing-default-model");
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn special_routing_default_streams_cover_success_paths() {
    let harness = TestHarness::new("special_routing_default_streams_cover_success_paths");

    for (index, (provider_id, expected_api, _, expected_auth_header)) in
        SPECIAL_ROUTING_CASES.into_iter().enumerate()
    {
        if expected_api != "openai-completions" {
            continue;
        }

        let server = harness.start_mock_http_server();
        let path_prefix = format!("/special/{index}/{}", provider_id.replace('-', "_"));
        let expected_path = format!("{path_prefix}/chat/completions");
        server.add_route(
            "POST",
            &expected_path,
            text_event_stream_response(openai_chat_sse_body()),
        );

        let mut entry = make_model_entry(
            provider_id,
            "special-openai-stream-model",
            &format!("{}{}", server.base_url(), path_prefix),
        );
        entry.model.api.clear();
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider should route {provider_id}: {e}"));
        assert_eq!(provider.api(), expected_api);

        let api_key = format!("special-openai-token-{index}");
        let context = Context {
            system_prompt: Some("Be concise.".to_string().into()),
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions {
            api_key: Some(api_key.clone()),
            max_tokens: Some(64),
            ..Default::default()
        };
        drive_provider_stream_to_done(provider, context, options);

        let requests = server.requests();
        assert_eq!(
            requests.len(),
            1,
            "expected exactly one request for {provider_id}"
        );
        let request = &requests[0];
        assert_eq!(request.path, expected_path);
        let expected_auth = format!("Bearer {api_key}");
        assert_eq!(
            request_header(&request.headers, "authorization").as_deref(),
            Some(expected_auth.as_str())
        );
        assert_eq!(
            request_header(&request.headers, "content-type").as_deref(),
            Some("application/json")
        );
        assert!(expected_auth_header);
    }

    let server = harness.start_mock_http_server();
    let path_prefix = "/special/zenmux";
    let expected_path = "/special/zenmux/v1/messages";
    server.add_route(
        "POST",
        expected_path,
        text_event_stream_response(anthropic_messages_sse_body()),
    );

    let mut entry = make_model_entry(
        "zenmux",
        "special-anthropic-stream-model",
        &format!("{}{path_prefix}", server.base_url()),
    );
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("create_provider should route zenmux");
    assert_eq!(provider.api(), "anthropic-messages");
    assert_eq!(provider.name(), "zenmux");

    let api_key = "special-zenmux-token".to_string();
    let context = Context {
        system_prompt: Some("Be concise.".to_string().into()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text("Ping".to_string()),
            timestamp: 0,
        })]
        .into(),
        tools: Vec::new().into(),
    };
    let options = StreamOptions {
        api_key: Some(api_key.clone()),
        max_tokens: Some(64),
        ..Default::default()
    };
    drive_provider_stream_to_done(provider, context, options);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "expected exactly one request for zenmux");
    let request = &requests[0];
    assert_eq!(request.path, expected_path);
    assert_eq!(
        request_header(&request.headers, "x-api-key").as_deref(),
        Some(api_key.as_str())
    );
    assert!(request_header(&request.headers, "authorization").is_none());
    assert_eq!(
        request_header(&request.headers, "anthropic-version").as_deref(),
        Some("2023-06-01")
    );
}

#[test]
fn special_routing_metadata_api_overrides_change_route_kind() {
    for provider_id in ["opencode", "vercel"] {
        let harness = TestHarness::new(format!("special_override_{provider_id}_openai_responses"));
        let server = harness.start_mock_http_server();
        let path_prefix = format!("/override/{provider_id}");
        let expected_path = format!("{path_prefix}/responses");
        server.add_route(
            "POST",
            &expected_path,
            text_event_stream_response(openai_responses_sse_body()),
        );

        let mut entry = make_model_entry(
            provider_id,
            "special-override-model",
            &format!("{}{}", server.base_url(), path_prefix),
        );
        entry.model.api = "openai-responses".to_string();

        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("override route failed for {provider_id}: {e}"));
        assert_eq!(provider.api(), "openai-responses");

        let context = Context {
            system_prompt: Some("Be concise.".to_string().into()),
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = StreamOptions {
            api_key: Some("override-token".to_string()),
            max_tokens: Some(64),
            ..Default::default()
        };
        drive_provider_stream_to_done(provider, context, options);

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, expected_path);
    }

    let harness = TestHarness::new("special_override_zenmux_openai_completions");
    let server = harness.start_mock_http_server();
    let expected_path = "/override/zenmux/chat/completions";
    server.add_route(
        "POST",
        expected_path,
        text_event_stream_response(openai_chat_sse_body()),
    );

    let mut entry = make_model_entry(
        "zenmux",
        "special-override-model",
        &format!("{}/override/zenmux", server.base_url()),
    );
    entry.model.api = "openai-completions".to_string();

    let provider = create_provider(&entry, None).expect("override route failed for zenmux");
    assert_eq!(provider.api(), "openai-completions");
    assert_eq!(provider.name(), "zenmux");

    let context = Context {
        system_prompt: Some("Be concise.".to_string().into()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text("Ping".to_string()),
            timestamp: 0,
        })]
        .into(),
        tools: Vec::new().into(),
    };
    let options = StreamOptions {
        api_key: Some("override-zenmux-token".to_string()),
        max_tokens: Some(64),
        ..Default::default()
    };
    drive_provider_stream_to_done(provider, context, options);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, expected_path);
    assert_eq!(
        request_header(&requests[0].headers, "authorization").as_deref(),
        Some("Bearer override-zenmux-token")
    );
}

#[test]
fn special_routing_unsupported_api_reports_provider_mismatch() {
    for provider_id in ["opencode", "vercel", "zenmux"] {
        let mut entry = make_model_entry(provider_id, "bad-model", "https://example.invalid/v1");
        entry.model.api = "unsupported-api-family".to_string();
        let Err(err) = create_provider(&entry, None) else {
            panic!("unsupported api override should fail with route diagnostic");
        };
        let msg = err.to_string();
        assert!(
            msg.contains(provider_id),
            "error for {provider_id} should include provider id, got: {msg}"
        );
        assert!(
            msg.contains("unsupported-api-family"),
            "error for {provider_id} should include api identifier, got: {msg}"
        );
    }
}
#[test]
fn fireworks_ai_alias_migration_matches_fireworks_canonical_defaults() {
    let harness =
        TestHarness::new("fireworks_ai_alias_migration_matches_fireworks_canonical_defaults");

    let fireworks_defaults =
        provider_routing_defaults("fireworks").expect("fireworks defaults should exist");
    let alias_defaults = provider_routing_defaults("fireworks-ai")
        .expect("fireworks-ai alias defaults should exist");
    harness
        .log()
        .info_ctx("alias", "fireworks migration", |ctx| {
            ctx.push((
                "canonical".to_string(),
                canonical_provider_id("fireworks")
                    .unwrap_or("missing")
                    .to_string(),
            ));
            ctx.push((
                "alias".to_string(),
                canonical_provider_id("fireworks-ai")
                    .unwrap_or("missing")
                    .to_string(),
            ));
            ctx.push((
                "canonical_base".to_string(),
                fireworks_defaults.base_url.to_string(),
            ));
            ctx.push((
                "alias_base".to_string(),
                alias_defaults.base_url.to_string(),
            ));
        });
    assert_eq!(canonical_provider_id("fireworks"), Some("fireworks"));
    assert_eq!(canonical_provider_id("fireworks-ai"), Some("fireworks"));
    assert_eq!(fireworks_defaults.api, alias_defaults.api);
    assert_eq!(fireworks_defaults.base_url, alias_defaults.base_url);
    assert_eq!(
        provider_auth_env_keys("fireworks"),
        provider_auth_env_keys("fireworks-ai")
    );

    let auth_path = harness.temp_path("auth.json");
    let mut auth = AuthStorage::load(auth_path).expect("load auth storage");
    auth.set(
        "fireworks",
        AuthCredential::ApiKey {
            key: "fireworks-schema-key".to_string(),
        },
    );
    auth.save().expect("save auth storage");

    let models_path = harness.create_file(
        "models.json",
        r#"{
  "providers": {
    "fireworks-ai": {
      "models": [{ "id": "accounts/fireworks/models/llama-v3p3-70b-instruct" }]
    }
  }
}"#,
    );
    let registry = ModelRegistry::load(&auth, Some(models_path));
    assert!(
        registry.error().is_none(),
        "unexpected models load error: {:?}",
        registry.error()
    );
    let entry = registry
        .find(
            "fireworks-ai",
            "accounts/fireworks/models/llama-v3p3-70b-instruct",
        )
        .expect("fireworks-ai schema model should load");
    assert_eq!(entry.model.provider, "fireworks-ai");
    assert_eq!(entry.model.api, "openai-completions");
    assert_eq!(
        entry.model.base_url,
        "https://api.fireworks.ai/inference/v1"
    );
    assert_eq!(entry.api_key.as_deref(), Some("fireworks-schema-key"));
    assert!(entry.auth_header);

    let provider = create_provider(&entry, None).expect("create fireworks-ai provider");
    assert_eq!(provider.name(), "fireworks-ai");
    assert_eq!(provider.api(), "openai-completions");
}

#[test]
fn create_provider_rejects_unknown_provider() {
    let harness = TestHarness::new("create_provider_rejects_unknown_provider");
    let entry = make_model_entry("mystery", "mystery-model", "https://example.com/v1");
    let err = create_provider(&entry, None)
        .err()
        .expect("expected unknown provider error");
    harness.log().info_ctx("provider", "unknown error", |ctx| {
        ctx.push(("error".to_string(), err.to_string()));
    });

    match err {
        Error::Provider { provider, message } => {
            assert_eq!(provider, "mystery");
            assert!(message.contains("not implemented"));
        }
        other => unreachable!("unexpected error: {other}"),
    }
}

#[test]
fn api_display_and_from_str_round_trip() {
    let harness = TestHarness::new("api_display_and_from_str_round_trip");
    let cases = vec![
        (Api::AnthropicMessages, "anthropic-messages"),
        (Api::OpenAICompletions, "openai-completions"),
        (Api::OpenAIResponses, "openai-responses"),
        (Api::AzureOpenAIResponses, "azure-openai-responses"),
        (Api::BedrockConverseStream, "bedrock-converse-stream"),
        (Api::GoogleGenerativeAI, "google-generative-ai"),
        (Api::GoogleGeminiCli, "google-gemini-cli"),
        (Api::GoogleVertex, "google-vertex"),
        (Api::Custom("custom-api".to_string()), "custom-api"),
    ];

    for (api, expected) in cases {
        harness.log().info_ctx("api", "round trip", |ctx| {
            ctx.push(("expected".to_string(), expected.to_string()));
            ctx.push(("display".to_string(), api.to_string()));
        });
        assert_eq!(api.to_string(), expected);
        let parsed = Api::from_str(expected).expect("parse api");
        assert_eq!(parsed, api);
    }
}

#[test]
fn api_from_str_empty_rejected() {
    let harness = TestHarness::new("api_from_str_empty_rejected");
    let err = Api::from_str("").expect_err("expected empty api error");
    harness.log().info_ctx("api", "empty error", |ctx| {
        ctx.push(("error".to_string(), err.clone()));
    });
    assert!(err.contains("empty"));
}

#[test]
fn api_from_str_noncanonical_values_become_custom() {
    let harness = TestHarness::new("api_from_str_noncanonical_values_become_custom");
    for input in ["cohere-v2", "google-generativeai"] {
        let parsed = Api::from_str(input).expect("parse as custom api");
        harness.log().info_ctx("api", "noncanonical parse", |ctx| {
            ctx.push(("input".to_string(), input.to_string()));
            ctx.push(("parsed".to_string(), parsed.to_string()));
        });
        assert_eq!(parsed, Api::Custom(input.to_string()));
    }
}

#[test]
fn known_provider_display_and_from_str_round_trip() {
    let harness = TestHarness::new("known_provider_display_and_from_str_round_trip");
    let cases = vec![
        (KnownProvider::Anthropic, "anthropic"),
        (KnownProvider::OpenAI, "openai"),
        (KnownProvider::Google, "google"),
        (KnownProvider::GoogleVertex, "google-vertex"),
        (KnownProvider::AmazonBedrock, "amazon-bedrock"),
        (KnownProvider::AzureOpenAI, "azure-openai"),
        (KnownProvider::GithubCopilot, "github-copilot"),
        (KnownProvider::XAI, "xai"),
        (KnownProvider::Groq, "groq"),
        (KnownProvider::Cerebras, "cerebras"),
        (KnownProvider::OpenRouter, "openrouter"),
        (KnownProvider::Mistral, "mistral"),
        (
            KnownProvider::Custom("custom-provider".to_string()),
            "custom-provider",
        ),
    ];

    for (provider, expected) in cases {
        harness.log().info_ctx("provider", "round trip", |ctx| {
            ctx.push(("expected".to_string(), expected.to_string()));
            ctx.push(("display".to_string(), provider.to_string()));
        });
        assert_eq!(provider.to_string(), expected);
        let parsed = KnownProvider::from_str(expected).expect("parse provider");
        assert_eq!(parsed, provider);
    }
}

#[test]
fn known_provider_from_str_empty_rejected() {
    let harness = TestHarness::new("known_provider_from_str_empty_rejected");
    let err = KnownProvider::from_str("").expect_err("expected empty provider error");
    harness.log().info_ctx("provider", "empty error", |ctx| {
        ctx.push(("error".to_string(), err.clone()));
    });
    assert!(err.contains("empty"));
}

#[test]
fn model_calculate_cost_zero_is_zero() {
    let harness = TestHarness::new("model_calculate_cost_zero_is_zero");
    let model = make_model_with_cost(ModelCost {
        input: 3.0,
        output: 6.0,
        cache_read: 1.0,
        cache_write: 2.0,
    });
    let cost = model.calculate_cost(0, 0, 0, 0);
    harness.log().info_ctx("cost", "zero tokens", |ctx| {
        ctx.push(("cost".to_string(), cost.to_string()));
    });
    assert!(cost.abs() <= f64::EPSILON);
}

#[test]
fn model_calculate_cost_matches_per_million_rates() {
    let harness = TestHarness::new("model_calculate_cost_matches_per_million_rates");
    let model = make_model_with_cost(ModelCost {
        input: 3.0,
        output: 6.0,
        cache_read: 1.0,
        cache_write: 2.0,
    });
    let input = 500_000;
    let output = 250_000;
    let cache_read = 100_000;
    let cache_write = 50_000;
    let expected = 3.2;
    let cost = model.calculate_cost(input, output, cache_read, cache_write);
    harness.log().info_ctx("cost", "typical tokens", |ctx| {
        ctx.push(("input".to_string(), input.to_string()));
        ctx.push(("output".to_string(), output.to_string()));
        ctx.push(("cache_read".to_string(), cache_read.to_string()));
        ctx.push(("cache_write".to_string(), cache_write.to_string()));
        ctx.push(("expected".to_string(), expected.to_string()));
        ctx.push(("actual".to_string(), cost.to_string()));
    });
    assert!((cost - expected).abs() < 1e-9);
}

#[test]
fn model_calculate_cost_is_monotonic() {
    let harness = TestHarness::new("model_calculate_cost_is_monotonic");
    let model = make_model_with_cost(ModelCost {
        input: 1.0,
        output: 1.0,
        cache_read: 1.0,
        cache_write: 1.0,
    });
    let base = model.calculate_cost(100, 100, 0, 0);
    let higher = model.calculate_cost(200, 150, 10, 5);
    harness.log().info_ctx("cost", "monotonic", |ctx| {
        ctx.push(("base".to_string(), base.to_string()));
        ctx.push(("higher".to_string(), higher.to_string()));
    });
    assert!(higher > base);
}

#[test]
fn stream_options_default_is_empty_and_safe() {
    let harness = TestHarness::new("stream_options_default_is_empty_and_safe");
    let options = StreamOptions::default();
    harness.log().info_ctx("stream_options", "defaults", |ctx| {
        ctx.push(("headers_len".to_string(), options.headers.len().to_string()));
        ctx.push((
            "cache_retention".to_string(),
            format!("{:?}", options.cache_retention),
        ));
    });
    assert!(options.temperature.is_none());
    assert!(options.max_tokens.is_none());
    assert!(options.api_key.is_none());
    assert!(options.session_id.is_none());
    assert!(options.thinking_level.is_none());
    assert!(options.thinking_budgets.is_none());
    assert!(options.headers.is_empty());
    assert_eq!(options.cache_retention, CacheRetention::None);
}

#[test]
fn input_type_rejects_unknown_values() {
    let harness = TestHarness::new("input_type_rejects_unknown_values");
    let err = serde_json::from_str::<InputType>("\"audio\"")
        .expect_err("expected unknown input type to fail");
    harness
        .log()
        .info_ctx("input_type", "invalid variant", |ctx| {
            ctx.push(("error".to_string(), err.to_string()));
        });
    assert!(err.to_string().contains("unknown variant"));
}

// ── Wave C2: omp-catalog delta presets (bd-cv653.7.3) ───────────────────
//
// Each case was machine-verified against omp's registry/catalog descriptors
// (docs/evidence/provider-catalog-diff-omp.json): api kind, base_url host,
// and auth env keys all come from the omp descriptor for the provider.

const WAVE_C2_PRESET_CASES: [(&str, &str, &str, bool); 8] = [
    (
        "gmi",
        "openai-completions",
        "https://api.gmi-serving.com/v1",
        true,
    ),
    (
        "coreweave",
        "openai-completions",
        "https://api.inference.wandb.ai/v1",
        true,
    ),
    (
        "sakana",
        "openai-responses",
        "https://api.sakana.ai/v1",
        true,
    ),
    (
        "wafer",
        "openai-completions",
        "https://pass.wafer.ai/v1",
        true,
    ),
    (
        "qianfan",
        "openai-completions",
        "https://qianfan.baidubce.com/v2",
        true,
    ),
    (
        "umans",
        "anthropic-messages",
        "https://api.code.umans.ai/v1/messages",
        false,
    ),
    (
        "kilo",
        "openai-completions",
        "https://api.kilo.ai/api/gateway",
        true,
    ),
    (
        "opencode-go",
        "openai-completions",
        "https://opencode.ai/zen/go/v1",
        true,
    ),
];

#[test]
fn wave_c2_presets_resolve_metadata_defaults_and_factory_route() {
    let harness = TestHarness::new("wave_c2_presets_resolve_metadata_defaults_and_factory_route");
    for (provider_id, expected_api, expected_base_url, expected_auth_header) in WAVE_C2_PRESET_CASES
    {
        let defaults = provider_routing_defaults(provider_id)
            .unwrap_or_else(|| panic!("missing metadata defaults for {provider_id}"));
        harness
            .log()
            .info_ctx("wave_c2.defaults", "metadata defaults", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
                ctx.push(("api".to_string(), defaults.api.to_string()));
                ctx.push(("base_url".to_string(), defaults.base_url.to_string()));
                ctx.push(("auth_header".to_string(), defaults.auth_header.to_string()));
            });
        assert_eq!(defaults.api, expected_api);
        assert_eq!(defaults.base_url, expected_base_url);
        assert_eq!(defaults.auth_header, expected_auth_header);
        assert_eq!(canonical_provider_id(provider_id), Some(provider_id));

        let mut entry = make_model_entry(provider_id, "wave-c2-default-model", expected_base_url);
        entry.model.api.clear();
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider should route {provider_id}: {e}"));
        harness
            .log()
            .info_ctx("wave_c2.factory", "factory route", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
                ctx.push(("name".to_string(), provider.name().to_string()));
                ctx.push(("api".to_string(), provider.api().to_string()));
            });
        assert_eq!(provider.name(), provider_id);
        assert_eq!(provider.api(), expected_api);
        assert_eq!(provider.model_id(), "wave-c2-default-model");
    }
}

#[test]
fn wave_c2_env_keys_match_omp_descriptors() {
    let harness = TestHarness::new("wave_c2_env_keys_match_omp_descriptors");
    let cases: [(&str, &[&str]); 8] = [
        ("gmi", &["GMI_API_KEY"]),
        ("coreweave", &["COREWEAVE_API_KEY", "WANDB_API_KEY"]),
        ("sakana", &["SAKANA_API_KEY", "FUGU_API_KEY"]),
        ("wafer", &["WAFER_SERVERLESS_API_KEY"]),
        ("qianfan", &["QIANFAN_API_KEY"]),
        ("umans", &["UMANS_AI_CODING_PLAN_API_KEY"]),
        ("kilo", &["KILO_API_KEY"]),
        ("opencode-go", &["OPENCODE_API_KEY"]),
    ];
    for (provider_id, expected_keys) in cases {
        let keys = provider_auth_env_keys(provider_id);
        harness
            .log()
            .info_ctx("wave_c2.env", "auth env keys", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
                ctx.push(("env_keys".to_string(), keys.join(",")));
            });
        assert_eq!(keys, expected_keys, "{provider_id} env keys drifted");
    }
}

fn wave_c2_ping_context() -> (Context<'static>, StreamOptions, String) {
    let context = Context {
        system_prompt: Some("Be concise.".to_string().into()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text("Ping".to_string()),
            timestamp: 0,
        })]
        .into(),
        tools: Vec::new().into(),
    };
    let options = StreamOptions {
        max_tokens: Some(64),
        ..Default::default()
    };
    (context, options, "Ping".to_string())
}

#[test]
fn wave_c2_openai_completions_streams_use_chat_completions_path_and_bearer_auth() {
    let harness = TestHarness::new(
        "wave_c2_openai_completions_streams_use_chat_completions_path_and_bearer_auth",
    );
    let completions_cases: [(&str, &str); 6] = [
        ("gmi", "wave-c2-gmi-token"),
        ("coreweave", "wave-c2-coreweave-token"),
        ("wafer", "wave-c2-wafer-token"),
        ("qianfan", "wave-c2-qianfan-token"),
        ("kilo", "wave-c2-kilo-token"),
        ("opencode-go", "wave-c2-opencode-go-token"),
    ];
    for (index, (provider_id, api_key)) in completions_cases.into_iter().enumerate() {
        let defaults = provider_routing_defaults(provider_id)
            .unwrap_or_else(|| panic!("missing metadata defaults for {provider_id}"));
        assert_eq!(defaults.api, "openai-completions");
        assert!(defaults.auth_header);

        let server = harness.start_mock_http_server();
        let path_prefix = format!("/wave-c2/{index}/{}", provider_id.replace('-', "_"));
        let expected_path = format!("{path_prefix}/chat/completions");
        server.add_route(
            "POST",
            &expected_path,
            text_event_stream_response(openai_chat_sse_body()),
        );

        // Route through the metadata-declared api kind, overriding only the
        // host with the fixture server (base_url path shape is preserved).
        let mut entry = make_model_entry(
            provider_id,
            "wave-c2-stream-model",
            &format!("{}{}", server.base_url(), path_prefix),
        );
        entry.model.api.clear();
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider should stream-route {provider_id}: {e}"));
        assert_eq!(provider.api(), "openai-completions");

        let (context, base_options, _) = wave_c2_ping_context();
        let options = StreamOptions {
            api_key: Some(api_key.to_string()),
            ..base_options
        };
        drive_provider_stream_to_done(provider, context, options);

        let requests = server.requests();
        assert_eq!(
            requests.len(),
            1,
            "expected exactly one request for {provider_id}"
        );
        let request = &requests[0];
        harness
            .log()
            .info_ctx("wave_c2.stream", "request lock", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
                ctx.push(("path".to_string(), request.path.clone()));
                ctx.push((
                    "authorization".to_string(),
                    request_header(&request.headers, "authorization").unwrap_or_default(),
                ));
            });
        assert_eq!(request.path, expected_path);
        let expected_auth = format!("Bearer {api_key}");
        assert_eq!(
            request_header(&request.headers, "authorization").as_deref(),
            Some(expected_auth.as_str())
        );
        assert_eq!(
            request_header(&request.headers, "content-type").as_deref(),
            Some("application/json")
        );
    }
}

#[test]
fn wave_c2_sakana_streams_via_openai_responses_path_with_bearer_auth() {
    let harness =
        TestHarness::new("wave_c2_sakana_streams_via_openai_responses_path_with_bearer_auth");
    let defaults = provider_routing_defaults("sakana").expect("sakana defaults");
    assert_eq!(defaults.api, "openai-responses");
    assert!(defaults.auth_header);

    let server = harness.start_mock_http_server();
    let expected_path = "/wave-c2/sakana/responses";
    server.add_route(
        "POST",
        expected_path,
        text_event_stream_response(openai_responses_sse_body()),
    );

    let mut entry = make_model_entry(
        "sakana",
        "wave-c2-sakana-model",
        &format!("{}/wave-c2/sakana", server.base_url()),
    );
    entry.model.api.clear();
    let provider =
        create_provider(&entry, None).expect("create_provider should stream-route sakana");
    assert_eq!(provider.api(), "openai-responses");

    let (context, base_options, _) = wave_c2_ping_context();
    let options = StreamOptions {
        api_key: Some("wave-c2-sakana-token".to_string()),
        ..base_options
    };
    drive_provider_stream_to_done(provider, context, options);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "expected exactly one request for sakana");
    let request = &requests[0];
    harness
        .log()
        .info_ctx("wave_c2.stream", "sakana request lock", |ctx| {
            ctx.push(("path".to_string(), request.path.clone()));
            ctx.push((
                "authorization".to_string(),
                request_header(&request.headers, "authorization").unwrap_or_default(),
            ));
        });
    assert_eq!(request.path, expected_path);
    assert_eq!(
        request_header(&request.headers, "authorization").as_deref(),
        Some("Bearer wave-c2-sakana-token")
    );
    assert_eq!(
        request_header(&request.headers, "content-type").as_deref(),
        Some("application/json")
    );
}

#[test]
fn wave_c2_umans_streams_via_anthropic_messages_path_with_x_api_key() {
    let harness =
        TestHarness::new("wave_c2_umans_streams_via_anthropic_messages_path_with_x_api_key");
    let defaults = provider_routing_defaults("umans").expect("umans defaults");
    assert_eq!(defaults.api, "anthropic-messages");
    assert!(!defaults.auth_header);

    let server = harness.start_mock_http_server();
    let expected_path = "/wave-c2/umans/v1/messages";
    server.add_route(
        "POST",
        expected_path,
        text_event_stream_response(anthropic_messages_sse_body()),
    );

    let mut entry = make_model_entry(
        "umans",
        "wave-c2-umans-model",
        &format!("{}/wave-c2/umans", server.base_url()),
    );
    entry.model.api.clear();
    let provider =
        create_provider(&entry, None).expect("create_provider should stream-route umans");
    assert_eq!(provider.api(), "anthropic-messages");

    let api_key = "wave-c2-umans-token";
    let (context, base_options, _) = wave_c2_ping_context();
    let options = StreamOptions {
        api_key: Some(api_key.to_string()),
        ..base_options
    };
    drive_provider_stream_to_done(provider, context, options);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "expected exactly one request for umans");
    let request = &requests[0];
    harness
        .log()
        .info_ctx("wave_c2.stream", "umans request lock", |ctx| {
            ctx.push(("path".to_string(), request.path.clone()));
            ctx.push((
                "x-api-key".to_string(),
                request_header(&request.headers, "x-api-key").unwrap_or_default(),
            ));
        });
    assert_eq!(request.path, expected_path);
    assert_eq!(
        request_header(&request.headers, "x-api-key").as_deref(),
        Some(api_key)
    );
    assert!(request_header(&request.headers, "authorization").is_none());
    assert_eq!(
        request_header(&request.headers, "anthropic-version").as_deref(),
        Some("2023-06-01")
    );
}

#[test]
fn wave_c2_aliases_resolve_and_do_not_collide() {
    let harness = TestHarness::new("wave_c2_aliases_resolve_and_do_not_collide");
    let cases: [(&str, &str); 10] = [
        ("gmi-cloud", "gmi"),
        ("gmi-serving", "gmi"),
        ("coreweave-serverless", "coreweave"),
        ("sakana-ai", "sakana"),
        ("wafer-serverless", "wafer"),
        ("baidu-qianfan", "qianfan"),
        ("umans-ai", "umans"),
        ("kilo-gateway", "kilo"),
        ("kilo-ai", "kilo"),
        ("opencode-zen", "opencode"),
    ];
    for (alias, expected_canonical) in cases {
        let resolved = canonical_provider_id(alias)
            .unwrap_or_else(|| panic!("alias '{alias}' should resolve"));
        harness.log().info_ctx("wave_c2.alias", "alias", |ctx| {
            ctx.push(("alias".to_string(), alias.to_string()));
            ctx.push(("canonical".to_string(), resolved.to_string()));
        });
        assert_eq!(resolved, expected_canonical);
        // Case-insensitive resolution must not produce a different provider.
        let upper = canonical_provider_id(&alias.to_uppercase())
            .unwrap_or_else(|| panic!("uppercase alias '{alias}' should resolve"));
        assert_eq!(upper, expected_canonical);
    }
    // The OpenCode tiers stay distinct after the opencode-zen alias was added.
    assert_eq!(canonical_provider_id("opencode"), Some("opencode"));
    assert_eq!(canonical_provider_id("opencode-go"), Some("opencode-go"));
    let zen = provider_routing_defaults("opencode").expect("zen defaults");
    let go = provider_routing_defaults("opencode-go").expect("go defaults");
    assert_ne!(zen.base_url, go.base_url);
}
