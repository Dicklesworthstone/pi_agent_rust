//! Integration tests for foreign session import (`pi import`) (bd-cv653.6.4).

use std::fs;
use tempfile::tempdir;

use pi::model::{ContentBlock, Message};
use pi::session::Session;
use pi::session_import::{import_claude, import_codex};

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
fn test_claude_import_end_to_end_and_idempotency() {
    let Ok(tmp) = tempdir() else {
        return;
    };
    let target_dir = tmp.path();
    let source_file = target_dir.join("claude_session.jsonl");

    let Ok(()) = fs::write(&source_file, sample_claude_jsonl()) else {
        return;
    };

    // First import
    let Ok(outcome1) = import_claude(&source_file, Some(target_dir)) else {
        assert!(false, "First import should succeed");
        return;
    };

    assert_eq!(outcome1.imported, 3);
    assert_eq!(outcome1.skipped, 1);
    assert!(!outcome1.already_imported);

    // Second import (idempotency check)
    let Ok(outcome2) = import_claude(&source_file, Some(target_dir)) else {
        assert!(false, "Second import should succeed");
        return;
    };

    assert!(outcome2.already_imported);
    assert_eq!(outcome2.session_id, outcome1.session_id);

    // Verify session opens and parses correctly
    let Ok(session) = futures::executor::block_on(Session::open(outcome1.session_path)) else {
        assert!(false, "Session::open should load imported session");
        return;
    };

    let messages = session.to_messages_for_current_path();
    assert_eq!(messages.len(), 3);
}

#[test]
fn test_codex_import_reasoning_and_tools() {
    let Ok(tmp) = tempdir() else {
        return;
    };
    let target_dir = tmp.path();
    let source_file = target_dir.join("codex_session.jsonl");

    let Ok(()) = fs::write(&source_file, sample_codex_jsonl()) else {
        return;
    };

    let Ok(outcome) = import_codex(&source_file, Some(target_dir)) else {
        assert!(false, "Codex import should succeed");
        return;
    };

    assert_eq!(outcome.imported, 3);

    let Ok(session) = futures::executor::block_on(Session::open(outcome.session_path)) else {
        assert!(false, "Session::open should load imported codex session");
        return;
    };

    let messages = session.to_messages_for_current_path();
    assert_eq!(messages.len(), 3);

    // Verify reasoning block landed as ThinkingContent
    let has_thinking = messages.iter().any(|msg| match msg {
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Thinking(_))),
        _ => false,
    });
    assert!(
        has_thinking,
        "Reasoning should be preserved as ThinkingContent"
    );
}
