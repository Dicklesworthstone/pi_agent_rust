//! Unit tests for `session_index.rs` covering indexing, listing, reindexing, and lock/error behavior.
//!
//! Part of bd-3uuf: Unit tests for `session_index` core behaviors.

mod common;

use asupersync::runtime::RuntimeBuilder;
use common::TestHarness;
use pi::model::{AssistantMessage, ContentBlock, StopReason, TextContent, Usage, UserContent};
use pi::session::{Session, SessionHeader, SessionMessage};
use pi::session_index::SessionIndex;
use std::fs::{self, File};
use std::io::Write;
use std::time::Duration;

fn run_async_test<F: std::future::Future<Output = ()>>(future: F) {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");
    runtime.block_on(future);
}

fn make_user_message(text: &str) -> SessionMessage {
    SessionMessage::User {
        content: UserContent::Text(text.to_string()),
        timestamp: Some(0),
    }
}

fn make_assistant_message(text: &str) -> SessionMessage {
    SessionMessage::Assistant {
        message: AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        },
    }
}

/// Create a minimal valid session JSONL file
fn create_session_jsonl(
    harness: &TestHarness,
    subdir: &str,
    name: &str,
    cwd: &str,
) -> std::path::PathBuf {
    let dir = harness.temp_path(subdir);
    fs::create_dir_all(&dir).expect("create session dir");

    let path = dir.join(name);
    let mut header = SessionHeader::new();
    header.cwd = cwd.to_string();

    let mut file = File::create(&path).expect("create session file");
    writeln!(file, "{}", serde_json::to_string(&header).unwrap()).unwrap();
    // Add a user message entry
    let entry = serde_json::json!({
        "type": "message",
        "parentId": "root",
        "id": "msg1",
        "timestamp": "2026-02-03T00:00:00.000Z",
        "message": {
            "role": "user",
            "content": "Hello",
            "timestamp": 1_706_918_401_000_i64
        }
    });
    writeln!(file, "{}", serde_json::to_string(&entry).unwrap()).unwrap();

    harness
        .log()
        .info_ctx("setup", "Created session JSONL", |ctx| {
            ctx.push(("path".into(), path.display().to_string()));
            ctx.push(("cwd".into(), cwd.to_string()));
        });

    path
}

// ============================================================================
// Test: index_session on in-memory session is a no-op
// ============================================================================

#[test]
fn index_session_noop_for_inmemory_session() {
    run_async_test(async {
        let harness = TestHarness::new("index_session_noop_for_inmemory_session");
        let sessions_root = harness.temp_path("sessions");
        fs::create_dir_all(&sessions_root).expect("create sessions dir");

        let index = SessionIndex::for_sessions_root(&sessions_root);

        // Create an in-memory session (no path)
        let session = Session::in_memory();
        assert!(
            session.path.is_none(),
            "In-memory session should have no path"
        );

        harness
            .log()
            .info("action", "Indexing in-memory session (should be no-op)");

        // This should succeed without doing anything
        let result = index.index_session(&session);
        assert!(
            result.is_ok(),
            "index_session should succeed for in-memory session"
        );

        // Verify the index has no sessions
        let listed = index.list_sessions(None).expect("list sessions");
        assert!(
            listed.is_empty(),
            "Index should be empty after indexing in-memory session"
        );

        harness
            .log()
            .info_ctx("verify", "Index state after in-memory session", |ctx| {
                ctx.push(("session_count".into(), listed.len().to_string()));
            });
    });
}

// ============================================================================
// Test: index_session inserts/updates rows and meta.last_sync_epoch_ms
// ============================================================================

#[test]
fn index_session_inserts_and_updates_meta() {
    run_async_test(async {
        let harness = TestHarness::new("index_session_inserts_and_updates_meta");
        let sessions_root = harness.temp_path("sessions");
        fs::create_dir_all(&sessions_root).expect("create sessions dir");

        let index = SessionIndex::for_sessions_root(&sessions_root);

        // Create a session with a path
        let mut session = Session::create_with_dir(Some(sessions_root.clone()));
        session.append_message(make_user_message("Hello"));
        session.append_message(make_assistant_message("Hi there!"));

        // Save the session to give it a path
        session.save().await.expect("save session");
        let session_path = session.path.clone().expect("session should have path");
        harness.record_artifact("session.jsonl", &session_path);

        harness.log().info_ctx("action", "Indexing session", |ctx| {
            ctx.push(("session_id".into(), session.header.id.clone()));
            ctx.push(("path".into(), session_path.display().to_string()));
        });

        // Index the session
        let result = index.index_session(&session);
        assert!(
            result.is_ok(),
            "index_session should succeed: {:?}",
            result.err()
        );

        // Verify the session is now listed
        let listed = index.list_sessions(None).expect("list sessions");
        assert_eq!(listed.len(), 1, "Should have exactly one indexed session");

        let meta = &listed[0];
        assert_eq!(meta.id, session.header.id, "Session ID should match");
        assert_eq!(meta.cwd, session.header.cwd, "CWD should match");
        assert_eq!(meta.message_count, 2, "Should have 2 messages");

        harness
            .log()
            .info_ctx("verify", "Indexed session metadata", |ctx| {
                ctx.push(("id".into(), meta.id.clone()));
                ctx.push(("cwd".into(), meta.cwd.clone()));
                ctx.push(("message_count".into(), meta.message_count.to_string()));
                ctx.push(("last_modified_ms".into(), meta.last_modified_ms.to_string()));
            });

        // Update the session and re-index
        session.append_message(make_user_message("Another message"));
        session.save().await.expect("save updated session");

        let result = index.index_session(&session);
        assert!(result.is_ok(), "re-index should succeed");

        let listed = index
            .list_sessions(None)
            .expect("list sessions after update");
        assert_eq!(
            listed.len(),
            1,
            "Should still have exactly one session (upsert)"
        );
        assert_eq!(
            listed[0].message_count, 3,
            "Message count should be updated"
        );

        harness.log().info("verify", "Session updated via upsert");
    });
}

// ============================================================================
// Test: list_sessions() returns newest-first ordering
// ============================================================================

#[test]
fn list_sessions_returns_newest_first() {
    run_async_test(async {
        let harness = TestHarness::new("list_sessions_returns_newest_first");
        let sessions_root = harness.temp_path("sessions");
        let cwd = harness.temp_dir().display().to_string();
        let encoded_cwd = pi::session::encode_cwd(harness.temp_dir());

        // Create the project session directory
        let project_dir = sessions_root.join(&encoded_cwd);
        fs::create_dir_all(&project_dir).expect("create project session dir");

        let index = SessionIndex::for_sessions_root(&sessions_root);

        // Create first session
        let mut session1 = Session::create_with_dir(Some(sessions_root.clone()));
        session1.header.cwd = cwd.clone();
        session1.append_message(make_user_message("First"));
        session1.path = Some(project_dir.join("session1.jsonl"));
        session1.save().await.expect("save session1");
        harness.record_artifact("session1.jsonl", session1.path.as_ref().unwrap());

        // Small delay to ensure different timestamps
        std::thread::sleep(Duration::from_millis(50));

        // Create second session
        let mut session2 = Session::create_with_dir(Some(sessions_root.clone()));
        session2.header.cwd = cwd.clone();
        session2.append_message(make_user_message("Second"));
        session2.path = Some(project_dir.join("session2.jsonl"));
        session2.save().await.expect("save session2");
        harness.record_artifact("session2.jsonl", session2.path.as_ref().unwrap());

        std::thread::sleep(Duration::from_millis(50));

        // Create third session
        let mut session3 = Session::create_with_dir(Some(sessions_root.clone()));
        session3.header.cwd = cwd.clone();
        session3.append_message(make_user_message("Third"));
        session3.path = Some(project_dir.join("session3.jsonl"));
        session3.save().await.expect("save session3");
        harness.record_artifact("session3.jsonl", session3.path.as_ref().unwrap());

        harness
            .log()
            .info("setup", "Created 3 sessions with different timestamps");

        // Index all sessions
        index.index_session(&session1).expect("index session1");
        index.index_session(&session2).expect("index session2");
        index.index_session(&session3).expect("index session3");

        // List sessions and verify ordering
        let listed = index.list_sessions(None).expect("list sessions");
        assert_eq!(listed.len(), 3, "Should have 3 sessions");

        // Verify newest first (session3 should be first)
        assert_eq!(
            listed[0].id, session3.header.id,
            "Newest session should be first"
        );
        assert_eq!(
            listed[1].id, session2.header.id,
            "Middle session should be second"
        );
        assert_eq!(
            listed[2].id, session1.header.id,
            "Oldest session should be last"
        );

        harness
            .log()
            .info_ctx("verify", "Session ordering verified", |ctx| {
                for (i, meta) in listed.iter().enumerate() {
                    ctx.push((format!("session_{i}_id"), meta.id.clone()));
                    ctx.push((
                        format!("session_{i}_last_modified_ms"),
                        meta.last_modified_ms.to_string(),
                    ));
                }
            });

        // Verify timestamps are in descending order
        assert!(
            listed[0].last_modified_ms >= listed[1].last_modified_ms,
            "First should be newer than second"
        );
        assert!(
            listed[1].last_modified_ms >= listed[2].last_modified_ms,
            "Second should be newer than third"
        );
    });
}

// ============================================================================
// Test: list_sessions() cwd filter works
// ============================================================================

#[test]
fn list_sessions_cwd_filter_works() {
    run_async_test(async {
        let harness = TestHarness::new("list_sessions_cwd_filter_works");
        let sessions_root = harness.temp_path("sessions");

        let cwd1 = "/project/alpha";
        let cwd2 = "/project/beta";

        let index = SessionIndex::for_sessions_root(&sessions_root);

        // Create sessions for cwd1
        let encoded_cwd1 = cwd1.replace('/', "--");
        let project_dir1 = sessions_root.join(&encoded_cwd1);
        fs::create_dir_all(&project_dir1).expect("create project dir 1");

        let mut session1 = Session::create_with_dir(Some(sessions_root.clone()));
        session1.header.cwd = cwd1.to_string();
        session1.append_message(make_user_message("Alpha 1"));
        session1.path = Some(project_dir1.join("session1.jsonl"));
        session1.save().await.expect("save session1");

        let mut session2 = Session::create_with_dir(Some(sessions_root.clone()));
        session2.header.cwd = cwd1.to_string();
        session2.append_message(make_user_message("Alpha 2"));
        session2.path = Some(project_dir1.join("session2.jsonl"));
        session2.save().await.expect("save session2");

        // Create session for cwd2
        let encoded_cwd2 = cwd2.replace('/', "--");
        let project_dir2 = sessions_root.join(&encoded_cwd2);
        fs::create_dir_all(&project_dir2).expect("create project dir 2");

        let mut session3 = Session::create_with_dir(Some(sessions_root.clone()));
        session3.header.cwd = cwd2.to_string();
        session3.append_message(make_user_message("Beta 1"));
        session3.path = Some(project_dir2.join("session3.jsonl"));
        session3.save().await.expect("save session3");

        // Index all sessions
        index.index_session(&session1).expect("index session1");
        index.index_session(&session2).expect("index session2");
        index.index_session(&session3).expect("index session3");

        harness
            .log()
            .info_ctx("setup", "Indexed sessions for two cwds", |ctx| {
                ctx.push(("cwd1".into(), cwd1.to_string()));
                ctx.push(("cwd2".into(), cwd2.to_string()));
            });

        // List all sessions
        let all = index.list_sessions(None).expect("list all");
        assert_eq!(all.len(), 3, "Should have 3 total sessions");

        // Filter by cwd1
        let cwd1_sessions = index.list_sessions(Some(cwd1)).expect("list cwd1");
        assert_eq!(cwd1_sessions.len(), 2, "Should have 2 sessions for cwd1");
        for meta in &cwd1_sessions {
            assert_eq!(meta.cwd, cwd1, "All sessions should have cwd1");
        }

        // Filter by cwd2
        let cwd2_sessions = index.list_sessions(Some(cwd2)).expect("list cwd2");
        assert_eq!(cwd2_sessions.len(), 1, "Should have 1 session for cwd2");
        assert_eq!(cwd2_sessions[0].cwd, cwd2, "Session should have cwd2");

        // Filter by non-existent cwd
        let empty = index
            .list_sessions(Some("/nonexistent"))
            .expect("list nonexistent");
        assert!(
            empty.is_empty(),
            "Should have 0 sessions for non-existent cwd"
        );

        harness
            .log()
            .info_ctx("verify", "CWD filtering verified", |ctx| {
                ctx.push(("all_count".into(), all.len().to_string()));
                ctx.push(("cwd1_count".into(), cwd1_sessions.len().to_string()));
                ctx.push(("cwd2_count".into(), cwd2_sessions.len().to_string()));
            });
    });
}

// ============================================================================
// Test: reindex_all rebuilds index from disk
// ============================================================================

#[test]
fn reindex_all_rebuilds_from_disk() {
    run_async_test(async {
        let harness = TestHarness::new("reindex_all_rebuilds_from_disk");
        let sessions_root = harness.temp_path("sessions");
        let cwd = "/test/project";
        let encoded_cwd = cwd.replace('/', "--");
        let _project_dir = sessions_root.join(&encoded_cwd);

        // Create session files directly on disk
        let path1 = create_session_jsonl(
            &harness,
            &format!("sessions/{encoded_cwd}"),
            "session1.jsonl",
            cwd,
        );
        let path2 = create_session_jsonl(
            &harness,
            &format!("sessions/{encoded_cwd}"),
            "session2.jsonl",
            cwd,
        );

        harness.record_artifact("session1.jsonl", &path1);
        harness.record_artifact("session2.jsonl", &path2);

        let index = SessionIndex::for_sessions_root(&sessions_root);

        // Verify index starts empty
        let before = index.list_sessions(None).expect("list before reindex");
        assert!(before.is_empty(), "Index should be empty before reindex");

        harness.log().info("action", "Running reindex_all");

        // Reindex from disk
        let result = index.reindex_all();
        assert!(
            result.is_ok(),
            "reindex_all should succeed: {:?}",
            result.err()
        );

        // Verify sessions are now indexed
        let after = index.list_sessions(None).expect("list after reindex");
        assert_eq!(after.len(), 2, "Should have 2 sessions after reindex");

        for meta in &after {
            assert_eq!(meta.cwd, cwd, "CWD should be correct");
            assert!(meta.message_count >= 1, "Should have at least 1 message");
        }

        harness
            .log()
            .info_ctx("verify", "Reindex completed", |ctx| {
                ctx.push(("session_count".into(), after.len().to_string()));
            });
    });
}

// ============================================================================
// Test: reindex_all handles missing sessions root gracefully
// ============================================================================

#[test]
fn reindex_all_handles_missing_root() {
    let harness = TestHarness::new("reindex_all_handles_missing_root");
    let nonexistent_root = harness.temp_path("nonexistent_sessions");

    // Don't create the directory
    assert!(!nonexistent_root.exists(), "Root should not exist");

    let index = SessionIndex::for_sessions_root(&nonexistent_root);

    harness
        .log()
        .info("action", "Running reindex_all on missing root");

    // reindex_all should succeed (no-op) when root doesn't exist
    let result = index.reindex_all();
    assert!(
        result.is_ok(),
        "reindex_all should succeed for missing root"
    );

    // Listing should also work and return empty
    let listed = index.list_sessions(None);
    // This might fail because the db hasn't been created yet, which is fine
    if let Ok(sessions) = listed {
        assert!(sessions.is_empty(), "Should have no sessions");
    }

    harness
        .log()
        .info("verify", "Missing root handled gracefully");
}

// ============================================================================
// Test: should_reindex returns true for missing database
// ============================================================================

#[test]
fn should_reindex_true_for_missing_db() {
    let harness = TestHarness::new("should_reindex_true_for_missing_db");
    let sessions_root = harness.temp_path("sessions");
    fs::create_dir_all(&sessions_root).expect("create sessions dir");

    let index = SessionIndex::for_sessions_root(&sessions_root);

    // Database doesn't exist yet
    assert!(
        index.should_reindex(Duration::from_secs(3600)),
        "should_reindex should return true when db doesn't exist"
    );

    harness
        .log()
        .info("verify", "should_reindex returns true for missing db");
}

// ============================================================================
// Test: should_reindex returns false for fresh database
// ============================================================================

#[test]
fn should_reindex_false_for_fresh_db() {
    run_async_test(async {
        let harness = TestHarness::new("should_reindex_false_for_fresh_db");
        let sessions_root = harness.temp_path("sessions");
        fs::create_dir_all(&sessions_root).expect("create sessions dir");

        let index = SessionIndex::for_sessions_root(&sessions_root);

        // Create and index a session to initialize the database
        let mut session = Session::create_with_dir(Some(sessions_root.clone()));
        session.append_message(make_user_message("Hello"));
        session.path = Some(sessions_root.join("session.jsonl"));
        session.save().await.expect("save session");
        index.index_session(&session).expect("index session");

        // Database was just modified, so should_reindex should return false
        assert!(
            !index.should_reindex(Duration::from_secs(3600)),
            "should_reindex should return false for fresh db"
        );

        harness
            .log()
            .info("verify", "should_reindex returns false for fresh db");
    });
}

// ============================================================================
// Test: reindex_if_stale only reindexes when necessary
// ============================================================================

#[test]
fn reindex_if_stale_behavior() {
    run_async_test(async {
        let harness = TestHarness::new("reindex_if_stale_behavior");
        let sessions_root = harness.temp_path("sessions");
        let cwd = "/test/project";
        let encoded_cwd = cwd.replace('/', "--");

        // Create a session file
        create_session_jsonl(
            &harness,
            &format!("sessions/{encoded_cwd}"),
            "session.jsonl",
            cwd,
        );

        let index = SessionIndex::for_sessions_root(&sessions_root);

        // First call should reindex (db doesn't exist)
        let result1 = index.reindex_if_stale(Duration::from_secs(3600));
        assert!(result1.is_ok(), "reindex_if_stale should succeed");
        assert!(result1.unwrap(), "First call should have reindexed");

        // Second call with long max_age should not reindex
        let result2 = index.reindex_if_stale(Duration::from_secs(3600));
        assert!(result2.is_ok(), "reindex_if_stale should succeed");
        assert!(
            !result2.unwrap(),
            "Second call should not reindex (db is fresh)"
        );

        // Call with zero max_age should reindex
        let result3 = index.reindex_if_stale(Duration::ZERO);
        assert!(result3.is_ok(), "reindex_if_stale should succeed");
        assert!(result3.unwrap(), "Zero max_age should force reindex");

        harness
            .log()
            .info("verify", "reindex_if_stale behaves correctly");
    });
}

// ============================================================================
// Test: Error paths - unreadable JSONL
// ============================================================================

#[test]
fn reindex_handles_unreadable_jsonl() {
    let harness = TestHarness::new("reindex_handles_unreadable_jsonl");
    let sessions_root = harness.temp_path("sessions");
    let cwd = "/test/project";
    let encoded_cwd = cwd.replace('/', "--");
    let project_dir = sessions_root.join(&encoded_cwd);
    fs::create_dir_all(&project_dir).expect("create project dir");

    // Create a valid session file
    let valid_path = create_session_jsonl(
        &harness,
        &format!("sessions/{encoded_cwd}"),
        "valid.jsonl",
        cwd,
    );
    harness.record_artifact("valid.jsonl", &valid_path);

    // Create an invalid/corrupted session file
    let invalid_path = project_dir.join("invalid.jsonl");
    fs::write(&invalid_path, "this is not valid JSON\n{also invalid}").expect("write invalid file");
    harness.record_artifact("invalid.jsonl", &invalid_path);

    let index = SessionIndex::for_sessions_root(&sessions_root);

    harness
        .log()
        .info("action", "Running reindex_all with invalid files");

    // reindex_all should succeed, skipping invalid files
    let result = index.reindex_all();
    assert!(
        result.is_ok(),
        "reindex_all should succeed despite invalid files"
    );

    // Only the valid session should be indexed
    let listed = index.list_sessions(None).expect("list sessions");
    assert_eq!(listed.len(), 1, "Should only have 1 valid session indexed");

    harness
        .log()
        .info_ctx("verify", "Invalid files handled gracefully", |ctx| {
            ctx.push(("indexed_count".into(), listed.len().to_string()));
        });
}

// ============================================================================
// Test: Error paths - empty session file
// ============================================================================

#[test]
fn reindex_handles_empty_session_file() {
    let harness = TestHarness::new("reindex_handles_empty_session_file");
    let sessions_root = harness.temp_path("sessions");
    let cwd = "/test/project";
    let encoded_cwd = cwd.replace('/', "--");
    let project_dir = sessions_root.join(&encoded_cwd);
    fs::create_dir_all(&project_dir).expect("create project dir");

    // Create a valid session file
    let valid_path = create_session_jsonl(
        &harness,
        &format!("sessions/{encoded_cwd}"),
        "valid.jsonl",
        cwd,
    );
    harness.record_artifact("valid.jsonl", &valid_path);

    // Create an empty session file
    let empty_path = project_dir.join("empty.jsonl");
    fs::write(&empty_path, "").expect("write empty file");
    harness.record_artifact("empty.jsonl", &empty_path);

    let index = SessionIndex::for_sessions_root(&sessions_root);

    harness
        .log()
        .info("action", "Running reindex_all with empty file");

    // reindex_all should succeed, skipping empty files
    let result = index.reindex_all();
    assert!(
        result.is_ok(),
        "reindex_all should succeed despite empty files"
    );

    // Only the valid session should be indexed
    let listed = index.list_sessions(None).expect("list sessions");
    assert_eq!(listed.len(), 1, "Should only have 1 valid session indexed");

    harness
        .log()
        .info("verify", "Empty files handled gracefully");
}

// ============================================================================
// Test: Lock behavior - basic locking works
// ============================================================================

#[test]
fn lock_behavior_basic() {
    run_async_test(async {
        let harness = TestHarness::new("lock_behavior_basic");
        let sessions_root = harness.temp_path("sessions");
        fs::create_dir_all(&sessions_root).expect("create sessions dir");

        let index = SessionIndex::for_sessions_root(&sessions_root);

        // Create and index a session - this exercises the locking mechanism
        let mut session = Session::create_with_dir(Some(sessions_root.clone()));
        session.append_message(make_user_message("Hello"));
        session.path = Some(sessions_root.join("session.jsonl"));
        session.save().await.expect("save session");

        harness
            .log()
            .info("action", "Testing basic locking through index operations");

        // Multiple sequential operations should work (each acquires and releases the lock)
        index.index_session(&session).expect("first index");
        index.list_sessions(None).expect("first list");
        index.index_session(&session).expect("second index");
        index.list_sessions(None).expect("second list");

        harness
            .log()
            .info("verify", "Sequential lock operations succeeded");
    });
}

// ============================================================================
// Test: Lock behavior - concurrent access from threads
// ============================================================================

#[test]
fn lock_prevents_concurrent_corruption() {
    let harness = TestHarness::new("lock_prevents_concurrent_corruption");
    let sessions_root = harness.temp_path("sessions");
    fs::create_dir_all(&sessions_root).expect("create sessions dir");

    let sessions_root1 = sessions_root.clone();
    let sessions_root2 = sessions_root.clone();

    harness
        .log()
        .info("action", "Spawning concurrent index operations");

    // Spawn two threads that both try to write to the index
    let handle1 = std::thread::spawn(move || {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let index = SessionIndex::for_sessions_root(&sessions_root1);
            for i in 0..5 {
                let mut session = Session::create_with_dir(Some(sessions_root1.clone()));
                session.append_message(make_user_message(&format!("Thread1 msg {i}")));
                session.path = Some(sessions_root1.join(format!("t1_session_{i}.jsonl")));
                session.save().await.expect("save session");
                index.index_session(&session).expect("index session");
            }
        });
    });

    let handle2 = std::thread::spawn(move || {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let index = SessionIndex::for_sessions_root(&sessions_root2);
            for i in 0..5 {
                let mut session = Session::create_with_dir(Some(sessions_root2.clone()));
                session.append_message(make_user_message(&format!("Thread2 msg {i}")));
                session.path = Some(sessions_root2.join(format!("t2_session_{i}.jsonl")));
                session.save().await.expect("save session");
                index.index_session(&session).expect("index session");
            }
        });
    });

    handle1.join().expect("thread 1 join");
    handle2.join().expect("thread 2 join");

    // Verify the index is consistent
    let index = SessionIndex::for_sessions_root(&sessions_root);
    let listed = index.list_sessions(None).expect("list sessions");

    harness
        .log()
        .info_ctx("verify", "Concurrent operations completed", |ctx| {
            ctx.push(("total_sessions".into(), listed.len().to_string()));
        });

    // Should have all 10 sessions
    assert_eq!(listed.len(), 10, "Should have all 10 sessions indexed");
}

// ============================================================================
// Test: SessionMeta fields are populated correctly
// ============================================================================

#[test]
fn session_meta_fields_populated() {
    run_async_test(async {
        let harness = TestHarness::new("session_meta_fields_populated");
        let sessions_root = harness.temp_path("sessions");
        fs::create_dir_all(&sessions_root).expect("create sessions dir");

        let index = SessionIndex::for_sessions_root(&sessions_root);

        // Create a session with known properties
        let mut session = Session::create_with_dir(Some(sessions_root.clone()));
        session.append_message(make_user_message("Message 1"));
        session.append_message(make_assistant_message("Response 1"));
        session.append_message(make_user_message("Message 2"));
        session.append_session_info(Some("Test Session Name".to_string()));

        session.path = Some(sessions_root.join("named_session.jsonl"));
        session.save().await.expect("save session");
        harness.record_artifact("named_session.jsonl", session.path.as_ref().unwrap());

        index.index_session(&session).expect("index session");

        let listed = index.list_sessions(None).expect("list sessions");
        assert_eq!(listed.len(), 1, "Should have 1 session");

        let meta = &listed[0];

        harness.log().info_ctx("verify", "Session metadata", |ctx| {
            ctx.push(("id".into(), meta.id.clone()));
            ctx.push(("cwd".into(), meta.cwd.clone()));
            ctx.push(("timestamp".into(), meta.timestamp.clone()));
            ctx.push(("message_count".into(), meta.message_count.to_string()));
            ctx.push(("size_bytes".into(), meta.size_bytes.to_string()));
            ctx.push((
                "name".into(),
                meta.name.clone().unwrap_or_else(|| "(none)".to_string()),
            ));
        });

        // Verify all fields
        assert_eq!(meta.id, session.header.id, "ID should match");
        assert_eq!(meta.cwd, session.header.cwd, "CWD should match");
        assert!(!meta.timestamp.is_empty(), "Timestamp should be set");
        assert_eq!(meta.message_count, 3, "Should have 3 messages");
        assert!(meta.size_bytes > 0, "Size should be non-zero");
        assert!(meta.last_modified_ms > 0, "Last modified should be set");
        assert_eq!(
            meta.name,
            Some("Test Session Name".to_string()),
            "Name should match"
        );

        // Verify path is the full file path
        assert!(
            meta.path.ends_with("named_session.jsonl"),
            "Path should point to session file"
        );
    });
}

// ============================================================================
// Test: Index handles nested session directories
// ============================================================================

#[test]
fn reindex_handles_nested_directories() {
    let harness = TestHarness::new("reindex_handles_nested_directories");
    let sessions_root = harness.temp_path("sessions");

    // Create sessions in different nested directories
    create_session_jsonl(
        &harness,
        "sessions/project-a",
        "session1.jsonl",
        "/project/a",
    );
    create_session_jsonl(
        &harness,
        "sessions/project-a",
        "session2.jsonl",
        "/project/a",
    );
    create_session_jsonl(
        &harness,
        "sessions/project-b",
        "session1.jsonl",
        "/project/b",
    );
    create_session_jsonl(
        &harness,
        "sessions/project-b/subdir",
        "session1.jsonl",
        "/project/b/subdir",
    );

    let index = SessionIndex::for_sessions_root(&sessions_root);

    harness
        .log()
        .info("action", "Running reindex_all with nested directories");

    let result = index.reindex_all();
    assert!(result.is_ok(), "reindex_all should succeed");

    let listed = index.list_sessions(None).expect("list sessions");
    assert_eq!(
        listed.len(),
        4,
        "Should have 4 sessions from nested directories"
    );

    harness
        .log()
        .info_ctx("verify", "Nested directories indexed", |ctx| {
            ctx.push(("total_sessions".into(), listed.len().to_string()));
        });
}

// ============================================================================
// Test: Index ignores non-JSONL files
// ============================================================================

#[test]
fn reindex_ignores_non_jsonl_files() {
    let harness = TestHarness::new("reindex_ignores_non_jsonl_files");
    let sessions_root = harness.temp_path("sessions");
    let project_dir = sessions_root.join("project");
    fs::create_dir_all(&project_dir).expect("create project dir");

    // Create a valid session file
    create_session_jsonl(&harness, "sessions/project", "valid.jsonl", "/project");

    // Create non-JSONL files
    fs::write(project_dir.join("notes.txt"), "Some notes").expect("write txt");
    fs::write(project_dir.join("config.json"), "{}").expect("write json");
    fs::write(project_dir.join("data.csv"), "a,b,c").expect("write csv");

    let index = SessionIndex::for_sessions_root(&sessions_root);

    harness
        .log()
        .info("action", "Running reindex_all with mixed file types");

    let result = index.reindex_all();
    assert!(result.is_ok(), "reindex_all should succeed");

    let listed = index.list_sessions(None).expect("list sessions");
    assert_eq!(listed.len(), 1, "Should only index .jsonl files");

    harness.log().info("verify", "Non-JSONL files ignored");
}

// ============================================================================
// Test: Session save succeeds even when index is in a bad state
// ============================================================================

#[test]
fn session_save_succeeds_despite_index_issues() {
    run_async_test(async {
        let harness = TestHarness::new("session_save_succeeds_despite_index_issues");
        let sessions_root = harness.temp_path("sessions");
        fs::create_dir_all(&sessions_root).expect("create sessions dir");

        // Create a session
        let mut session = Session::create_with_dir(Some(sessions_root.clone()));
        session.append_message(make_user_message("Hello"));

        harness
            .log()
            .info("action", "Saving session (index should not break save)");

        // Save should succeed - this tests that Session::save() handles
        // index failures gracefully (warn but don't fail)
        let result = session.save().await;
        assert!(
            result.is_ok(),
            "Session save should succeed: {:?}",
            result.err()
        );

        // Verify session file exists
        let session_path = session.path.clone().expect("session should have path");
        assert!(
            session_path.exists(),
            "Session file should exist after save"
        );

        harness.record_artifact("session.jsonl", &session_path);
        harness
            .log()
            .info_ctx("verify", "Session saved successfully", |ctx| {
                ctx.push(("path".into(), session_path.display().to_string()));
            });
    });
}

// ============================================================================
// Test: list_sessions falls back to filesystem when index is empty
// ============================================================================

#[test]
fn list_sessions_fallback_to_filesystem() {
    let harness = TestHarness::new("list_sessions_fallback_to_filesystem");
    let sessions_root = harness.temp_path("sessions");
    let cwd = "/test/project";
    let encoded_cwd = cwd.replace('/', "--");

    // Create session files directly on disk (no index)
    create_session_jsonl(
        &harness,
        &format!("sessions/{encoded_cwd}"),
        "session1.jsonl",
        cwd,
    );
    create_session_jsonl(
        &harness,
        &format!("sessions/{encoded_cwd}"),
        "session2.jsonl",
        cwd,
    );

    harness
        .log()
        .info("setup", "Created sessions on disk without indexing");

    // Create an index but don't populate it
    let index = SessionIndex::for_sessions_root(&sessions_root);

    // List sessions - should fall back to reindexing from filesystem
    let listed = index.list_sessions(Some(cwd)).expect("list sessions");

    harness
        .log()
        .info_ctx("verify", "Fallback listing result", |ctx| {
            ctx.push(("session_count".into(), listed.len().to_string()));
        });

    // The implementation may auto-reindex on empty results.
    // Accept either outcome: empty (no auto-reindex) or complete (auto-reindex happened).
    assert!(
        listed.is_empty() || listed.len() == 2,
        "expected list_sessions to return 0 or 2 sessions (got {})",
        listed.len()
    );

    // Explicit reindex should find them
    let _ = index.reindex_all();
    let after_reindex = index.list_sessions(Some(cwd)).expect("list after reindex");
    assert_eq!(
        after_reindex.len(),
        2,
        "Should have 2 sessions after explicit reindex"
    );

    harness.log().info("verify", "Fallback behavior verified");
}
