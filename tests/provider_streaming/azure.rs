use super::{
    ScenarioExpectation, StreamExpectations, assert_stream_expectations,
    assistant_tool_call_message, cassette_root, collect_events, log_summary, tool_result_message,
    user_text, vcr_mode, vcr_strict,
};
use crate::common::TestHarness;
use pi::http::client::Client;
use pi::model::{Message, StopReason};
use pi::provider::{Context, Provider, StreamOptions, ToolDef};
use pi::providers::azure::AzureOpenAIProvider;
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
    deployment: String,
    messages: Vec<Message>,
    tools: Vec<ToolDef>,
    options: ScenarioOptions,
    expectation: ScenarioExpectation,
}

fn azure_resource() -> String {
    env::var("AZURE_OPENAI_RESOURCE").unwrap_or_else(|_| "test-resource".to_string())
}

fn azure_deployment() -> String {
    env::var("AZURE_OPENAI_DEPLOYMENT").unwrap_or_else(|_| "test-deployment".to_string())
}

fn azure_api_key(mode: VcrMode) -> String {
    match mode {
        VcrMode::Record => env::var("AZURE_OPENAI_API_KEY")
            .expect("AZURE_OPENAI_API_KEY required for VCR record mode"),
        _ => env::var("AZURE_OPENAI_API_KEY").unwrap_or_else(|_| "test-key".to_string()),
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
    let harness = TestHarness::new(format!("azure_{}", scenario.name));
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

    let api_key = azure_api_key(mode);
    let recorder = VcrRecorder::new_with(scenario.name, mode, &cassette_dir);
    let client = Client::new().with_vcr(recorder);
    let resource = azure_resource();
    let provider =
        AzureOpenAIProvider::new(resource, scenario.deployment.clone()).with_client(client);
    let context = build_context(&scenario);
    let options = build_options(&scenario, api_key);

    harness
        .log()
        .info_ctx("scenario", "Azure OpenAI scenario", |ctx| {
            ctx.push(("name".into(), scenario.name.to_string()));
            ctx.push(("description".into(), scenario.description.to_string()));
            ctx.push(("mode".into(), format!("{mode:?}")));
            ctx.push(("deployment".into(), scenario.deployment.clone()));
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

fn scenario_simple_text(deployment: &str) -> Scenario {
    Scenario {
        name: "azure_simple_text",
        description: "Simple text response",
        deployment: deployment.to_string(),
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

fn scenario_unicode(deployment: &str) -> Scenario {
    Scenario {
        name: "azure_unicode_content",
        description: "Unicode content response",
        deployment: deployment.to_string(),
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

fn scenario_tool_call_single(deployment: &str) -> Scenario {
    Scenario {
        name: "azure_tool_call_single",
        description: "Single tool call response",
        deployment: deployment.to_string(),
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

fn scenario_tool_result_processing(deployment: &str) -> Scenario {
    let tool_call_id = "tool_1";
    Scenario {
        name: "azure_tool_result_processing",
        description: "Tool result message handling",
        deployment: deployment.to_string(),
        messages: vec![
            user_text("Call the echo tool with text='hello'."),
            assistant_tool_call_message(
                "azure-openai",
                "azure",
                deployment,
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

fn scenario_auth_failure(deployment: &str) -> Scenario {
    Scenario {
        name: "azure_auth_failure_401",
        description: "Auth failure error (HTTP 401)",
        deployment: deployment.to_string(),
        messages: vec![user_text("Trigger an auth failure error.")],
        tools: Vec::new(),
        options: ScenarioOptions::default(),
        expectation: ScenarioExpectation::Error(super::ErrorExpectation {
            status: 401,
            contains: None,
        }),
    }
}

fn scenario_long_response(deployment: &str) -> Scenario {
    Scenario {
        name: "azure_very_long_response",
        description: "Very long response hitting max tokens",
        deployment: deployment.to_string(),
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

macro_rules! azure_test {
    ($test_name:ident, $scenario_fn:ident) => {
        #[test]
        fn $test_name() {
            asupersync::test_utils::run_test(|| async {
                run_scenario($scenario_fn(&azure_deployment())).await;
            });
        }
    };
}

azure_test!(azure_simple_text, scenario_simple_text);
azure_test!(azure_unicode_content, scenario_unicode);
azure_test!(azure_tool_call_single, scenario_tool_call_single);
azure_test!(
    azure_tool_result_processing,
    scenario_tool_result_processing
);
azure_test!(azure_auth_failure_401, scenario_auth_failure);
azure_test!(azure_very_long_response, scenario_long_response);
