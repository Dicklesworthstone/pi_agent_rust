//! Tests for the structured cross-session/cross-agent handoff generator (`bd-cv653.3.17`).

use pi::handoff::{
    Decision, FailedApproach, FileTouched, HANDOFF_SCHEMA_V1, HandoffDocument, HandoffGenerator,
    HandoffTarget,
};
use pi::model::{AssistantMessage, ContentBlock, TextContent, ToolCall, UserContent};
use pi::session::{CompactionEntry, EntryBase, MessageEntry, SessionEntry, SessionMessage};
use std::fs;
use tempfile::tempdir;

#[test]
fn test_handoff_schema_and_metadata() {
    let doc = HandoffDocument {
        schema: HANDOFF_SCHEMA_V1.to_string(),
        session_id: "test-session-001".to_string(),
        timestamp: "2026-08-21T12:00:00Z".to_string(),
        goal: "Implement memory compaction with zero-copy stream slicing".to_string(),
        current_state: "Compaction engine passing all unit tests".to_string(),
        decisions: vec![Decision {
            decision: "Select fsqlite for embedded concurrent storage".to_string(),
            rationale: Some("Better lock-free write throughput".to_string()),
            ref_point: Some("turn#1".to_string()),
        }],
        failed_approaches: vec![FailedApproach {
            attempt: "Naive mutex around SQLite connection".to_string(),
            reason: "Thread starvation under concurrent reader pool".to_string(),
            ref_point: Some("turn#3".to_string()),
        }],
        files_touched: vec![FileTouched {
            path: "src/session.rs".to_string(),
            role: "modified".to_string(),
            line_refs: vec!["L100-L150".to_string()],
        }],
        blockers: vec!["Awaiting ARM64 CI runner provisioning".to_string()],
        open_threads: vec!["Benchmark cold-start compaction latency".to_string()],
        next_steps: vec![
            "Submit PR to main".to_string(),
            "Run performance regression harness".to_string(),
        ],
        lessons: vec!["Always release write locks before async awaits".to_string()],
        compaction_summaries_count: 0,
    };

    let md = doc.to_markdown();
    assert!(md.contains("# Session Handoff Brief"));
    assert!(md.contains("test-session-001"));
    assert!(md.contains("## 1. Goal & Objective"));
    assert!(md.contains("## 4. Failed Approaches & Failure Memory"));
    assert!(md.contains("Thread starvation"));
    assert!(md.contains("## 5. Files Touched"));
    assert!(md.contains("src/session.rs"));
    assert!(md.contains("## 8. Lessons Learned"));
    assert!(md.contains("Always release write locks"));

    let Ok(json_val) = serde_json::to_value(&doc) else {
        return;
    };
    assert_eq!(json_val["schema"], "pi.handoff.v1");
    assert_eq!(json_val["session_id"], "test-session-001");
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_handoff_generator_end_to_end_extraction() {
    let entries = vec![
        SessionEntry::Message(MessageEntry {
            base: EntryBase::new(None, "msg-1".to_string()),
            message: SessionMessage::User {
                content: UserContent::Text(
                    "Fix authentication header injection with sk-ant-secret1234567890123456"
                        .to_string(),
                ),
                timestamp: None,
            },
        }),
        SessionEntry::Message(MessageEntry {
            base: EntryBase::new(Some("msg-1".to_string()), "msg-2".to_string()),
            message: SessionMessage::Assistant {
                message: AssistantMessage {
                    content: vec![
                        ContentBlock::Text(TextContent {
                            text: "Decision: Sanitize authorization headers\nLesson: Never pass raw credentials in query params\n- [ ] Update auth.rs\n- [ ] Add regression test".to_string(),
                            text_signature: None,
                        }),
                        ContentBlock::ToolCall(ToolCall {
                            id: "tc-edit-1".to_string(),
                            name: "edit".to_string(),
                            arguments: serde_json::json!({
                                "path": "src/auth.rs",
                                "StartLine": 50,
                                "EndLine": 75
                            }),
                            thought_signature: None,
                        }),
                    ],
                    provider: "anthropic".to_string(),
                    model: "claude-3-5-sonnet".to_string(),
                    ..Default::default()
                },
            },
        }),
        SessionEntry::Message(MessageEntry {
            base: EntryBase::new(Some("msg-2".to_string()), "msg-3".to_string()),
            message: SessionMessage::ToolResult {
                tool_call_id: "tc-edit-1".to_string(),
                tool_name: "edit".to_string(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "Error: target content not found in src/auth.rs".to_string(),
                    text_signature: None,
                })],
                details: None,
                is_error: true,
                timestamp: None,
            },
        }),
        SessionEntry::Message(MessageEntry {
            base: EntryBase::new(Some("msg-3".to_string()), "msg-4".to_string()),
            message: SessionMessage::BashExecution {
                command: "cargo test auth::tests".to_string(),
                output: "test auth::tests::test_header_format ... FAILED\nassertion failed: `(left == right)`".to_string(),
                exit_code: 101,
                cancelled: None,
                truncated: None,
                full_output_path: None,
                timestamp: None,
                extra: std::collections::HashMap::new(),
            },
        }),
    ];

    let handoff = HandoffGenerator::generate_from_entries("auth-fix-sess", &entries);

    // Verify secret screening in goal
    assert!(!handoff.goal.contains("sk-ant-secret"));
    assert!(handoff.goal.contains("[REDACTED_ANTHROPIC_KEY]"));

    // Verify decisions and lessons
    assert!(
        handoff
            .decisions
            .iter()
            .any(|d| d.decision.contains("Sanitize authorization headers"))
    );
    assert!(
        handoff
            .lessons
            .iter()
            .any(|l| l.contains("Never pass raw credentials"))
    );

    // Verify next steps
    assert!(
        handoff
            .next_steps
            .iter()
            .any(|s| s.contains("Update auth.rs"))
    );
    assert!(
        handoff
            .next_steps
            .iter()
            .any(|s| s.contains("Add regression test"))
    );

    // Verify files touched
    let Some(file) = handoff
        .files_touched
        .iter()
        .find(|f| f.path == "src/auth.rs")
    else {
        panic!("src/auth.rs should exist");
    };
    assert_eq!(file.role, "modified");
    assert!(file.line_refs.contains(&"L50-L75".to_string()));

    // Verify failed approaches
    assert_eq!(handoff.failed_approaches.len(), 2);
    assert!(handoff.failed_approaches[0].attempt.contains("edit"));
    assert!(
        handoff.failed_approaches[0]
            .reason
            .contains("target content not found")
    );

    assert!(
        handoff.failed_approaches[1]
            .attempt
            .contains("cargo test auth::tests")
    );
    assert!(handoff.failed_approaches[1].reason.contains("FAILED"));
}

#[test]
fn test_handoff_delivery_to_disk() {
    let Ok(tmp) = tempdir() else {
        return;
    };
    let md_path = tmp.path().join("my_handoff.md");

    let doc = HandoffDocument {
        schema: HANDOFF_SCHEMA_V1.to_string(),
        session_id: "delivery-sess".to_string(),
        timestamp: "2026-08-21T12:30:00Z".to_string(),
        goal: "Test delivery report".to_string(),
        current_state: "Clean".to_string(),
        decisions: vec![],
        failed_approaches: vec![],
        files_touched: vec![],
        blockers: vec![],
        open_threads: vec![],
        next_steps: vec![],
        lessons: vec![],
        compaction_summaries_count: 0,
    };

    let Ok(report) = HandoffGenerator::deliver(&doc, &HandoffTarget::Human, Some(&md_path)) else {
        return;
    };

    assert!(report.external_delivery_success);
    assert!(md_path.exists());
    let js_path = tmp.path().join("my_handoff.json");
    assert!(js_path.exists());

    let Ok(content) = fs::read_to_string(&md_path) else {
        return;
    };
    assert!(content.contains("# Session Handoff Brief"));
    assert!(content.contains("delivery-sess"));
}

#[test]
fn test_handoff_compaction_rehydration() {
    let entries = vec![
        SessionEntry::Compaction(CompactionEntry {
            base: EntryBase::new(None, "comp-1".to_string()),
            summary: "Compacted Session Summary:\nDecision: Adopted rich_rust markup\nFailed approach: Raw ANSI escape sequences broke Windows console\nFile touched: src/tui.rs\nLesson: Rely on platform-abstracted terminal backends".to_string(),
            first_kept_entry_id: "msg-post-1".to_string(),
            tokens_before: 120_000,
            details: None,
            from_hook: None,
        }),
        SessionEntry::Message(MessageEntry {
            base: EntryBase::new(Some("comp-1".to_string()), "msg-post-1".to_string()),
            message: SessionMessage::User {
                content: UserContent::Text("Verify TUI color contrast".to_string()),
                timestamp: None,
            },
        }),
    ];

    let doc = HandoffGenerator::generate_from_entries("compacted-flow", &entries);
    assert_eq!(doc.compaction_summaries_count, 1);
    assert!(
        doc.decisions
            .iter()
            .any(|d| d.decision.contains("rich_rust markup"))
    );
    assert!(
        doc.failed_approaches
            .iter()
            .any(|f| f.reason.contains("Raw ANSI escape sequences"))
    );
    assert!(doc.files_touched.iter().any(|f| f.path == "src/tui.rs"));
    assert!(
        doc.lessons
            .iter()
            .any(|l| l.contains("platform-abstracted terminal backends"))
    );
}
