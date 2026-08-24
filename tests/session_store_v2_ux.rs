#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::session_store_v2::SessionStoreV2;
use serde_json::json;
use std::time::Instant;

const MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    assert!(harness.write_jsonl_logs(&path).is_ok(), "write JSONL logs");
    let payload = std::fs::read_to_string(&path).unwrap_or_default();
    let errors = validate_jsonl_v2_only(&payload);
    assert!(
        errors.is_empty(),
        "JSONL schema violations in {case}.jsonl: {errors:?}"
    );
    harness.record_artifact(format!("{case}.jsonl"), &path);
}

#[test]
fn test_resume_latency_and_tail_read() {
    let harness = TestHarness::new("session_store_v2_resume_ux");
    let store_dir = harness.temp_path("session_v2_resume");
    let _ = std::fs::create_dir_all(&store_dir);

    let store_res = SessionStoreV2::create(&store_dir, MAX_SEGMENT_BYTES);
    assert!(store_res.is_ok(), "open store");
    let mut store = match store_res {
        Ok(s) => s,
        Err(_) => return,
    };

    // Populate 50 entries
    for i in 1..=50 {
        let entry_id = format!("msg_{i:04}"); // ubs:ignore test fixture string generation
        let parent_id = if i > 1 {
            Some(format!("msg_{:04}", i - 1)) // ubs:ignore test fixture string generation
        } else {
            None
        };
        let payload = json!({ "role": "user", "content": "message payload" });
        let append_res = store.append_entry(entry_id, parent_id, "message", payload);
        assert!(append_res.is_ok(), "append entry");
    }

    // Measure tail resume latency
    let start = Instant::now();
    let tail_res = store.read_tail_entries(10);
    let resume_ms = start.elapsed().as_secs_f64() * 1000.0;

    assert!(tail_res.is_ok(), "read tail entries");
    if let Ok(tail_entries) = tail_res {
        assert_eq!(tail_entries.len(), 10);
        assert_eq!(
            tail_entries.last().map(|e| e.entry_id.as_str()),
            Some("msg_0050")
        );
    }
    assert!(
        resume_ms < 50.0,
        "resume latency should be < 50ms, was {resume_ms}ms"
    );

    harness
        .log()
        .info("perf", format!("tail_resume_ms={resume_ms:.3} count=10"));

    finish_case(&harness, "session_store_v2_resume_ux");
}

#[test]
fn test_fork_and_export_snapshot_consistency() {
    let harness = TestHarness::new("session_store_v2_fork_export_ux");
    let store_dir = harness.temp_path("session_v2_origin");
    let _ = std::fs::create_dir_all(&store_dir);

    let store_res = SessionStoreV2::create(&store_dir, MAX_SEGMENT_BYTES);
    assert!(store_res.is_ok(), "open origin store");
    let mut store = match store_res {
        Ok(s) => s,
        Err(_) => return,
    };

    // Append 20 entries
    for i in 1..=20 {
        let entry_id = format!("turn_{i:03}"); // ubs:ignore test fixture string generation
        let parent_id = if i > 1 {
            Some(format!("turn_{:03}", i - 1)) // ubs:ignore test fixture string generation
        } else {
            None
        };
        let payload = json!({ "role": "assistant", "text": "turn text" });
        let append_res = store.append_entry(entry_id, parent_id, "message", payload);
        assert!(append_res.is_ok(), "append");
    }

    // Create checkpoint at turn 20
    let cp_res = store.create_checkpoint(1, "checkpoint at turn 20");
    assert!(cp_res.is_ok(), "checkpoint failed: {cp_res:?}");
    if let Ok(cp) = cp_res {
        assert_eq!(cp.checkpoint_seq, 1);
    }

    // Append more entries after checkpoint (turns 21..30)
    for i in 21..=30 {
        let entry_id = format!("turn_{i:03}"); // ubs:ignore test fixture string generation
        let parent_id = Some(format!("turn_{:03}", i - 1)); // ubs:ignore test fixture string generation
        let payload = json!({ "role": "assistant", "text": "turn text" });
        let append_res = store.append_entry(entry_id, parent_id, "message", payload);
        assert!(append_res.is_ok(), "append post checkpoint");
    }

    // Fork at checkpoint 1 to new directory
    let fork_dir = harness.temp_path("session_v2_fork");
    let fork_start = Instant::now();
    let fork_res = store.fork_at_checkpoint(&fork_dir, 1);
    let fork_ms = fork_start.elapsed().as_secs_f64() * 1000.0;
    assert!(fork_res.is_ok(), "fork at checkpoint");

    // Verify forked store has exactly 20 entries
    let forked_store_res = SessionStoreV2::open_for_inspection(&fork_dir, MAX_SEGMENT_BYTES);
    assert!(forked_store_res.is_ok(), "open forked store");
    if let Ok(forked_store) = forked_store_res {
        let forked_all_res = forked_store.read_all_entries();
        assert!(forked_all_res.is_ok(), "read forked all");
        if let Ok(forked_all) = forked_all_res {
            assert_eq!(forked_all.len(), 20);
            assert_eq!(
                forked_all.last().map(|e| e.entry_id.as_str()),
                Some("turn_020")
            );
        }
    }

    // Export snapshot at checkpoint 1 to a JSONL file
    let export_file = harness.temp_path("session_v2_snapshot.jsonl");
    let export_start = Instant::now();
    let export_res = store.export_snapshot(&export_file, 1);
    let export_ms = export_start.elapsed().as_secs_f64() * 1000.0;
    assert!(export_res.is_ok(), "export snapshot");

    let exported_content = std::fs::read_to_string(&export_file).unwrap_or_default();
    let exported_lines: Vec<&str> = exported_content.lines().collect();
    assert_eq!(exported_lines.len(), 20);

    harness.log().info(
        "perf",
        format!("fork_ms={fork_ms:.3} export_ms={export_ms:.3} snapshot_entries=20"),
    );

    finish_case(&harness, "session_store_v2_fork_export_ux");
}
