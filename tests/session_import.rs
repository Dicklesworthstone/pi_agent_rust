//! Integration tests for foreign session import (bd-cv653.6.4).
//!
//! Acceptance coverage:
//! 1. Claude fixture imports: message order, roles, tool pairs intact;
//!    the session opens via the native loader.
//! 2. Codex fixture imports with reasoning preserved as thinking blocks.
//! 3. Double import is idempotent (same session id, notice).
//! 4. Corrupt lines tolerated: skipped with count in the report.
//!
//! Logging: structured JSONL per tests/common/logging.rs, v2-validated,
//! recorded as artifacts.

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::session::Session;
use pi::session_import::{import_claude, import_codex};

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(errors.is_empty(), "JSONL v2 validation errors: {errors:?}");
}

fn write_fixture(harness: &TestHarness, name: &str, content: &str) -> std::path::PathBuf {
    let path = harness.temp_path(name);
    std::fs::write(&path, content).expect("write fixture");
    path
}

fn sample_claude_jsonl() -> String {
    [
        r#"{"type":"user","timestamp":"2026-02-01T10:00:00Z","message":{"role":"user","content":[{"type":"text","text":"Implement the new parser"}]}}"#,
        r#"{"type":"assistant","timestamp":"2026-02-01T10:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"I will examine the grammar."},{"type":"thinking","thinking":"Analyzing BNF"}],"model":"claude-3-5-sonnet"}}"#,
        "INVALID_JSON_CORRUPT_LINE",
        r#"{"type":"assistant","timestamp":"2026-02-01T10:00:02Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"call_123","name":"read","input":{"path":"src/grammar.rs"}}]}}"#,
    ]
    .join("\n")
}

fn sample_codex_jsonl() -> String {
    [
        r#"{"type":"session_meta","timestamp":"2026-02-01T10:00:00.000Z","payload":{"id":"sess_1","cwd":"/repo"}}"#,
        r#"{"type":"response_item","timestamp":"2026-02-01T10:00:01.000Z","payload":{"type":"message","role":"user","content":[{"text":"Fix unit tests"}]}}"#,
        r#"{"type":"response_item","timestamp":"2026-02-01T10:00:02.000Z","payload":{"type":"reasoning","summary":[{"text":"Running cargo test first"}]}}"#,
        r#"{"type":"response_item","timestamp":"2026-02-01T10:00:03.000Z","payload":{"type":"function_call","name":"bash","arguments":"{\"command\":\"cargo test\"}","call_id":"call_456"}}"#,
    ]
    .join("\n")
}

#[test]
fn claude_end_to_end_fidelity_and_idempotency() {
    let case = "claude_end_to_end_fidelity_and_idempotency";
    let harness = TestHarness::new(case);
    let target = harness.temp_path("target");
    let source = write_fixture(&harness, "claude.jsonl", &sample_claude_jsonl());

    let outcome = import_claude(&source, Some(&target)).expect("import");
    harness.log().info(
        "verify",
        format!(
            "imported {} skipped {} -> {}",
            outcome.imported, outcome.skipped, outcome.session_path
        ),
    );
    assert_eq!(outcome.imported, 3, "{:?}", outcome.report);
    assert_eq!(outcome.skipped, 1, "{:?}", outcome.report);
    assert!(!outcome.already_imported);
    assert!(
        outcome.report.iter().any(|line| line.contains("corrupt")),
        "corruption counted in the report: {:?}",
        outcome.report
    );

    let again = import_claude(&source, Some(&target)).expect("re-import");
    assert!(again.already_imported);
    assert_eq!(again.session_id, outcome.session_id);
    assert_eq!(again.imported, 0);

    let session = futures::executor::block_on(Session::open(&outcome.session_path)).expect("open");
    let messages = session.to_messages_for_current_path();
    harness
        .log()
        .info("verify", format!("replayed {} messages", messages.len()));
    assert_eq!(messages.len(), 3);
    let has_tool_call = messages.iter().any(|message| match message {
        pi::model::Message::Assistant(assistant) => assistant
            .content
            .iter()
            .any(|block| matches!(block, pi::model::ContentBlock::ToolCall(call) if call.id == "call_123")),
        _ => false,
    });
    assert!(has_tool_call, "tool_use pair must import: {messages:?}");
    finish_case(&harness, case);
}

#[test]
fn codex_reasoning_and_tools_import() {
    let case = "codex_reasoning_and_tools_import";
    let harness = TestHarness::new(case);
    let target = harness.temp_path("target");
    let source = write_fixture(&harness, "codex.jsonl", &sample_codex_jsonl());

    let outcome = import_codex(&source, Some(&target)).expect("import");
    harness.log().info(
        "verify",
        format!(
            "codex imported {} -> {}",
            outcome.imported, outcome.session_path
        ),
    );
    assert_eq!(outcome.imported, 3, "{:?}", outcome.report);

    let session = futures::executor::block_on(Session::open(&outcome.session_path)).expect("open");
    let messages = session.to_messages_for_current_path();
    assert_eq!(messages.len(), 3);
    let has_thinking = messages.iter().any(|message| match message {
        pi::model::Message::Assistant(assistant) => assistant
            .content
            .iter()
            .any(|block| matches!(block, pi::model::ContentBlock::Thinking(_))),
        _ => false,
    });
    let has_tool_call = messages.iter().any(|message| match message {
        pi::model::Message::Assistant(assistant) => assistant
            .content
            .iter()
            .any(|block| matches!(block, pi::model::ContentBlock::ToolCall(_))),
        _ => false,
    });
    assert!(has_thinking, "reasoning must import as a thinking block");
    assert!(has_tool_call, "function_call must import as a tool call");
    finish_case(&harness, case);
}

#[test]
fn corruption_never_aborts_import() {
    let case = "corruption_never_aborts_import";
    let harness = TestHarness::new(case);
    let target = harness.temp_path("target");
    let mut content = sample_claude_jsonl();
    content.push_str("\n{broken\n{also broken\n");
    let source = write_fixture(&harness, "claude-broken.jsonl", &content);

    let outcome = import_claude(&source, Some(&target)).expect("import");
    harness.log().info(
        "verify",
        format!(
            "broken-file import: {} imported, {} skipped",
            outcome.imported, outcome.skipped
        ),
    );
    assert_eq!(
        outcome.imported, 3,
        "valid lines still import: {:?}",
        outcome.report
    );
    assert_eq!(
        outcome.skipped, 3,
        "all three corrupt lines counted: {:?}",
        outcome.report
    );
    finish_case(&harness, case);
}
