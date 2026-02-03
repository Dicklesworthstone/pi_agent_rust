//! Conformance tests for session JSONL format (v3).

mod common;

use asupersync::runtime::RuntimeBuilder;
use common::TestHarness;
use pi::session::{Session, SessionEntry};
use std::future::Future;

fn write_session_file(harness: &TestHarness, contents: &str) -> std::path::PathBuf {
    harness.create_file("session.jsonl", contents)
}

fn run_async_test<F: Future<Output = ()>>(future: F) {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");
    runtime.block_on(future);
}

#[test]
fn load_session_accepts_parent_session_alias_and_fills_ids() {
    run_async_test(async {
        let harness = TestHarness::new("load_session_accepts_parent_session_alias_and_fills_ids");
        let jsonl = r#"
{"type":"session","version":3,"id":"sess-123","timestamp":"2026-02-03T00:00:00.000Z","cwd":"/tmp/project","provider":"anthropic","modelId":"claude-sonnet-4-20250514","thinkingLevel":"medium","parentSession":"/tmp/parent.jsonl"}
{"type":"message","parentId":"root","timestamp":"2026-02-03T00:00:01.000Z","message":{"role":"user","content":"Hello","timestamp":1706918401000}}
{"type":"model_change","timestamp":"2026-02-03T00:00:02.000Z","provider":"anthropic","modelId":"claude-sonnet-4-20250514"}
{"type":"thinking_level_change","timestamp":"2026-02-03T00:00:03.000Z","thinkingLevel":"medium"}
{"type":"compaction","timestamp":"2026-02-03T00:00:04.000Z","summary":"compacted","firstKeptEntryId":"a1b2c3d4","tokensBefore":128}
{"type":"branch_summary","timestamp":"2026-02-03T00:00:05.000Z","fromId":"root","summary":"branch summary"}
{"type":"label","timestamp":"2026-02-03T00:00:06.000Z","targetId":"a1b2c3d4","label":"checkpoint"}
{"type":"session_info","timestamp":"2026-02-03T00:00:07.000Z","name":"demo session"}
{"type":"custom","timestamp":"2026-02-03T00:00:08.000Z","customType":"note","data":{"tag":"demo"}}
"#;

        let path = write_session_file(&harness, jsonl.trim_start());
        let session = Session::open(path.to_string_lossy().as_ref())
            .await
            .expect("open session");

        assert_eq!(
            session.header.parent_session.as_deref(),
            Some("/tmp/parent.jsonl")
        );
        assert_eq!(session.entries.len(), 8);
        assert!(
            session
                .entries
                .iter()
                .all(|entry| entry.base().id.as_ref().is_some())
        );

        let leaf = session.leaf_id.as_deref();
        let last_id = session
            .entries
            .last()
            .and_then(SessionEntry::base_id)
            .map(String::as_str);
        assert_eq!(leaf, last_id);
    });
}

#[test]
fn session_header_serializes_branched_from_field() {
    run_async_test(async {
        let _harness = TestHarness::new("session_header_serializes_branched_from_field");
        let mut session = Session::create();
        session.header.parent_session = Some("/tmp/parent.jsonl".to_string());

        let header_json = serde_json::to_string(&session.header).expect("serialize header");
        assert!(header_json.contains("\"branchedFrom\""));
        assert!(!header_json.contains("parentSession"));
    });
}
