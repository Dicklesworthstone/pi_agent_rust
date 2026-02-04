//! `OpenAI` provider streaming tests (VCR playback/recording).

use super::{
    ScenarioExpectation, StreamExpectations, assert_stream_expectations,
    assistant_tool_call_message, cassette_root, collect_events, log_summary, tool_result_message,
    user_text, vcr_mode, vcr_strict,
};
use crate::common::TestHarness;
use pi::http::client::Client;
use pi::model::{Message, StopReason};
use pi::provider::{Context, Provider, StreamOptions, ToolDef};
use pi::providers::openai::OpenAIProvider;
use pi::vcr::{VcrMode, VcrRecorder};
use serde_json::json;
use std::env;

const SYSTEM_PROMPT: &str =
    "You are a test harness model. Follow instructions precisely and deterministically.";

#[derive(Clone)]
struct ScenarioOptions {
    max_tokens: u32,
    temperature: Option<f32>,
}

impl Default for ScenarioOptions {
    fn default() -> Self {
        Self {
            max_tokens: 256,
            temperature: Some(0.0),
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

fn openai_model() -> String {
    env::var("OPENAI_TEST_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string())
}

fn openai_api_key(mode: VcrMode) -> String {
    match mode {
        VcrMode::Record => {
            env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY required for VCR record mode")
        }
        _ => env::var("OPENAI_API_KEY").unwrap_or_else(|_| "test-key".to_string()),
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
    StreamOptions {
        api_key: Some(api_key),
        max_tokens: Some(scenario.options.max_tokens),
        temperature: scenario.options.temperature,
        ..Default::default()
    }
}

async fn run_scenario(scenario: Scenario) {
    let harness = TestHarness::new(format!("openai_{}", scenario.name));
    let cassette_dir = cassette_root();
    let mode = vcr_mode();
    let cassette_path = cassette_dir.join(format!("{}.json", scenario.name));
    harness.record_artifact(format!("{}.json", scenario.name), &cassette_path);

    if mode == VcrMode::Playback && !cassette_path.exists() {
        let message = format!("Missing cassette {}", cassette_path.display());
        if vcr_strict() {
            panic!("{message}");
        } else {
            harness.log().warn("vcr", message);
            return;
        }
    }

    let api_key = openai_api_key(mode);
    let recorder = VcrRecorder::new_with(scenario.name, mode, &cassette_dir);
    let client = Client::new().with_vcr(recorder);
    let provider = OpenAIProvider::new(scenario.model.clone()).with_client(client);
    let context = build_context(&scenario);
    let options = build_options(&scenario, api_key);

    harness
        .log()
        .info_ctx("scenario", "OpenAI scenario", |ctx| {
            ctx.push(("name".into(), scenario.name.to_string()));
            ctx.push(("description".into(), scenario.description.to_string()));
            ctx.push(("mode".into(), format!("{mode:?}")));
            ctx.push(("model".into(), scenario.model.clone()));
            ctx.push(("max_tokens".into(), scenario.options.max_tokens.to_string()));
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
                panic!("expected error, got success");
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

fn scenario_simple_text(model: &str) -> Scenario {
    Scenario {
        name: "openai_simple_text",
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
        name: "openai_multi_paragraph",
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

fn scenario_unicode(model: &str) -> Scenario {
    Scenario {
        name: "openai_unicode_content",
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

fn scenario_tool_call_single(model: &str) -> Scenario {
    Scenario {
        name: "openai_tool_call_single",
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
        name: "openai_tool_call_multiple",
        description: "Multiple tool calls response",
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
        name: "openai_tool_result_processing",
        description: "Tool result message handling",
        model: model.to_string(),
        messages: vec![
            user_text("Call the echo tool with text='hello'."),
            assistant_tool_call_message(
                "openai-responses",
                "openai",
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

fn scenario_bad_request(model: &str) -> Scenario {
    Scenario {
        name: "openai_bad_request_400",
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

fn scenario_auth_failure(model: &str) -> Scenario {
    Scenario {
        name: "openai_auth_failure_401",
        description: "Auth failure error (HTTP 401)",
        model: model.to_string(),
        messages: vec![user_text("Trigger an auth failure error.")],
        tools: Vec::new(),
        options: ScenarioOptions::default(),
        expectation: ScenarioExpectation::Error(super::ErrorExpectation {
            status: 401,
            contains: None,
        }),
    }
}

fn scenario_rate_limit(model: &str) -> Scenario {
    Scenario {
        name: "openai_rate_limit_429",
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

fn scenario_long_response(model: &str) -> Scenario {
    Scenario {
        name: "openai_very_long_response",
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

macro_rules! openai_test {
    ($test_name:ident, $scenario_fn:ident) => {
        #[test]
        fn $test_name() {
            asupersync::test_utils::run_test(|| async {
                run_scenario($scenario_fn(&openai_model())).await;
            });
        }
    };
}

openai_test!(openai_simple_text, scenario_simple_text);
openai_test!(openai_multi_paragraph, scenario_multi_paragraph);
openai_test!(openai_unicode_content, scenario_unicode);
openai_test!(openai_tool_call_single, scenario_tool_call_single);
openai_test!(openai_tool_call_multiple, scenario_tool_call_multiple);
openai_test!(
    openai_tool_result_processing,
    scenario_tool_result_processing
);
openai_test!(openai_bad_request_400, scenario_bad_request);
openai_test!(openai_auth_failure_401, scenario_auth_failure);
openai_test!(openai_rate_limit_429, scenario_rate_limit);
openai_test!(openai_very_long_response, scenario_long_response);
