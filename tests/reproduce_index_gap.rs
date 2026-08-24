#![forbid(unsafe_code)]

use pi::PiResult;
use pi::session_store_v2::SessionStoreV2;
use serde_json::json;
use std::fs;
use tempfile::tempdir;

#[test]
fn rebuild_index_skips_subsequent_segments_on_corruption() -> PiResult<()> {
    let dir = tempdir()?;
    // Small threshold to force multiple segments
    let mut store = SessionStoreV2::create(dir.path(), 500)?;

    // Write enough data to span 2 segments.
    // Each entry is roughly ~50-60 bytes.
    // We want segment 1 to have some entries, and segment 2 to have some.

    // Segment 1
    store.append_entry("e1", None, "message", json!({"data": "x".repeat(50)}))?;
    store.append_entry(
        "e2",
        Some("e1".into()),
        "message",
        json!({"data": "x".repeat(50)}),
    )?;
    store.append_entry(
        "e3",
        Some("e2".into()),
        "message",
        json!({"data": "x".repeat(50)}),
    )?;

    // Check we have multiple segments (at least 1, maybe 2 if 200 bytes reached)
    // 3 * ~100 bytes > 200.

    let index = store.read_index()?;
    let segs: std::collections::HashSet<u64> = index.iter().map(|r| r.segment_seq).collect();
    // Force more if needed
    if segs.len() < 2 {
        store.append_entry(
            "e4",
            Some("e3".into()),
            "message",
            json!({"data": "x".repeat(50)}),
        )?;
        store.append_entry(
            "e5",
            Some("e4".into()),
            "message",
            json!({"data": "x".repeat(50)}),
        )?;
    }

    let index = store.read_index()?;
    let segs: std::collections::HashSet<u64> = index.iter().map(|r| r.segment_seq).collect();
    assert!(segs.len() >= 2, "Setup failed: need at least 2 segments");

    let seg1_path = store.segment_file_path(1);
    let seg2_path = store.segment_file_path(2);

    assert!(seg1_path.exists());
    assert!(seg2_path.exists());

    // Corrupt segment 1 by truncating the last byte (the newline of the last frame).
    let len = fs::metadata(&seg1_path)?.len();
    fs::OpenOptions::new()
        .write(true)
        .open(&seg1_path)?
        .set_len(len - 1)?;

    // Close store and reopen to force rebuild
    drop(store);

    // Manually remove index to force rebuild_index
    let index_path = dir.path().join("index").join("offsets.jsonl");
    fs::remove_file(&index_path)?;

    let mut store = SessionStoreV2::create(dir.path(), 200)?;
    let _rebuilt_count = store.rebuild_index()?;

    // CURRENT BEHAVIOR (suspected bug):
    // It processes segment 1, hits missing newline, truncates it (dropping last frame),
    // THEN proceeds to segment 2 and indexes it.
    // So we expect e1, e2 (if e3 was last in seg1) AND e4, e5 (from seg2).
    // The gap (e3) is lost, but e4, e5 are kept.

    // DESIRED BEHAVIOR (fix):
    // It stops at segment 1 corruption. e4, e5 are ignored/dropped from index because
    // the chain is broken.

    let new_index = store.read_index()?;
    // Check if segment 2 entries are present.
    // Assuming e4 is in segment 2.
    // We need to know which entries are in which segment to be sure.

    // Let's check if the index contains ANY entries from segment 2.
    let has_seg2 = new_index.iter().any(|r| r.segment_seq == 2);

    if has_seg2 {
        println!(
            "Index contains entries from segment 2 after segment 1 corruption. (Regression/Bug)"
        );
        panic!("Index should NOT contain entries from segment 2 after segment 1 corruption");
    } else {
        println!("Index correctly stopped at segment 1. (Fix Verified)");
    }

    assert!(
        !seg2_path.exists(),
        "Orphaned segment 2 file should have been deleted"
    );

    Ok(())
}
