use super::{
    ScenarioExpectation, StreamExpectations, assert_stream_expectations,
    assistant_tool_call_message, cassette_root, collect_events, log_summary, sha256_hex,
    tool_result_message, user_text, vcr_mode, vcr_strict,
};
use crate::common::TestHarness;
use chrono::{SecondsFormat, Utc};
use pi::http::client::Client;
use pi::model::{Message, StopReason, ThinkingLevel, UserContent};
use pi::provider::{CacheRetention, Context, Provider, StreamOptions, ThinkingBudgets, ToolDef};
use pi::providers::anthropic::AnthropicProvider;
use pi::vcr::{Cassette, Interaction, RecordedRequest, RecordedResponse, VcrMode, VcrRecorder};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const SYSTEM_PROMPT: &str =
    "You are a test harness model. Follow instructions precisely and deterministically.";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

#[derive(Clone)]
struct ScenarioOptions {
    max_tokens: u32,
    temperature: Option<f32>,
    thinking_level: Option<ThinkingLevel>,
    thinking_budgets: Option<ThinkingBudgets>,
    cache_retention: CacheRetention,
}

impl Default for ScenarioOptions {
    fn default() -> Self {
        Self {
            max_tokens: 256,
            temperature: Some(0.0),
            thinking_level: None,
            thinking_budgets: None,
            cache_retention: CacheRetention::None,
        }
    }
}

struct Scenario {
    name: &'static str,
    description: &'static str,
    model: String,
    messages: Vec<Message>,
    tools: Vec<ToolDef>,
    options: ScenarioOptions,
    expectation: ScenarioExpectation,
}

fn anthropic_model() -> String {
    env::var("ANTHROPIC_TEST_MODEL").unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string())
}

fn anthropic_api_key(mode: VcrMode) -> String {
    match mode {
        VcrMode::Record => {
            env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required for VCR record mode")
        }
        _ => env::var("ANTHROPIC_API_KEY").unwrap_or_else(|_| "vcr-playback".to_string()),
    }
}

fn build_context(scenario: &Scenario) -> Context {
    Context {
        system_prompt: Some(SYSTEM_PROMPT.to_string()),
        messages: scenario.messages.clone(),
        tools: scenario.tools.clone(),
    }
}

fn build_options(scenario: &Scenario, api_key: String) -> StreamOptions {
    const MIN_THINKING_BUDGET_TOKENS: u32 = 1024;

    // Anthropic requires `temperature` to be 1.0 when extended thinking is enabled.
    let thinking_enabled = scenario
        .options
        .thinking_level
        .is_some_and(|level| level != ThinkingLevel::Off);
    let temperature = if thinking_enabled {
        Some(1.0)
    } else {
        scenario.options.temperature
    };
    // If a scenario enables thinking but doesn't provide budgets, keep the recording cheap and
    // avoid invalid combinations like `max_tokens <= thinking.budget_tokens`.
    let thinking_budgets = scenario.options.thinking_budgets.clone().map_or_else(
        || {
            thinking_enabled.then_some(ThinkingBudgets {
                minimal: MIN_THINKING_BUDGET_TOKENS,
                low: MIN_THINKING_BUDGET_TOKENS,
                medium: MIN_THINKING_BUDGET_TOKENS,
                high: MIN_THINKING_BUDGET_TOKENS,
                xhigh: MIN_THINKING_BUDGET_TOKENS,
            })
        },
        |budgets| {
            Some(ThinkingBudgets {
                minimal: budgets.minimal.max(MIN_THINKING_BUDGET_TOKENS),
                low: budgets.low.max(MIN_THINKING_BUDGET_TOKENS),
                medium: budgets.medium.max(MIN_THINKING_BUDGET_TOKENS),
                high: budgets.high.max(MIN_THINKING_BUDGET_TOKENS),
                xhigh: budgets.xhigh.max(MIN_THINKING_BUDGET_TOKENS),
            })
        },
    );
    let max_tokens = if thinking_enabled {
        let budgets = thinking_budgets
            .as_ref()
            .expect("thinking budgets when thinking is enabled");
        let thinking_level = scenario
            .options
            .thinking_level
            .expect("thinking level when thinking is enabled");
        let budget_tokens = match thinking_level {
            ThinkingLevel::Off => 0,
            ThinkingLevel::Minimal => budgets.minimal,
            ThinkingLevel::Low => budgets.low,
            ThinkingLevel::Medium => budgets.medium,
            ThinkingLevel::High => budgets.high,
            ThinkingLevel::XHigh => budgets.xhigh,
        };
        scenario
            .options
            .max_tokens
            .max(budget_tokens.saturating_add(256))
    } else {
        scenario.options.max_tokens
    };
    StreamOptions {
        api_key: Some(api_key),
        max_tokens: Some(max_tokens),
        temperature,
        thinking_level: scenario.options.thinking_level,
        thinking_budgets,
        cache_retention: scenario.options.cache_retention,
        ..Default::default()
    }
}

/// Generate a VCR error fixture cassette for scenarios where errors cannot be recorded from
/// real API endpoints on demand. These are deterministic fixtures that match the request format
/// the provider sends and return an appropriate error response.
fn generate_error_fixture(
    cassette_path: &Path,
    scenario: &Scenario,
    expectation: &super::ErrorExpectation,
    harness: &TestHarness,
) {
    let options = build_options(scenario, "vcr-playback".to_string());

    // Build the request body matching AnthropicProvider's serialization.
    // Error scenarios use simple user text messages only.
    let messages: Vec<serde_json::Value> = scenario
        .messages
        .iter()
        .map(|msg| match msg {
            Message::User(u) => {
                let text = match &u.content {
                    UserContent::Text(t) => t.clone(),
                    _ => String::new(),
                };
                json!({"role": "user", "content": [{"type": "text", "text": text}]})
            }
            _ => json!({}),
        })
        .collect();

    let mut body = json!({
        "max_tokens": options.max_tokens.unwrap_or(8192),
        "messages": messages,
        "model": scenario.model,
        "stream": true,
    });
    let context = build_context(scenario);
    if let Some(system) = &context.system_prompt {
        body["system"] = json!(system);
    }
    if let Some(temp) = options.temperature {
        body["temperature"] = json!(temp);
    }

    let (error_type, error_message) = match expectation.status {
        400 => ("invalid_request_error", "Bad request."),
        401 => ("authentication_error", "Invalid API key provided."),
        403 => ("permission_error", "Forbidden."),
        429 => (
            "rate_limit_error",
            "Rate limit exceeded. Please retry after 30 seconds.",
        ),
        500 => ("api_error", "Internal server error."),
        529 => ("overloaded_error", "API is temporarily overloaded."),
        _ => ("api_error", "Unknown error."),
    };

    let mut response_headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if expectation.status == 429 {
        response_headers.push(("retry-after".to_string(), "30".to_string()));
    }

    let error_body = json!({
        "type": "error",
        "error": {"type": error_type, "message": error_message},
    });

    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: scenario.name.to_string(),
        recorded_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        interactions: vec![Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: ANTHROPIC_MESSAGES_URL.to_string(),
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("Accept".to_string(), "text/event-stream".to_string()),
                    ("X-API-Key".to_string(), "[REDACTED]".to_string()),
                    ("anthropic-version".to_string(), "2023-06-01".to_string()),
                    ("Content-Type".to_string(), "application/json".to_string()),
                ],
                body: Some(body),
                body_text: None,
            },
            response: RecordedResponse {
                status: expectation.status,
                headers: response_headers,
                body_chunks: vec![serde_json::to_string(&error_body).unwrap_or_default()],
            },
        }],
    };

    let serialized = serde_json::to_string_pretty(&cassette)
        .unwrap_or_else(|err| panic!("Failed to serialize error fixture: {err}"));
    if let Some(parent) = cassette_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(cassette_path, &serialized).unwrap_or_else(|err| {
        panic!(
            "Failed to write error fixture {}: {err}",
            cassette_path.display()
        )
    });
    harness.log().info(
        "fixture",
        format!(
            "Generated error fixture {} (HTTP {})",
            cassette_path.display(),
            expectation.status
        ),
    );
}

#[allow(clippy::too_many_lines)]
async fn run_scenario(scenario: Scenario) {
    let harness = TestHarness::new(format!("anthropic_{}", scenario.name));
    let cassette_dir = cassette_root();
    let mode = vcr_mode();
    let cassette_path = cassette_dir.join(format!("{}.json", scenario.name));
    harness.record_artifact(format!("{}.json", scenario.name), &cassette_path);

    let error_expectation = match &scenario.expectation {
        ScenarioExpectation::Error(expectation) => Some(expectation.clone()),
        ScenarioExpectation::Stream(_) => None,
    };

    // Error scenarios use pre-built fixture cassettes rather than mock HTTP servers.
    // Errors (4xx, 5xx, 529) cannot be triggered on demand from real API endpoints, so we
    // generate deterministic fixture cassettes that the VCR replays in playback mode.
    let effective_mode = if let Some(expectation) = error_expectation.as_ref() {
        if !cassette_path.exists() {
            generate_error_fixture(&cassette_path, &scenario, expectation, &harness);
        }
        VcrMode::Playback
    } else {
        mode
    };

    let cassette_exists = cassette_path.exists();
    if effective_mode == VcrMode::Playback && !cassette_exists {
        let message = format!("Missing cassette {}", cassette_path.display());
        if vcr_strict() {
            assert!(cassette_exists, "{}", message);
        } else {
            harness.log().warn("vcr", message);
            return;
        }
    }

    let recorder = VcrRecorder::new_with(scenario.name, effective_mode, &cassette_dir);
    let client = Client::new().with_vcr(recorder);
    let provider = AnthropicProvider::new(scenario.model.clone()).with_client(client);
    let context = build_context(&scenario);
    let options = build_options(&scenario, anthropic_api_key(effective_mode));

    harness
        .log()
        .info_ctx("scenario", "Anthropic scenario", |ctx| {
            ctx.push(("name".into(), scenario.name.to_string()));
            ctx.push(("description".into(), scenario.description.to_string()));
            ctx.push(("mode".into(), format!("{effective_mode:?}")));
            ctx.push(("model".into(), scenario.model.clone()));
            ctx.push(("max_tokens".into(), scenario.options.max_tokens.to_string()));
            if let Some(level) = scenario.options.thinking_level {
                ctx.push(("thinking_level".into(), format!("{level:?}")));
            }
        });

    match scenario.expectation.clone() {
        ScenarioExpectation::Stream(expectations) => {
            let stream = provider
                .stream(&context, &options)
                .await
                .expect("expected stream");
            let outcome = collect_events(stream).await;
            let summary = super::summarize_events(&outcome);
            log_summary(&harness, scenario.name, &summary);
            assert_stream_expectations(&harness, scenario.name, &summary, &expectations);
        }
        ScenarioExpectation::Error(expectation) => {
            let Err(err) = provider.stream(&context, &options).await else {
                unreachable!("expected error, got success for scenario {}", scenario.name);
            };
            let message = err.to_string();
            let needle = format!("HTTP {}", expectation.status);
            assert!(
                message.contains(&needle),
                "expected error to contain '{needle}', got '{message}'"
            );
            if let Some(fragment) = expectation.contains {
                assert!(
                    message.contains(fragment),
                    "expected error to contain '{fragment}', got '{message}'"
                );
            }
            harness.log().info("error", message);
        }
    }

    if effective_mode == VcrMode::Record {
        update_manifest(&cassette_path, &scenario, &harness);
    }
}

fn tool_echo() -> ToolDef {
    ToolDef {
        name: "echo".to_string(),
        description: "Echo the provided text.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" }
            },
            "required": ["text"]
        }),
    }
}

fn tool_add() -> ToolDef {
    ToolDef {
        name: "add".to_string(),
        description: "Add two numbers.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "a": { "type": "number" },
                "b": { "type": "number" }
            },
            "required": ["a", "b"]
        }),
    }
}

fn tool_multiply() -> ToolDef {
    ToolDef {
        name: "multiply".to_string(),
        description: "Multiply two numbers.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "a": { "type": "number" },
                "b": { "type": "number" }
            },
            "required": ["a", "b"]
        }),
    }
}

fn tool_store_blob() -> ToolDef {
    ToolDef {
        name: "store_blob".to_string(),
        description: "Store a blob payload.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "payload": { "type": "string" },
                "metadata": { "type": "object" }
            },
            "required": ["payload"]
        }),
    }
}

fn scenario_simple_text(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_simple_text",
        description: "Simple text response",
        model: model.to_string(),
        messages: vec![user_text("Reply with the single word: pong.")],
        tools: Vec::new(),
        options: ScenarioOptions {
            max_tokens: 64,
            ..ScenarioOptions::default()
        },
        expectation: ScenarioExpectation::Stream(StreamExpectations {
            min_text_deltas: 1,
            allowed_stop_reasons: Some(vec![StopReason::Stop]),
            ..StreamExpectations::default()
        }),
    }
}

fn scenario_multi_paragraph(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_multi_paragraph",
        description: "Multi-paragraph response",
        model: model.to_string(),
        messages: vec![user_text(
            "Reply with two paragraphs separated by a blank line. \
             Paragraph one must be 'Paragraph 1.' and paragraph two must be 'Paragraph 2.'.",
        )],
        tools: Vec::new(),
        options: ScenarioOptions::default(),
        expectation: ScenarioExpectation::Stream(StreamExpectations {
            min_text_deltas: 1,
            require_blank_line: true,
            ..StreamExpectations::default()
        }),
    }
}

fn scenario_extended_thinking(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_extended_thinking",
        description: "Thinking enabled with text response",
        model: model.to_string(),
        messages: vec![user_text("Compute 12 * 13 and reply with 'Result: 156'.")],
        tools: Vec::new(),
        options: ScenarioOptions {
            thinking_level: Some(ThinkingLevel::Medium),
            ..ScenarioOptions::default()
        },
        expectation: ScenarioExpectation::Stream(StreamExpectations {
            min_text_deltas: 1,
            min_thinking_deltas: 1,
            ..StreamExpectations::default()
        }),
    }
}

fn scenario_tool_call_single(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_tool_call_single",
        description: "Single tool call response",
        model: model.to_string(),
        messages: vec![user_text(
            "Call the echo tool with text='hello'. Do not answer in text.",
        )],
        tools: vec![tool_echo()],
        options: ScenarioOptions::default(),
        expectation: ScenarioExpectation::Stream(StreamExpectations {
            min_tool_calls: 1,
            allowed_stop_reasons: Some(vec![StopReason::ToolUse]),
            ..StreamExpectations::default()
        }),
    }
}

fn scenario_tool_call_multiple(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_tool_call_multiple",
        description: "Multiple tool calls in one response",
        model: model.to_string(),
        messages: vec![user_text(
            "Call add with a=2 b=3, then call multiply with a=4 b=5. Do not answer in text.",
        )],
        tools: vec![tool_add(), tool_multiply()],
        options: ScenarioOptions::default(),
        expectation: ScenarioExpectation::Stream(StreamExpectations {
            min_tool_calls: 2,
            allowed_stop_reasons: Some(vec![StopReason::ToolUse]),
            ..StreamExpectations::default()
        }),
    }
}

fn scenario_tool_result_processing(model: &str) -> Scenario {
    let tool_call_id = "tool_1";
    Scenario {
        name: "anthropic_tool_result_processing",
        description: "Tool result message handling",
        model: model.to_string(),
        messages: vec![
            user_text("Call the echo tool with text='hello'."),
            assistant_tool_call_message(
                "anthropic-messages",
                "anthropic",
                model,
                tool_call_id,
                "echo",
                json!({"text": "hello"}),
            ),
            tool_result_message(tool_call_id, "echo", "hello", false),
            user_text("Respond with the single word: done."),
        ],
        tools: vec![tool_echo()],
        options: ScenarioOptions::default(),
        expectation: ScenarioExpectation::Stream(StreamExpectations {
            min_text_deltas: 1,
            allowed_stop_reasons: Some(vec![StopReason::Stop]),
            ..StreamExpectations::default()
        }),
    }
}

fn scenario_rate_limit(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_rate_limit_429",
        description: "Rate limit error (HTTP 429)",
        model: model.to_string(),
        messages: vec![user_text("Trigger a rate limit error.")],
        tools: Vec::new(),
        options: ScenarioOptions::default(),
        expectation: ScenarioExpectation::Error(super::ErrorExpectation {
            status: 429,
            contains: None,
        }),
    }
}

fn scenario_auth_failure(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_auth_failure_401",
        description: "Auth failure (HTTP 401)",
        model: model.to_string(),
        messages: vec![user_text("Trigger an auth failure.")],
        tools: Vec::new(),
        options: ScenarioOptions::default(),
        expectation: ScenarioExpectation::Error(super::ErrorExpectation {
            status: 401,
            contains: None,
        }),
    }
}

fn scenario_forbidden(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_forbidden_403",
        description: "Forbidden error (HTTP 403)",
        model: model.to_string(),
        messages: vec![user_text("Trigger a forbidden error.")],
        tools: Vec::new(),
        options: ScenarioOptions::default(),
        expectation: ScenarioExpectation::Error(super::ErrorExpectation {
            status: 403,
            contains: None,
        }),
    }
}

fn scenario_bad_request(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_bad_request_400",
        description: "Bad request error (HTTP 400)",
        model: model.to_string(),
        messages: vec![user_text("Trigger a bad request error.")],
        tools: Vec::new(),
        options: ScenarioOptions::default(),
        expectation: ScenarioExpectation::Error(super::ErrorExpectation {
            status: 400,
            contains: None,
        }),
    }
}

fn scenario_server_error(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_server_error_500",
        description: "Server error (HTTP 500)",
        model: model.to_string(),
        messages: vec![user_text("Trigger a server error.")],
        tools: Vec::new(),
        options: ScenarioOptions::default(),
        expectation: ScenarioExpectation::Error(super::ErrorExpectation {
            status: 500,
            contains: None,
        }),
    }
}

fn scenario_overloaded(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_overloaded_529",
        description: "Overloaded error (HTTP 529)",
        model: model.to_string(),
        messages: vec![user_text("Trigger an overloaded error.")],
        tools: Vec::new(),
        options: ScenarioOptions::default(),
        expectation: ScenarioExpectation::Error(super::ErrorExpectation {
            status: 529,
            contains: None,
        }),
    }
}

fn scenario_empty_response(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_empty_response",
        description: "Empty response (no content blocks)",
        model: model.to_string(),
        messages: vec![user_text("Respond with an empty message and no text.")],
        tools: Vec::new(),
        options: ScenarioOptions {
            max_tokens: 16,
            ..ScenarioOptions::default()
        },
        expectation: ScenarioExpectation::Stream(StreamExpectations::default()),
    }
}

fn scenario_long_response(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_very_long_response",
        description: "Very long response hitting max tokens",
        model: model.to_string(),
        messages: vec![user_text(
            "Write 50 short sentences about Rust without stopping early.",
        )],
        tools: Vec::new(),
        options: ScenarioOptions {
            max_tokens: 64,
            ..ScenarioOptions::default()
        },
        expectation: ScenarioExpectation::Stream(StreamExpectations {
            min_text_deltas: 1,
            allowed_stop_reasons: Some(vec![StopReason::Length, StopReason::Stop]),
            ..StreamExpectations::default()
        }),
    }
}

fn scenario_unicode(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_unicode_content",
        description: "Unicode content response",
        model: model.to_string(),
        messages: vec![user_text("Reply with: 😀 你好 שלום")],
        tools: Vec::new(),
        options: ScenarioOptions::default(),
        expectation: ScenarioExpectation::Stream(StreamExpectations {
            min_text_deltas: 1,
            require_unicode: true,
            ..StreamExpectations::default()
        }),
    }
}

fn scenario_large_tool_args(model: &str) -> Scenario {
    let payload = "A".repeat(2048);
    let prompt = format!("Call store_blob with payload exactly: {payload}. Do not answer in text.");
    Scenario {
        name: "anthropic_large_tool_args",
        description: "Large tool call arguments",
        model: model.to_string(),
        messages: vec![user_text(&prompt)],
        tools: vec![tool_store_blob()],
        options: ScenarioOptions::default(),
        expectation: ScenarioExpectation::Stream(StreamExpectations {
            min_tool_calls: 1,
            min_tool_args_bytes: Some(1024),
            allowed_stop_reasons: Some(vec![StopReason::ToolUse]),
            ..StreamExpectations::default()
        }),
    }
}

fn scenario_stream_interruption(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_stream_interruption",
        description: "Stream interruption / partial response",
        model: model.to_string(),
        messages: vec![user_text("Reply with the word: interrupted.")],
        tools: Vec::new(),
        options: ScenarioOptions {
            max_tokens: 32,
            ..ScenarioOptions::default()
        },
        expectation: ScenarioExpectation::Stream(StreamExpectations {
            allow_stream_error: true,
            ..StreamExpectations::default()
        }),
    }
}

fn scenario_thinking_only(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_thinking_only",
        description: "Thinking only, no final text",
        model: model.to_string(),
        messages: vec![user_text(
            "Think about primes but do not provide any final text response.",
        )],
        tools: Vec::new(),
        options: ScenarioOptions {
            thinking_level: Some(ThinkingLevel::High),
            ..ScenarioOptions::default()
        },
        expectation: ScenarioExpectation::Stream(StreamExpectations {
            min_thinking_deltas: 1,
            ..StreamExpectations::default()
        }),
    }
}

fn scenario_thinking_with_tools(model: &str) -> Scenario {
    Scenario {
        name: "anthropic_thinking_with_tool_calls",
        description: "Thinking with tool calls",
        model: model.to_string(),
        messages: vec![user_text(
            "Think, then call echo with text='hi'. Do not answer in text.",
        )],
        tools: vec![tool_echo()],
        options: ScenarioOptions {
            thinking_level: Some(ThinkingLevel::High),
            ..ScenarioOptions::default()
        },
        expectation: ScenarioExpectation::Stream(StreamExpectations {
            min_thinking_deltas: 1,
            min_tool_calls: 1,
            allowed_stop_reasons: Some(vec![StopReason::ToolUse]),
            ..StreamExpectations::default()
        }),
    }
}

fn scenario_thinking_budget_exceeded(model: &str) -> Scenario {
    let budgets = ThinkingBudgets {
        minimal: 64,
        low: 64,
        medium: 64,
        high: 64,
        xhigh: 64,
    };
    Scenario {
        name: "anthropic_thinking_budget_exceeded",
        description: "Thinking budget exceeded",
        model: model.to_string(),
        messages: vec![user_text(
            "Think in detail about prime numbers, then answer with a summary.",
        )],
        tools: Vec::new(),
        options: ScenarioOptions {
            thinking_level: Some(ThinkingLevel::High),
            thinking_budgets: Some(budgets),
            max_tokens: 96,
            ..ScenarioOptions::default()
        },
        expectation: ScenarioExpectation::Stream(StreamExpectations {
            min_thinking_deltas: 1,
            allowed_stop_reasons: Some(vec![StopReason::Length, StopReason::Stop]),
            ..StreamExpectations::default()
        }),
    }
}

macro_rules! anthropic_test {
    ($test_name:ident, $scenario_fn:ident) => {
        #[test]
        fn $test_name() {
            asupersync::test_utils::run_test(|| async {
                run_scenario($scenario_fn(&anthropic_model())).await;
            });
        }
    };
}

anthropic_test!(anthropic_simple_text, scenario_simple_text);
anthropic_test!(anthropic_multi_paragraph, scenario_multi_paragraph);
anthropic_test!(anthropic_extended_thinking, scenario_extended_thinking);
anthropic_test!(anthropic_tool_call_single, scenario_tool_call_single);
anthropic_test!(anthropic_tool_call_multiple, scenario_tool_call_multiple);
anthropic_test!(
    anthropic_tool_result_processing,
    scenario_tool_result_processing
);
anthropic_test!(anthropic_rate_limit_429, scenario_rate_limit);
anthropic_test!(anthropic_auth_failure_401, scenario_auth_failure);
anthropic_test!(anthropic_forbidden_403, scenario_forbidden);
anthropic_test!(anthropic_bad_request_400, scenario_bad_request);
anthropic_test!(anthropic_server_error_500, scenario_server_error);
anthropic_test!(anthropic_overloaded_529, scenario_overloaded);
anthropic_test!(anthropic_empty_response, scenario_empty_response);
anthropic_test!(anthropic_very_long_response, scenario_long_response);
anthropic_test!(anthropic_unicode_content, scenario_unicode);
anthropic_test!(anthropic_large_tool_args, scenario_large_tool_args);
anthropic_test!(anthropic_stream_interruption, scenario_stream_interruption);
anthropic_test!(anthropic_thinking_only, scenario_thinking_only);
anthropic_test!(anthropic_thinking_with_tools, scenario_thinking_with_tools);
anthropic_test!(
    anthropic_thinking_budget_exceeded,
    scenario_thinking_budget_exceeded
);

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: String,
    generated_at: String,
    scenarios: Vec<ManifestEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestEntry {
    name: String,
    description: String,
    cassette: String,
    model: String,
    max_tokens: u32,
    temperature: Option<f32>,
    thinking_level: Option<String>,
    prompt_hash: String,
    recorded_at: Option<String>,
}

static MANIFEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn manifest_lock() -> &'static Mutex<()> {
    MANIFEST_LOCK.get_or_init(|| Mutex::new(()))
}

fn manifest_path() -> PathBuf {
    cassette_root().join("anthropic_manifest.json")
}

fn update_manifest(cassette_path: &Path, scenario: &Scenario, harness: &TestHarness) {
    let _lock = manifest_lock().lock().expect("manifest lock");
    let raw = match std::fs::read_to_string(cassette_path) {
        Ok(raw) => raw,
        Err(err) => {
            harness
                .log()
                .warn("manifest", format!("Failed to read cassette: {err}"));
            return;
        }
    };
    let cassette: Cassette = match serde_json::from_str(&raw) {
        Ok(cassette) => cassette,
        Err(err) => {
            harness
                .log()
                .warn("manifest", format!("Failed to parse cassette: {err}"));
            return;
        }
    };
    let Some(interaction) = cassette.interactions.last() else {
        harness
            .log()
            .warn("manifest", "No interactions in cassette");
        return;
    };
    let prompt_hash = hash_request(&interaction.request);

    let path = manifest_path();
    let mut manifest = if path.exists() {
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        serde_json::from_str(&raw).unwrap_or_else(|_| Manifest {
            version: "1.0".to_string(),
            generated_at: String::new(),
            scenarios: Vec::new(),
        })
    } else {
        Manifest {
            version: "1.0".to_string(),
            generated_at: String::new(),
            scenarios: Vec::new(),
        }
    };

    let entry = ManifestEntry {
        name: scenario.name.to_string(),
        description: scenario.description.to_string(),
        cassette: cassette_path.file_name().map_or_else(
            || cassette_path.display().to_string(),
            |name| name.to_string_lossy().to_string(),
        ),
        model: scenario.model.clone(),
        max_tokens: scenario.options.max_tokens,
        temperature: scenario.options.temperature,
        thinking_level: scenario
            .options
            .thinking_level
            .map(|level| format!("{level:?}")),
        prompt_hash,
        recorded_at: Some(cassette.recorded_at.clone()),
    };

    if let Some(existing) = manifest
        .scenarios
        .iter_mut()
        .find(|item| item.name == entry.name)
    {
        *existing = entry;
    } else {
        manifest.scenarios.push(entry);
    }
    manifest.scenarios.sort_by(|a, b| a.name.cmp(&b.name));
    manifest.generated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);

    let serialized = match serde_json::to_string_pretty(&manifest) {
        Ok(serialized) => serialized,
        Err(err) => {
            harness
                .log()
                .warn("manifest", format!("Failed to serialize manifest: {err}"));
            return;
        }
    };
    if let Err(err) = std::fs::write(&path, serialized) {
        harness
            .log()
            .warn("manifest", format!("Failed to write manifest: {err}"));
        return;
    }
    harness.record_artifact("anthropic_manifest.json", &path);
}

fn hash_request(request: &RecordedRequest) -> String {
    request.body.as_ref().map_or_else(
        || {
            request
                .body_text
                .as_ref()
                .map_or_else(|| sha256_hex(&[]), |text| sha256_hex(text.as_bytes()))
        },
        |body| {
            serde_json::to_vec(body).map_or_else(|_| sha256_hex(&[]), |bytes| sha256_hex(&bytes))
        },
    )
}
