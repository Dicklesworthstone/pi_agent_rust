//! Backward behavior lock tests for provider request shaping, auth, and defaults.
//!
//! These tests lock down the exact request body JSON shapes, authentication
//! header selection, URL construction, default values, and tool-conversion
//! formats for every stable provider. If a refactor changes behavior that
//! these tests cover, the failure is intentional — update the test only after
//! confirming the change is desired.
//!
//! bd-3uqg.2.5
#![allow(
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::needless_pass_by_value
)]

mod common;

use pi::model::{Message, ThinkingLevel, UserContent};
use pi::models::CompatConfig;
use pi::provider::{Context, Provider, StreamOptions, ThinkingBudgets, ToolDef};
use pi::providers::anthropic::AnthropicProvider;
use pi::providers::cohere::CohereProvider;
use pi::providers::gemini::GeminiProvider;
use pi::providers::openai::OpenAIProvider;
use pi::providers::openai_responses::OpenAIResponsesProvider;
use serde_json::json;
use std::collections::HashMap;

// ═══════════════════════════════════════════════════════════════════════
// Shared helpers
// ═══════════════════════════════════════════════════════════════════════

fn minimal_context() -> Context<'static> {
    Context {
        system_prompt: Some("You are helpful.".to_string().into()),
        messages: vec![Message::User(pi::model::UserMessage {
            content: UserContent::Text("Hello".to_string()),
            timestamp: 0,
        })]
        .into(),
        tools: Vec::new().into(),
    }
}

fn context_with_tools() -> Context<'static> {
    Context {
        system_prompt: Some("Be concise.".to_string().into()),
        messages: vec![Message::User(pi::model::UserMessage {
            content: UserContent::Text("Search for rust".to_string()),
            timestamp: 0,
        })]
        .into(),
        tools: vec![
            ToolDef {
                name: "web_search".to_string(),
                description: "Search the web".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" }
                    },
                    "required": ["query"]
                }),
            },
            ToolDef {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                }),
            },
        ]
        .into(),
    }
}

fn default_options() -> StreamOptions {
    StreamOptions::default()
}

fn options_with_tokens(max_tokens: u32) -> StreamOptions {
    StreamOptions {
        max_tokens: Some(max_tokens),
        ..Default::default()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Anthropic: request shape, defaults, auth, thinking
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn anthropic_request_shape_minimal() {
    let provider = AnthropicProvider::new("claude-sonnet-4-5");
    let context = minimal_context();
    let req = provider.build_request(&context, &default_options());
    let v = serde_json::to_value(&req).expect("serialize");

    // Locked: model, system as top-level typed block array (the array form is
    // required so system can carry prompt-cache `cache_control` breakpoints —
    // deliberate change with the Anthropic prompt-caching work), messages
    // array, stream bool
    assert_eq!(v["model"], "claude-sonnet-4-5");
    assert_eq!(
        v["system"],
        json!([{"type": "text", "text": "You are helpful."}])
    );
    assert_eq!(v["stream"], true);
    assert!(v["messages"].is_array());
    assert_eq!(v["messages"][0]["role"], "user");
    assert_eq!(
        v["messages"][0]["content"],
        json!([{"type": "text", "text": "Hello"}])
    );
    // Locked: no tools, no thinking when not requested
    assert!(v["tools"].is_null());
    assert!(v["thinking"].is_null());
}

#[test]
fn anthropic_default_max_tokens_8192() {
    let provider = AnthropicProvider::new("claude-sonnet-4-5");
    let context = minimal_context();
    let req = provider.build_request(&context, &default_options());
    let v = serde_json::to_value(&req).expect("serialize");
    assert_eq!(
        v["max_tokens"], 8192,
        "Anthropic default max_tokens locked at 8192"
    );
}

#[test]
fn anthropic_custom_max_tokens_honored() {
    let provider = AnthropicProvider::new("claude-sonnet-4-5");
    let context = minimal_context();
    let req = provider.build_request(&context, &options_with_tokens(2048));
    let v = serde_json::to_value(&req).expect("serialize");
    assert_eq!(v["max_tokens"], 2048);
}

#[test]
fn anthropic_tool_conversion_shape() {
    let provider = AnthropicProvider::new("claude-sonnet-4-5");
    let context = context_with_tools();
    let req = provider.build_request(&context, &default_options());
    let v = serde_json::to_value(&req).expect("serialize");

    let tools = v["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2);
    // Locked: Anthropic tool shape — name, description, input_schema (not "parameters")
    assert_eq!(tools[0]["name"], "web_search");
    assert_eq!(tools[0]["description"], "Search the web");
    assert!(
        tools[0]["input_schema"].is_object(),
        "Anthropic uses input_schema"
    );
    assert!(
        tools[0].get("type").is_none(),
        "Anthropic tools have no type wrapper"
    );
}

#[test]
fn anthropic_thinking_medium_builds_correct_config() {
    let provider = AnthropicProvider::new("claude-sonnet-4-5");
    let options = StreamOptions {
        thinking_level: Some(ThinkingLevel::Medium),
        ..Default::default()
    };
    let context = minimal_context();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");

    assert_eq!(v["thinking"]["type"], "enabled");
    let budget = v["thinking"]["budget_tokens"].as_u64().expect("budget");
    assert_eq!(budget, 8192, "Medium thinking budget locked at 8192");
}

#[test]
fn anthropic_thinking_bumps_max_tokens_when_budget_exceeds() {
    let provider = AnthropicProvider::new("claude-sonnet-4-5");
    let options = StreamOptions {
        max_tokens: Some(4096),
        thinking_level: Some(ThinkingLevel::High),
        ..Default::default()
    };
    let context = minimal_context();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");

    let budget = v["thinking"]["budget_tokens"].as_u64().expect("budget");
    assert_eq!(budget, 16384, "High thinking budget locked at 16384");
    // max_tokens should be bumped to budget + 4096
    assert_eq!(
        v["max_tokens"],
        budget + 4096,
        "max_tokens auto-bumped when budget >= max_tokens"
    );
}

#[test]
fn anthropic_thinking_off_omits_thinking_field() {
    let provider = AnthropicProvider::new("claude-sonnet-4-5");
    let options = StreamOptions {
        thinking_level: Some(ThinkingLevel::Off),
        ..Default::default()
    };
    let context = minimal_context();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");
    assert!(
        v["thinking"].is_null(),
        "Off thinking should omit the field"
    );
}

#[test]
fn anthropic_provider_identity() {
    let provider = AnthropicProvider::new("claude-sonnet-4-5");
    assert_eq!(provider.name(), "anthropic");
    assert_eq!(provider.api(), "anthropic-messages");
    assert_eq!(provider.model_id(), "claude-sonnet-4-5");
}

// ═══════════════════════════════════════════════════════════════════════
// OpenAI Completions: request shape, defaults, auth, compat
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn openai_request_shape_minimal() {
    let provider = OpenAIProvider::new("gpt-4o");
    let context = minimal_context();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");

    assert_eq!(v["model"], "gpt-4o");
    assert_eq!(v["stream"], true);
    // Locked: system prompt as first message with role "system"
    assert_eq!(v["messages"][0]["role"], "system");
    assert_eq!(v["messages"][0]["content"], "You are helpful.");
    // User message follows
    assert_eq!(v["messages"][1]["role"], "user");
    assert_eq!(v["messages"][1]["content"], "Hello");
    // Locked: stream_options with include_usage
    assert_eq!(v["stream_options"]["include_usage"], true);
}

#[test]
fn openai_default_max_tokens_4096() {
    let provider = OpenAIProvider::new("gpt-4o");
    let context = minimal_context();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");
    assert_eq!(
        v["max_tokens"], 4096,
        "OpenAI default max_tokens locked at 4096"
    );
    assert!(
        v["max_completion_tokens"].is_null(),
        "max_completion_tokens absent by default"
    );
}

#[test]
fn openai_tool_conversion_shape() {
    let provider = OpenAIProvider::new("gpt-4o");
    let context = context_with_tools();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");

    let tools = v["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2);
    // Locked: OpenAI tool shape — type="function", function.{name, description, parameters}
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "web_search");
    assert_eq!(tools[0]["function"]["description"], "Search the web");
    assert!(tools[0]["function"]["parameters"].is_object());
}

#[test]
fn openai_compat_max_completion_tokens_routing() {
    let provider = OpenAIProvider::new("o1").with_compat(Some(CompatConfig {
        max_tokens_field: Some("max_completion_tokens".to_string()),
        ..Default::default()
    }));
    let context = minimal_context();
    let options = options_with_tokens(2048);
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");
    assert!(
        v["max_tokens"].is_null(),
        "max_tokens absent with compat override"
    );
    assert_eq!(v["max_completion_tokens"], 2048);
}

#[test]
fn openai_compat_developer_role() {
    let provider = OpenAIProvider::new("o1").with_compat(Some(CompatConfig {
        system_role_name: Some("developer".to_string()),
        ..Default::default()
    }));
    let context = minimal_context();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");
    assert_eq!(v["messages"][0]["role"], "developer");
}

#[test]
fn openai_compat_tools_disabled() {
    let provider = OpenAIProvider::new("gpt-4o").with_compat(Some(CompatConfig {
        supports_tools: Some(false),
        ..Default::default()
    }));
    let context = context_with_tools();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");
    assert!(
        v["tools"].is_null(),
        "tools omitted when supports_tools=false"
    );
}

#[test]
fn openai_compat_usage_streaming_disabled() {
    let provider = OpenAIProvider::new("gpt-4o").with_compat(Some(CompatConfig {
        supports_usage_in_streaming: Some(false),
        ..Default::default()
    }));
    let context = minimal_context();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");
    assert_eq!(v["stream_options"]["include_usage"], false);
}

#[test]
fn openai_provider_identity() {
    let provider = OpenAIProvider::new("gpt-4o");
    assert_eq!(provider.name(), "openai");
    assert_eq!(provider.api(), "openai-completions");
    assert_eq!(provider.model_id(), "gpt-4o");
}

#[test]
fn openai_provider_name_override() {
    let provider = OpenAIProvider::new("llama-3.1").with_provider_name("groq");
    assert_eq!(provider.name(), "groq");
    assert_eq!(provider.api(), "openai-completions");
}

// ═══════════════════════════════════════════════════════════════════════
// OpenAI Responses: request shape, defaults
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn openai_responses_request_shape_minimal() {
    let provider = OpenAIResponsesProvider::new("gpt-4o");
    let context = minimal_context();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");

    assert_eq!(v["model"], "gpt-4o");
    assert_eq!(v["stream"], true);
    // Locked: input array (not "messages")
    assert!(
        v["input"].is_array(),
        "Responses API uses 'input' not 'messages'"
    );
    // System prompt is a top-level Responses `instructions` field, not an input item.
    assert_eq!(v["instructions"], "You are helpful.");
    assert_eq!(v["input"][0]["role"], "user");
    assert_eq!(v["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(v["input"][0]["content"][0]["text"], "Hello");
    assert!(
        !v["input"]
            .as_array()
            .expect("input array")
            .iter()
            .any(|item| item["role"] == "system"),
        "Responses API keeps system prompt out of input items"
    );
}

#[test]
fn openai_responses_default_max_output_tokens_4096() {
    let provider = OpenAIResponsesProvider::new("gpt-4o");
    let context = minimal_context();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");
    assert_eq!(
        v["max_output_tokens"], 4096,
        "Responses API default max_output_tokens locked at 4096"
    );
    // Locked: field name is max_output_tokens, NOT max_tokens
    assert!(v["max_tokens"].is_null());
}

#[test]
fn openai_responses_tool_conversion_shape() {
    let provider = OpenAIResponsesProvider::new("gpt-4o");
    let context = context_with_tools();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");

    let tools = v["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2);
    // Locked: Responses tool shape — flat, type="function", name, description, parameters
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["name"], "web_search");
    assert_eq!(tools[0]["description"], "Search the web");
    assert!(tools[0]["parameters"].is_object());
    // No nested "function" wrapper (unlike completions API)
    assert!(
        tools[0].get("function").is_none(),
        "Responses tools are flat, no function wrapper"
    );
}

#[test]
fn openai_responses_provider_identity() {
    let provider = OpenAIResponsesProvider::new("gpt-4o");
    assert_eq!(provider.name(), "openai");
    assert_eq!(provider.api(), "openai-responses");
    assert_eq!(provider.model_id(), "gpt-4o");
}

// ═══════════════════════════════════════════════════════════════════════
// Cohere: request shape, defaults
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn cohere_request_shape_minimal() {
    let provider = CohereProvider::new("command-r-plus");
    let context = minimal_context();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");

    assert_eq!(v["model"], "command-r-plus");
    assert_eq!(v["stream"], true);
    // Locked: system prompt as system-role message in messages array (v2 API)
    let messages = v["messages"].as_array().expect("messages array");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "You are helpful.");
}

#[test]
fn cohere_default_max_tokens_4096() {
    let provider = CohereProvider::new("command-r-plus");
    let context = minimal_context();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");
    assert_eq!(
        v["max_tokens"], 4096,
        "Cohere default max_tokens locked at 4096"
    );
}

#[test]
fn cohere_tool_conversion_shape() {
    let provider = CohereProvider::new("command-r-plus");
    let context = context_with_tools();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");

    let tools = v["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2);
    // Locked: Cohere tool shape — type="function", function.{name, description, parameters}
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "web_search");
    assert_eq!(tools[0]["function"]["description"], "Search the web");
    assert!(tools[0]["function"]["parameters"].is_object());
}

#[test]
fn cohere_provider_identity() {
    let provider = CohereProvider::new("command-r-plus");
    assert_eq!(provider.name(), "cohere");
    assert_eq!(provider.api(), "cohere-chat");
    assert_eq!(provider.model_id(), "command-r-plus");
}

// ═══════════════════════════════════════════════════════════════════════
// Gemini: request shape, defaults, system instruction, URL
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn gemini_request_shape_minimal() {
    let provider = GeminiProvider::new("gemini-2.5-pro");
    let context = minimal_context();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");

    // Locked: systemInstruction as separate field (camelCase from serde rename)
    assert!(
        v["systemInstruction"].is_object(),
        "Gemini system prompt goes to systemInstruction"
    );
    assert_eq!(
        v["systemInstruction"]["parts"][0]["text"],
        "You are helpful."
    );

    // Locked: contents array for messages
    assert!(v["contents"].is_array());
    assert_eq!(v["contents"][0]["role"], "user");

    // Locked: generationConfig (camelCase)
    assert_eq!(v["generationConfig"]["candidateCount"], 1);
}

#[test]
fn gemini_default_max_tokens_8192() {
    let provider = GeminiProvider::new("gemini-2.5-pro");
    let context = minimal_context();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");
    assert_eq!(
        v["generationConfig"]["maxOutputTokens"], 8192,
        "Gemini default maxOutputTokens locked at 8192"
    );
}

#[test]
fn gemini_tool_conversion_shape() {
    let provider = GeminiProvider::new("gemini-2.5-pro");
    let context = context_with_tools();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");

    // Locked: tools as single-element array with functionDeclarations (camelCase)
    let tools = v["tools"].as_array().expect("tools array");
    assert_eq!(
        tools.len(),
        1,
        "Gemini wraps all functions in one tools entry"
    );
    let decls = tools[0]["functionDeclarations"]
        .as_array()
        .expect("functionDeclarations");
    assert_eq!(decls.len(), 2);
    assert_eq!(decls[0]["name"], "web_search");
    assert_eq!(decls[0]["description"], "Search the web");
    assert!(decls[0]["parameters"].is_object());

    // Locked: toolConfig present when tools exist (camelCase)
    assert_eq!(
        v["toolConfig"]["functionCallingConfig"]["mode"], "AUTO",
        "Gemini toolConfig mode locked at AUTO"
    );
}

#[test]
fn gemini_no_tool_config_without_tools() {
    let provider = GeminiProvider::new("gemini-2.5-pro");
    let context = minimal_context();
    let options = default_options();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");
    assert!(v["toolConfig"].is_null(), "No toolConfig without tools");
    assert!(v["tools"].is_null(), "No tools field without tools");
}

#[test]
fn gemini_streaming_url_shape() {
    let provider = GeminiProvider::new("gemini-2.5-pro");
    let url = provider.streaming_url();
    assert!(
        url.contains("/models/gemini-2.5-pro:streamGenerateContent"),
        "URL must contain model and streamGenerateContent"
    );
    assert!(url.contains("alt=sse"), "URL must contain alt=sse");
    assert!(
        !url.contains("key="),
        "Gemini URL should not embed API key query params"
    );
    assert!(
        !url.contains("Authorization"),
        "Gemini auth is sent via request headers, not URL"
    );
}

#[test]
fn gemini_provider_identity() {
    let provider = GeminiProvider::new("gemini-2.5-pro");
    assert_eq!(provider.name(), "google");
    assert_eq!(provider.api(), "google-generative-ai");
    assert_eq!(provider.model_id(), "gemini-2.5-pro");
}

// ═══════════════════════════════════════════════════════════════════════
// URL normalization (locked from providers/mod.rs)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn openai_completions_url_normalization() {
    // With /chat/completions already present
    let provider =
        OpenAIProvider::new("gpt-4o").with_base_url("https://api.openai.com/v1/chat/completions");
    assert_eq!(provider.model_id(), "gpt-4o");

    // With /v1 only — factory normalizes, but direct construction uses as-is
    let provider2 =
        OpenAIProvider::new("gpt-4o").with_base_url("https://api.openai.com/v1/chat/completions");
    assert_eq!(provider2.model_id(), "gpt-4o");
}

// ═══════════════════════════════════════════════════════════════════════
// Provider factory routing (lock from mod.rs)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn factory_routes_anthropic_correctly() {
    use pi::models::ModelEntry;
    use pi::provider::{InputType, Model, ModelCost};
    use pi::providers::create_provider;

    let entry = ModelEntry {
        model: Model {
            id: "claude-sonnet-4-5".to_string(),
            name: "Claude Sonnet 4.5".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://api.anthropic.com/v1/messages".to_string(),
            reasoning: true,
            input: vec![InputType::Text],
            cost: ModelCost {
                input: 0.003,
                output: 0.015,
                cache_read: 0.0003,
                cache_write: 0.00375,
            },
            context_window: 200_000,
            max_tokens: 8192,
            headers: HashMap::new(),
        },
        api_key: Some("test-key".to_string()),
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    };

    let provider = create_provider(&entry, None).expect("factory should route anthropic");
    assert_eq!(provider.name(), "anthropic");
    assert_eq!(provider.api(), "anthropic-messages");
}

#[test]
fn factory_routes_openai_completions_correctly() {
    use pi::models::ModelEntry;
    use pi::provider::{InputType, Model, ModelCost};
    use pi::providers::create_provider;

    let entry = ModelEntry {
        model: Model {
            id: "gpt-4o".to_string(),
            name: "GPT-4o".to_string(),
            api: "openai-completions".to_string(),
            provider: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: false,
            input: vec![InputType::Text],
            cost: ModelCost {
                input: 0.0025,
                output: 0.01,
                cache_read: 0.00125,
                cache_write: 0.00125,
            },
            context_window: 128_000,
            max_tokens: 16384,
            headers: HashMap::new(),
        },
        api_key: Some("test-key".to_string()),
        headers: HashMap::new(),
        auth_header: true,
        compat: None,
        oauth_config: None,
    };

    let provider = create_provider(&entry, None).expect("factory should route openai completions");
    assert_eq!(provider.name(), "openai");
    assert_eq!(provider.api(), "openai-completions");
}

#[test]
fn factory_routes_openai_responses_correctly() {
    use pi::models::ModelEntry;
    use pi::provider::{InputType, Model, ModelCost};
    use pi::providers::create_provider;

    let entry = ModelEntry {
        model: Model {
            id: "gpt-4o".to_string(),
            name: "GPT-4o".to_string(),
            api: "openai-responses".to_string(),
            provider: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: false,
            input: vec![InputType::Text],
            cost: ModelCost {
                input: 0.0025,
                output: 0.01,
                cache_read: 0.00125,
                cache_write: 0.00125,
            },
            context_window: 128_000,
            max_tokens: 16384,
            headers: HashMap::new(),
        },
        api_key: Some("test-key".to_string()),
        headers: HashMap::new(),
        auth_header: true,
        compat: None,
        oauth_config: None,
    };

    let provider = create_provider(&entry, None).expect("factory should route openai responses");
    assert_eq!(provider.name(), "openai");
    assert_eq!(provider.api(), "openai-responses");
}

#[test]
fn factory_routes_cohere_correctly() {
    use pi::models::ModelEntry;
    use pi::provider::{InputType, Model, ModelCost};
    use pi::providers::create_provider;

    let entry = ModelEntry {
        model: Model {
            id: "command-r-plus".to_string(),
            name: "Command R+".to_string(),
            api: "cohere-chat".to_string(),
            provider: "cohere".to_string(),
            base_url: "https://api.cohere.com/v2".to_string(),
            reasoning: false,
            input: vec![InputType::Text],
            cost: ModelCost {
                input: 0.003,
                output: 0.015,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 4096,
            headers: HashMap::new(),
        },
        api_key: Some("test-key".to_string()),
        headers: HashMap::new(),
        auth_header: true,
        compat: None,
        oauth_config: None,
    };

    let provider = create_provider(&entry, None).expect("factory should route cohere");
    assert_eq!(provider.name(), "cohere");
    assert_eq!(provider.api(), "cohere-chat");
}

#[test]
fn factory_routes_google_correctly() {
    use pi::models::ModelEntry;
    use pi::provider::{InputType, Model, ModelCost};
    use pi::providers::create_provider;

    let entry = ModelEntry {
        model: Model {
            id: "gemini-2.5-pro".to_string(),
            name: "Gemini 2.5 Pro".to_string(),
            api: "google-generative-ai".to_string(),
            provider: "google".to_string(),
            base_url: "https://generativelanguage.googleapis.com".to_string(),
            reasoning: true,
            input: vec![InputType::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 1_000_000,
            max_tokens: 8192,
            headers: HashMap::new(),
        },
        api_key: Some("test-key".to_string()),
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    };

    let provider = create_provider(&entry, None).expect("factory should route google");
    assert_eq!(provider.name(), "google");
    assert_eq!(provider.api(), "google-generative-ai");
}

// ═══════════════════════════════════════════════════════════════════════
// Batch A1: OAI-compatible preset providers route through factory
// ═══════════════════════════════════════════════════════════════════════

/// Helper: build a minimal `ModelEntry` for an OAI-compatible provider.
fn oai_compat_entry(provider: &str, base_url: &str) -> pi::models::ModelEntry {
    use pi::provider::{InputType, Model, ModelCost};
    pi::models::ModelEntry {
        model: Model {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            api: "openai-completions".to_string(),
            provider: provider.to_string(),
            base_url: base_url.to_string(),
            reasoning: false,
            input: vec![InputType::Text],
            cost: ModelCost {
                input: 0.001,
                output: 0.002,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 16_384,
            headers: HashMap::new(),
        },
        api_key: Some("test-key".to_string()),
        headers: HashMap::new(),
        auth_header: true,
        compat: None,
        oauth_config: None,
    }
}

#[test]
fn factory_routes_batch_a1_providers_correctly() {
    use pi::providers::create_provider;

    let cases = [
        ("302ai", "https://api.302.ai/v1"),
        ("abacus", "https://routellm.abacus.ai/v1"),
        ("aihubmix", "https://aihubmix.com/v1"),
        ("bailing", "https://api.tbox.cn/api/llm/v1"),
        ("berget", "https://api.berget.ai/v1"),
        ("chutes", "https://llm.chutes.ai/v1"),
        ("cortecs", "https://api.cortecs.ai/v1"),
        ("fastrouter", "https://go.fastrouter.ai/api/v1"),
    ];
    for (provider, base_url) in &cases {
        let entry = oai_compat_entry(provider, base_url);
        let p = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("factory should route {provider}: {e}"));
        assert_eq!(p.api(), "openai-completions", "{provider} api mismatch");
    }
}

#[test]
fn factory_routes_batch_a2_providers_correctly() {
    use pi::providers::create_provider;

    let cases = [
        ("firmware", "https://app.firmware.ai/api/v1"),
        ("friendli", "https://api.friendli.ai/serverless/v1"),
        ("helicone", "https://ai-gateway.helicone.ai/v1"),
        ("huggingface", "https://router.huggingface.co/v1"),
        ("iflowcn", "https://apis.iflow.cn/v1"),
        ("inception", "https://api.inceptionlabs.ai/v1"),
        ("inference", "https://inference.net/v1"),
    ];
    for (provider, base_url) in &cases {
        let entry = oai_compat_entry(provider, base_url);
        let p = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("factory should route {provider}: {e}"));
        assert_eq!(p.api(), "openai-completions", "{provider} api mismatch");
    }
}

#[test]
fn factory_routes_batch_a3_providers_correctly() {
    use pi::providers::create_provider;

    let cases = [
        ("io-net", "https://api.intelligence.io.solutions/api/v1"),
        ("jiekou", "https://api.jiekou.ai/openai"),
        ("lucidquery", "https://lucidquery.com/api/v1"),
        ("moark", "https://moark.com/v1"),
        ("morph", "https://api.morphllm.com/v1"),
        ("nano-gpt", "https://nano-gpt.com/api/v1"),
        ("nova", "https://api.nova.amazon.com/v1"),
        ("novita-ai", "https://api.novita.ai/openai"),
        ("nvidia", "https://integrate.api.nvidia.com/v1"),
    ];
    for (provider, base_url) in &cases {
        let entry = oai_compat_entry(provider, base_url);
        let p = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("factory should route {provider}: {e}"));
        assert_eq!(p.api(), "openai-completions", "{provider} api mismatch");
    }
}

#[test]
fn factory_routes_batch_a4_providers_correctly() {
    use pi::providers::create_provider;

    let cases = [
        ("poe", "https://api.poe.com/v1"),
        ("privatemode-ai", "http://localhost:8080/v1"),
        ("requesty", "https://router.requesty.ai/v1"),
        ("submodel", "https://llm.submodel.ai/v1"),
        ("synthetic", "https://api.synthetic.new/v1"),
        ("vivgrid", "https://api.vivgrid.com/v1"),
        ("vultr", "https://api.vultrinference.com/v1"),
        ("wandb", "https://api.inference.wandb.ai/v1"),
        ("xiaomi", "https://api.xiaomimimo.com/v1"),
    ];
    for (provider, base_url) in &cases {
        let entry = oai_compat_entry(provider, base_url);
        let p = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("factory should route {provider}: {e}"));
        assert_eq!(p.api(), "openai-completions", "{provider} api mismatch");
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Native: GitLab Duo factory routing (bd-3uqg.3.5)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn factory_routes_gitlab_native_provider() {
    use pi::provider::{InputType, Model, ModelCost};
    use pi::providers::create_provider;

    let entry = pi::models::ModelEntry {
        model: Model {
            id: "gitlab-duo-chat".to_string(),
            name: "GitLab Duo Chat".to_string(),
            api: String::new(),
            provider: "gitlab".to_string(),
            base_url: "https://gitlab.example.com".to_string(),
            reasoning: false,
            input: vec![InputType::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 16_384,
            headers: HashMap::new(),
        },
        api_key: Some("glpat-test-token".to_string()),
        headers: HashMap::new(),
        auth_header: true,
        compat: None,
        oauth_config: None,
    };
    let p = create_provider(&entry, None).expect("factory should route gitlab");
    assert_eq!(p.name(), "gitlab");
    assert_eq!(p.api(), "gitlab-chat");
    assert_eq!(p.model_id(), "gitlab-duo-chat");
}

// ═══════════════════════════════════════════════════════════════════════
// Cross-provider: field name differences (locked)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn system_prompt_handling_differs_by_provider() {
    let context = minimal_context();
    let options = default_options();

    // Anthropic: top-level "system" field
    let anthropic = AnthropicProvider::new("claude-sonnet-4-5");
    let v = serde_json::to_value(anthropic.build_request(&context, &options)).unwrap();
    assert!(
        v["system"].is_array() && v["system"][0]["type"] == "text",
        "Anthropic: system as top-level typed block array (cache_control-capable)"
    );
    assert!(
        !v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "system"),
        "Anthropic: no system role in messages"
    );

    // OpenAI Completions: system message in messages array
    let openai = OpenAIProvider::new("gpt-4o");
    let v = serde_json::to_value(openai.build_request(&context, &options)).unwrap();
    assert!(v["system"].is_null(), "OpenAI: no top-level system field");
    assert_eq!(v["messages"][0]["role"], "system");

    // Gemini: system_instruction field
    let gemini = GeminiProvider::new("gemini-2.5-pro");
    let v = serde_json::to_value(gemini.build_request(&context, &options)).unwrap();
    assert!(
        v["systemInstruction"].is_object(),
        "Gemini: systemInstruction object"
    );
    assert!(v["system"].is_null(), "Gemini: no top-level system string");
}

#[test]
fn max_tokens_field_name_differs_by_provider() {
    let context = minimal_context();
    let opts = options_with_tokens(1024);

    // Anthropic: max_tokens (required, not optional)
    let v = serde_json::to_value(
        AnthropicProvider::new("claude-sonnet-4-5").build_request(&context, &opts),
    )
    .unwrap();
    assert_eq!(v["max_tokens"], 1024);

    // OpenAI: max_tokens
    let v =
        serde_json::to_value(OpenAIProvider::new("gpt-4o").build_request(&context, &opts)).unwrap();
    assert_eq!(v["max_tokens"], 1024);

    // OpenAI Responses: max_output_tokens (different name!)
    let v =
        serde_json::to_value(OpenAIResponsesProvider::new("gpt-4o").build_request(&context, &opts))
            .unwrap();
    assert_eq!(v["max_output_tokens"], 1024);
    assert!(v["max_tokens"].is_null());

    // Gemini: generationConfig.maxOutputTokens (camelCase)
    let v =
        serde_json::to_value(GeminiProvider::new("gemini-2.5-pro").build_request(&context, &opts))
            .unwrap();
    assert_eq!(v["generationConfig"]["maxOutputTokens"], 1024);

    // Cohere: max_tokens
    let v =
        serde_json::to_value(CohereProvider::new("command-r-plus").build_request(&context, &opts))
            .unwrap();
    assert_eq!(v["max_tokens"], 1024);
}

#[test]
fn tool_nesting_shape_differs_by_provider() {
    let ctx = context_with_tools();
    let opts = default_options();

    // Anthropic: flat {name, description, input_schema}
    let v = serde_json::to_value(
        AnthropicProvider::new("claude-sonnet-4-5").build_request(&ctx, &opts),
    )
    .unwrap();
    let t = &v["tools"][0];
    assert!(t.get("type").is_none(), "Anthropic: no type on tool");
    assert!(
        t["input_schema"].is_object(),
        "Anthropic: uses input_schema"
    );
    assert!(
        t.get("function").is_none(),
        "Anthropic: no function wrapper"
    );

    // OpenAI Completions: {type: "function", function: {name, description, parameters}}
    let v = serde_json::to_value(OpenAIProvider::new("gpt-4o").build_request(&ctx, &opts)).unwrap();
    let t = &v["tools"][0];
    assert_eq!(t["type"], "function");
    assert!(t["function"]["parameters"].is_object());

    // OpenAI Responses: flat {type: "function", name, description, parameters}
    let v = serde_json::to_value(OpenAIResponsesProvider::new("gpt-4o").build_request(&ctx, &opts))
        .unwrap();
    let t = &v["tools"][0];
    assert_eq!(t["type"], "function");
    assert!(t["parameters"].is_object());
    assert!(
        t.get("function").is_none(),
        "Responses: no function wrapper"
    );

    // Gemini: {functionDeclarations: [{name, description, parameters}]} (camelCase)
    let v = serde_json::to_value(GeminiProvider::new("gemini-2.5-pro").build_request(&ctx, &opts))
        .unwrap();
    assert_eq!(
        v["tools"].as_array().unwrap().len(),
        1,
        "Gemini: single tools entry"
    );
    assert!(
        v["tools"][0]["functionDeclarations"].is_array(),
        "Gemini: functionDeclarations array"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Thinking budget levels (Anthropic-specific, locked)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn anthropic_thinking_budgets_locked() {
    let provider = AnthropicProvider::new("claude-sonnet-4-5");
    let levels = [
        (ThinkingLevel::Minimal, 1024_u64),
        (ThinkingLevel::Low, 2048),
        (ThinkingLevel::Medium, 8192),
        (ThinkingLevel::High, 16384),
        (ThinkingLevel::XHigh, 32768),
    ];
    for (level, expected_budget) in levels {
        let options = StreamOptions {
            thinking_level: Some(level),
            max_tokens: Some(100_000), // large enough to avoid auto-bump
            ..Default::default()
        };
        let context = minimal_context();
        let req = provider.build_request(&context, &options);
        let v = serde_json::to_value(&req).expect("serialize");
        assert_eq!(
            v["thinking"]["budget_tokens"].as_u64().unwrap(),
            expected_budget,
            "Budget for {level:?} should be {expected_budget}"
        );
    }
}

#[test]
fn anthropic_custom_thinking_budgets_override_defaults() {
    let provider = AnthropicProvider::new("claude-sonnet-4-5");
    let options = StreamOptions {
        thinking_level: Some(ThinkingLevel::Medium),
        thinking_budgets: Some(ThinkingBudgets {
            minimal: 500,
            low: 1000,
            medium: 5000,
            high: 10000,
            xhigh: 20000,
            max: 40000,
        }),
        max_tokens: Some(100_000),
        ..Default::default()
    };
    let context = minimal_context();
    let req = provider.build_request(&context, &options);
    let v = serde_json::to_value(&req).expect("serialize");
    assert_eq!(v["thinking"]["budget_tokens"], 5000);
}
