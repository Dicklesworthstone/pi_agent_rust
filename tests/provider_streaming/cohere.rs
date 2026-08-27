//! Cohere provider streaming tests (VCR playback/recording).

use super::{
    ProviderReplayCacheSpec, ScenarioExpectation, StreamExpectations, assert_error_translation,
    assert_stream_expectations, assert_tool_schema_fidelity, cassette_root, collect_events,
    log_summary, provider_request_schema_hash, record_provider_replay_cache_artifact,
    record_stream_contract_artifact, user_text, vcr_mode, vcr_strict,
};
use crate::common::TestHarness;
use pi::http::client::Client;
use pi::model::{Message, StopReason};
use pi::provider::{Context, Provider, StreamOptions, ToolDef};
use pi::providers::cohere::CohereProvider;
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

fn cohere_model() -> String {
    env::var("COHERE_TEST_MODEL").unwrap_or_else(|_| "command-a-03-2025".to_string())
}

fn cohere_api_key(mode: VcrMode) -> String {
    match mode {
        VcrMode::Record => {
            env::var("COHERE_API_KEY").expect("COHERE_API_KEY required for VCR record mode")
        }
        _ => env::var("COHERE_API_KEY").unwrap_or_else(|_| "test-key".to_string()),
    }
}

fn build_context(scenario: &Scenario) -> Context<'static> {
    Context {
        system_prompt: Some(SYSTEM_PROMPT.to_string().into()),
        messages: scenario.messages.clone().into(),
        tools: scenario.tools.clone().into(),
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
    let harness = TestHarness::new(format!("cohere_{}", scenario.name));
    let cassette_dir = cassette_root();
    let mode = vcr_mode();
    let cassette_path = cassette_dir.join(format!("{}.json", scenario.name));
    harness.record_artifact(format!("{}.json", scenario.name), &cassette_path);

    if mode == VcrMode::Playback && !cassette_path.exists() {
        let message = format!("Missing cassette {}", cassette_path.display());
        assert!(!vcr_strict(), "{message}");
        harness.log().warn("vcr", message);
        return;
    }

    let request_schema_hash = provider_request_schema_hash(
        &scenario.messages,
        &scenario.tools,
        &json!({
            "systemPrompt": SYSTEM_PROMPT,
            "maxTokens": scenario.options.max_tokens,
            "temperature": scenario.options.temperature,
        }),
    );
    record_provider_replay_cache_artifact(
        &harness,
        &ProviderReplayCacheSpec {
            provider: "cohere",
            route: "POST https://api.cohere.com/v2/chat",
            model: &scenario.model,
            scenario: scenario.name,
            cassette_path: &cassette_path,
            request_schema_hash: &request_schema_hash,
            mode,
        },
    );

    let api_key = cohere_api_key(mode);
    let recorder = VcrRecorder::new_with(scenario.name, mode, &cassette_dir);
    let client = Client::new().with_vcr(recorder);
    let provider = CohereProvider::new(scenario.model.clone()).with_client(client);
    let context = build_context(&scenario);
    let options = build_options(&scenario, api_key);

    harness
        .log()
        .info_ctx("scenario", "Cohere scenario", |ctx| {
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
            assert_tool_schema_fidelity(
                &harness,
                scenario.name,
                &scenario.tools,
                &summary.tool_calls,
            );
            record_stream_contract_artifact(
                &harness,
                "cohere",
                scenario.name,
                scenario.description,
                &summary,
            );
        }
        ScenarioExpectation::Error(expectation) => {
            let Err(err) = provider.stream(&context, &options).await else {
                panic!("expected error, got success");
            };
            let message = err.to_string();
            assert_error_translation(
                &harness,
                "cohere",
                scenario.name,
                scenario.description,
                &expectation,
                &message,
            );
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
        name: "cohere_simple_text",
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
        name: "cohere_multi_paragraph",
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
        name: "cohere_unicode_content",
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
        name: "cohere_tool_call_single",
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
        name: "cohere_tool_call_multiple",
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

fn scenario_long_response(model: &str) -> Scenario {
    Scenario {
        name: "cohere_very_long_response",
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

macro_rules! cohere_test {
    ($test_name:ident, $scenario_fn:ident) => {
        #[test]
        fn $test_name() {
            asupersync::test_utils::run_test(|| async {
                run_scenario($scenario_fn(&cohere_model())).await;
            });
        }
    };
}

cohere_test!(cohere_simple_text, scenario_simple_text);
cohere_test!(cohere_multi_paragraph, scenario_multi_paragraph);
cohere_test!(cohere_unicode_content, scenario_unicode);
cohere_test!(cohere_tool_call_single, scenario_tool_call_single);
cohere_test!(cohere_tool_call_multiple, scenario_tool_call_multiple);
cohere_test!(cohere_very_long_response, scenario_long_response);
