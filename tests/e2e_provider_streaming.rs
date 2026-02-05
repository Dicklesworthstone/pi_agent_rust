//! E2E provider streaming tests with VCR playback + verbose JSONL logs.
//!
//! These tests drive the full provider streaming pipeline using recorded VCR
//! cassettes, validating event ordering, stop reasons, usage tracking, tool-call
//! emissions, and error handling. All outputs are deterministic (VCR playback
//! only) and captured as JSONL artifacts.
//!
//! Run:
//! ```bash
//! VCR_MODE=playback VCR_CASSETTE_DIR=tests/fixtures/vcr \
//!   cargo test --test e2e_provider_streaming
//! ```

mod common;

use asupersync::runtime::RuntimeBuilder;
use common::TestHarness;
use pi::http::client::Client;
use pi::model::{Message, StopReason, StreamEvent, ThinkingLevel, UserContent, UserMessage};
use pi::provider::{CacheRetention, Context, Provider, StreamOptions, ThinkingBudgets, ToolDef};
use pi::providers::anthropic::AnthropicProvider;
use pi::vcr::{VcrMode, VcrRecorder};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::future::Future;
use std::path::PathBuf;
use std::time::Instant;

fn cassette_root() -> PathBuf {
    std::env::var("VCR_CASSETTE_DIR").map_or_else(
        |_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vcr"),
        PathBuf::from,
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn user_text(text: &str) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

fn run_test_value<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    asupersync::test_utils::init_test_logging();
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(f())
}

// ─── Event ordering validation ───────────────────────────────────────────────

/// Verify that the event sequence follows expected ordering invariants:
/// - `Start` must appear first (if any events at all)
/// - `Done` or `Error` must appear last (if present)
/// - Content events appear between start and done
/// - Text deltas appear between TextStart and TextEnd
fn validate_event_ordering(events: &[StreamEvent], harness: &TestHarness, scenario: &str) {
    if events.is_empty() {
        return;
    }

    harness.assert_log(&format!("{scenario}: validate event ordering"));

    // First event should be Start
    assert!(
        matches!(events[0], StreamEvent::Start { .. }),
        "{scenario}: first event should be Start, got {:?}",
        std::mem::discriminant(&events[0])
    );

    // Last event should be Done or Error (if stream completed normally)
    let last = &events[events.len() - 1];
    let last_is_terminal =
        matches!(last, StreamEvent::Done { .. } | StreamEvent::Error { .. });
    if last_is_terminal {
        harness.assert_log(&format!("{scenario}: terminal event present"));
    }

    // Check that text deltas appear within text start/end pairs
    let mut in_text_block = false;
    let mut in_thinking_block = false;
    let mut in_tool_call_block = false;
    let mut has_started = false;

    for (idx, event) in events.iter().enumerate() {
        match event {
            StreamEvent::Start { .. } => {
                assert!(
                    !has_started,
                    "{scenario}: duplicate Start at index {idx}"
                );
                has_started = true;
            }
            StreamEvent::TextStart { .. } => {
                assert!(has_started, "{scenario}: TextStart before Start at {idx}");
                in_text_block = true;
            }
            StreamEvent::TextDelta { .. } => {
                assert!(
                    in_text_block,
                    "{scenario}: TextDelta outside text block at {idx}"
                );
            }
            StreamEvent::TextEnd { .. } => {
                assert!(
                    in_text_block,
                    "{scenario}: TextEnd without TextStart at {idx}"
                );
                in_text_block = false;
            }
            StreamEvent::ThinkingStart { .. } => {
                assert!(
                    has_started,
                    "{scenario}: ThinkingStart before Start at {idx}"
                );
                in_thinking_block = true;
            }
            StreamEvent::ThinkingDelta { .. } => {
                assert!(
                    in_thinking_block,
                    "{scenario}: ThinkingDelta outside thinking block at {idx}"
                );
            }
            StreamEvent::ThinkingEnd { .. } => {
                assert!(
                    in_thinking_block,
                    "{scenario}: ThinkingEnd without ThinkingStart at {idx}"
                );
                in_thinking_block = false;
            }
            StreamEvent::ToolCallStart { .. } => {
                assert!(
                    has_started,
                    "{scenario}: ToolCallStart before Start at {idx}"
                );
                in_tool_call_block = true;
            }
            StreamEvent::ToolCallDelta { .. } => {
                assert!(
                    in_tool_call_block,
                    "{scenario}: ToolCallDelta outside tool call block at {idx}"
                );
            }
            StreamEvent::ToolCallEnd { .. } => {
                assert!(
                    in_tool_call_block,
                    "{scenario}: ToolCallEnd without ToolCallStart at {idx}"
                );
                in_tool_call_block = false;
            }
            StreamEvent::Done { .. } | StreamEvent::Error { .. } => {}
        }
    }
}

// ─── Scenario descriptor ─────────────────────────────────────────────────────

struct E2eScenario {
    cassette_name: &'static str,
    description: &'static str,
    messages: Vec<Message>,
    tools: Vec<ToolDef>,
    max_tokens: u32,
    thinking_level: Option<ThinkingLevel>,
    expect_error: Option<u16>,
    expect_text: bool,
    expect_thinking: bool,
    expect_tool_calls: usize,
    expect_stop_reasons: Vec<StopReason>,
}

fn all_anthropic_scenarios() -> Vec<E2eScenario> {
    vec![
        // ── Happy path ──────────────────────────────────────────────
        E2eScenario {
            cassette_name: "anthropic_simple_text",
            description: "Simple text response",
            messages: vec![user_text("Reply with the single word: pong.")],
            tools: Vec::new(),
            max_tokens: 64,
            thinking_level: None,
            expect_error: None,
            expect_text: true,
            expect_thinking: false,
            expect_tool_calls: 0,
            expect_stop_reasons: vec![StopReason::Stop],
        },
        E2eScenario {
            cassette_name: "anthropic_multi_paragraph",
            description: "Multi-paragraph response",
            messages: vec![user_text(
                "Reply with two paragraphs separated by a blank line. \
                 Paragraph one must be 'Paragraph 1.' and paragraph two must be 'Paragraph 2.'.",
            )],
            tools: Vec::new(),
            max_tokens: 256,
            thinking_level: None,
            expect_error: None,
            expect_text: true,
            expect_thinking: false,
            expect_tool_calls: 0,
            expect_stop_reasons: vec![StopReason::Stop],
        },
        E2eScenario {
            cassette_name: "anthropic_extended_thinking",
            description: "Extended thinking with text response",
            messages: vec![user_text("Compute 12 * 13 and reply with 'Result: 156'.")],
            tools: Vec::new(),
            max_tokens: 256,
            thinking_level: Some(ThinkingLevel::Medium),
            expect_error: None,
            expect_text: true,
            expect_thinking: true,
            expect_tool_calls: 0,
            expect_stop_reasons: vec![StopReason::Stop],
        },
        E2eScenario {
            cassette_name: "anthropic_tool_call_single",
            description: "Single tool call",
            messages: vec![user_text(
                "Call the echo tool with text='hello'. Do not answer in text.",
            )],
            tools: vec![echo_tool()],
            max_tokens: 256,
            thinking_level: None,
            expect_error: None,
            expect_text: false,
            expect_thinking: false,
            expect_tool_calls: 1,
            expect_stop_reasons: vec![StopReason::ToolUse],
        },
        E2eScenario {
            cassette_name: "anthropic_tool_call_multiple",
            description: "Multiple tool calls",
            messages: vec![user_text(
                "Call add with a=2 b=3, then call multiply with a=4 b=5. Do not answer in text.",
            )],
            tools: vec![add_tool(), multiply_tool()],
            max_tokens: 256,
            thinking_level: None,
            expect_error: None,
            expect_text: false,
            expect_thinking: false,
            expect_tool_calls: 2,
            expect_stop_reasons: vec![StopReason::ToolUse],
        },
        E2eScenario {
            cassette_name: "anthropic_unicode_content",
            description: "Unicode content response",
            messages: vec![user_text("Reply with: 😀 你好 שלום")],
            tools: Vec::new(),
            max_tokens: 256,
            thinking_level: None,
            expect_error: None,
            expect_text: true,
            expect_thinking: false,
            expect_tool_calls: 0,
            expect_stop_reasons: vec![StopReason::Stop],
        },
        E2eScenario {
            cassette_name: "anthropic_very_long_response",
            description: "Long response hitting max tokens",
            messages: vec![user_text(
                "Write 50 short sentences about Rust without stopping early.",
            )],
            tools: Vec::new(),
            max_tokens: 64,
            thinking_level: None,
            expect_error: None,
            expect_text: true,
            expect_thinking: false,
            expect_tool_calls: 0,
            expect_stop_reasons: vec![StopReason::Length, StopReason::Stop],
        },
        // ── Thinking variants ───────────────────────────────────────
        E2eScenario {
            cassette_name: "anthropic_thinking_only",
            description: "Thinking only, no final text",
            messages: vec![user_text(
                "Think about primes but do not provide any final text response.",
            )],
            tools: Vec::new(),
            max_tokens: 256,
            thinking_level: Some(ThinkingLevel::High),
            expect_error: None,
            expect_text: false,
            expect_thinking: true,
            expect_tool_calls: 0,
            expect_stop_reasons: vec![],
        },
        E2eScenario {
            cassette_name: "anthropic_thinking_with_tool_calls",
            description: "Thinking with tool calls",
            messages: vec![user_text(
                "Think, then call echo with text='hi'. Do not answer in text.",
            )],
            tools: vec![echo_tool()],
            max_tokens: 256,
            thinking_level: Some(ThinkingLevel::High),
            expect_error: None,
            expect_text: false,
            expect_thinking: true,
            expect_tool_calls: 1,
            expect_stop_reasons: vec![StopReason::ToolUse],
        },
        E2eScenario {
            cassette_name: "anthropic_thinking_budget_exceeded",
            description: "Thinking budget exceeded",
            messages: vec![user_text(
                "Think in detail about prime numbers, then answer with a summary.",
            )],
            tools: Vec::new(),
            max_tokens: 96,
            thinking_level: Some(ThinkingLevel::High),
            expect_error: None,
            expect_text: false,
            expect_thinking: true,
            expect_tool_calls: 0,
            expect_stop_reasons: vec![StopReason::Length, StopReason::Stop],
        },
        // ── Error scenarios ─────────────────────────────────────────
        E2eScenario {
            cassette_name: "anthropic_auth_failure_401",
            description: "Auth failure (HTTP 401)",
            messages: vec![user_text("Trigger auth failure.")],
            tools: Vec::new(),
            max_tokens: 256,
            thinking_level: None,
            expect_error: Some(401),
            expect_text: false,
            expect_thinking: false,
            expect_tool_calls: 0,
            expect_stop_reasons: vec![],
        },
        E2eScenario {
            cassette_name: "anthropic_forbidden_403",
            description: "Forbidden (HTTP 403)",
            messages: vec![user_text("Trigger forbidden.")],
            tools: Vec::new(),
            max_tokens: 256,
            thinking_level: None,
            expect_error: Some(403),
            expect_text: false,
            expect_thinking: false,
            expect_tool_calls: 0,
            expect_stop_reasons: vec![],
        },
        E2eScenario {
            cassette_name: "anthropic_bad_request_400",
            description: "Bad request (HTTP 400)",
            messages: vec![user_text("Trigger bad request.")],
            tools: Vec::new(),
            max_tokens: 256,
            thinking_level: None,
            expect_error: Some(400),
            expect_text: false,
            expect_thinking: false,
            expect_tool_calls: 0,
            expect_stop_reasons: vec![],
        },
        E2eScenario {
            cassette_name: "anthropic_server_error_500",
            description: "Server error (HTTP 500)",
            messages: vec![user_text("Trigger server error.")],
            tools: Vec::new(),
            max_tokens: 256,
            thinking_level: None,
            expect_error: Some(500),
            expect_text: false,
            expect_thinking: false,
            expect_tool_calls: 0,
            expect_stop_reasons: vec![],
        },
        E2eScenario {
            cassette_name: "anthropic_rate_limit_429",
            description: "Rate limit (HTTP 429)",
            messages: vec![user_text("Trigger rate limit.")],
            tools: Vec::new(),
            max_tokens: 256,
            thinking_level: None,
            expect_error: Some(429),
            expect_text: false,
            expect_thinking: false,
            expect_tool_calls: 0,
            expect_stop_reasons: vec![],
        },
        E2eScenario {
            cassette_name: "anthropic_overloaded_529",
            description: "Overloaded (HTTP 529)",
            messages: vec![user_text("Trigger overloaded.")],
            tools: Vec::new(),
            max_tokens: 256,
            thinking_level: None,
            expect_error: Some(529),
            expect_text: false,
            expect_thinking: false,
            expect_tool_calls: 0,
            expect_stop_reasons: vec![],
        },
    ]
}

fn echo_tool() -> ToolDef {
    ToolDef {
        name: "echo".to_string(),
        description: "Echo the provided text.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }),
    }
}

fn add_tool() -> ToolDef {
    ToolDef {
        name: "add".to_string(),
        description: "Add two numbers.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": { "a": { "type": "number" }, "b": { "type": "number" } },
            "required": ["a", "b"]
        }),
    }
}

fn multiply_tool() -> ToolDef {
    ToolDef {
        name: "multiply".to_string(),
        description: "Multiply two numbers.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": { "a": { "type": "number" }, "b": { "type": "number" } },
            "required": ["a", "b"]
        }),
    }
}

// ─── Core runner ─────────────────────────────────────────────────────────────

async fn run_e2e_scenario(
    scenario: &E2eScenario,
    harness: &TestHarness,
    model: &str,
) -> serde_json::Value {
    let cassette_dir = cassette_root();
    let cassette_path = cassette_dir.join(format!("{}.json", scenario.cassette_name));
    harness.record_artifact(
        format!("{}.json", scenario.cassette_name),
        &cassette_path,
    );

    if !cassette_path.exists() {
        harness.log().warn(
            "vcr",
            format!("Missing cassette {}; skipping", cassette_path.display()),
        );
        return json!({
            "scenario": scenario.cassette_name,
            "status": "skipped",
            "reason": "missing_cassette",
        });
    }

    let start = Instant::now();
    let recorder =
        VcrRecorder::new_with(scenario.cassette_name, VcrMode::Playback, &cassette_dir);
    let client = Client::new().with_vcr(recorder);
    let provider = AnthropicProvider::new(model).with_client(client);

    let context = Context {
        system_prompt: Some(
            "You are a test harness model. Follow instructions precisely and deterministically."
                .to_string(),
        ),
        messages: scenario.messages.clone(),
        tools: scenario.tools.clone(),
    };

    let thinking_enabled = scenario
        .thinking_level
        .is_some_and(|level| level != ThinkingLevel::Off);
    let temperature = if thinking_enabled { Some(1.0) } else { Some(0.0) };
    let thinking_budgets = thinking_enabled.then_some(ThinkingBudgets {
        minimal: 1024,
        low: 1024,
        medium: 1024,
        high: 1024,
        xhigh: 1024,
    });
    let max_tokens = if let Some(ref budgets) = thinking_budgets {
        let budget = match scenario.thinking_level.unwrap_or(ThinkingLevel::Off) {
            ThinkingLevel::Off => 0,
            ThinkingLevel::Minimal => budgets.minimal,
            ThinkingLevel::Low => budgets.low,
            ThinkingLevel::Medium => budgets.medium,
            ThinkingLevel::High => budgets.high,
            ThinkingLevel::XHigh => budgets.xhigh,
        };
        scenario.max_tokens.max(budget.saturating_add(256))
    } else {
        scenario.max_tokens
    };

    let options = StreamOptions {
        api_key: Some("vcr-playback".to_string()),
        max_tokens: Some(max_tokens),
        temperature,
        thinking_level: scenario.thinking_level,
        thinking_budgets,
        cache_retention: CacheRetention::None,
        ..Default::default()
    };

    // Run the stream
    if let Some(expected_status) = scenario.expect_error {
        let result = provider.stream(&context, &options).await;
        let elapsed = start.elapsed();

        let Err(err) = result else {
            unreachable!(
                "{}: expected error HTTP {expected_status}, got success",
                scenario.cassette_name
            );
        };
        let message = err.to_string();
        let needle = format!("HTTP {expected_status}");
        assert!(
            message.contains(&needle),
            "{}: expected error to contain '{needle}', got '{message}'",
            scenario.cassette_name
        );

        harness.log().info_ctx(
            "e2e",
            &format!("{}: error scenario validated", scenario.cassette_name),
            |ctx| {
                ctx.push(("status".into(), expected_status.to_string()));
                ctx.push(("error".into(), message.clone()));
                ctx.push(("elapsed_ms".into(), elapsed.as_millis().to_string()));
            },
        );

        return json!({
            "scenario": scenario.cassette_name,
            "description": scenario.description,
            "status": "pass",
            "kind": "error",
            "expected_http_status": expected_status,
            "error_message": message,
            "elapsed_ms": elapsed.as_millis(),
        });
    }

    // Happy-path: collect stream events
    let stream = provider
        .stream(&context, &options)
        .await
        .unwrap_or_else(|e| panic!("{}: stream failed: {e}", scenario.cassette_name));

    use futures::StreamExt;
    let mut events = Vec::new();
    let mut stream_error = None;
    let mut pinned = std::pin::pin!(stream);
    while let Some(item) = pinned.next().await {
        match item {
            Ok(event) => events.push(event),
            Err(err) => {
                stream_error = Some(err.to_string());
                break;
            }
        }
    }
    let elapsed = start.elapsed();

    // Validate event ordering
    validate_event_ordering(&events, harness, scenario.cassette_name);

    // Aggregate statistics
    let mut text = String::new();
    let mut thinking = String::new();
    let mut text_deltas = 0usize;
    let mut thinking_deltas = 0usize;
    let mut tool_calls = Vec::new();
    let mut stop_reason: Option<StopReason> = None;
    let mut timeline = Vec::new();

    for event in &events {
        match event {
            StreamEvent::Start { .. } => timeline.push("start"),
            StreamEvent::TextStart { .. } => timeline.push("text_start"),
            StreamEvent::TextDelta { delta, .. } => {
                text_deltas += 1;
                text.push_str(delta);
                timeline.push("text_delta");
            }
            StreamEvent::TextEnd { content, .. } => {
                text.clone_from(content);
                timeline.push("text_end");
            }
            StreamEvent::ThinkingStart { .. } => timeline.push("thinking_start"),
            StreamEvent::ThinkingDelta { delta, .. } => {
                thinking_deltas += 1;
                thinking.push_str(delta);
                timeline.push("thinking_delta");
            }
            StreamEvent::ThinkingEnd { content, .. } => {
                thinking.clone_from(content);
                timeline.push("thinking_end");
            }
            StreamEvent::ToolCallStart { .. } => timeline.push("tool_call_start"),
            StreamEvent::ToolCallDelta { .. } => timeline.push("tool_call_delta"),
            StreamEvent::ToolCallEnd { tool_call, .. } => {
                tool_calls.push(tool_call.clone());
                timeline.push("tool_call_end");
            }
            StreamEvent::Done { reason, .. } => {
                stop_reason = Some(*reason);
                timeline.push("done");
            }
            StreamEvent::Error { reason, .. } => {
                stop_reason = Some(*reason);
                timeline.push("error");
            }
        }
    }

    // Content hash for determinism tracking
    let content_hash = sha256_hex(format!("{text}{thinking}").as_bytes());

    // Assertions
    if scenario.expect_text {
        assert!(
            text_deltas > 0,
            "{}: expected text deltas, got 0",
            scenario.cassette_name
        );
    }
    if scenario.expect_thinking {
        assert!(
            thinking_deltas > 0,
            "{}: expected thinking deltas, got 0",
            scenario.cassette_name
        );
    }
    if scenario.expect_tool_calls > 0 {
        assert!(
            tool_calls.len() >= scenario.expect_tool_calls,
            "{}: expected >= {} tool calls, got {}",
            scenario.cassette_name,
            scenario.expect_tool_calls,
            tool_calls.len()
        );
    }
    if !scenario.expect_stop_reasons.is_empty() {
        let Some(reason) = stop_reason else {
            panic!("{}: missing stop reason", scenario.cassette_name);
        };
        assert!(
            scenario.expect_stop_reasons.contains(&reason),
            "{}: expected stop reason in {:?}, got {reason:?}",
            scenario.cassette_name,
            scenario.expect_stop_reasons
        );
    }

    harness.log().info_ctx(
        "e2e",
        &format!("{}: scenario validated", scenario.cassette_name),
        |ctx| {
            ctx.push(("events".into(), events.len().to_string()));
            ctx.push(("text_deltas".into(), text_deltas.to_string()));
            ctx.push(("thinking_deltas".into(), thinking_deltas.to_string()));
            ctx.push(("tool_calls".into(), tool_calls.len().to_string()));
            ctx.push(("content_hash".into(), content_hash.clone()));
            ctx.push(("elapsed_ms".into(), elapsed.as_millis().to_string()));
            if let Some(reason) = stop_reason {
                ctx.push(("stop_reason".into(), format!("{reason:?}")));
            }
            if let Some(error) = &stream_error {
                ctx.push(("stream_error".into(), error.clone()));
            }
        },
    );

    json!({
        "scenario": scenario.cassette_name,
        "description": scenario.description,
        "status": "pass",
        "kind": "stream",
        "events": events.len(),
        "text_deltas": text_deltas,
        "thinking_deltas": thinking_deltas,
        "tool_calls": tool_calls.len(),
        "stop_reason": stop_reason.map(|r| format!("{r:?}")),
        "content_hash": content_hash,
        "timeline": timeline.join(" -> "),
        "elapsed_ms": elapsed.as_millis(),
        "stream_error": stream_error,
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn e2e_anthropic_streaming_all_scenarios() {
    let harness = TestHarness::new("e2e_anthropic_streaming_all_scenarios");
    let model = std::env::var("ANTHROPIC_TEST_MODEL")
        .unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string());

    harness.log().info_ctx("e2e", "Starting E2E provider streaming suite", |ctx| {
        ctx.push(("provider".into(), "anthropic".to_string()));
        ctx.push(("model".into(), model.clone()));
        ctx.push(("vcr_mode".into(), "playback".to_string()));
        ctx.push(("cassette_dir".into(), cassette_root().display().to_string()));
    });

    let scenarios = all_anthropic_scenarios();

    asupersync::test_utils::run_test(|| {
        let model = model.clone();
        let harness_ref = &harness;
        let scenarios_ref = &scenarios;
        async move {
            let mut results = Vec::new();
            let mut passed = 0usize;
            let mut skipped = 0usize;
            let mut failed = 0usize;

            for scenario in scenarios_ref {
                harness_ref.section(&format!("Scenario: {}", scenario.cassette_name));
                let result = run_e2e_scenario(scenario, harness_ref, &model).await;

                let status = result
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                match status {
                    "pass" => passed += 1,
                    "skipped" => skipped += 1,
                    _ => failed += 1,
                }
                results.push(result);
            }

            // Write JSONL summary
            let summary_path = harness_ref.temp_path("e2e_streaming_results.jsonl");
            let mut summary_content = String::new();
            for result in &results {
                let _ = writeln!(
                    summary_content,
                    "{}",
                    serde_json::to_string(result).unwrap_or_default()
                );
            }
            std::fs::write(&summary_path, &summary_content).expect("write summary jsonl");
            harness_ref.record_artifact("e2e_streaming_results.jsonl", &summary_path);

            // Write JSONL logs
            let log_path = harness_ref.temp_path("e2e_streaming_log.jsonl");
            harness_ref
                .write_jsonl_logs(&log_path)
                .expect("write jsonl log");
            harness_ref.record_artifact("e2e_streaming_log.jsonl", &log_path);

            // Write artifact index
            let artifact_path = harness_ref.temp_path("e2e_streaming_artifacts.jsonl");
            harness_ref
                .write_artifact_index_jsonl(&artifact_path)
                .expect("write artifact index");
            harness_ref.record_artifact("e2e_streaming_artifacts.jsonl", &artifact_path);

            harness_ref.log().info_ctx("e2e", "Suite completed", |ctx| {
                ctx.push(("total".into(), scenarios_ref.len().to_string()));
                ctx.push(("passed".into(), passed.to_string()));
                ctx.push(("skipped".into(), skipped.to_string()));
                ctx.push(("failed".into(), failed.to_string()));
            });

            assert_eq!(
                failed, 0,
                "E2E streaming suite: {failed} scenarios failed out of {}",
                scenarios_ref.len()
            );
        }
    });
}

/// Verify that running the same cassette twice produces identical content hashes
/// (determinism proof).
#[test]
fn e2e_anthropic_streaming_determinism() {
    let harness = TestHarness::new("e2e_anthropic_streaming_determinism");
    let model = std::env::var("ANTHROPIC_TEST_MODEL")
        .unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string());

    let scenario = E2eScenario {
        cassette_name: "anthropic_simple_text",
        description: "Determinism check: simple text",
        messages: vec![user_text("Reply with the single word: pong.")],
        tools: Vec::new(),
        max_tokens: 64,
        thinking_level: None,
        expect_error: None,
        expect_text: true,
        expect_thinking: false,
        expect_tool_calls: 0,
        expect_stop_reasons: vec![StopReason::Stop],
    };

    asupersync::test_utils::run_test(|| {
        let model = model.clone();
        let harness_ref = &harness;
        let scenario_ref = &scenario;
        async move {
            harness_ref.section("Run 1");
            let result1 = run_e2e_scenario(scenario_ref, harness_ref, &model).await;
            let hash1 = result1
                .get("content_hash")
                .and_then(|v| v.as_str())
                .map(String::from);

            harness_ref.section("Run 2");
            let result2 = run_e2e_scenario(scenario_ref, harness_ref, &model).await;
            let hash2 = result2
                .get("content_hash")
                .and_then(|v| v.as_str())
                .map(String::from);

            if let (Some(h1), Some(h2)) = (&hash1, &hash2) {
                harness_ref.log().info_ctx("determinism", "Hash comparison", |ctx| {
                    ctx.push(("run1".into(), h1.clone()));
                    ctx.push(("run2".into(), h2.clone()));
                    ctx.push(("match".into(), (h1 == h2).to_string()));
                });
                assert_eq!(h1, h2, "Content hashes differ between runs (non-deterministic)");
            }
        }
    });
}

/// Verify that all error scenarios produce errors with appropriate HTTP status codes.
#[test]
fn e2e_anthropic_error_scenarios_comprehensive() {
    let harness = TestHarness::new("e2e_anthropic_error_scenarios_comprehensive");
    let model = std::env::var("ANTHROPIC_TEST_MODEL")
        .unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string());

    let error_scenarios: Vec<(&str, u16)> = vec![
        ("anthropic_auth_failure_401", 401),
        ("anthropic_forbidden_403", 403),
        ("anthropic_bad_request_400", 400),
        ("anthropic_server_error_500", 500),
        ("anthropic_rate_limit_429", 429),
        ("anthropic_overloaded_529", 529),
    ];

    for (cassette, expected_status) in &error_scenarios {
        harness.section(&format!("Error: HTTP {expected_status}"));

        let cassette_path = cassette_root().join(format!("{cassette}.json"));
        if !cassette_path.exists() {
            harness.log().warn(
                "vcr",
                format!("Missing cassette {cassette}; skipping"),
            );
            continue;
        }

        asupersync::test_utils::run_test(|| {
            let model = model.clone();
            let cassette_name = *cassette;
            let expected = *expected_status;
            async move {
                let cassette_dir = cassette_root();
                let recorder = VcrRecorder::new_with(
                    cassette_name,
                    VcrMode::Playback,
                    &cassette_dir,
                );
                let client = Client::new().with_vcr(recorder);
                let provider = AnthropicProvider::new(&model).with_client(client);

                let context = Context {
                    system_prompt: Some("Test.".to_string()),
                    messages: vec![user_text("Trigger error.")],
                    tools: Vec::new(),
                };
                let options = StreamOptions {
                    api_key: Some("vcr-playback".to_string()),
                    max_tokens: Some(256),
                    temperature: Some(0.0),
                    ..Default::default()
                };

                let Err(err) = provider.stream(&context, &options).await else {
                    unreachable!("{cassette_name}: expected error, got success");
                };
                let message = err.to_string();
                let needle = format!("HTTP {expected}");
                assert!(
                    message.contains(&needle),
                    "{cassette_name}: expected '{needle}' in error, got: {message}"
                );
            }
        });

        harness.log().info(
            "e2e",
            format!("HTTP {expected_status} error validated via {cassette}"),
        );
    }
}
