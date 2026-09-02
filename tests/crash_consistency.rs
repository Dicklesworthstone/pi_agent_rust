//! Crash-consistency and recovery tests for buffered persistence (bd-3ar8v.2.7).
//!
//! These tests prove correctness under crashes, cancellation, partial writes,
//! and corrupted session file scenarios. They exercise the public Session API
//! for file recovery and the autosave queue state machine for flush failure
//! semantics.

use asupersync::runtime::RuntimeBuilder;
use pi::model::UserContent;
use pi::session::{Session, SessionMessage};
use serde_json::json;
use std::future::Future;
use std::io::Write as _;

fn run_async<T>(future: impl Future<Output = T>) -> T {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    runtime.block_on(future)
}

fn make_test_message(text: &str) -> SessionMessage {
    SessionMessage::User {
        content: UserContent::Text(text.to_string()),
        timestamp: Some(0),
    }
}

/// Build a valid JSONL session file string with header + N entries.
fn build_session_file(num_entries: usize) -> String {
    let header = json!({
        "type": "session",
        "version": 3,
        "id": "crash-test",
        "timestamp": "2024-06-01T00:00:00.000Z",
        "cwd": "/tmp/test"
    });

    let mut lines = vec![serde_json::to_string(&header).unwrap()];
    for i in 0..num_entries {
        let entry = json!({
            "type": "message",
            "id": format!("entry-{i}"),
            "timestamp": "2024-06-01T00:00:00.000Z",
            "message": {"role": "user", "content": format!("message {i}")}
        });
        lines.push(serde_json::to_string(&entry).unwrap());
    }
    lines.join("\n")
}

fn valid_entry_json(id: &str, text: &str) -> String {
    json!({
        "type": "message",
        "id": id,
        "timestamp": "2024-06-01T00:00:00.000Z",
        "message": {"role": "user", "content": text}
    })
    .to_string()
}

fn valid_header_json() -> String {
    serde_json::to_string(&json!({
        "type": "session",
        "version": 3,
        "id": "crash-test-hdr",
        "timestamp": "2024-06-01T00:00:00.000Z",
        "cwd": "/tmp/test"
    }))
    .unwrap()
}

// ===========================================================================
// File recovery tests
// ===========================================================================

#[test]
fn crash_empty_file_returns_error() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("empty.jsonl");
    std::fs::write(&file_path, "").unwrap();

    let result = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    });
    assert!(result.is_err(), "empty file should fail to open");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("Empty session file"),
        "unexpected error: {err_msg}"
    );
}

#[test]
fn crash_corrupted_header_returns_error() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("bad_header.jsonl");
    std::fs::write(&file_path, "NOT VALID JSON\n{\"type\":\"message\"}\n").unwrap();

    let result = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    });
    assert!(result.is_err(), "corrupted header should fail");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("Invalid header"),
        "unexpected error: {err_msg}"
    );
}

#[test]
fn crash_header_only_no_entries_loads_empty() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("header_only.jsonl");
    std::fs::write(&file_path, format!("{}\n", valid_header_json())).unwrap();

    let (session, diagnostics) = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    })
    .unwrap();

    assert!(session.entries.is_empty());
    assert!(diagnostics.skipped_entries.is_empty());
}

#[test]
fn crash_truncated_last_entry_recovers_preceding() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("truncated.jsonl");

    let mut content = build_session_file(3);
    // Truncate in the middle of the last entry to simulate crash mid-write.
    let truncation_point = content.rfind('\n').unwrap();
    content.truncate(truncation_point);
    content.push_str("\n{\"type\":\"message\",\"id\":\"partial");

    std::fs::write(&file_path, &content).unwrap();

    let (session, diagnostics) = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    })
    .unwrap();

    assert_eq!(session.entries.len(), 2, "2 valid entries should survive");
    assert_eq!(diagnostics.skipped_entries.len(), 1);
}

#[test]
fn crash_multiple_corrupted_entries_recovers_valid() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("multi_corrupt.jsonl");

    let lines = [
        valid_header_json(),
        valid_entry_json("v1", "first"),
        "GARBAGE LINE 1".to_string(),
        valid_entry_json("v2", "second"),
        "{incomplete json".to_string(),
        valid_entry_json("v3", "third"),
    ];

    std::fs::write(&file_path, lines.join("\n")).unwrap();

    let (session, diagnostics) = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    })
    .unwrap();

    assert_eq!(session.entries.len(), 3, "3 valid entries should survive");
    assert_eq!(diagnostics.skipped_entries.len(), 2);
}

#[test]
fn crash_binary_garbage_in_entries_skipped() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("binary.jsonl");

    let content = format!(
        "{}\n{}\n\u{FFFD}\u{FFFD}\u{FFFD} still not json\n",
        valid_header_json(),
        valid_entry_json("survivor", "I survived"),
    );

    std::fs::write(&file_path, &content).unwrap();

    let (session, diagnostics) = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    })
    .unwrap();

    assert_eq!(session.entries.len(), 1, "only the valid entry survives");
    assert_eq!(diagnostics.skipped_entries.len(), 1);
}

#[test]
fn crash_file_with_trailing_newlines_loads_cleanly() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("trailing_nl.jsonl");

    let mut content = build_session_file(2);
    content.push_str("\n\n\n");

    std::fs::write(&file_path, &content).unwrap();

    let (session, diagnostics) = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    })
    .unwrap();

    assert_eq!(
        session.entries.len(),
        2,
        "trailing newlines should not create phantom entries"
    );
    // Interior empty lines are parsed as invalid JSON and skipped;
    // the key invariant: valid entries survive intact.
    for skip in &diagnostics.skipped_entries {
        assert!(
            skip.error.contains("EOF") || skip.error.contains("expected"),
            "skipped entries should be parse errors, got: {}",
            skip.error
        );
    }
}

// ===========================================================================
// Incremental append crash recovery
// ===========================================================================

#[test]
fn crash_incremental_append_survives_partial_write() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());

    session.append_message(make_test_message("msg A"));
    session.append_message(make_test_message("msg B"));
    run_async(async { session.save().await }).unwrap();
    let path = session.path.clone().unwrap();

    // Simulate crash during incremental append: append truncated JSON.
    // save() writes trailing newline, so the partial JSON lands on its own line.
    let partial = "{\"type\":\"message\",\"id\":\"crash\",\"timestamp\":\"2024";
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    write!(file, "{partial}").unwrap();
    drop(file);

    let (loaded, diagnostics) =
        run_async(async { Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await })
            .unwrap();

    assert_eq!(loaded.entries.len(), 2, "original entries recovered");
    assert_eq!(
        diagnostics.skipped_entries.len(),
        1,
        "partial entry skipped"
    );
}

#[test]
fn crash_full_rewrite_atomic_persist_semantics() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());

    session.append_message(make_test_message("original"));
    run_async(async { session.save().await }).unwrap();
    let path = session.path.clone().unwrap();

    let original = std::fs::read_to_string(&path).unwrap();

    // Force full rewrite by dirtying header and adding entry.
    session.set_model_header(Some("new-provider".to_string()), None, None);
    session.append_message(make_test_message("second"));
    run_async(async { session.save().await }).unwrap();

    let updated = std::fs::read_to_string(&path).unwrap();
    assert_ne!(original, updated, "content should change after rewrite");

    let loaded = run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
    assert_eq!(loaded.entries.len(), 2);
}

#[test]
fn crash_save_reload_preserves_all_entry_types() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());

    let id_a = session.append_message(make_test_message("msg A"));
    session.append_model_change("prov-x".to_string(), "model-y".to_string());
    session.append_thinking_level_change("high".to_string());
    session.append_compaction("summary text".to_string(), id_a, 500, None, None);
    session.append_message(make_test_message("msg B"));

    run_async(async { session.save().await }).unwrap();
    let path = session.path.clone().unwrap();

    let loaded = run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
    assert_eq!(
        loaded.entries.len(),
        session.entries.len(),
        "all entry types should round-trip"
    );
}

#[test]
fn crash_no_op_save_after_reload_is_idempotent() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());

    session.append_message(make_test_message("hello"));
    run_async(async { session.save().await }).unwrap();
    let path = session.path.clone().unwrap();

    let before = std::fs::read_to_string(&path).unwrap();

    let mut loaded =
        run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
    run_async(async { loaded.save().await }).unwrap();

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(before, after, "no-op save should not modify file");
}

// ===========================================================================
// Durability mode + shutdown semantics
// ===========================================================================

#[test]
fn crash_shutdown_strict_propagates_save_error() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut session = Session::create();
    // Point at non-existent directory to force IO failure.
    session.path = Some(temp_dir.path().join("nodir").join("session.jsonl"));
    session.set_autosave_durability_mode(pi::session::AutosaveDurabilityMode::Strict);
    session.append_message(make_test_message("must save"));

    // Explicit save (not autosave) so we don't need queue mutation.
    let result = run_async(async { session.save().await });
    assert!(
        result.is_err(),
        "strict: save to non-existent dir should fail"
    );
}

#[test]
fn crash_shutdown_balanced_swallows_via_shutdown_api() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut session = Session::create();
    session.path = Some(temp_dir.path().join("nodir").join("session.jsonl"));
    session.set_autosave_durability_mode(pi::session::AutosaveDurabilityMode::Balanced);
    session.append_message(make_test_message("best effort"));

    // flush_autosave_on_shutdown is best-effort in balanced mode.
    let result = run_async(async { session.flush_autosave_on_shutdown().await });
    // Balanced swallows errors — but only if there are pending mutations.
    // Since we used append_message without going through the autosave queue enqueue,
    // flush_autosave_on_shutdown won't attempt the save (no pending mutations).
    // This test verifies the shutdown path doesn't panic.
    assert!(result.is_ok(), "balanced shutdown should not fail");
}

#[test]
fn crash_shutdown_throughput_skips_flush() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut session = Session::create();
    session.path = Some(temp_dir.path().join("nodir").join("session.jsonl"));
    session.set_autosave_durability_mode(pi::session::AutosaveDurabilityMode::Throughput);
    session.append_message(make_test_message("no flush"));

    let result = run_async(async { session.flush_autosave_on_shutdown().await });
    assert!(result.is_ok(), "throughput should skip flush entirely");
}

// ===========================================================================
// Concurrent append + checkpoint recovery
// ===========================================================================

#[test]
fn crash_concurrent_appends_then_checkpoint() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());

    session.append_message(make_test_message("initial"));
    run_async(async { session.save().await }).unwrap();
    let path = session.path.clone().unwrap();

    // Multiple incremental appends.
    for i in 0..5 {
        session.append_message(make_test_message(&format!("append {i}")));
        run_async(async { session.save().await }).unwrap();
    }

    // Force full rewrite by changing header.
    session.set_model_header(Some("checkpoint-prov".to_string()), None, None);
    session.append_message(make_test_message("after checkpoint"));
    run_async(async { session.save().await }).unwrap();

    let loaded = run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
    assert_eq!(
        loaded.entries.len(),
        7,
        "all 7 entries should survive checkpoint"
    );
}

#[test]
fn crash_corrupt_mid_file_then_continue_operation() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("corrupt_mid.jsonl");

    // Write header + good entry + corrupted entry.
    let content = format!(
        "{}\n{}\n{{BROKEN}}\n",
        valid_header_json(),
        valid_entry_json("good1", "first message"),
    );
    std::fs::write(&file_path, &content).unwrap();

    // Load with diagnostics — should recover the good entry.
    let (session, diagnostics) = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    })
    .unwrap();

    assert_eq!(session.entries.len(), 1);
    assert_eq!(diagnostics.skipped_entries.len(), 1);
    assert!(
        session.leaf_id().is_some(),
        "leaf should point to surviving entry"
    );
}

#[test]
fn crash_all_entries_corrupted_loads_empty_session() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("all_bad.jsonl");

    let content = format!(
        "{}\nNOT_JSON_1\nNOT_JSON_2\nNOT_JSON_3\n",
        valid_header_json(),
    );
    std::fs::write(&file_path, &content).unwrap();

    let (session, diagnostics) = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    })
    .unwrap();

    assert!(session.entries.is_empty(), "all entries corrupted = empty");
    assert_eq!(diagnostics.skipped_entries.len(), 3);
}

#[test]
fn crash_duplicate_entry_ids_do_not_panic() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("dup_ids.jsonl");

    let content = format!(
        "{}\n{}\n{}\n",
        valid_header_json(),
        valid_entry_json("same-id", "first"),
        valid_entry_json("same-id", "second (dup)"),
    );
    std::fs::write(&file_path, &content).unwrap();

    // Duplicate entry ids are rejected before the replay cache is built (see
    // `session::tests::proptest_session::duplicate_entry_ids_fail_before_replay_cache_construction`);
    // what this test guards is that the loader reports it as an error
    // instead of panicking or silently keeping one of the two entries.
    let error = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    })
    .expect_err("duplicate entry ids must be rejected, not loaded");
    assert!(
        error.to_string().contains("duplicate entry ID"),
        "unexpected error: {error}"
    );
}

#[test]
fn crash_session_nonexistent_file_returns_error() {
    let result = run_async(async {
        Session::open_with_diagnostics("/nonexistent/path/session.jsonl").await
    });
    assert!(result.is_err());
}

#[test]
fn crash_large_session_survives_incremental_append_cycle() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());

    // Initial save with many entries.
    for i in 0..20 {
        session.append_message(make_test_message(&format!("msg {i}")));
    }
    run_async(async { session.save().await }).unwrap();
    let path = session.path.clone().unwrap();

    // Multiple incremental append cycles.
    for i in 20..30 {
        session.append_message(make_test_message(&format!("msg {i}")));
        run_async(async { session.save().await }).unwrap();
    }

    let loaded = run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
    assert_eq!(loaded.entries.len(), 30, "all 30 entries survive");
}

#[test]
fn crash_autosave_metrics_are_observable() {
    let session = Session::create();
    let metrics = session.autosave_metrics();
    assert_eq!(metrics.pending_mutations, 0);
    assert_eq!(metrics.flush_started, 0);
    assert_eq!(metrics.flush_succeeded, 0);
    assert_eq!(metrics.flush_failed, 0);
    assert!(metrics.last_flush_duration_ms.is_none());
}

#[test]
fn crash_durability_mode_is_readable_and_settable() {
    let mut session = Session::create();
    // Default is Balanced (from env, which defaults to Balanced).
    let mode = session.autosave_durability_mode();
    assert_eq!(mode.as_str(), "balanced");

    session.set_autosave_durability_mode(pi::session::AutosaveDurabilityMode::Strict);
    assert_eq!(session.autosave_durability_mode().as_str(), "strict");

    session.set_autosave_durability_mode(pi::session::AutosaveDurabilityMode::Throughput);
    assert_eq!(session.autosave_durability_mode().as_str(), "throughput");
}
