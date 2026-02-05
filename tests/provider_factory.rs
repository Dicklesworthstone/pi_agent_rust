//! Provider factory and base URL normalization tests (no network).

mod common;

use common::TestHarness;
use pi::Error;
use pi::models::ModelEntry;
use pi::provider::{
    Api, CacheRetention, InputType, KnownProvider, Model, ModelCost, StreamOptions,
};
use pi::providers::{create_provider, normalize_openai_base};
use std::collections::HashMap;
use std::str::FromStr;

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

#[test]
fn normalize_openai_base_appends_for_plain_host() {
    let harness = TestHarness::new("normalize_openai_base_appends_for_plain_host");
    let input = "https://api.openai.com";
    let expected = "https://api.openai.com/chat/completions";
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
fn normalize_openai_base_preserves_responses() {
    let harness = TestHarness::new("normalize_openai_base_preserves_responses");
    let input = "https://api.openai.com/v1/responses";
    let expected = "https://api.openai.com/v1/responses";
    harness
        .log()
        .info_ctx("normalize", "responses endpoint", |ctx| {
            ctx.push(("input".to_string(), input.to_string()));
            ctx.push(("expected".to_string(), expected.to_string()));
        });
    let normalized = normalize_openai_base(input);
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
fn normalize_openai_base_trims_trailing_slash_for_responses() {
    let harness = TestHarness::new("normalize_openai_base_trims_trailing_slash_for_responses");
    let input = "https://api.openai.com/v1/responses/";
    let expected = "https://api.openai.com/v1/responses";
    harness
        .log()
        .info_ctx("normalize", "responses trailing slash", |ctx| {
            ctx.push(("input".to_string(), input.to_string()));
            ctx.push(("expected".to_string(), expected.to_string()));
        });
    let normalized = normalize_openai_base(input);
    assert_eq!(normalized, expected);
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
    assert_eq!(provider.api(), "gemini");
    assert_eq!(provider.model_id(), "gemini-test");
}

#[test]
fn create_provider_rejects_azure_without_deployment() {
    let harness = TestHarness::new("create_provider_rejects_azure_without_deployment");
    let entry = make_model_entry("azure-openai", "gpt-4o", "https://example.openai.azure.com");
    let err = match create_provider(&entry, None) {
        Ok(_) => panic!("expected azure-openai error"),
        Err(err) => err,
    };
    harness.log().info_ctx("provider", "azure error", |ctx| {
        ctx.push(("error".to_string(), err.to_string()));
    });

    match err {
        Error::Provider { provider, message } => {
            assert_eq!(provider, "azure-openai");
            assert!(message.contains("resource+deployment"));
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn create_provider_rejects_unknown_provider() {
    let harness = TestHarness::new("create_provider_rejects_unknown_provider");
    let entry = make_model_entry("mystery", "mystery-model", "https://example.com/v1");
    let err = match create_provider(&entry, None) {
        Ok(_) => panic!("expected unknown provider error"),
        Err(err) => err,
    };
    harness.log().info_ctx("provider", "unknown error", |ctx| {
        ctx.push(("error".to_string(), err.to_string()));
    });

    match err {
        Error::Provider { provider, message } => {
            assert_eq!(provider, "mystery");
            assert!(message.contains("not implemented"));
        }
        other => panic!("unexpected error: {other}"),
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
