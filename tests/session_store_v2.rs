#![forbid(unsafe_code)]

use pi::PiResult;
use pi::session::{CustomEntry, EntryBase, MigrationState, Session, SessionEntry, SessionHeader};
use pi::session_store_v2::{
    Manifest, MigrationEvent, MigrationVerification, SessionStoreV2, frame_to_session_entry,
    session_entry_to_frame_args,
};
use proptest::prelude::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::future::Future;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};
use tempfile::tempdir;

const TEST_MANIFEST_SESSION_ID: &str = "f4c03c8c-cf0a-4c90-9535-e95f2d02b393";

const fn lcg_next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

fn append_linear_entries(store: &mut SessionStoreV2, count: usize) -> PiResult<Vec<String>> {
    let mut ids = Vec::with_capacity(count);
    let mut parent: Option<String> = None;
    for n in 1..=count {
        let id = format!("entry_{n:08}");
        store.append_entry(
            id.clone(),
            parent.clone(),
            "message",
            json!({"kind":"message","ordinal":n}),
        )?;
        parent = Some(id.clone());
        ids.push(id);
    }
    Ok(ids)
}

fn frame_ids(frames: &[pi::session_store_v2::SegmentFrame]) -> Vec<String> {
    frames.iter().map(|frame| frame.entry_id.clone()).collect()
}

fn read_index_json_rows(path: &Path) -> PiResult<Vec<Value>> {
    let content = fs::read_to_string(path)?;
    let mut rows = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        rows.push(serde_json::from_str::<Value>(line)?);
    }
    Ok(rows)
}

fn write_index_json_rows(path: &Path, rows: &[Value]) -> PiResult<()> {
    let mut output = String::new();
    for row in rows {
        output.push_str(&serde_json::to_string(row)?);
        output.push('\n');
    }
    fs::write(path, output)?;
    Ok(())
}

fn rewrite_single_segment_frames_and_index(
    store: &SessionStoreV2,
    frames: &[pi::session_store_v2::SegmentFrame],
) -> PiResult<()> {
    let mut index_rows = read_index_json_rows(&store.index_file_path())?;
    assert_eq!(index_rows.len(), frames.len());
    let segment_seq = frames.first().map_or(1, |frame| frame.segment_seq);
    assert!(frames.iter().all(|frame| frame.segment_seq == segment_seq));

    let mut segment_bytes = Vec::new();
    for (index_row, frame) in index_rows.iter_mut().zip(frames) {
        let byte_offset = segment_bytes.len();
        let mut record = serde_json::to_vec(frame)?;
        record.push(b'\n');
        index_row["byteOffset"] = json!(byte_offset);
        index_row["byteLength"] = json!(record.len());
        index_row["crc32c"] = json!(format!("{:08X}", crc32c::crc32c(&record)));
        segment_bytes.extend_from_slice(&record);
    }
    fs::write(store.segment_file_path(segment_seq), segment_bytes)?;
    write_index_json_rows(&store.index_file_path(), &index_rows)
}

fn write_rehashed_manifest(path: &Path, mut manifest: Manifest) -> PiResult<()> {
    manifest.integrity.manifest_hash.clear();
    manifest.integrity.manifest_hash =
        format!("{:x}", Sha256::digest(serde_json::to_vec(&manifest)?));
    fs::write(path, serde_json::to_vec_pretty(&manifest)?)?;
    Ok(())
}

#[test]
fn segmented_append_and_index_round_trip() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;

    store.append_entry(
        "entry_00000001",
        None,
        "message",
        json!({"role":"user","text":"a"}),
    )?;
    store.append_entry(
        "entry_00000002",
        Some("entry_00000001".to_string()),
        "message",
        json!({"role":"assistant","text":"b"}),
    )?;

    let index = store.read_index()?;
    assert_eq!(index.len(), 2);
    assert_eq!(index[0].entry_seq, 1);
    assert_eq!(index[1].entry_seq, 2);

    let segment_one = store.read_segment(1)?;
    assert_eq!(segment_one.len(), 2);
    assert_eq!(segment_one[0].entry_id, "entry_00000001");
    assert_eq!(segment_one[1].entry_id, "entry_00000002");

    store.validate_integrity()?;
    Ok(())
}

#[test]
fn rotates_segment_when_threshold_is_hit() -> PiResult<()> {
    let dir = tempdir()?;
    let payload = json!({
        "kind": "message",
        "text": "x".repeat(180)
    });

    let mut probe = SessionStoreV2::create(dir.path().join("probe"), 4 * 1024)?;
    let threshold = probe
        .append_entry("entry_00000001", None, "message", payload.clone())?
        .byte_length;
    let mut store = SessionStoreV2::create(dir.path().join("store"), threshold)?;

    store.append_entry("entry_00000001", None, "message", payload.clone())?;
    store.append_entry("entry_00000002", None, "message", payload)?;

    let index = store.read_index()?;
    assert_eq!(index.len(), 2);
    assert!(index[1].segment_seq > index[0].segment_seq);
    Ok(())
}

#[test]
fn append_path_preserves_prior_bytes_prefix() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;

    let first = store.append_entry(
        "entry_00000001",
        None,
        "message",
        json!({"kind":"message","text":"first"}),
    )?;
    let first_segment = store.segment_file_path(first.segment_seq);
    let before = fs::read(&first_segment)?;

    store.append_entry(
        "entry_00000002",
        Some("entry_00000001".to_string()),
        "message",
        json!({"kind":"message","text":"second"}),
    )?;
    let after = fs::read(&first_segment)?;

    assert!(
        after.starts_with(&before),
        "append should preserve existing segment prefix bytes"
    );
    Ok(())
}

#[test]
fn corruption_is_detected_from_indexed_checksum() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;

    let row = store.append_entry("entry_00000001", None, "message", json!({"text":"hello"}))?;
    let segment_path = store.segment_file_path(row.segment_seq);

    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&segment_path)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(b"[")?;
    file.flush()?;

    let err = store
        .validate_integrity()
        .expect_err("checksum mismatch should be detected");
    assert!(
        err.to_string().contains("checksum mismatch"),
        "unexpected error: {err}"
    );

    Ok(())
}

#[test]
fn bootstrap_fails_if_index_points_to_missing_segment() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let row = store.append_entry("entry_00000001", None, "message", json!({"text":"hello"}))?;

    let segment_path = store.segment_file_path(row.segment_seq);
    fs::remove_file(&segment_path)?;

    let err = SessionStoreV2::create(dir.path(), 4 * 1024)
        .expect_err("bootstrap should fail when active segment is missing");
    assert!(
        err.to_string().contains("failed to stat active segment"),
        "unexpected error: {err}"
    );
    Ok(())
}

#[test]
fn create_recovers_when_index_file_is_missing_but_segments_exist() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let expected_ids = append_linear_entries(&mut store, 4)?;

    let index_path = store.index_file_path();
    fs::remove_file(&index_path)?;

    let recovered = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    recovered.validate_integrity()?;
    assert_eq!(recovered.entry_count(), 4);
    assert_eq!(frame_ids(&recovered.read_all_entries()?), expected_ids);
    Ok(())
}

#[test]
fn create_recovers_when_index_json_is_corrupt() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let expected_ids = append_linear_entries(&mut store, 5)?;

    let index_path = store.index_file_path();
    fs::write(&index_path, "{ definitely-not-json }\n")?;

    let recovered = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    recovered.validate_integrity()?;
    assert_eq!(recovered.entry_count(), 5);
    assert_eq!(frame_ids(&recovered.read_all_entries()?), expected_ids);
    Ok(())
}

#[test]
fn create_rebuilds_unterminated_index_before_append() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let mut expected_ids = append_linear_entries(&mut store, 4)?;
    let index_path = store.index_file_path();
    let mut index_bytes = fs::read(&index_path)?;
    assert_eq!(
        index_bytes.pop(),
        Some(b'\n'),
        "fixture index must end in LF"
    );
    fs::write(&index_path, index_bytes)?;
    drop(store);

    let mut recovered = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    assert_eq!(frame_ids(&recovered.read_all_entries()?), expected_ids);
    assert_eq!(fs::read(&index_path)?.last(), Some(&b'\n'));

    let next_id = "entry_00000005".to_string();
    recovered.append_entry(
        next_id.clone(),
        expected_ids.last().cloned(),
        "message",
        json!({"kind":"message","ordinal":5}),
    )?;
    expected_ids.push(next_id);
    recovered.validate_integrity()?;
    assert_eq!(frame_ids(&recovered.read_all_entries()?), expected_ids);
    Ok(())
}

#[test]
fn create_recovers_when_index_bounds_are_corrupt() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let expected_ids = append_linear_entries(&mut store, 6)?;

    let index_path = store.index_file_path();
    let mut rows = read_index_json_rows(&index_path)?;
    rows[0]["byteLength"] = json!(9_999_999_u64);
    write_index_json_rows(&index_path, &rows)?;

    let recovered = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    recovered.validate_integrity()?;
    assert_eq!(recovered.entry_count(), 6);
    assert_eq!(frame_ids(&recovered.read_all_entries()?), expected_ids);
    Ok(())
}

#[test]
fn create_recovers_when_index_frame_metadata_is_corrupt() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let expected_ids = append_linear_entries(&mut store, 5)?;

    let index_path = store.index_file_path();
    let mut rows = read_index_json_rows(&index_path)?;
    rows[0]["entryId"] = json!("entry_corrupted");
    write_index_json_rows(&index_path, &rows)?;

    let recovered = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    recovered.validate_integrity()?;
    assert_eq!(recovered.entry_count(), 5);
    assert_eq!(frame_ids(&recovered.read_all_entries()?), expected_ids);
    Ok(())
}

#[test]
fn create_recovers_when_segment_has_truncated_trailing_frame() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let expected_ids = append_linear_entries(&mut store, 4)?;

    let seg_path = store.segment_file_path(1);
    let bytes = fs::read(&seg_path)?;
    let newline_positions: Vec<usize> = bytes
        .iter()
        .enumerate()
        .filter_map(|(idx, byte)| (*byte == b'\n').then_some(idx))
        .collect();
    assert!(
        newline_positions.len() >= 4,
        "expected at least 4 lines in segment"
    );
    let start_of_last_line = newline_positions[newline_positions.len() - 2].saturating_add(1);
    let truncate_to = start_of_last_line.saturating_add(8);
    fs::OpenOptions::new()
        .write(true)
        .open(&seg_path)?
        .set_len(u64::try_from(truncate_to).unwrap_or(u64::MAX))?;
    drop(store);

    let recovered = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    recovered.validate_integrity()?;
    assert_eq!(recovered.entry_count(), 3);
    assert_eq!(
        frame_ids(&recovered.read_all_entries()?),
        expected_ids[..3].to_vec()
    );
    assert_eq!(
        fs::metadata(&seg_path)?.len(),
        u64::try_from(start_of_last_line).unwrap_or(u64::MAX)
    );
    Ok(())
}

#[test]
fn create_recovers_when_final_frame_has_no_trailing_newline() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let expected_ids = append_linear_entries(&mut store, 4)?;

    let seg_path = store.segment_file_path(1);
    let original_len = fs::metadata(&seg_path)?.len();
    assert!(original_len > 0, "segment file must be non-empty");
    let bytes = fs::read(&seg_path)?;
    assert!(
        bytes.last() == Some(&b'\n'),
        "expected segment file to end with newline"
    );
    fs::OpenOptions::new()
        .write(true)
        .open(&seg_path)?
        .set_len(original_len.saturating_sub(1))?;

    // Force index rebuild path.
    fs::remove_file(store.index_file_path())?;
    drop(store);

    let recovered = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    recovered.validate_integrity()?;
    assert_eq!(recovered.entry_count(), 4);
    assert_eq!(frame_ids(&recovered.read_all_entries()?), expected_ids);
    Ok(())
}

#[test]
fn rebuild_index_recovers_when_final_frame_has_no_trailing_newline_and_allows_append()
-> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let mut expected_ids = append_linear_entries(&mut store, 4)?;

    let seg_path = store.segment_file_path(1);
    let original_len = fs::metadata(&seg_path)?.len();
    assert!(original_len > 0, "segment file must be non-empty");
    let bytes = fs::read(&seg_path)?;
    assert!(
        bytes.last() == Some(&b'\n'),
        "expected segment file to end with newline"
    );
    fs::OpenOptions::new()
        .write(true)
        .open(&seg_path)?
        .set_len(original_len.saturating_sub(1))?;

    fs::remove_file(store.index_file_path())?;

    let rebuilt = store.rebuild_index()?;
    assert_eq!(rebuilt, 4);
    store.validate_integrity()?;
    assert_eq!(store.entry_count(), 4);
    assert_eq!(frame_ids(&store.read_all_entries()?), expected_ids);

    let next_id = "entry_00000005".to_string();
    store.append_entry(
        next_id.clone(),
        expected_ids.last().cloned(),
        "message",
        json!({"kind":"message","ordinal":5}),
    )?;
    expected_ids.push(next_id);

    store.validate_integrity()?;
    assert_eq!(store.entry_count(), 5);
    assert_eq!(frame_ids(&store.read_all_entries()?), expected_ids);
    Ok(())
}

#[test]
fn create_fails_closed_when_non_eof_segment_frame_is_corrupt() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    append_linear_entries(&mut store, 4)?;

    let seg_path = store.segment_file_path(1);
    let segment_text = fs::read_to_string(&seg_path)?;
    let mut lines: Vec<String> = segment_text.lines().map(ToString::to_string).collect();
    assert!(lines.len() >= 4, "expected at least 4 frames in segment");
    lines[1] = "{ malformed-json-frame".to_string();
    let rewritten = format!("{}\n", lines.join("\n"));
    fs::write(&seg_path, rewritten)?;

    // Force create() into rebuild path.
    fs::remove_file(store.index_file_path())?;
    drop(store);

    let err = SessionStoreV2::create(dir.path(), 4 * 1024)
        .expect_err("non-EOF segment corruption must fail closed");
    assert!(
        err.to_string()
            .contains("failed to parse segment frame while rebuilding index"),
        "unexpected error: {err}"
    );
    Ok(())
}

// ── O(index+tail) resume path tests ──────────────────────────────────

/// Helper: build a `SessionEntry::Custom` with the given id and parent.
fn make_custom_entry(id: &str, parent_id: Option<&str>) -> SessionEntry {
    SessionEntry::Custom(CustomEntry {
        base: EntryBase::new(parent_id.map(String::from), id.to_string()),
        custom_type: "test".to_string(),
        data: Some(json!({"id": id})),
    })
}

/// Append a `SessionEntry` to a V2 store via the conversion helpers.
fn append_session_entry(
    store: &mut SessionStoreV2,
    entry: &SessionEntry,
) -> PiResult<pi::session_store_v2::OffsetIndexEntry> {
    let (entry_id, parent_id, entry_type, payload) = session_entry_to_frame_args(entry)?;
    store.append_entry(entry_id, parent_id, entry_type, payload)
}

#[test]
fn read_tail_entries_returns_last_n() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let ids = append_linear_entries(&mut store, 5)?;

    let tail = store.read_tail_entries(2)?;
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].entry_id, ids[3]);
    assert_eq!(tail[1].entry_id, ids[4]);

    // Requesting more than available returns all.
    let all = store.read_tail_entries(100)?;
    assert_eq!(all.len(), 5);
    assert_eq!(frame_ids(&all), ids);

    // Zero returns empty.
    let zero = store.read_tail_entries(0)?;
    assert!(zero.is_empty());

    Ok(())
}

#[test]
fn bounded_resume_skips_unselected_corruption_but_rejects_it_when_accessed() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 64 * 1024)?;
    let ids = append_linear_entries(&mut store, 6)?;
    store.write_manifest(TEST_MANIFEST_SESSION_ID, "native_v2")?;
    let index = store.read_index()?;

    let first = &index[0];
    let first_segment_path = store.segment_file_path(first.segment_seq);
    let mut first_segment = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&first_segment_path)?;
    first_segment.seek(SeekFrom::Start(first.byte_offset))?;
    first_segment.write_all(b"[")?;
    first_segment.sync_all()?;

    let manifest = store
        .validate_resume_manifest_against_store()?
        .expect("structurally valid bounded resume manifest");
    assert_eq!(manifest.counters.entries_total, 6);
    let tail = store.read_tail_entries(1)?;
    assert_eq!(frame_ids(&tail), ids[5..].to_vec());

    let audit_error = store
        .validate_integrity()
        .expect_err("a full audit must still inspect the corrupt sibling frame");
    assert!(
        audit_error.to_string().contains("checksum mismatch"),
        "unexpected audit error: {audit_error}"
    );
    let accessed_error = store
        .lookup_entry(first.entry_seq)
        .expect_err("fetching the corrupt sibling must fail closed");
    assert!(
        accessed_error.to_string().contains("checksum mismatch"),
        "unexpected accessed-frame error: {accessed_error}"
    );
    Ok(())
}

#[test]
fn fetched_frame_rejects_payload_hash_corruption_with_matching_index_crc() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let row = store.append_entry(
        "entry_00000001",
        None,
        "message",
        json!({"kind":"message","text":"payload-bound"}),
    )?;
    let frame = store
        .lookup_entry(row.entry_seq)?
        .expect("freshly appended frame");
    let digest = frame.payload_sha256.as_bytes();

    let segment_path = store.segment_file_path(row.segment_seq);
    let mut segment_bytes = fs::read(&segment_path)?;
    let start = usize::try_from(row.byte_offset).expect("frame offset fits usize");
    let length = usize::try_from(row.byte_length).expect("frame length fits usize");
    let end = start.checked_add(length).expect("frame range fits usize");
    let record = &mut segment_bytes[start..end];
    let digest_offset = record
        .windows(digest.len())
        .position(|window| window == digest)
        .expect("payload digest occurs in frame");
    record[digest_offset] = if record[digest_offset] == b'0' {
        b'1'
    } else {
        b'0'
    };
    let replacement_crc = format!("{:08X}", crc32c::crc32c(record));
    fs::write(&segment_path, &segment_bytes)?;

    let mut index_rows = read_index_json_rows(&store.index_file_path())?;
    index_rows[0]["crc32c"] = json!(replacement_crc);
    write_index_json_rows(&store.index_file_path(), &index_rows)?;

    let error = store
        .lookup_entry(row.entry_seq)
        .expect_err("payload hash mismatch must fail even when the index CRC matches");
    assert!(
        error.to_string().contains("payload integrity mismatch"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[test]
fn fetched_frame_requires_lf_even_when_index_crc_matches() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let row = store.append_entry(
        "entry_00000001",
        None,
        "message",
        json!({"kind":"message","text":"lf-bound"}),
    )?;
    let segment_path = store.segment_file_path(row.segment_seq);
    let mut segment_bytes = fs::read(&segment_path)?;
    let start = usize::try_from(row.byte_offset).expect("frame offset fits usize");
    let length = usize::try_from(row.byte_length).expect("frame length fits usize");
    let end = start.checked_add(length).expect("frame range fits usize");
    let record = &mut segment_bytes[start..end];
    assert_eq!(record.last(), Some(&b'\n'));
    *record.last_mut().expect("nonempty frame") = b' ';
    let replacement_crc = format!("{:08X}", crc32c::crc32c(record));
    fs::write(&segment_path, &segment_bytes)?;

    let mut index_rows = read_index_json_rows(&store.index_file_path())?;
    index_rows[0]["crc32c"] = json!(replacement_crc);
    write_index_json_rows(&store.index_file_path(), &index_rows)?;

    let error = store
        .lookup_entry(row.entry_seq)
        .expect_err("a fetched frame without LF termination must fail closed");
    assert!(
        error.to_string().contains("not LF-terminated"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[test]
fn bounded_fetch_rejects_a_selected_self_parent_with_matching_crc() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let ids = append_linear_entries(&mut store, 2)?;
    let index = store.read_index()?;
    let row = &index[1];

    let segment_path = store.segment_file_path(row.segment_seq);
    let mut segment_bytes = fs::read(&segment_path)?;
    let start = usize::try_from(row.byte_offset).expect("frame offset fits usize");
    let length = usize::try_from(row.byte_length).expect("frame length fits usize");
    let end = start.checked_add(length).expect("frame range fits usize");
    let record = &mut segment_bytes[start..end];
    let parent_offset = record
        .windows(ids[0].len())
        .position(|window| window == ids[0].as_bytes())
        .expect("second frame contains its parent ID");
    record[parent_offset..parent_offset + ids[1].len()].copy_from_slice(ids[1].as_bytes());
    let replacement_crc = format!("{:08X}", crc32c::crc32c(record));
    fs::write(&segment_path, segment_bytes)?;

    let mut index_rows = read_index_json_rows(&store.index_file_path())?;
    index_rows[1]["crc32c"] = json!(replacement_crc);
    write_index_json_rows(&store.index_file_path(), &index_rows)?;

    let error = store
        .read_tail_entries(1)
        .expect_err("a fetched self-parent must be rejected before hydration");
    assert!(
        error.to_string().contains("cyclic parent chain"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[test]
fn wire_readers_reject_unknown_index_and_frame_fields() -> PiResult<()> {
    let dir = tempdir()?;

    let index_root = dir.path().join("index");
    let mut index_store = SessionStoreV2::create(&index_root, 4 * 1024)?;
    append_linear_entries(&mut index_store, 1)?;
    let mut index_rows = read_index_json_rows(&index_store.index_file_path())?;
    index_rows[0]["unexpectedField"] = json!(true);
    write_index_json_rows(&index_store.index_file_path(), &index_rows)?;
    let error = index_store
        .read_index()
        .expect_err("unknown offset-index fields must be rejected");
    assert!(error.to_string().contains("unknown field"));

    let frame_root = dir.path().join("frame");
    let mut frame_store = SessionStoreV2::create(&frame_root, 4 * 1024)?;
    let row = frame_store.append_entry(
        "entry_00000001",
        None,
        "message",
        json!({"kind":"message","ordinal":1}),
    )?;
    let segment_path = frame_store.segment_file_path(row.segment_seq);
    let segment_bytes = fs::read(&segment_path)?;
    let mut frame_json: Value = serde_json::from_slice(
        segment_bytes
            .strip_suffix(b"\n")
            .expect("stored frame is LF-terminated"),
    )?;
    frame_json["unexpectedField"] = json!(true);
    let mut forged_record = serde_json::to_vec(&frame_json)?;
    forged_record.push(b'\n');
    fs::write(&segment_path, &forged_record)?;

    let mut frame_index_rows = read_index_json_rows(&frame_store.index_file_path())?;
    frame_index_rows[0]["byteLength"] = json!(forged_record.len());
    frame_index_rows[0]["crc32c"] = json!(format!("{:08X}", crc32c::crc32c(&forged_record)));
    write_index_json_rows(&frame_store.index_file_path(), &frame_index_rows)?;

    let error = frame_store
        .lookup_entry(row.entry_seq)
        .expect_err("unknown segment-frame fields must be rejected before use");
    assert!(error.to_string().contains("unknown field"));
    Ok(())
}

#[test]
fn checkpoint_reader_rejects_unknown_fields() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    append_linear_entries(&mut store, 1)?;
    store.create_checkpoint(1, "manual")?;

    let checkpoint_path = dir.path().join("checkpoints/0000000000000001.json");
    let mut checkpoint: Value = serde_json::from_slice(&fs::read(&checkpoint_path)?)?;
    checkpoint["unexpectedField"] = json!(true);
    fs::write(&checkpoint_path, serde_json::to_vec_pretty(&checkpoint)?)?;

    let error = store
        .read_checkpoint(1)
        .expect_err("unknown checkpoint fields must be rejected");
    assert!(error.to_string().contains("unknown field"));
    Ok(())
}

#[test]
fn bounded_resume_rejects_an_unindexed_empty_segment_file() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    append_linear_entries(&mut store, 2)?;
    store.write_manifest(TEST_MANIFEST_SESSION_ID, "native_v2")?;

    let unindexed_segment = store.segment_file_path(99);
    fs::write(&unindexed_segment, [])?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&unindexed_segment, fs::Permissions::from_mode(0o600))?;
    }

    let error = store
        .validate_resume_manifest_against_store()
        .expect_err("even an empty unindexed segment must fail structural coverage validation");
    assert!(
        error.to_string().contains("segment byte coverage mismatch"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[test]
fn read_active_path_linear_returns_all() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let ids = append_linear_entries(&mut store, 5)?;

    let path = store.read_active_path(&ids[4])?;
    assert_eq!(frame_ids(&path), ids);
    Ok(())
}

#[test]
fn read_active_path_branching_returns_only_branch() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;

    // Build a tree:
    //   A → B → C (main branch)
    //        ↘ D → E (side branch)
    store.append_entry("A", None, "message", json!({"v":"A"}))?;
    store.append_entry("B", Some("A".to_string()), "message", json!({"v":"B"}))?;
    store.append_entry("C", Some("B".to_string()), "message", json!({"v":"C"}))?;
    store.append_entry("D", Some("B".to_string()), "message", json!({"v":"D"}))?;
    store.append_entry("E", Some("D".to_string()), "message", json!({"v":"E"}))?;

    // Active path from leaf E: E→D→B→A, reversed to A→B→D→E.
    let path = store.read_active_path("E")?;
    assert_eq!(frame_ids(&path), vec!["A", "B", "D", "E"]);

    // Active path from leaf C: C→B→A, reversed to A→B→C.
    let path = store.read_active_path("C")?;
    assert_eq!(frame_ids(&path), vec!["A", "B", "C"]);

    // Unknown leaf returns empty.
    let path = store.read_active_path("UNKNOWN")?;
    assert!(path.is_empty());

    Ok(())
}

#[test]
fn read_active_path_errors_on_cyclic_parent_chain() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    store.append_entry("A", None, "message", json!({"v":"A"}))?;
    store.append_entry("B", Some("A".to_string()), "message", json!({"v":"B"}))?;

    let mut frames = store.read_segment(1)?;
    assert_eq!(frames.len(), 2);
    frames[1].parent_entry_id = Some("B".to_string());
    rewrite_single_segment_frames_and_index(&store, &frames)?;

    let err = store
        .read_active_path("B")
        .expect_err("cyclic parent chain must fail");
    assert!(err.to_string().contains("cyclic parent chain detected"));
    Ok(())
}

#[test]
fn read_active_path_errors_on_duplicate_entry_ids() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    store.append_entry("A", None, "message", json!({"v":"A"}))?;
    store.append_entry("B", Some("A".to_string()), "message", json!({"v":"B"}))?;

    let index_path = store.index_file_path();
    let mut rows = read_index_json_rows(&index_path)?;
    assert_eq!(rows.len(), 2);
    rows[1]["entryId"] = Value::String("A".to_string());
    write_index_json_rows(&index_path, &rows)?;

    let err = store
        .read_active_path("A")
        .expect_err("duplicate entry_id must fail");
    assert!(err.to_string().contains("duplicate entry_id detected"));
    Ok(())
}

#[test]
fn read_active_path_errors_on_missing_leaf_frame() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    store.append_entry("A", None, "message", json!({"v":"A"}))?;
    store.append_entry("B", Some("A".to_string()), "message", json!({"v":"B"}))?;

    let index_path = store.index_file_path();
    let mut rows = read_index_json_rows(&index_path)?;
    assert_eq!(rows.len(), 2);
    rows[1]["segmentSeq"] = Value::from(999_u64);
    write_index_json_rows(&index_path, &rows)?;

    let err = store
        .read_active_path("B")
        .expect_err("missing indexed leaf frame must fail");
    assert!(err.to_string().contains("index references missing segment"));
    Ok(())
}

#[test]
fn read_active_path_errors_on_missing_parent_reference() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    store.append_entry("A", None, "message", json!({"v":"A"}))?;
    store.append_entry("B", Some("A".to_string()), "message", json!({"v":"B"}))?;

    let mut frames = store.read_segment(1)?;
    assert_eq!(frames.len(), 2);
    frames[1].parent_entry_id = Some("Z".to_string());
    rewrite_single_segment_frames_and_index(&store, &frames)?;

    let err = store
        .read_active_path("B")
        .expect_err("missing mid-chain parent must fail");
    assert!(err.to_string().contains("missing parent entry detected"));
    Ok(())
}

#[test]
fn validate_integrity_rejects_duplicate_frame_entry_ids() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    store.append_entry("A", None, "message", json!({"v":"A"}))?;
    store.append_entry("B", Some("A".to_string()), "message", json!({"v":"B"}))?;

    let segment_path = store.segment_file_path(1);
    let mut frames = store.read_segment(1)?;
    assert_eq!(frames.len(), 2);
    frames[1].entry_id = "A".to_string();

    let mut encoded = String::new();
    for frame in frames {
        encoded.push_str(&serde_json::to_string(&frame)?);
        encoded.push('\n');
    }
    fs::write(&segment_path, encoded)?;
    store.rebuild_index()?;

    let err = store
        .validate_integrity()
        .expect_err("duplicate frame entry IDs must fail integrity validation");
    assert!(err.to_string().contains("duplicate entry_id detected"));
    Ok(())
}

#[test]
fn validate_integrity_rejects_cyclic_parent_chain() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    store.append_entry("A", None, "message", json!({"v":"A"}))?;
    store.append_entry("B", Some("A".to_string()), "message", json!({"v":"B"}))?;

    let segment_path = store.segment_file_path(1);
    let mut frames = store.read_segment(1)?;
    assert_eq!(frames.len(), 2);
    frames[1].parent_entry_id = Some("B".to_string());

    let mut encoded = String::new();
    for frame in frames {
        encoded.push_str(&serde_json::to_string(&frame)?);
        encoded.push('\n');
    }
    fs::write(&segment_path, encoded)?;
    store.rebuild_index()?;

    let err = store
        .validate_integrity()
        .expect_err("cyclic parent chains must fail integrity validation");
    assert!(err.to_string().contains("cyclic parent chain detected"));
    Ok(())
}

#[test]
fn validate_integrity_rejects_missing_parent_reference() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    store.append_entry("A", None, "message", json!({"v":"A"}))?;
    store.append_entry("B", Some("A".to_string()), "message", json!({"v":"B"}))?;

    let segment_path = store.segment_file_path(1);
    let mut frames = store.read_segment(1)?;
    assert_eq!(frames.len(), 2);
    frames[1].parent_entry_id = Some("Z".to_string());

    let mut encoded = String::new();
    for frame in frames {
        encoded.push_str(&serde_json::to_string(&frame)?);
        encoded.push('\n');
    }
    fs::write(&segment_path, encoded)?;
    store.rebuild_index()?;

    let err = store
        .validate_integrity()
        .expect_err("dangling parent references must fail integrity validation");
    assert!(err.to_string().contains("missing parent entry detected"));
    Ok(())
}

#[test]
fn frame_to_session_entry_roundtrip() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;

    let entry = make_custom_entry("e1", None);
    append_session_entry(&mut store, &entry)?;

    let frames = store.read_all_entries()?;
    assert_eq!(frames.len(), 1);

    let recovered = frame_to_session_entry(&frames[0])?;
    assert_eq!(recovered.base_id(), entry.base_id());
    assert_eq!(recovered.base().parent_id, entry.base().parent_id);

    // Verify the payload round-trips correctly.
    let original_json = serde_json::to_value(&entry)?;
    let recovered_json = serde_json::to_value(&recovered)?;
    assert_eq!(original_json, recovered_json);

    Ok(())
}

#[test]
fn frame_to_session_entry_rejects_metadata_payload_mismatch() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    append_session_entry(&mut store, &make_custom_entry("parent", None))?;
    let entry = make_custom_entry("entry", Some("parent"));
    append_session_entry(&mut store, &entry)?;
    let frame = store
        .read_all_entries()?
        .into_iter()
        .nth(1)
        .expect("child frame");

    let mut wrong_id = frame.clone();
    wrong_id.entry_id = "other".to_string();
    assert!(
        frame_to_session_entry(&wrong_id)
            .expect_err("entry ID mismatch must fail")
            .to_string()
            .contains("frame entry_id mismatch")
    );

    let mut wrong_parent = frame.clone();
    wrong_parent.parent_entry_id = Some("other-parent".to_string());
    assert!(
        frame_to_session_entry(&wrong_parent)
            .expect_err("parent mismatch must fail")
            .to_string()
            .contains("frame parent_entry_id mismatch")
    );

    let mut wrong_type = frame;
    wrong_type.entry_type = "message".to_string();
    assert!(
        frame_to_session_entry(&wrong_type)
            .expect_err("entry type mismatch must fail")
            .to_string()
            .contains("frame entry_type mismatch")
    );
    Ok(())
}

#[test]
fn session_integrity_rejects_tampered_frame_metadata() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    append_session_entry(&mut store, &make_custom_entry("entry", None))?;

    let segment_path = store.segment_file_path(1);
    let mut frames = store.read_segment(1)?;
    frames[0].entry_type = "message".to_string();
    fs::write(
        &segment_path,
        format!("{}\n", serde_json::to_string(&frames[0])?),
    )?;
    store.rebuild_index()?;

    store.validate_integrity()?;
    let err = store
        .validate_session_integrity()
        .expect_err("session integrity must bind frame metadata to its payload");
    assert!(err.to_string().contains("frame entry_type mismatch"));
    Ok(())
}

#[test]
fn session_entry_to_frame_args_preserves_fields() -> PiResult<()> {
    let entry = make_custom_entry("my_id", Some("parent_id"));
    let (entry_id, parent_id, entry_type, payload) = session_entry_to_frame_args(&entry)?;

    assert_eq!(entry_id, "my_id");
    assert_eq!(parent_id.as_deref(), Some("parent_id"));
    assert_eq!(entry_type, "custom");
    assert!(payload.is_object());
    assert_eq!(payload["type"], "custom");

    // Entry without ID should fail.
    let mut no_id = make_custom_entry("x", None);
    no_id.base_mut().id = None;
    let err = session_entry_to_frame_args(&no_id);
    assert!(err.is_err());

    Ok(())
}

#[test]
fn read_tail_entries_on_1000_entry_store_reads_only_10_frames() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 64 * 1024 * 1024)?;
    let ids = append_linear_entries(&mut store, 1000)?;

    let tail = store.read_tail_entries(10)?;
    assert_eq!(tail.len(), 10);
    assert_eq!(frame_ids(&tail), ids[990..].to_vec());

    // Verify the frames are in entry_seq order.
    for window in tail.windows(2) {
        assert!(
            window[0].entry_seq < window[1].entry_seq,
            "tail entries must be in entry_seq order"
        );
    }

    Ok(())
}

#[test]
fn seeded_randomized_append_replay_invariants() -> PiResult<()> {
    const SEEDS: [u64; 6] = [
        0x00C0_FFEE_D15E_A5E5,
        0x0000_0000_DEAD_BEEF,
        0x0000_0000_1234_5678,
        0x0000_0000_0BAD_F00D,
        0x0000_0000_5EED_CAFE,
        0x0000_0000_A11C_EBAD,
    ];

    for seed in SEEDS {
        let dir = tempdir()?;
        let artifact_hint = dir.path().display().to_string();
        let mut state = seed;
        let max_segment_bytes = 512 + (lcg_next(&mut state) % 768);
        let mut store = SessionStoreV2::create(dir.path(), max_segment_bytes)?;

        let entry_count = 24 + usize::try_from(lcg_next(&mut state) % 32).unwrap_or(0);
        let mut expected_ids: Vec<String> = Vec::with_capacity(entry_count);
        for idx in 0..entry_count {
            let entry_id = format!("entry_{:08}", idx + 1);
            let parent_entry_id = if idx == 0 {
                None
            } else if lcg_next(&mut state).is_multiple_of(5) {
                let parent_index = usize::try_from(lcg_next(&mut state)).unwrap_or(0) % idx;
                Some(expected_ids[parent_index].clone())
            } else {
                Some(expected_ids[idx - 1].clone())
            };
            let entropy = lcg_next(&mut state);
            let payload = json!({
                "seed": format!("{seed:016x}"),
                "index": idx,
                "entropy": entropy,
                "parentHint": parent_entry_id,
            });

            let row = store.append_entry(
                entry_id.clone(),
                parent_entry_id.clone(),
                "message",
                payload,
            )?;
            assert_eq!(
                row.entry_seq,
                u64::try_from(idx + 1).unwrap_or(u64::MAX),
                "seed={seed:016x} artifact={artifact_hint}"
            );
            expected_ids.push(entry_id);
        }

        let integrity = store.validate_integrity();
        assert!(
            integrity.is_ok(),
            "seed={seed:016x} artifact={artifact_hint} err={}",
            integrity
                .err()
                .map_or_else(String::new, |err| err.to_string())
        );

        let index = store.read_index()?;
        assert_eq!(
            index.len(),
            entry_count,
            "seed={seed:016x} artifact={artifact_hint}"
        );
        for (idx, row) in index.iter().enumerate() {
            assert_eq!(
                row.entry_seq,
                u64::try_from(idx + 1).unwrap_or(u64::MAX),
                "seed={seed:016x} artifact={artifact_hint}"
            );
            let looked_up = store
                .lookup_entry(row.entry_seq)?
                .expect("entry should exist");
            assert_eq!(
                looked_up.entry_id, row.entry_id,
                "seed={seed:016x} artifact={artifact_hint}"
            );
        }

        let from_seq = 1 + (lcg_next(&mut state) % u64::try_from(entry_count).unwrap_or(1));
        let from_entries = store.read_entries_from(from_seq)?;
        assert_eq!(
            from_entries.len(),
            entry_count.saturating_sub(usize::try_from(from_seq).unwrap_or(1) - 1),
            "seed={seed:016x} artifact={artifact_hint}"
        );

        let tail_count = 1 + (usize::try_from(lcg_next(&mut state)).unwrap_or(0) % 8);
        let expected_tail = expected_ids[entry_count - tail_count..].to_vec();
        let tail_entries =
            store.read_tail_entries(u64::try_from(tail_count).unwrap_or(u64::MAX))?;
        assert_eq!(
            frame_ids(&tail_entries),
            expected_tail,
            "seed={seed:016x} artifact={artifact_hint}"
        );

        drop(store);
        let reopened = SessionStoreV2::create(dir.path(), max_segment_bytes)?;
        let replayed_ids = frame_ids(&reopened.read_all_entries()?);
        assert_eq!(
            replayed_ids, expected_ids,
            "seed={seed:016x} artifact={artifact_hint}"
        );
    }

    Ok(())
}

#[test]
fn corruption_corpus_index_bounds_violation_is_detected_and_recoverable() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let expected_ids = append_linear_entries(&mut store, 6)?;

    let index_path = store.index_file_path();
    let mut rows = read_index_json_rows(&index_path)?;
    rows[0]["byteLength"] = json!(9_999_999_u64);
    write_index_json_rows(&index_path, &rows)?;

    let err = store
        .validate_integrity()
        .expect_err("bounds corruption must fail integrity validation");
    assert!(
        err.to_string().contains("index out of bounds"),
        "unexpected error: {err}"
    );

    let rebuilt = store.rebuild_index()?;
    assert_eq!(rebuilt, 6);
    store.validate_integrity()?;
    assert_eq!(frame_ids(&store.read_all_entries()?), expected_ids);

    Ok(())
}

#[test]
fn corruption_corpus_index_frame_mismatch_is_detected_and_recoverable() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let expected_ids = append_linear_entries(&mut store, 5)?;

    let index_path = store.index_file_path();
    let mut rows = read_index_json_rows(&index_path)?;
    rows[0]["entryId"] = json!("entry_corrupted");
    write_index_json_rows(&index_path, &rows)?;

    let fetched_error = store
        .lookup_entry(1)
        .expect_err("an accessed frame must be bound to its index coordinates and ID");
    assert!(
        fetched_error.to_string().contains("index/frame mismatch"),
        "unexpected fetched-frame error: {fetched_error}"
    );

    let err = store
        .validate_integrity()
        .expect_err("entry_id tampering must fail integrity validation");
    assert!(
        err.to_string().contains("index/frame mismatch"),
        "unexpected error: {err}"
    );

    let rebuilt = store.rebuild_index()?;
    assert_eq!(rebuilt, 5);
    store.validate_integrity()?;
    assert_eq!(frame_ids(&store.read_all_entries()?), expected_ids);

    Ok(())
}

#[test]
fn checkpoint_replay_is_deterministic_after_reopen_and_rebuild() -> PiResult<()> {
    let dir = tempdir()?;
    let max_segment_bytes = 512;
    let mut store = SessionStoreV2::create(dir.path(), max_segment_bytes)?;
    let expected_ids = append_linear_entries(&mut store, 14)?;

    let checkpoint = store.create_checkpoint(1, "manual")?;
    let baseline_ids = frame_ids(&store.read_all_entries()?);
    let tail_from = checkpoint.head_entry_seq.saturating_sub(4).max(1);
    let baseline_tail_ids = frame_ids(&store.read_entries_from(tail_from)?);

    assert_eq!(
        checkpoint.head_entry_id,
        expected_ids
            .last()
            .cloned()
            .expect("non-empty expected IDs"),
    );
    assert_eq!(baseline_ids, expected_ids);

    drop(store);
    let mut reopened = SessionStoreV2::create(dir.path(), max_segment_bytes)?;
    let reopened_checkpoint = reopened
        .read_checkpoint(1)?
        .expect("checkpoint should exist after reopen");
    assert_eq!(
        reopened_checkpoint.head_entry_seq,
        checkpoint.head_entry_seq
    );
    assert_eq!(reopened_checkpoint.head_entry_id, checkpoint.head_entry_id);
    assert_eq!(reopened_checkpoint.chain_hash, checkpoint.chain_hash);

    assert_eq!(frame_ids(&reopened.read_all_entries()?), baseline_ids);
    assert_eq!(
        frame_ids(&reopened.read_entries_from(tail_from)?),
        baseline_tail_ids
    );

    let rebuilt = reopened.rebuild_index()?;
    assert_eq!(
        rebuilt,
        u64::try_from(expected_ids.len()).unwrap_or(u64::MAX)
    );
    reopened.validate_integrity()?;
    assert_eq!(frame_ids(&reopened.read_all_entries()?), baseline_ids);

    Ok(())
}

#[test]
fn migration_events_roundtrip_via_ledger() -> PiResult<()> {
    let dir = tempdir()?;
    let store = SessionStoreV2::create(dir.path(), 4 * 1024)?;

    let event = MigrationEvent {
        schema: "pi.session_store_v2.migration_event.v1".to_string(),
        migration_id: "00000000-0000-0000-0000-000000000001".to_string(),
        phase: "completed".to_string(),
        at: "2026-02-15T20:00:00Z".to_string(),
        source_path: "sessions/legacy.jsonl".to_string(),
        target_path: "sessions/legacy.v2/".to_string(),
        source_format: "jsonl_v3".to_string(),
        target_format: "native_v2".to_string(),
        verification: MigrationVerification {
            entry_count_match: true,
            hash_chain_match: true,
            index_consistent: true,
        },
        outcome: "ok".to_string(),
        error_class: None,
        correlation_id: "mig_20260215_200000".to_string(),
    };

    store.append_migration_event(event.clone())?;
    let events = store.read_migration_events()?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0], event);
    Ok(())
}

#[test]
fn rollback_to_checkpoint_truncates_tail_and_records_event() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 512)?;
    let all_ids = append_linear_entries(&mut store, 8)?;

    let checkpoint = store.create_checkpoint(1, "pre_migration")?;
    let mut parent = all_ids.last().cloned();
    for n in 9..=11 {
        let id = format!("entry_{n:08}");
        store.append_entry(
            id.clone(),
            parent.clone(),
            "message",
            json!({"kind":"message","ordinal":n}),
        )?;
        parent = Some(id);
    }

    let event = store.rollback_to_checkpoint(
        1,
        "00000000-0000-0000-0000-00000000000a",
        "rollback_20260215_204900",
    )?;
    assert_eq!(event.phase, "rollback");
    assert_eq!(event.outcome, "ok");
    assert!(event.verification.entry_count_match);
    assert!(event.verification.hash_chain_match);
    assert!(event.verification.index_consistent);
    assert_eq!(event.migration_id, "00000000-0000-0000-0000-00000000000a");

    let ids_after = frame_ids(&store.read_all_entries()?);
    assert_eq!(ids_after, all_ids);
    assert_eq!(store.entry_count(), checkpoint.head_entry_seq);
    assert_eq!(store.chain_hash(), checkpoint.chain_hash);
    store.validate_integrity()?;

    let ledger = store.read_migration_events()?;
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].phase, "rollback");
    assert_eq!(ledger[0].outcome, "ok");
    assert_eq!(ledger[0].correlation_id, "rollback_20260215_204900");
    Ok(())
}

#[test]
fn rollback_reconciles_manifest_and_quarantines_future_artifacts() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 512)?;
    append_linear_entries(&mut store, 4)?;
    let checkpoint = store.create_checkpoint(1, "pre_migration")?;
    store.write_manifest(TEST_MANIFEST_SESSION_ID, "native_v2")?;

    let mut parent = Some(checkpoint.head_entry_id.clone());
    for ordinal in 5..=8 {
        let id = format!("entry_{ordinal:08}");
        store.append_entry(
            id.clone(),
            parent,
            "message",
            json!({"kind":"message","ordinal":ordinal}),
        )?;
        parent = Some(id);
    }
    store.create_checkpoint(2, "periodic")?;
    store.write_manifest(TEST_MANIFEST_SESSION_ID, "native_v2")?;

    store.rollback_to_checkpoint(
        1,
        "00000000-0000-0000-0000-000000000021",
        "rollback_manifest_reconcile",
    )?;

    let manifest = store
        .validate_manifest_against_store()?
        .expect("rollback must preserve and reconcile the manifest");
    assert_eq!(manifest.session_id, TEST_MANIFEST_SESSION_ID);
    assert_eq!(manifest.counters.entries_total, checkpoint.head_entry_seq);
    assert_eq!(manifest.head.entry_id, checkpoint.head_entry_id);
    assert!(store.read_checkpoint(2)?.is_none());
    assert!(
        fs::read_dir(dir.path().join("checkpoints"))?.any(|entry| {
            entry.is_ok_and(|entry| entry.file_name().to_string_lossy().contains(".bak"))
        }),
        "future checkpoint must be retained under a quarantine name"
    );
    assert!(
        fs::symlink_metadata(dir.path().join("tmp/rollback.intent.json")).is_err(),
        "durable intent must be removed only after successful reconciliation"
    );
    Ok(())
}

#[test]
fn stale_store_instance_refreshes_after_locked_rollback_before_append() -> PiResult<()> {
    let dir = tempdir()?;
    let mut writer = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    writer.append_entry("entry_00000001", None, "message", json!({"ordinal":1}))?;
    writer.create_checkpoint(1, "manual")?;
    writer.append_entry(
        "entry_00000002",
        Some("entry_00000001".to_string()),
        "message",
        json!({"ordinal":2}),
    )?;
    let mut stale = SessionStoreV2::create(dir.path(), 4 * 1024)?;

    writer.rollback_to_checkpoint(
        1,
        "00000000-0000-0000-0000-000000000022",
        "rollback_two_instance_serialization",
    )?;
    stale.append_entry(
        "entry_00000003",
        Some("entry_00000001".to_string()),
        "message",
        json!({"ordinal":3}),
    )?;

    let reopened = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    assert_eq!(
        frame_ids(&reopened.read_all_entries()?),
        vec!["entry_00000001".to_string(), "entry_00000003".to_string()]
    );
    reopened.validate_integrity()?;
    Ok(())
}

#[test]
fn stale_store_instance_detects_same_size_rollback_replacement() -> PiResult<()> {
    let dir = tempdir()?;
    let mut writer = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    writer.append_entry("entry_00000001", None, "message", json!({"ordinal":1}))?;
    writer.create_checkpoint(1, "manual")?;
    writer.append_entry(
        "entry_00000002",
        Some("entry_00000001".to_string()),
        "message",
        json!({"ordinal":2}),
    )?;
    let original_index_len = fs::metadata(writer.index_file_path())?.len();
    let original_segment_len = fs::metadata(writer.segment_file_path(1))?.len();
    let mut stale = SessionStoreV2::create(dir.path(), 4 * 1024)?;

    writer.rollback_to_checkpoint(
        1,
        "00000000-0000-0000-0000-000000000023",
        "rollback_same_size_two_instance",
    )?;
    writer.append_entry(
        "other_00000002",
        Some("entry_00000001".to_string()),
        "message",
        json!({"ordinal":9}),
    )?;
    assert_eq!(
        fs::metadata(writer.index_file_path())?.len(),
        original_index_len
    );
    assert_eq!(
        fs::metadata(writer.segment_file_path(1))?.len(),
        original_segment_len
    );

    stale.append_entry(
        "entry_00000003",
        Some("other_00000002".to_string()),
        "message",
        json!({"ordinal":3}),
    )?;
    let reopened = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    assert_eq!(
        frame_ids(&reopened.read_all_entries()?),
        vec![
            "entry_00000001".to_string(),
            "other_00000002".to_string(),
            "entry_00000003".to_string(),
        ]
    );
    assert_eq!(stale.chain_hash(), reopened.chain_hash());
    reopened.validate_integrity()?;
    Ok(())
}

#[test]
fn rollback_missing_checkpoint_is_a_pure_preflight_rejection() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    append_linear_entries(&mut store, 3)?;

    let err = store
        .rollback_to_checkpoint(
            42,
            "00000000-0000-0000-0000-000000000042",
            "rollback_missing_checkpoint",
        )
        .expect_err("missing checkpoint should fail");
    let err_text = err.to_string();
    assert!(
        err_text.contains("checkpoint 42 not found"),
        "unexpected error: {err_text}"
    );

    assert!(
        store.read_migration_events()?.is_empty(),
        "preflight rejection must not mutate the migration ledger"
    );
    Ok(())
}

#[test]
fn rollback_with_tampered_checkpoint_is_rejected_before_mutation() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 512)?;
    append_linear_entries(&mut store, 6)?;
    store.create_checkpoint(1, "manual")?;

    let mut parent = Some("entry_00000006".to_string());
    for ordinal in 7..=9 {
        let id = format!("entry_{ordinal:08}");
        store.append_entry(
            id.clone(),
            parent.clone(),
            "message",
            json!({"kind":"message","ordinal":ordinal}),
        )?;
        parent = Some(id);
    }

    let checkpoint_path = dir.path().join("checkpoints").join("0000000000000001.json");
    let mut checkpoint_json: Value = serde_json::from_str(&fs::read_to_string(&checkpoint_path)?)?;
    checkpoint_json["chainHash"] = Value::String(
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_string(),
    );
    fs::write(
        &checkpoint_path,
        serde_json::to_vec_pretty(&checkpoint_json)?,
    )?;
    let index_path = dir.path().join("index/offsets.jsonl");
    let index_before = fs::read(&index_path)?;
    let checkpoint_before = fs::read(&checkpoint_path)?;
    let mut segments_before = Vec::new();
    for entry in fs::read_dir(dir.path().join("segments"))? {
        let path = entry?.path();
        segments_before.push((path.clone(), fs::read(path)?));
    }
    segments_before.sort_by(|left, right| left.0.cmp(&right.0));

    let err = store
        .rollback_to_checkpoint(
            1,
            "00000000-0000-0000-0000-000000000111",
            "rollback_tampered_checkpoint",
        )
        .expect_err("tampered checkpoint should fail preflight");
    assert!(
        err.to_string().contains("rollback preflight rejected"),
        "unexpected error: {err}"
    );
    assert_eq!(fs::read(&index_path)?, index_before);
    assert_eq!(fs::read(&checkpoint_path)?, checkpoint_before);
    for (path, bytes) in segments_before {
        assert_eq!(fs::read(path)?, bytes);
    }
    assert!(
        store.read_migration_events()?.is_empty(),
        "a rejected rollback must not mutate the migration ledger"
    );
    assert_eq!(store.read_all_entries()?.len(), 9);
    Ok(())
}

// ── Manifest tests ──────────────────────────────────────────────────────

#[test]
fn manifest_write_and_read_round_trip() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    append_linear_entries(&mut store, 5)?;

    let manifest = store.write_manifest(TEST_MANIFEST_SESSION_ID, "jsonl_v3")?;
    assert_eq!(manifest.store_version, 2);
    assert_eq!(manifest.session_id, TEST_MANIFEST_SESSION_ID);
    assert_eq!(manifest.source_format, "jsonl_v3");
    assert_eq!(manifest.counters.entries_total, 5);
    assert_eq!(manifest.head.entry_seq, 5);
    assert_eq!(manifest.head.entry_id, "entry_00000005");
    assert!(!manifest.integrity.chain_hash.is_empty());
    assert!(!manifest.integrity.manifest_hash.is_empty());

    let read_back = store.read_manifest()?.expect("manifest should exist");
    assert_eq!(read_back.session_id, manifest.session_id);
    assert_eq!(read_back.head.entry_seq, manifest.head.entry_seq);
    assert_eq!(
        read_back.integrity.chain_hash,
        manifest.integrity.chain_hash
    );

    Ok(())
}

#[test]
fn manifest_reader_rejects_wrong_schema_version_and_hash() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    append_linear_entries(&mut store, 2)?;
    let manifest = store.write_manifest(TEST_MANIFEST_SESSION_ID, "jsonl_v3")?;
    let manifest_path = dir.path().join("manifest.json");

    let mut wrong_schema = manifest.clone();
    wrong_schema.schema = "pi.session_store_v2.manifest.v999".to_string();
    write_rehashed_manifest(&manifest_path, wrong_schema)?;
    let error = store
        .read_manifest()
        .expect_err("an unsupported schema must be rejected before use");
    assert!(error.to_string().contains("unsupported manifest schema"));

    let mut wrong_version = manifest.clone();
    wrong_version.store_version = 99;
    write_rehashed_manifest(&manifest_path, wrong_version)?;
    let error = store
        .read_manifest()
        .expect_err("an unsupported store version must be rejected before use");
    assert!(
        error
            .to_string()
            .contains("unsupported manifest storeVersion")
    );

    let mut wrong_source_format = manifest.clone();
    wrong_source_format.source_format = "jsonl".to_string();
    write_rehashed_manifest(&manifest_path, wrong_source_format)?;
    let error = store
        .read_manifest()
        .expect_err("a source format outside the manifest contract must be rejected");
    assert!(
        error
            .to_string()
            .contains("unsupported manifest sourceFormat")
    );

    let mut wrong_index_path = manifest.clone();
    wrong_index_path.files.index_path = "elsewhere/offsets.jsonl".to_string();
    write_rehashed_manifest(&manifest_path, wrong_index_path)?;
    let error = store
        .read_manifest()
        .expect_err("a rehashed fixed-path mismatch must be rejected");
    assert!(error.to_string().contains("files.indexPath mismatch"));

    let mut unknown_field = serde_json::to_value(&manifest)?;
    unknown_field["unexpectedField"] = json!(true);
    fs::write(&manifest_path, serde_json::to_vec_pretty(&unknown_field)?)?;
    let error = store
        .read_manifest()
        .expect_err("unknown manifest fields must not bypass the self-hash contract");
    assert!(error.to_string().contains("unknown field"));

    let mut wrong_hash = manifest;
    wrong_hash.integrity.manifest_hash = "0".repeat(64);
    fs::write(&manifest_path, serde_json::to_vec_pretty(&wrong_hash)?)?;
    let error = store
        .read_manifest()
        .expect_err("a noncanonical manifest hash must be rejected before use");
    assert!(error.to_string().contains("manifest hash mismatch"));
    Ok(())
}

#[test]
fn manifest_reader_bounds_oversized_input_before_parsing() -> PiResult<()> {
    let dir = tempdir()?;
    let store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let manifest_path = dir.path().join("manifest.json");
    fs::write(&manifest_path, vec![b' '; 2 * 1024 * 1024])?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600))?;
    }

    let error = store
        .read_manifest()
        .expect_err("an oversized manifest must be rejected by the bounded reader");
    assert!(error.to_string().contains("byte read limit"));
    Ok(())
}

#[test]
fn manifest_store_validation_rejects_rehashed_forged_messages_total() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    append_linear_entries(&mut store, 3)?;
    let mut manifest = store.write_manifest(TEST_MANIFEST_SESSION_ID, "jsonl_v3")?;
    manifest.counters.messages_total = 9_999;
    write_rehashed_manifest(&dir.path().join("manifest.json"), manifest)?;

    assert!(
        store.read_manifest()?.is_some(),
        "the fixture must carry a valid recomputed manifest hash"
    );
    let error = store
        .validate_manifest_against_store()
        .expect_err("a rehashed but forged message counter must be rejected");
    assert!(error.to_string().contains("counters.messagesTotal"));
    Ok(())
}

#[test]
fn manifest_store_validation_rejects_rehashed_false_integrity_invariants() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    append_linear_entries(&mut store, 3)?;
    let manifest = store.write_manifest(TEST_MANIFEST_SESSION_ID, "jsonl_v3")?;
    let manifest_path = dir.path().join("manifest.json");

    let mut variants = Vec::new();
    let mut forged = manifest.clone();
    forged.invariants.parent_links_closed = false;
    variants.push(("parentLinksClosed", forged));
    let mut forged = manifest.clone();
    forged.invariants.monotonic_entry_seq = false;
    variants.push(("monotonicEntrySeq", forged));
    let mut forged = manifest.clone();
    forged.invariants.monotonic_segment_seq = false;
    variants.push(("monotonicSegmentSeq", forged));
    let mut forged = manifest.clone();
    forged.invariants.index_within_segment_bounds = false;
    variants.push(("indexWithinSegmentBounds", forged));
    let mut forged = manifest.clone();
    forged.invariants.branch_heads_indexed = false;
    variants.push(("branchHeadsIndexed", forged));
    let mut forged = manifest.clone();
    forged.invariants.checkpoints_monotonic = false;
    variants.push(("checkpointsMonotonic", forged));
    let mut forged = manifest;
    forged.invariants.hash_chain_valid = false;
    variants.push(("hashChainValid", forged));

    for (field, forged) in variants {
        write_rehashed_manifest(&manifest_path, forged)?;
        assert!(
            store.read_manifest()?.is_some(),
            "{field} fixture must carry a valid recomputed self-hash"
        );
        let error = store
            .validate_manifest_against_store()
            .expect_err("a false integrity invariant must not survive semantic validation");
        assert!(
            error.to_string().contains(field),
            "unexpected {field} validation error: {error}"
        );
    }
    Ok(())
}

#[test]
fn manifest_store_validation_recomputes_branch_and_checkpoint_claims() -> PiResult<()> {
    let dir = tempdir()?;
    let branch_root = dir.path().join("branch-coverage");
    let mut branch_store = SessionStoreV2::create(&branch_root, 4 * 1024)?;
    append_linear_entries(&mut branch_store, 2)?;
    branch_store.write_manifest(TEST_MANIFEST_SESSION_ID, "jsonl_v3")?;
    let segment_path = branch_store.segment_file_path(1);
    fs::OpenOptions::new()
        .append(true)
        .open(&segment_path)?
        .write_all(b"unindexed trailing bytes\n")?;
    let error = branch_store
        .validate_manifest_against_store()
        .expect_err("a true branchHeadsIndexed claim must not hide unindexed segment bytes");
    assert!(error.to_string().contains("segment byte coverage mismatch"));

    let checkpoint_root = dir.path().join("checkpoint-order");
    let mut checkpoint_store = SessionStoreV2::create(&checkpoint_root, 4 * 1024)?;
    append_linear_entries(&mut checkpoint_store, 2)?;
    checkpoint_store.create_checkpoint(1, "manual")?;
    checkpoint_store.write_manifest(TEST_MANIFEST_SESSION_ID, "jsonl_v3")?;
    let checkpoint_path = checkpoint_root.join("checkpoints/0000000000000001.json");
    let mut checkpoint: Value = serde_json::from_slice(&fs::read(&checkpoint_path)?)?;
    checkpoint["checkpointSeq"] = json!(2);
    fs::write(&checkpoint_path, serde_json::to_vec_pretty(&checkpoint)?)?;
    let error = checkpoint_store
        .validate_manifest_against_store()
        .expect_err("a true checkpointsMonotonic claim must be recomputed from checkpoint files");
    assert!(
        error
            .to_string()
            .contains("does not match requested sequence or filename"),
        "unexpected checkpoint identity error: {error}"
    );

    let regressing_root = dir.path().join("checkpoint-regression");
    let mut regressing_store = SessionStoreV2::create(&regressing_root, 4 * 1024)?;
    let first_ids = append_linear_entries(&mut regressing_store, 1)?;
    regressing_store.create_checkpoint(1, "manual")?;
    regressing_store.append_entry(
        "entry_00000002",
        Some(first_ids[0].clone()),
        "message",
        json!({"kind":"message","ordinal":2}),
    )?;
    regressing_store.create_checkpoint(2, "manual")?;
    regressing_store.write_manifest(TEST_MANIFEST_SESSION_ID, "jsonl_v3")?;
    let first_checkpoint_path = regressing_root.join("checkpoints/0000000000000001.json");
    let second_checkpoint_path = regressing_root.join("checkpoints/0000000000000002.json");
    let mut first_checkpoint: Value = serde_json::from_slice(&fs::read(&first_checkpoint_path)?)?;
    let second_checkpoint: Value = serde_json::from_slice(&fs::read(&second_checkpoint_path)?)?;
    first_checkpoint["headEntrySeq"] = json!(
        second_checkpoint["headEntrySeq"]
            .as_u64()
            .expect("checkpoint head sequence is an integer")
            + 1
    );
    fs::write(
        &first_checkpoint_path,
        serde_json::to_vec_pretty(&first_checkpoint)?,
    )?;
    let error = regressing_store
        .validate_manifest_against_store()
        .expect_err("checkpoint heads must not regress as checkpoint sequence increases");
    assert!(error.to_string().contains("head sequence regresses"));
    Ok(())
}

#[test]
fn manifest_absent_returns_none() -> PiResult<()> {
    let dir = tempdir()?;
    let store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    assert!(store.read_manifest()?.is_none());
    Ok(())
}

#[test]
fn manifest_on_empty_store_has_zero_counters() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let manifest = store.write_manifest(TEST_MANIFEST_SESSION_ID, "native_v2")?;
    assert_eq!(manifest.counters.entries_total, 0);
    assert_eq!(manifest.head.entry_seq, 0);
    assert_eq!(manifest.head.entry_id, "");
    Ok(())
}

// ── Hash chain tests ────────────────────────────────────────────────────

#[test]
fn chain_hash_is_deterministic_across_reopens() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    append_linear_entries(&mut store, 10)?;
    let chain_after_write = store.chain_hash().to_string();

    drop(store);
    let reopened = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    assert_eq!(
        reopened.chain_hash(),
        chain_after_write,
        "chain hash must be deterministic after reopen"
    );
    Ok(())
}

#[test]
fn chain_hash_changes_with_each_append() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;

    let genesis = store.chain_hash().to_string();
    store.append_entry("e1", None, "message", json!({"text":"a"}))?;
    let after_one = store.chain_hash().to_string();
    assert_ne!(genesis, after_one);

    store.append_entry("e2", Some("e1".into()), "message", json!({"text":"b"}))?;
    let after_two = store.chain_hash().to_string();
    assert_ne!(after_one, after_two);

    Ok(())
}

// ── Head and accessor tests ─────────────────────────────────────────────

#[test]
fn head_and_entry_count_track_appends() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;

    assert!(store.head().is_none());
    assert_eq!(store.entry_count(), 0);
    assert_eq!(store.total_bytes(), 0);

    store.append_entry("e1", None, "message", json!({"text":"a"}))?;
    let head = store.head().expect("head after one append");
    assert_eq!(head.entry_seq, 1);
    assert_eq!(head.entry_id, "e1");
    assert_eq!(store.entry_count(), 1);
    assert!(store.total_bytes() > 0);

    Ok(())
}

// ── Index summary tests ─────────────────────────────────────────────────

#[test]
fn index_summary_empty_store() -> PiResult<()> {
    let dir = tempdir()?;
    let store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    assert!(store.index_summary()?.is_none());
    Ok(())
}

#[test]
fn index_summary_populated_store() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    append_linear_entries(&mut store, 12)?;

    let summary = store.index_summary()?.expect("should have summary");
    assert_eq!(summary.entry_count, 12);
    assert_eq!(summary.first_entry_seq, 1);
    assert_eq!(summary.last_entry_seq, 12);
    assert_eq!(summary.last_entry_id, "entry_00000012");
    Ok(())
}

// ── V2 sidecar discovery tests ──────────────────────────────────────────

#[test]
fn v2_sidecar_path_derivation() {
    use std::path::PathBuf;

    let p = PathBuf::from("/home/user/sessions/my-session.jsonl");
    let sidecar = pi::session_store_v2::v2_sidecar_path(&p);
    assert_eq!(sidecar, PathBuf::from("/home/user/sessions/my-session.v2"));

    let p2 = PathBuf::from("relative/path.jsonl");
    let sidecar2 = pi::session_store_v2::v2_sidecar_path(&p2);
    assert_eq!(sidecar2, PathBuf::from("relative/path.v2"));
}

#[test]
fn has_v2_sidecar_detection() -> PiResult<()> {
    let dir = tempdir()?;
    let jsonl_path = dir.path().join("test-session.jsonl");
    fs::write(&jsonl_path, "{}\n")?;

    assert!(!pi::session_store_v2::has_v2_sidecar(&jsonl_path));

    let sidecar_root = pi::session_store_v2::v2_sidecar_path(&jsonl_path);
    let mut store = SessionStoreV2::create(&sidecar_root, 4 * 1024)?;
    store.append_entry("e1", None, "message", json!({"text":"a"}))?;

    assert!(pi::session_store_v2::has_v2_sidecar(&jsonl_path));
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

    #[test]
    fn proptest_v2_sidecar_path_preserves_parent_and_stem(
        parent_parts in prop::collection::vec("[A-Za-z0-9_-]{1,12}", 1..4),
        stem in "[A-Za-z0-9_-]{1,24}",
        ext in prop_oneof![Just(String::new()), "[A-Za-z0-9_-]{1,8}".prop_map(|s| format!(".{s}"))],
    ) {
        let mut jsonl = Path::new("/tmp").to_path_buf();
        for part in parent_parts {
            jsonl.push(part);
        }
        jsonl.push(format!("{stem}{ext}"));

        let sidecar = pi::session_store_v2::v2_sidecar_path(&jsonl);
        let expected_name = format!("{stem}.v2");
        prop_assert_eq!(sidecar.parent(), jsonl.parent());
        prop_assert_eq!(
            sidecar.file_name().and_then(|name| name.to_str()),
            Some(expected_name.as_str())
        );
        prop_assert_eq!(pi::session_store_v2::v2_sidecar_path(&jsonl), sidecar);
    }

    #[test]
    fn proptest_v2_sidecar_path_is_extension_agnostic(
        parent_parts in prop::collection::vec("[A-Za-z0-9_-]{1,12}", 1..4),
        stem in "[A-Za-z0-9_-]{1,24}",
        ext1 in prop_oneof![Just(String::new()), "[A-Za-z0-9_-]{1,8}".prop_map(|s| format!(".{s}"))],
        ext2 in prop_oneof![Just(String::new()), "[A-Za-z0-9_-]{1,8}".prop_map(|s| format!(".{s}"))],
    ) {
        let mut base = Path::new("/tmp").to_path_buf();
        for part in parent_parts {
            base.push(part);
        }

        let path_a = base.join(format!("{stem}{ext1}"));
        let path_b = base.join(format!("{stem}{ext2}"));
        prop_assert_eq!(
            pi::session_store_v2::v2_sidecar_path(&path_a),
            pi::session_store_v2::v2_sidecar_path(&path_b)
        );
    }

    #[test]
    fn proptest_has_v2_sidecar_matches_manifest_or_index_invariant(
        create_manifest in any::<bool>(),
        create_index in any::<bool>(),
    ) {
        let dir = tempdir().expect("tempdir");
        let jsonl = dir.path().join("session.jsonl");
        fs::write(&jsonl, "{}\n").expect("write jsonl");

        let sidecar_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
        if create_manifest {
            fs::create_dir_all(&sidecar_root).expect("create sidecar root");
            fs::write(sidecar_root.join("manifest.json"), "{}\n").expect("write manifest");
        }
        if create_index {
            let index_dir = sidecar_root.join("index");
            fs::create_dir_all(&index_dir).expect("create index dir");
            fs::write(index_dir.join("offsets.jsonl"), "{}\n").expect("write offsets");
        }

        prop_assert_eq!(
            pi::session_store_v2::has_v2_sidecar(&jsonl),
            create_manifest || create_index
        );
    }

    #[test]
    fn proptest_linear_appends_keep_index_and_head_consistent(
        count in 1usize..64,
        threshold in 512_u64..4096_u64,
    ) {
        let dir = tempdir().expect("tempdir");
        let mut store = SessionStoreV2::create(dir.path(), threshold).expect("create store");
        let ids = append_linear_entries(&mut store, count).expect("append entries");
        let index = store.read_index().expect("read index");

        prop_assert_eq!(index.len(), count);
        for (offset, row) in index.iter().enumerate() {
            let expected_seq = u64::try_from(offset + 1).expect("sequence fits in u64");
            prop_assert_eq!(row.entry_seq, expected_seq);
            prop_assert_eq!(row.entry_id.as_str(), ids[offset].as_str());
        }

        let expected_count = u64::try_from(count).expect("count fits in u64");
        let head = store.head().expect("head");
        prop_assert_eq!(head.entry_seq, expected_count);
        prop_assert_eq!(head.entry_id.as_str(), ids[count - 1].as_str());
        store.validate_integrity().expect("integrity");
    }

    #[test]
    fn proptest_reopen_preserves_chain_hash_and_ids(
        count in 1usize..48,
        threshold in 512_u64..4096_u64,
    ) {
        let dir = tempdir().expect("tempdir");

        let (expected_ids, expected_chain_hash) = {
            let mut store = SessionStoreV2::create(dir.path(), threshold).expect("create store");
            let ids = append_linear_entries(&mut store, count).expect("append entries");
            store.validate_integrity().expect("integrity");
            (ids, store.chain_hash().to_string())
        };

        let reopened = SessionStoreV2::create(dir.path(), threshold).expect("reopen store");
        prop_assert_eq!(reopened.chain_hash(), expected_chain_hash.as_str());
        prop_assert_eq!(
            frame_ids(&reopened.read_all_entries().expect("read all entries")),
            expected_ids
        );
    }
}

// ── Rebuild index from scratch ──────────────────────────────────────────

#[test]
fn rebuild_index_from_missing_index_file() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;
    let ids = append_linear_entries(&mut store, 8)?;
    let chain_before = store.chain_hash().to_string();

    let index_path = store.index_file_path();
    fs::remove_file(&index_path)?;

    let rebuilt = store.rebuild_index()?;
    assert_eq!(rebuilt, 8);
    assert_eq!(store.chain_hash(), chain_before);
    store.validate_integrity()?;
    assert_eq!(frame_ids(&store.read_all_entries()?), ids);
    Ok(())
}

// ── Multi-segment stress ────────────────────────────────────────────────

#[test]
fn many_segments_with_small_threshold() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 512)?;
    let ids = append_linear_entries(&mut store, 50)?;

    let index = store.read_index()?;
    assert_eq!(index.len(), 50);

    let max_seg = index.iter().map(|r| r.segment_seq).max().unwrap_or(0);
    assert!(
        max_seg >= 10,
        "50 entries with 512-byte threshold should produce many segments, got {max_seg}"
    );

    store.validate_integrity()?;
    assert_eq!(frame_ids(&store.read_all_entries()?), ids);
    Ok(())
}

// ── Rewrite amplification measurement ───────────────────────────────────

#[test]
fn v2_append_has_no_rewrite_amplification() -> PiResult<()> {
    let dir = tempdir()?;
    let mut store = SessionStoreV2::create(dir.path(), 4 * 1024)?;

    let mut cumulative_disk_bytes = Vec::new();
    for i in 1..=20 {
        let parent = if i == 1 {
            None
        } else {
            Some(format!("e{}", i - 1))
        };
        store.append_entry(
            format!("e{i}"),
            parent,
            "message",
            json!({"idx": i, "data": "x".repeat(50)}),
        )?;

        let seg_bytes: u64 = (1..=store.head().map_or(1, |h| h.segment_seq))
            .filter_map(|s| {
                let p = store.segment_file_path(s);
                fs::metadata(&p).ok().map(|m| m.len())
            })
            .sum();
        let idx_bytes = fs::metadata(store.index_file_path()).map_or(0, |m| m.len());
        cumulative_disk_bytes.push(seg_bytes + idx_bytes);
    }

    // V2 property: each append adds roughly constant bytes (no full rewrite).
    for window in cumulative_disk_bytes.windows(2) {
        let growth = window[1] - window[0];
        assert!(
            growth < 1024,
            "append growth {growth} bytes is too large; suggests rewrite amplification"
        );
    }

    Ok(())
}

// ─── V2 Resume Integration Tests ─────────────────────────────────────────────

/// Build a minimal JSONL session file with the given entries.
fn build_test_jsonl(dir: &Path, entries: &[pi::session::SessionEntry]) -> std::path::PathBuf {
    use std::io::Write;

    let path = dir.join("test_session.jsonl");
    let mut file = fs::File::create(&path).unwrap();

    // Write header (first line).
    let header = pi::session::SessionHeader::new();
    serde_json::to_writer(&mut file, &header).unwrap();
    file.write_all(b"\n").unwrap();

    // Write entries.
    for entry in entries {
        serde_json::to_writer(&mut file, entry).unwrap();
        file.write_all(b"\n").unwrap();
    }
    file.flush().unwrap();
    path
}

fn make_message_entry(id: &str, parent_id: Option<&str>, text: &str) -> pi::session::SessionEntry {
    pi::session::SessionEntry::Message(pi::session::MessageEntry {
        base: pi::session::EntryBase::new(parent_id.map(String::from), id.to_string()),
        message: pi::session::SessionMessage::User {
            content: pi::model::UserContent::Text(text.to_string()),
            timestamp: None,
        },
    })
}

fn make_user_session_message(
    text: impl Into<String>,
    timestamp: i64,
) -> pi::session::SessionMessage {
    pi::session::SessionMessage::User {
        content: pi::model::UserContent::Text(text.into()),
        timestamp: Some(timestamp),
    }
}

fn run_async<T, Fut>(future: Fut) -> T
where
    Fut: Future<Output = T>,
{
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build asupersync runtime");
    runtime.block_on(future)
}

fn elapsed_test_us(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn test_timing_start() -> Instant {
    let start: fn() -> Instant = Instant::now;
    start()
}

fn usize_to_u64(value: usize, label: &str) -> PiResult<u64> {
    u64::try_from(value).map_err(|_| pi::Error::session(format!("{label} does not fit in u64")))
}

fn build_large_history_entries(count: usize, payload_bytes: usize) -> Vec<SessionEntry> {
    let mut entries = Vec::with_capacity(count);
    let mut parent_id: Option<String> = None;
    for idx in 0..count {
        let id = format!("stress-{idx:06}");
        let text = format!(
            "large-session-store-v2 recovery stress entry {idx:06}: {}",
            "x".repeat(payload_bytes)
        );
        entries.push(make_message_entry(&id, parent_id.as_deref(), &text));
        parent_id = Some(id);
    }
    entries
}

fn session_entry_ids(session: &Session) -> Vec<String> {
    session
        .entries
        .iter()
        .filter_map(|entry| entry.base_id().cloned())
        .collect()
}

fn append_jsonl_entry(path: &Path, entry: &SessionEntry) -> PiResult<()> {
    let mut file = fs::OpenOptions::new().append(true).open(path)?;
    serde_json::to_writer(&mut file, entry)?;
    file.write_all(b"\n")?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn emit_session_store_v2_recovery_evidence(report: &Value) -> PiResult<std::path::PathBuf> {
    let evidence_dir = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || std::env::temp_dir().join("pi_agent_rust_perf"),
        |target_dir| std::path::PathBuf::from(target_dir).join("perf"),
    );
    fs::create_dir_all(&evidence_dir)?;

    let path = evidence_dir.join("session_store_v2_recovery_swarm.jsonl");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    serde_json::to_writer(&mut file, report)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(path)
}

fn index_concurrent_session_snapshots(root: &Path) -> PiResult<usize> {
    let index = pi::session_index::SessionIndex::for_sessions_root(root);
    let writer_threads = 4usize;
    let sessions_per_thread = 8usize;
    let mut handles = Vec::with_capacity(writer_threads);

    for worker in 0..writer_threads {
        let index = index.clone();
        let root = root.to_path_buf();
        handles.push(std::thread::spawn(move || -> PiResult<()> {
            let worker_dir = root.join(format!("worker-{worker:02}"));
            fs::create_dir_all(&worker_dir)?;

            for item in 0..sessions_per_thread {
                let mut header = SessionHeader::new();
                header.id = format!("swarm-index-{worker:02}-{item:02}");
                header.timestamp = format!("2026-05-11T00:00:{item:02}.000Z");
                header.cwd = root.display().to_string();

                let path = worker_dir.join(format!("session-{item:02}.jsonl"));
                let mut file = fs::File::create(&path)?;
                serde_json::to_writer(&mut file, &header)?;
                file.write_all(b"\n")?;
                file.flush()?;
                file.sync_all()?;

                index.index_session_snapshot(
                    &path,
                    &header,
                    1,
                    Some(format!("worker {worker} session {item}")),
                )?;

                if item % 2 == 0 {
                    let listed = index.list_sessions(None)?;
                    let expected_id = header.id.as_str();
                    assert!(
                        listed.iter().any(|meta| meta.id.as_str().eq(expected_id)),
                        "concurrent SessionIndex read missed {} in root {}",
                        header.id,
                        root.display()
                    );
                }
            }
            Ok(())
        }));
    }

    for handle in handles {
        handle
            .join()
            .map_err(|_| pi::Error::session("SessionIndex stress worker panicked"))??;
    }

    let listed = index.list_sessions(None)?;
    let expected = writer_threads * sessions_per_thread;
    assert!(
        listed.len() >= expected,
        "SessionIndex only listed {} rows after concurrent writes; expected at least {expected}; root={}",
        listed.len(),
        root.display()
    );
    Ok(listed.len())
}

fn concurrent_save_resume_index_chaos(root: &Path) -> PiResult<usize> {
    const WORKERS: usize = 4;
    const MESSAGES_PER_SESSION: usize = 6;

    fs::create_dir_all(root)?;
    let index = pi::session_index::SessionIndex::for_sessions_root(root);
    let start = Arc::new(Barrier::new(WORKERS));
    let mut handles = Vec::with_capacity(WORKERS);

    for worker in 0..WORKERS {
        let index = index.clone();
        let root = root.to_path_buf();
        let start = Arc::clone(&start);
        handles.push(std::thread::spawn(move || -> PiResult<()> {
            start.wait();
            let mut session = Session::create_with_dir(Some(root.clone()));
            for item in 0..MESSAGES_PER_SESSION {
                session.append_message(make_user_session_message(
                    format!("chaos-worker-{worker:02}-message-{item:02}"),
                    i64::try_from(item).unwrap_or(i64::MAX),
                ));
            }

            run_async(async { session.save().await })?;
            let path = session
                .path
                .clone()
                .ok_or_else(|| pi::Error::session("chaos worker saved session without path"))?;
            let path_string = path.display().to_string();

            pi::session::create_v2_sidecar_from_jsonl(&path)?;
            let resumed = run_async(async { Session::open(&path_string).await })?;
            assert_eq!(
                resumed.entries.len(),
                MESSAGES_PER_SESSION,
                "resumed chaos worker session lost entries; worker={worker}; path={}",
                path.display()
            );
            assert_eq!(
                resumed.leaf_id(),
                session.leaf_id(),
                "resumed chaos worker session changed leaf; worker={worker}; path={}",
                path.display()
            );

            index.index_session(&resumed)?;
            let listed = index.list_sessions(None)?;
            assert!(
                listed
                    .iter()
                    .any(|meta| meta.path.as_str().eq(path_string.as_str())),
                "concurrent SessionIndex reader missed worker={worker} path={path_string}"
            );
            Ok(())
        }));
    }

    for handle in handles {
        handle
            .join()
            .map_err(|_| pi::Error::session("concurrent save/resume worker panicked"))??;
    }

    let refresh = index.refresh_incremental()?;
    let listed = index.list_sessions(None)?;
    assert!(
        listed.len() >= WORKERS,
        "stale SessionIndex refresh lost worker rows; listed={} expected_at_least={WORKERS} refreshed={} reused={} root={}",
        listed.len(),
        refresh.refreshed_files,
        refresh.reused_files,
        root.display()
    );
    Ok(listed.len())
}

fn assert_crash_resilient_session_save(root: &Path) -> PiResult<std::path::PathBuf> {
    let mut session = Session::create_with_dir(Some(root.to_path_buf()));
    for idx in 0..32 {
        session.append_message(make_user_session_message(
            format!("crash-resilient-save-{idx:02}: {}", "s".repeat(128)),
            i64::from(idx),
        ));
    }

    run_async(async { session.save().await })?;
    let path = session
        .path
        .clone()
        .ok_or_else(|| pi::Error::session("saved session did not record a path"))?;
    let path_string = path.display().to_string();
    let (reopened, diagnostics) =
        run_async(async { Session::open_with_diagnostics(&path_string).await })?;
    assert_eq!(
        reopened.entries.len(),
        32,
        "saved session reopened with wrong entry count; path={}",
        path.display()
    );
    assert!(
        diagnostics.skipped_entries.is_empty(),
        "save round-trip produced skipped entries at {}: {:?}",
        path.display(),
        diagnostics.skipped_entries
    );

    let mut leftover_tmp = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if file_name.contains(".tmp") || file_name.starts_with("tmp") {
            leftover_tmp.insert(leftover_tmp.len(), file_name);
        }
    }
    assert!(
        leftover_tmp.is_empty(),
        "atomic session save left temp artifacts in {}: {:?}",
        root.display(),
        leftover_tmp
    );

    Ok(path)
}

struct LargeMigrationOutcome {
    v2_root: PathBuf,
    index_path: PathBuf,
    checkpoint_head_entry_seq: u64,
    checkpoint_head_entry_id: String,
    migration_elapsed_us: u64,
    checkpoint_rebuild_elapsed_us: u64,
}

struct TruncatedRecoveryOutcome {
    segment_path: PathBuf,
    recovered_count: u64,
    elapsed_us: u64,
}

struct StaleFallbackOutcome {
    opened_backend: String,
    sidecar_present: bool,
    sidecar_stale: bool,
    total_entries: usize,
    elapsed_us: u64,
}

struct JsonlResumeBaseline {
    jsonl: PathBuf,
    jsonl_path: String,
    ids: Vec<String>,
    leaf_id: Option<String>,
    opened_backend: String,
}

struct V2ResumeParity {
    v2_root: PathBuf,
    store: SessionStoreV2,
    opened_backend: String,
}

struct CorruptSidecarRepair {
    opened_backend: String,
    selected_backend: String,
}

fn migrate_large_history_and_rebuild_checkpoint(
    jsonl: &Path,
    base_entries: usize,
    max_segment_bytes: u64,
) -> PiResult<LargeMigrationOutcome> {
    let migration_start = test_timing_start();
    let dry_run = pi::session::migrate_dry_run(jsonl)?;
    assert!(dry_run.entry_count_match);
    assert!(dry_run.hash_chain_match);
    assert!(dry_run.index_consistent);

    let migration = pi::session::migrate_jsonl_to_v2(jsonl, "bd-07cku.6-large-stress")?;
    assert_eq!(migration.outcome, "ok");
    assert!(migration.verification.entry_count_match);
    let migration_elapsed_us = elapsed_test_us(migration_start);

    let v2_root = pi::session_store_v2::v2_sidecar_path(jsonl);
    let mut store = SessionStoreV2::create(&v2_root, max_segment_bytes)?;
    let base_entries_u64 = usize_to_u64(base_entries, "base_entries")?;
    assert_eq!(store.entry_count(), base_entries_u64);
    store.validate_integrity()?;

    let checkpoint_start = test_timing_start();
    let checkpoint = store.create_checkpoint(1, "manual")?;
    assert_eq!(checkpoint.head_entry_seq, base_entries_u64);

    let index_path = v2_root.join("index").join("offsets.jsonl");
    fs::write(&index_path, "{not-valid-offset-index-json}\n")?;
    let rebuilt = SessionStoreV2::create(&v2_root, max_segment_bytes)?;
    rebuilt.validate_integrity()?;
    assert_eq!(
        rebuilt.entry_count(),
        base_entries_u64,
        "checkpoint/index rebuild lost entries; v2_root={}",
        v2_root.display()
    );
    let rebuilt_checkpoint = rebuilt
        .read_checkpoint(1)?
        .ok_or_else(|| pi::Error::session("checkpoint missing after index rebuild"))?;
    assert_eq!(rebuilt_checkpoint.head_entry_id, checkpoint.head_entry_id);

    Ok(LargeMigrationOutcome {
        v2_root,
        index_path,
        checkpoint_head_entry_seq: checkpoint.head_entry_seq,
        checkpoint_head_entry_id: checkpoint.head_entry_id,
        migration_elapsed_us,
        checkpoint_rebuild_elapsed_us: elapsed_test_us(checkpoint_start),
    })
}

fn append_tail_and_recover_truncated_frame(
    v2_root: &Path,
    base_entries: usize,
    tail_entries: usize,
    max_segment_bytes: u64,
) -> PiResult<TruncatedRecoveryOutcome> {
    let mut store = SessionStoreV2::create(v2_root, max_segment_bytes)?;
    let mut parent_id = store.head().map(|head| head.entry_id);
    for idx in 0..tail_entries {
        let id = format!("stress-tail-{idx:04}");
        store.append_entry(
            id.clone(),
            parent_id.clone(),
            "message",
            json!({
                "kind": "message",
                "ordinal": base_entries + idx,
                "body": "tail-frame".repeat(64),
            }),
        )?;
        parent_id = Some(id);
    }

    let expected_after_tail = base_entries + tail_entries;
    assert_eq!(
        store.entry_count(),
        usize_to_u64(expected_after_tail, "expected_after_tail")?
    );

    let segment_path = v2_root.join("segments").join("0000000000000001.seg");
    let segment_len = fs::metadata(&segment_path)?.len();
    assert!(
        segment_len > 64,
        "segment too small to truncate meaningfully; segment={}",
        segment_path.display()
    );
    fs::OpenOptions::new()
        .write(true)
        .open(&segment_path)?
        .set_len(segment_len - 32)?;

    let recovery_start = test_timing_start();
    let recovered = SessionStoreV2::create(v2_root, max_segment_bytes)?;
    recovered.validate_integrity()?;
    let recovered_count = recovered.entry_count();
    assert_eq!(
        recovered_count,
        usize_to_u64(expected_after_tail - 1, "expected_after_tail minus one")?,
        "truncated trailing frame recovery should drop exactly the partial final frame; segment={}",
        segment_path.display()
    );
    let recovered_tail = recovered.read_tail_entries(2)?;
    assert_eq!(
        recovered_tail.last().map(|frame| frame.entry_id.as_str()),
        Some("stress-tail-0014")
    );

    Ok(TruncatedRecoveryOutcome {
        segment_path,
        recovered_count,
        elapsed_us: elapsed_test_us(recovery_start),
    })
}

fn assert_stale_sidecar_fallback(
    jsonl: &Path,
    jsonl_root: &Path,
    v2_root: &Path,
    base_entries: usize,
) -> PiResult<StaleFallbackOutcome> {
    std::thread::sleep(Duration::from_millis(20));
    let stale_entry = make_message_entry(
        "stale-jsonl-newer-than-sidecar",
        Some(&format!("stress-{:06}", base_entries - 1)),
        "JSONL source advanced after V2 sidecar migration",
    );
    append_jsonl_entry(jsonl, &stale_entry)?;

    let stale_start = test_timing_start();
    let stale_trace =
        run_async(async { Session::cold_start_trace_bundle(jsonl, jsonl_root).await })?;
    assert!(stale_trace.storage.v2_sidecar_present);
    assert!(
        stale_trace.storage.v2_sidecar_stale,
        "expected stale V2 sidecar fallback; jsonl={} v2_root={}",
        jsonl.display(),
        v2_root.display()
    );
    assert_eq!(stale_trace.storage.opened_backend, "jsonl");
    assert_eq!(stale_trace.input.total_entries, base_entries + 1);

    Ok(StaleFallbackOutcome {
        opened_backend: stale_trace.storage.opened_backend,
        sidecar_present: stale_trace.storage.v2_sidecar_present,
        sidecar_stale: stale_trace.storage.v2_sidecar_stale,
        total_entries: stale_trace.input.total_entries,
        elapsed_us: elapsed_test_us(stale_start),
    })
}

fn assert_jsonl_v2_resume_parity_and_corrupt_sidecar_repair(jsonl_root: &Path) -> PiResult<Value> {
    let baseline = build_jsonl_resume_baseline(jsonl_root)?;
    let sidecar = assert_v2_resume_parity(jsonl_root, &baseline)?;
    let (index_path, segment_path) =
        assert_recoverable_v2_index_rebuild(&sidecar.store, &sidecar.v2_root, &baseline.ids)?;
    corrupt_sidecar_segment(&segment_path)?;
    let repair = assert_corrupt_sidecar_verified_repair(jsonl_root, &baseline, &segment_path)?;

    Ok(json!({
        "jsonl_path": baseline.jsonl.display().to_string(),
        "v2_root": sidecar.v2_root.display().to_string(),
        "rebuilt_index": index_path.display().to_string(),
        "corrupt_segment": segment_path.display().to_string(),
        "entry_count": baseline.ids.len(),
        "baseline_backend": baseline.opened_backend,
        "sidecar_backend": sidecar.opened_backend,
        "repair_backend": repair.opened_backend,
        "sidecar_selected_before_repair": repair.selected_backend,
    }))
}

fn build_jsonl_resume_baseline(jsonl_root: &Path) -> PiResult<JsonlResumeBaseline> {
    const ENTRIES: usize = 32;
    const PAYLOAD_BYTES: usize = 96;

    fs::create_dir_all(jsonl_root)?;
    let entries = build_large_history_entries(ENTRIES, PAYLOAD_BYTES);
    let jsonl = build_test_jsonl(jsonl_root, &entries);
    let jsonl_path = jsonl.display().to_string();
    let jsonl_trace =
        run_async(async { Session::cold_start_trace_bundle(&jsonl, jsonl_root).await })?;
    assert_eq!(
        jsonl_trace.storage.opened_backend,
        "jsonl",
        "baseline JSONL open used unexpected backend; jsonl={}",
        jsonl.display()
    );

    let (jsonl_session, jsonl_diag) =
        run_async(async { Session::open_with_diagnostics(&jsonl_path).await })?;
    assert!(
        jsonl_diag.skipped_entries.is_empty(),
        "baseline JSONL open skipped entries: {:?}",
        jsonl_diag.skipped_entries
    );

    Ok(JsonlResumeBaseline {
        jsonl,
        jsonl_path,
        ids: session_entry_ids(&jsonl_session),
        leaf_id: jsonl_session.leaf_id().map(str::to_string),
        opened_backend: jsonl_trace.storage.opened_backend,
    })
}

fn assert_v2_resume_parity(
    jsonl_root: &Path,
    baseline: &JsonlResumeBaseline,
) -> PiResult<V2ResumeParity> {
    let store = pi::session::create_v2_sidecar_from_jsonl(&baseline.jsonl)?;
    store.validate_integrity()?;
    let v2_root = pi::session_store_v2::v2_sidecar_path(&baseline.jsonl);
    let v2_trace =
        run_async(async { Session::cold_start_trace_bundle(&baseline.jsonl, jsonl_root).await })?;
    assert_eq!(
        v2_trace.storage.selected_backend,
        "v2_sidecar",
        "V2 sidecar was not selected after migration; jsonl={}",
        baseline.jsonl.display()
    );
    assert_eq!(
        v2_trace.storage.opened_backend,
        "v2_sidecar",
        "V2 sidecar did not open cleanly before corruption; jsonl={}",
        baseline.jsonl.display()
    );

    let (v2_session, v2_diag) =
        run_async(async { Session::open_with_diagnostics(&baseline.jsonl_path).await })?;
    assert!(
        v2_diag.skipped_entries.is_empty(),
        "V2 sidecar open skipped entries: {:?}",
        v2_diag.skipped_entries
    );
    assert_eq!(
        session_entry_ids(&v2_session),
        baseline.ids,
        "V2 resume entry IDs diverged from JSONL baseline; jsonl={}",
        baseline.jsonl.display()
    );
    assert_eq!(
        v2_session.leaf_id(),
        baseline.leaf_id.as_deref(),
        "V2 resume leaf diverged from JSONL baseline; jsonl={}",
        baseline.jsonl.display()
    );

    Ok(V2ResumeParity {
        v2_root,
        store,
        opened_backend: v2_trace.storage.opened_backend,
    })
}

fn assert_recoverable_v2_index_rebuild(
    store: &SessionStoreV2,
    v2_root: &Path,
    jsonl_ids: &[String],
) -> PiResult<(PathBuf, PathBuf)> {
    const MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

    let index_path = store.index_file_path();
    fs::write(&index_path, "{not-valid-offset-index-json}\n")?;
    let rebuilt = SessionStoreV2::create(v2_root, MAX_SEGMENT_BYTES)?;
    rebuilt.validate_integrity()?;
    assert_eq!(
        frame_ids(&rebuilt.read_all_entries()?),
        jsonl_ids,
        "recoverable V2 index rebuild changed entry IDs; index={}",
        index_path.display()
    );

    Ok((index_path, rebuilt.segment_file_path(1)))
}

fn corrupt_sidecar_segment(segment_path: &Path) -> PiResult<()> {
    let segment_text = fs::read_to_string(segment_path)?;
    let mut lines: Vec<String> = segment_text.lines().map(ToString::to_string).collect();
    assert!(
        lines.len() >= 2,
        "sidecar segment needs multiple frames for corrupt fallback test; segment={}",
        segment_path.display()
    );
    let frame = lines.get_mut(1).ok_or_else(|| {
        pi::Error::session("sidecar segment frame disappeared after length check")
    })?;
    *frame = "{malformed-sidecar-frame".to_string();
    fs::write(segment_path, format!("{}\n", lines.join("\n")))?;
    Ok(())
}

fn assert_corrupt_sidecar_verified_repair(
    jsonl_root: &Path,
    baseline: &JsonlResumeBaseline,
    segment_path: &Path,
) -> PiResult<CorruptSidecarRepair> {
    let repair_trace =
        run_async(async { Session::cold_start_trace_bundle(&baseline.jsonl, jsonl_root).await })?;
    assert_eq!(
        repair_trace.storage.selected_backend,
        "v2_sidecar",
        "corrupt sidecar fixture should first select V2; jsonl={}",
        baseline.jsonl.display()
    );
    assert_eq!(
        repair_trace.storage.opened_backend,
        "v2_sidecar",
        "corrupt V2 sidecar was not rebuilt and reopened after verification; segment={}",
        segment_path.display()
    );
    let repaired_session = run_async(async { Session::open(&baseline.jsonl_path).await })?;
    assert_eq!(
        session_entry_ids(&repaired_session),
        baseline.ids,
        "verified V2 repair changed authoritative JSONL entry IDs; segment={}",
        segment_path.display()
    );
    assert_eq!(
        pi::session::migration_status(&baseline.jsonl),
        MigrationState::Migrated,
        "verified V2 repair did not leave a healthy migrated store"
    );

    Ok(CorruptSidecarRepair {
        opened_backend: repair_trace.storage.opened_backend,
        selected_backend: repair_trace.storage.selected_backend,
    })
}

#[test]
fn v2_sidecar_path_derives_from_jsonl_stem() {
    let jsonl = Path::new("/tmp/sessions/my_session.jsonl");
    let sidecar = pi::session_store_v2::v2_sidecar_path(jsonl);
    assert_eq!(sidecar, Path::new("/tmp/sessions/my_session.v2"));
}

#[test]
fn has_v2_sidecar_returns_false_for_bare_jsonl() {
    let dir = tempdir().unwrap();
    let jsonl = dir.path().join("session.jsonl");
    fs::write(&jsonl, "{}").unwrap();
    assert!(!pi::session_store_v2::has_v2_sidecar(&jsonl));
}

#[test]
fn create_v2_sidecar_round_trips_entries() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("e1", None, "hello"),
        make_message_entry("e2", Some("e1"), "world"),
        make_message_entry("e3", Some("e2"), "foo"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    // Create sidecar.
    let store = pi::session::create_v2_sidecar_from_jsonl(&jsonl)?;

    // Verify sidecar was created.
    assert!(pi::session_store_v2::has_v2_sidecar(&jsonl));

    // Verify entry count.
    assert_eq!(store.entry_count(), 3);

    // Verify round-trip: read back frames and convert to entries.
    let frames = store.read_all_entries()?;
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].entry_id, "e1");
    assert_eq!(frames[1].entry_id, "e2");
    assert_eq!(frames[2].entry_id, "e3");
    assert_eq!(frames[1].parent_entry_id.as_deref(), Some("e1"));

    // Convert back to SessionEntry and verify content.
    for (frame, original) in frames.iter().zip(entries.iter()) {
        let recovered = pi::session_store_v2::frame_to_session_entry(frame)?;
        let recovered_id = recovered.base_id().unwrap();
        let original_id = original.base_id().unwrap();
        assert_eq!(recovered_id, original_id);
    }

    Ok(())
}

#[test]
fn v2_resume_loads_same_entries_as_jsonl() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("msg1", None, "first message"),
        make_message_entry("msg2", Some("msg1"), "second message"),
        make_message_entry("msg3", Some("msg2"), "third message"),
        make_message_entry("msg4", Some("msg3"), "fourth message"),
        make_message_entry("msg5", Some("msg4"), "fifth message"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    // Create V2 sidecar.
    pi::session::create_v2_sidecar_from_jsonl(&jsonl)?;

    // Open via Session (will use V2 sidecar if detected) and assert inside
    // runtime harness, since run_test futures return ().
    let jsonl_str = jsonl
        .to_str()
        .expect("temporary jsonl path must be valid UTF-8")
        .to_string();
    asupersync::test_utils::run_test(|| async move {
        let (session, diag) = pi::session::Session::open_with_diagnostics(&jsonl_str)
            .await
            .expect("session open should succeed");

        assert_eq!(session.entries.len(), 5);
        assert!(diag.skipped_entries.is_empty());

        let ids: Vec<String> = session
            .entries
            .iter()
            .filter_map(|e| e.base_id().cloned())
            .collect();
        assert_eq!(ids, vec!["msg1", "msg2", "msg3", "msg4", "msg5"]);
    });

    // Verify the V2 sidecar path was used (the has_v2_sidecar check).
    assert!(pi::session_store_v2::has_v2_sidecar(&jsonl));

    Ok(())
}

#[test]
fn v2_sidecar_with_empty_entries_produces_empty_session() -> PiResult<()> {
    let dir = tempdir()?;
    let entries: Vec<pi::session::SessionEntry> = vec![];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    // Create sidecar (empty).
    let store = pi::session::create_v2_sidecar_from_jsonl(&jsonl)?;
    assert_eq!(store.entry_count(), 0);

    // Verify sidecar directory exists.
    let sidecar_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    assert!(sidecar_root.join("index").exists());

    Ok(())
}

#[test]
fn v2_sidecar_preserves_entry_parent_chain() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("root", None, "start"),
        make_message_entry("child1", Some("root"), "step 1"),
        make_message_entry("child2", Some("child1"), "step 2"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);
    let store = pi::session::create_v2_sidecar_from_jsonl(&jsonl)?;

    // Read active path from leaf to root.
    let path_frames = store.read_active_path("child2")?;
    assert_eq!(path_frames.len(), 3);
    assert_eq!(path_frames[0].entry_id, "root");
    assert_eq!(path_frames[1].entry_id, "child1");
    assert_eq!(path_frames[2].entry_id, "child2");

    Ok(())
}

#[test]
fn v2_sidecar_integrity_valid_after_migration() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("a", None, "alpha"),
        make_message_entry("b", Some("a"), "beta"),
        make_message_entry("c", Some("b"), "gamma"),
        make_message_entry("d", Some("c"), "delta"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);
    let store = pi::session::create_v2_sidecar_from_jsonl(&jsonl)?;

    // Validate integrity — should not error.
    store.validate_integrity()?;

    Ok(())
}

// ─── Migration Tooling Tests ────────────────────────────────────────────────

#[test]
fn migrate_jsonl_to_v2_creates_verified_sidecar() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("m1", None, "first"),
        make_message_entry("m2", Some("m1"), "second"),
        make_message_entry("m3", Some("m2"), "third"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    let event = pi::session::migrate_jsonl_to_v2(&jsonl, "test-corr-001")?;

    assert_eq!(event.outcome, "ok");
    assert_eq!(event.source_format, "jsonl_v3");
    assert_eq!(event.target_format, "native_v2");
    assert!(event.verification.entry_count_match);
    assert!(event.verification.hash_chain_match);
    assert!(event.verification.index_consistent);
    assert_eq!(event.correlation_id, "test-corr-001");

    // Verify ledger was written.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    let ledger = store.read_migration_events()?;
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].phase, "completed");

    Ok(())
}

#[test]
fn migrate_jsonl_to_v2_preserves_existing_sidecar_on_rebuild_failure() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("keep1", None, "first"),
        make_message_entry("keep2", Some("keep1"), "second"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    let initial_event = pi::session::migrate_jsonl_to_v2(&jsonl, "initial-corr")?;
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let baseline_store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    let baseline_ids = frame_ids(&baseline_store.read_all_entries()?);

    let mut file = fs::OpenOptions::new().append(true).open(&jsonl)?;
    file.write_all(b"{ definitely-not-json }\n")?;

    let err = pi::session::migrate_jsonl_to_v2(&jsonl, "retry-corr")
        .expect_err("invalid JSONL should abort remigration");
    assert!(
        err.to_string().contains("Bad JSONL entry"),
        "unexpected error: {err}"
    );

    let recovered_store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    assert_eq!(
        frame_ids(&recovered_store.read_all_entries()?),
        baseline_ids
    );

    let ledger = recovered_store.read_migration_events()?;
    assert_eq!(ledger.len(), 1, "failed remigration must keep prior ledger");
    assert_eq!(ledger[0].correlation_id, initial_event.correlation_id);

    Ok(())
}

#[test]
fn create_v2_sidecar_from_jsonl_preserves_existing_sidecar_on_rebuild_failure() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("keep1", None, "first"),
        make_message_entry("keep2", Some("keep1"), "second"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    pi::session::create_v2_sidecar_from_jsonl(&jsonl)?;
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let baseline_store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    let baseline_ids = frame_ids(&baseline_store.read_all_entries()?);

    let mut file = fs::OpenOptions::new().append(true).open(&jsonl)?;
    file.write_all(b"{ definitely-not-json }\n")?;

    let err = pi::session::create_v2_sidecar_from_jsonl(&jsonl)
        .expect_err("invalid JSONL should abort sidecar rebuild");
    assert!(
        err.to_string().contains("Bad JSONL entry"),
        "unexpected error: {err}"
    );

    let recovered_store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    assert_eq!(
        frame_ids(&recovered_store.read_all_entries()?),
        baseline_ids
    );

    Ok(())
}

#[test]
fn migrate_jsonl_to_v2_failure_does_not_leave_partial_sidecars() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![make_message_entry("bad1", None, "first")];
    let jsonl = build_test_jsonl(dir.path(), &entries);
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let file_name = v2_root
        .file_name()
        .expect("sidecar path must have a file name")
        .to_string_lossy()
        .to_string();

    let mut file = fs::OpenOptions::new().append(true).open(&jsonl)?;
    file.write_all(b"{ invalid-json }\n")?;

    let err = pi::session::migrate_jsonl_to_v2(&jsonl, "bad-corr")
        .expect_err("invalid JSONL should fail first migration");
    assert!(
        err.to_string().contains("Bad JSONL entry"),
        "unexpected error: {err}"
    );
    assert!(
        !pi::session_store_v2::has_v2_sidecar(&jsonl),
        "failed migration must not leave a live sidecar"
    );
    assert!(
        !v2_root.exists(),
        "failed migration must clean final sidecar path"
    );

    let parent = v2_root
        .parent()
        .expect("sidecar path must have a parent directory");
    let staging_prefix = format!("{file_name}.staging.");
    let backup_prefix = format!("{file_name}.backup.");
    for entry in fs::read_dir(parent)? {
        let name = entry?.file_name().to_string_lossy().to_string();
        assert!(
            !name.starts_with(&staging_prefix) && !name.starts_with(&backup_prefix),
            "failed migration left transient sidecar directory: {name}"
        );
    }

    Ok(())
}

#[test]
fn verify_v2_against_jsonl_detects_matching_entries() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("v1", None, "hello"),
        make_message_entry("v2", Some("v1"), "world"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);
    let store = pi::session::create_v2_sidecar_from_jsonl(&jsonl)?;

    let verification = pi::session::verify_v2_against_jsonl(&jsonl, &store)?;

    assert!(verification.entry_count_match);
    assert!(verification.hash_chain_match);
    assert!(verification.index_consistent);

    Ok(())
}

#[test]
fn rollback_v2_sidecar_removes_sidecar_directory() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![make_message_entry("r1", None, "test")];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    // Migrate forward.
    pi::session::migrate_jsonl_to_v2(&jsonl, "rollback-test")?;
    assert!(pi::session_store_v2::has_v2_sidecar(&jsonl));

    // Rollback.
    pi::session::rollback_v2_sidecar(&jsonl, "rollback-test")?;
    assert!(!pi::session_store_v2::has_v2_sidecar(&jsonl));

    // Original JSONL still intact.
    assert!(jsonl.exists());

    Ok(())
}

#[test]
fn rollback_v2_sidecar_is_idempotent() -> PiResult<()> {
    let dir = tempdir()?;
    let jsonl = build_test_jsonl(dir.path(), &[make_message_entry("x", None, "data")]);

    // Rollback when no sidecar exists — should succeed silently.
    pi::session::rollback_v2_sidecar(&jsonl, "noop")?;
    assert!(!pi::session_store_v2::has_v2_sidecar(&jsonl));

    Ok(())
}

#[test]
fn migration_status_unmigrated_when_no_sidecar() {
    let dir = tempdir().unwrap();
    let jsonl = build_test_jsonl(dir.path(), &[make_message_entry("s1", None, "data")]);
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Unmigrated
    );
}

#[test]
fn migration_status_migrated_after_successful_migration() -> PiResult<()> {
    let dir = tempdir()?;
    let jsonl = build_test_jsonl(dir.path(), &[make_message_entry("s1", None, "data")]);
    pi::session::migrate_jsonl_to_v2(&jsonl, "status-test")?;

    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Migrated
    );

    Ok(())
}

#[test]
fn migration_status_partial_when_sidecar_incomplete() {
    let dir = tempdir().unwrap();
    let jsonl = build_test_jsonl(dir.path(), &[make_message_entry("s1", None, "data")]);

    // Create a bare sidecar directory without proper structure.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    fs::create_dir_all(&v2_root).unwrap();

    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Partial
    );
}

#[test]
fn migration_status_is_read_only_and_recovery_heals_damaged_index() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("c1", None, "one"),
        make_message_entry("c2", Some("c1"), "two"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);
    pi::session::migrate_jsonl_to_v2(&jsonl, "corrupt-test")?;

    // Corrupt the index file.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let index_path = v2_root.join("index").join("offsets.jsonl");
    fs::write(&index_path, "not valid json\n")?;

    // Status inspection reports corruption without mutating the sidecar.
    match pi::session::migration_status(&jsonl) {
        MigrationState::Corrupt { .. } => {}
        other => panic!("expected corrupt migration state, got {other:?}"),
    }
    assert_eq!(fs::read_to_string(&index_path)?, "not valid json\n");

    // Explicit recovery owns the mutation and returns the store to a verified state.
    assert_eq!(
        pi::session::recover_partial_migration(&jsonl, "corrupt-status-recovery", true)?,
        MigrationState::Migrated
    );

    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    store.validate_integrity()?;
    assert_eq!(store.entry_count(), 2);
    assert_eq!(
        frame_ids(&store.read_all_entries()?),
        vec!["c1".to_string(), "c2".to_string()]
    );

    Ok(())
}

#[test]
fn migrate_dry_run_validates_without_persisting() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("d1", None, "dry"),
        make_message_entry("d2", Some("d1"), "run"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    let verification = pi::session::migrate_dry_run(&jsonl)?;

    // Dry run should report success.
    assert!(verification.entry_count_match);
    assert!(verification.hash_chain_match);
    assert!(verification.index_consistent);

    // No sidecar should have been created.
    assert!(!pi::session_store_v2::has_v2_sidecar(&jsonl));
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Unmigrated
    );

    Ok(())
}

#[test]
fn migrate_dry_run_matches_real_migration_for_legacy_idless_entries() -> PiResult<()> {
    let dir = tempdir()?;
    let mut legacy_entry = make_message_entry("placeholder", None, "legacy idless row");
    legacy_entry.base_mut().id = None;
    let jsonl = build_test_jsonl(dir.path(), &[legacy_entry]);

    let verification = pi::session::migrate_dry_run(&jsonl)?;
    assert!(verification.entry_count_match);
    assert!(verification.hash_chain_match);
    assert!(verification.index_consistent);
    assert!(
        !pi::session_store_v2::has_v2_sidecar(&jsonl),
        "dry-run normalization must not persist a sidecar"
    );
    Ok(())
}

#[test]
fn migration_validates_graph_without_reordering_authoritative_jsonl() -> PiResult<()> {
    let dir = tempdir()?;
    let jsonl = build_test_jsonl(
        dir.path(),
        &[
            make_message_entry("child-first", Some("parent-second"), "child"),
            make_message_entry("parent-second", None, "parent"),
        ],
    );

    let store = pi::session::create_v2_sidecar_from_jsonl(&jsonl)?;
    assert_eq!(
        frame_ids(&store.read_all_entries()?),
        vec!["child-first".to_string(), "parent-second".to_string()],
        "graph validation must not rewrite authoritative source order"
    );
    let manifest = store
        .validate_manifest_against_store()?
        .expect("migrated manifest");
    assert!(manifest.invariants.parent_links_closed);
    Ok(())
}

#[test]
fn recover_partial_migration_cleans_up_and_optionally_re_migrates() -> PiResult<()> {
    let dir = tempdir()?;
    let jsonl = build_test_jsonl(dir.path(), &[make_message_entry("r1", None, "data")]);

    // Create a partial sidecar.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    fs::create_dir_all(&v2_root)?;

    // Recover without re-migration.
    let state = pi::session::recover_partial_migration(&jsonl, "recover-test", false)?;
    assert_eq!(state, MigrationState::Unmigrated);
    assert!(!v2_root.exists());

    // Create partial again, recover WITH re-migration.
    fs::create_dir_all(&v2_root)?;
    let state = pi::session::recover_partial_migration(&jsonl, "recover-test-2", true)?;
    assert_eq!(state, MigrationState::Migrated);
    assert!(pi::session_store_v2::has_v2_sidecar(&jsonl));

    Ok(())
}

#[test]
fn failed_remigration_preserves_the_prior_v2_tree_byte_for_byte() -> PiResult<()> {
    let dir = tempdir()?;
    let jsonl = build_test_jsonl(
        dir.path(),
        &[
            make_message_entry("preserve-1", None, "first"),
            make_message_entry("preserve-2", Some("preserve-1"), "second"),
        ],
    );
    pi::session::migrate_jsonl_to_v2(&jsonl, "preserve-before-failure")?;

    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let source_state = v2_root.join("source-state.json");
    fs::write(&source_state, b"invalid source state\n")?;
    let preserved_paths = [
        source_state,
        v2_root.join("manifest.json"),
        v2_root.join("index/offsets.jsonl"),
        v2_root.join("segments/0000000000000001.seg"),
        v2_root.join("migrations/ledger.jsonl"),
    ];
    let before = preserved_paths
        .iter()
        .map(|path| Ok((path.clone(), fs::read(path)?)))
        .collect::<PiResult<Vec<_>>>()?;

    let header = fs::read_to_string(&jsonl)?
        .lines()
        .next()
        .expect("JSONL fixture has a header")
        .to_string();
    fs::write(&jsonl, format!("{header}\nnot valid JSON\n"))?;
    assert!(matches!(
        pi::session::migration_status(&jsonl),
        MigrationState::Corrupt { .. }
    ));

    pi::session::recover_partial_migration(&jsonl, "preserve-failed-remigration", true)
        .expect_err("an unreadable authoritative JSONL must reject re-migration");

    assert!(
        v2_root.is_dir(),
        "failed recovery removed the prior V2 root"
    );
    for (path, expected) in before {
        assert_eq!(
            fs::read(&path)?,
            expected,
            "failed recovery changed preserved V2 artifact {}",
            path.display()
        );
    }
    let inspector = SessionStoreV2::open_for_inspection(&v2_root, 64 * 1024 * 1024)?;
    inspector.validate_session_integrity()?;
    Ok(())
}

#[test]
fn ambiguous_jsonl_graph_repair_fails_closed_and_preserves_prior_v2_tree() -> PiResult<()> {
    let root = tempdir()?;
    let mut duplicate_second = make_message_entry("duplicate", Some("duplicate"), "second");
    duplicate_second.base_mut().id = Some("duplicate".to_string());
    let cases = [
        (
            "duplicate",
            vec![
                make_message_entry("duplicate", None, "first"),
                duplicate_second,
            ],
            "duplicate entry ID",
        ),
        (
            "missing-parent",
            vec![make_message_entry(
                "orphan",
                Some("absent-parent"),
                "orphan",
            )],
            "references missing parent",
        ),
        (
            "cycle",
            vec![
                make_message_entry("cycle-a", Some("cycle-b"), "a"),
                make_message_entry("cycle-b", Some("cycle-a"), "b"),
            ],
            "contains a cycle",
        ),
    ];

    for (case_name, invalid_entries, expected_error) in cases {
        let case_dir = root.path().join(case_name);
        fs::create_dir(&case_dir)?;
        let jsonl = build_test_jsonl(
            &case_dir,
            &[
                make_message_entry("prior-1", None, "first"),
                make_message_entry("prior-2", Some("prior-1"), "second"),
            ],
        );
        pi::session::migrate_jsonl_to_v2(&jsonl, &format!("seed-{case_name}"))?;
        let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
        let preserved_paths = [
            v2_root.join("source-state.json"),
            v2_root.join("manifest.json"),
            v2_root.join("index/offsets.jsonl"),
            v2_root.join("segments/0000000000000001.seg"),
            v2_root.join("migrations/ledger.jsonl"),
        ];
        let before = preserved_paths
            .iter()
            .map(|path| Ok((path.clone(), fs::read(path)?)))
            .collect::<PiResult<Vec<_>>>()?;

        let header = fs::read_to_string(&jsonl)?
            .lines()
            .next()
            .expect("JSONL fixture has a header")
            .to_string();
        let mut replacement = Vec::new();
        replacement.extend_from_slice(header.as_bytes());
        replacement.push(b'\n');
        for entry in invalid_entries {
            serde_json::to_writer(&mut replacement, &entry)?;
            replacement.push(b'\n');
        }
        fs::write(&jsonl, replacement)?;

        let error =
            pi::session::recover_partial_migration(&jsonl, &format!("reject-{case_name}"), true)
                .expect_err("ambiguous authoritative JSONL must not replace a prior V2 store");
        assert!(
            error.to_string().contains(expected_error),
            "unexpected {case_name} validation error: {error}"
        );
        for (path, expected) in before {
            assert_eq!(
                fs::read(&path)?,
                expected,
                "{case_name} rejection changed prior V2 artifact {}",
                path.display()
            );
        }
    }

    Ok(())
}

#[test]
fn forged_manifest_repair_rebuilds_a_valid_counter_from_authoritative_jsonl() -> PiResult<()> {
    let dir = tempdir()?;
    let jsonl = build_test_jsonl(
        dir.path(),
        &[
            make_message_entry("repair-manifest-1", None, "first"),
            make_message_entry("repair-manifest-2", Some("repair-manifest-1"), "second"),
        ],
    );
    pi::session::migrate_jsonl_to_v2(&jsonl, "manifest-repair-seed")?;

    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let inspector = SessionStoreV2::open_for_inspection(&v2_root, 64 * 1024 * 1024)?;
    let mut manifest = inspector.read_manifest()?.expect("migrated manifest");
    manifest.counters.messages_total = 8_888;
    write_rehashed_manifest(&v2_root.join("manifest.json"), manifest)?;
    assert!(matches!(
        pi::session::migration_status(&jsonl),
        MigrationState::Corrupt { .. }
    ));

    assert_eq!(
        pi::session::recover_partial_migration(&jsonl, "manifest-repair", true)?,
        MigrationState::Migrated
    );
    let repaired = SessionStoreV2::open_for_inspection(&v2_root, 64 * 1024 * 1024)?;
    let manifest = repaired
        .validate_manifest_against_store()?
        .expect("repair must regenerate the manifest");
    assert_eq!(manifest.counters.messages_total, 2);
    Ok(())
}

#[test]
fn jsonl_v2_manifest_identity_and_source_format_are_semantically_bound() -> PiResult<()> {
    let dir = tempdir()?;
    let jsonl = build_test_jsonl(
        dir.path(),
        &[
            make_message_entry("identity-1", None, "first"),
            make_message_entry("identity-2", Some("identity-1"), "second"),
        ],
    );
    pi::session::migrate_jsonl_to_v2(&jsonl, "manifest-identity-seed")?;
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let manifest_path = v2_root.join("manifest.json");
    let header: SessionHeader = serde_json::from_str(
        fs::read_to_string(&jsonl)?
            .lines()
            .next()
            .expect("JSONL header"),
    )?;

    let inspector = SessionStoreV2::open_for_inspection(&v2_root, 64 * 1024 * 1024)?;
    let mut manifest = inspector.read_manifest()?.expect("migrated manifest");
    let forged_session_id = if header.id == "75d59e5f-d4bc-4e29-941a-72eb0d31b9d1" {
        "f5734512-2ad8-4656-aafd-e5402c6ac6bb"
    } else {
        "75d59e5f-d4bc-4e29-941a-72eb0d31b9d1"
    };
    manifest.session_id = forged_session_id.to_string();
    write_rehashed_manifest(&manifest_path, manifest)?;
    let MigrationState::Corrupt { error } = pi::session::migration_status(&jsonl) else {
        panic!("a valid but different manifest UUID must be rejected semantically");
    };
    assert!(
        error.contains("V2 manifest sessionId mismatch"),
        "unexpected semantic identity error: {error}"
    );
    pi::session::recover_partial_migration(&jsonl, "repair-manifest-session-id", true)?;

    let inspector = SessionStoreV2::open_for_inspection(&v2_root, 64 * 1024 * 1024)?;
    let mut manifest = inspector.read_manifest()?.expect("repaired manifest");
    manifest.source_format = "native_v2".to_string();
    write_rehashed_manifest(&manifest_path, manifest)?;
    let MigrationState::Corrupt { error } = pi::session::migration_status(&jsonl) else {
        panic!("a valid but wrong source format must be rejected semantically");
    };
    assert!(
        error.contains("V2 manifest sourceFormat mismatch"),
        "unexpected semantic source-format error: {error}"
    );
    pi::session::recover_partial_migration(&jsonl, "repair-manifest-source-format", true)?;

    let repaired = SessionStoreV2::open_for_inspection(&v2_root, 64 * 1024 * 1024)?;
    let manifest = repaired
        .validate_manifest_against_store()?
        .expect("verified repaired manifest");
    assert_eq!(manifest.session_id, header.id);
    assert_eq!(manifest.source_format, "jsonl_v3");
    Ok(())
}

#[test]
fn failed_forged_manifest_repair_preserves_prior_v2_tree_byte_for_byte() -> PiResult<()> {
    let dir = tempdir()?;
    let jsonl = build_test_jsonl(
        dir.path(),
        &[
            make_message_entry("failed-manifest-1", None, "first"),
            make_message_entry("failed-manifest-2", Some("failed-manifest-1"), "second"),
        ],
    );
    pi::session::migrate_jsonl_to_v2(&jsonl, "failed-manifest-seed")?;

    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let inspector = SessionStoreV2::open_for_inspection(&v2_root, 64 * 1024 * 1024)?;
    let mut manifest = inspector.read_manifest()?.expect("migrated manifest");
    manifest.counters.messages_total = 7_777;
    let manifest_path = v2_root.join("manifest.json");
    write_rehashed_manifest(&manifest_path, manifest)?;
    let preserved_paths = [
        v2_root.join("source-state.json"),
        manifest_path,
        v2_root.join("index/offsets.jsonl"),
        v2_root.join("segments/0000000000000001.seg"),
        v2_root.join("migrations/ledger.jsonl"),
    ];
    let before = preserved_paths
        .iter()
        .map(|path| Ok((path.clone(), fs::read(path)?)))
        .collect::<PiResult<Vec<_>>>()?;

    let header = fs::read_to_string(&jsonl)?
        .lines()
        .next()
        .expect("JSONL fixture has a header")
        .to_string();
    fs::write(&jsonl, format!("{header}\nnot valid JSON\n"))?;
    assert!(matches!(
        pi::session::migration_status(&jsonl),
        MigrationState::Corrupt { .. }
    ));
    pi::session::recover_partial_migration(&jsonl, "failed-manifest-repair", true)
        .expect_err("invalid authoritative JSONL must reject manifest repair");

    for (path, expected) in before {
        assert_eq!(
            fs::read(&path)?,
            expected,
            "failed manifest repair changed prior V2 artifact {}",
            path.display()
        );
    }
    Ok(())
}

#[test]
fn migrate_then_rollback_then_re_migrate_round_trip() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("rt1", None, "alpha"),
        make_message_entry("rt2", Some("rt1"), "beta"),
        make_message_entry("rt3", Some("rt2"), "gamma"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    // Step 1: Migrate.
    let event1 = pi::session::migrate_jsonl_to_v2(&jsonl, "round-trip")?;
    assert_eq!(event1.outcome, "ok");

    // Step 2: Rollback.
    pi::session::rollback_v2_sidecar(&jsonl, "round-trip")?;
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Unmigrated
    );

    // Step 3: Re-migrate.
    let event2 = pi::session::migrate_jsonl_to_v2(&jsonl, "round-trip-2")?;
    assert_eq!(event2.outcome, "ok");
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Migrated
    );

    // Verify the re-migrated store has correct entry count.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    assert_eq!(store.entry_count(), 3);

    Ok(())
}

#[test]
fn migrate_empty_session_succeeds() -> PiResult<()> {
    let dir = tempdir()?;
    let entries: Vec<SessionEntry> = vec![];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    let event = pi::session::migrate_jsonl_to_v2(&jsonl, "empty-test")?;
    assert_eq!(event.outcome, "ok");
    assert!(event.verification.entry_count_match);
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Migrated
    );

    Ok(())
}

#[test]
fn migrate_large_session_preserves_all_entries() -> PiResult<()> {
    let dir = tempdir()?;
    let mut entries = Vec::new();
    for i in 0..100 {
        let parent = if i == 0 {
            None
        } else {
            Some(format!("e{}", i - 1))
        };
        entries.push(make_message_entry(
            &format!("e{i}"),
            parent.as_deref(),
            &format!("message number {i}"),
        ));
    }
    let jsonl = build_test_jsonl(dir.path(), &entries);

    let event = pi::session::migrate_jsonl_to_v2(&jsonl, "large-test")?;
    assert_eq!(event.outcome, "ok");
    assert!(event.verification.entry_count_match);

    // Verify all entries round-trip.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    assert_eq!(store.entry_count(), 100);

    let frames = store.read_all_entries()?;
    assert_eq!(frames.len(), 100);
    assert_eq!(frames[0].entry_id, "e0");
    assert_eq!(frames[99].entry_id, "e99");

    Ok(())
}

#[test]
fn migrate_branching_session_preserves_all_branches() -> PiResult<()> {
    let dir = tempdir()?;
    // Create a session with a fork:
    //   root → a → b
    //             → c (branch from a)
    let entries = vec![
        make_message_entry("root", None, "start"),
        make_message_entry("a", Some("root"), "step A"),
        make_message_entry("b", Some("a"), "branch 1"),
        make_message_entry("c", Some("a"), "branch 2"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    let event = pi::session::migrate_jsonl_to_v2(&jsonl, "branch-test")?;
    assert_eq!(event.outcome, "ok");
    assert!(event.verification.entry_count_match);

    // All 4 entries should be in the store.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    assert_eq!(store.entry_count(), 4);

    // Active path from branch "b" should be: root → a → b.
    let path_b = store.read_active_path("b")?;
    let ids_b: Vec<&str> = path_b.iter().map(|f| f.entry_id.as_str()).collect();
    assert_eq!(ids_b, vec!["root", "a", "b"]);

    // Active path from branch "c" should be: root → a → c.
    let path_c = store.read_active_path("c")?;
    let ids_c: Vec<&str> = path_c.iter().map(|f| f.entry_id.as_str()).collect();
    assert_eq!(ids_c, vec!["root", "a", "c"]);

    Ok(())
}

#[test]
fn migration_ledger_accumulates_events() -> PiResult<()> {
    let dir = tempdir()?;
    let jsonl = build_test_jsonl(dir.path(), &[make_message_entry("l1", None, "data")]);

    // Migrate.
    pi::session::migrate_jsonl_to_v2(&jsonl, "ledger-1")?;

    // Check ledger has 1 event.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    let events = store.read_migration_events()?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].phase, "completed");

    Ok(())
}

// ─── E2E Migration/Rollback with Forensic Logging ────────────────────────────
//
// These tests exercise the full migration lifecycle end-to-end and assert
// forensic log completeness at every step.

/// Full V1→V2→rollback→V1 round-trip with forensic ledger verification.
#[test]
fn e2e_full_migration_rollback_round_trip_with_forensic_log() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("e1", None, "alpha"),
        make_message_entry("e2", Some("e1"), "beta"),
        make_message_entry("e3", Some("e2"), "gamma"),
        make_message_entry("e4", Some("e3"), "delta"),
        make_message_entry("e5", Some("e4"), "epsilon"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    // Phase 0: Confirm unmigrated state.
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Unmigrated
    );

    // Phase 1: Forward migration.
    let fwd_event = pi::session::migrate_jsonl_to_v2(&jsonl, "e2e-round-trip")?;
    assert_eq!(fwd_event.phase, "completed");
    assert_eq!(fwd_event.outcome, "ok");
    assert_eq!(fwd_event.source_format, "jsonl_v3");
    assert_eq!(fwd_event.target_format, "native_v2");
    assert_eq!(fwd_event.correlation_id, "e2e-round-trip");
    assert!(fwd_event.verification.entry_count_match);
    assert!(fwd_event.verification.hash_chain_match);
    assert!(fwd_event.verification.index_consistent);
    assert!(fwd_event.error_class.is_none());
    assert!(!fwd_event.migration_id.is_empty());
    assert!(!fwd_event.at.is_empty());

    // Verify migrated state.
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Migrated
    );

    // Verify V2 store contents are correct.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    assert_eq!(store.entry_count(), 5);
    let frames = store.read_all_entries()?;
    let frame_entry_ids: Vec<&str> = frames.iter().map(|f| f.entry_id.as_str()).collect();
    assert_eq!(frame_entry_ids, vec!["e1", "e2", "e3", "e4", "e5"]);

    // Verify forensic ledger has exactly 1 forward event.
    let ledger = store.read_migration_events()?;
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].phase, "completed");
    assert_eq!(ledger[0].schema, "pi.session_store_v2.migration_event.v1");

    // Verify the JSONL source is still intact (migration is non-destructive).
    assert!(jsonl.exists());
    let jsonl_content = fs::read_to_string(&jsonl)?;
    let jsonl_entry_count = jsonl_content
        .lines()
        .skip(1) // skip header
        .filter(|l| !l.trim().is_empty())
        .count();
    assert_eq!(jsonl_entry_count, 5);

    // Phase 2: Rollback to JSONL-only.
    pi::session::rollback_v2_sidecar(&jsonl, "e2e-round-trip")?;
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Unmigrated
    );
    assert!(!pi::session_store_v2::has_v2_sidecar(&jsonl));

    // JSONL is still intact after rollback.
    assert!(jsonl.exists());
    let post_rollback_content = fs::read_to_string(&jsonl)?;
    assert_eq!(jsonl_content, post_rollback_content);

    Ok(())
}

/// Full V1→V2→rollback→re-migrate cycle with ledger accumulation.
#[test]
fn e2e_migrate_rollback_remigrate_ledger_accumulates() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("a1", None, "first"),
        make_message_entry("a2", Some("a1"), "second"),
        make_message_entry("a3", Some("a2"), "third"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    // First migration.
    let event1 = pi::session::migrate_jsonl_to_v2(&jsonl, "cycle-01")?;
    assert_eq!(event1.phase, "completed");

    // Check ledger before rollback.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    assert_eq!(store.read_migration_events()?.len(), 1);
    drop(store);

    // Rollback (note: rollback removes the V2 sidecar, so the ledger is lost).
    pi::session::rollback_v2_sidecar(&jsonl, "cycle-1-rollback")?;
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Unmigrated
    );

    // Re-migrate — fresh sidecar, fresh ledger.
    let event2 = pi::session::migrate_jsonl_to_v2(&jsonl, "cycle-02")?;
    assert_eq!(event2.phase, "completed");
    assert_eq!(event2.correlation_id, "cycle-02");

    // New ledger should have 1 event (fresh sidecar after rollback).
    let store2 = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    let ledger = store2.read_migration_events()?;
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].correlation_id, "cycle-02");

    // Verify data integrity after re-migration.
    assert_eq!(store2.entry_count(), 3);
    store2.validate_integrity()?;

    Ok(())
}

/// Dry-run followed by real migration — confirms no side effects from dry run.
#[test]
fn e2e_dry_run_then_real_migration() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("dr1", None, "one"),
        make_message_entry("dr2", Some("dr1"), "two"),
        make_message_entry("dr3", Some("dr2"), "three"),
        make_message_entry("dr4", Some("dr3"), "four"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    // Dry run — no sidecar should exist.
    let dry_verification = pi::session::migrate_dry_run(&jsonl)?;
    assert!(dry_verification.entry_count_match);
    assert!(dry_verification.hash_chain_match);
    assert!(dry_verification.index_consistent);
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Unmigrated
    );
    assert!(!pi::session_store_v2::has_v2_sidecar(&jsonl));

    // Real migration.
    let event = pi::session::migrate_jsonl_to_v2(&jsonl, "dry-then-real")?;
    assert_eq!(event.outcome, "ok");
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Migrated
    );

    // Verify the real migration matched dry-run verification.
    assert_eq!(
        event.verification.entry_count_match,
        dry_verification.entry_count_match
    );
    assert_eq!(
        event.verification.hash_chain_match,
        dry_verification.hash_chain_match
    );
    assert_eq!(
        event.verification.index_consistent,
        dry_verification.index_consistent
    );

    Ok(())
}

/// Partial migration recovery with re-migration and forensic verification.
#[test]
fn e2e_partial_migration_recovery_with_forensic_check() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("p1", None, "alpha"),
        make_message_entry("p2", Some("p1"), "beta"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    // Simulate a partial migration: create V2 dir with segments but no index.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    fs::create_dir_all(v2_root.join("segments"))?;
    fs::write(
        v2_root.join("segments").join("0000000000000001.seg"),
        "partial_data\n",
    )?;

    // Status should be Partial.
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Partial
    );

    // Recover with re-migration.
    let state = pi::session::recover_partial_migration(&jsonl, "partial-recovery-e2e", true)?;
    assert_eq!(state, MigrationState::Migrated);

    // Verify data integrity after recovery.
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    assert_eq!(store.entry_count(), 2);
    store.validate_integrity()?;

    // Verify forensic ledger exists with forward event.
    let ledger = store.read_migration_events()?;
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].phase, "completed");
    assert_eq!(ledger[0].correlation_id, "partial-recovery-e2e");

    Ok(())
}

/// Corrupt migration recovery without re-migration (just cleanup).
#[test]
fn e2e_corrupt_migration_recovery_cleanup_only() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![make_message_entry("c1", None, "data")];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    // Create a valid V2 sidecar then corrupt a segment (not recoverable by index rebuild).
    pi::session::migrate_jsonl_to_v2(&jsonl, "pre-corrupt")?;
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let seg_path = v2_root.join("segments").join("0000000000000001.seg");
    assert!(
        seg_path.exists(),
        "expected segment file to exist before corruption"
    );
    fs::write(&seg_path, "corrupted segment data\n")?;

    // Status should be Corrupt.
    match pi::session::migration_status(&jsonl) {
        MigrationState::Corrupt { .. } => {}
        other => panic!("Expected Corrupt, got {other:?}"),
    }

    // Recover WITHOUT re-migration.
    let state = pi::session::recover_partial_migration(&jsonl, "corrupt-cleanup", false)?;
    assert_eq!(state, MigrationState::Unmigrated);
    assert!(!v2_root.exists());

    // JSONL is still intact.
    assert!(jsonl.exists());

    Ok(())
}

/// Migration event forensic fields are all populated with valid data.
#[test]
fn e2e_forensic_event_field_completeness() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("f1", None, "first"),
        make_message_entry("f2", Some("f1"), "second"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);
    let jsonl_display = jsonl.display().to_string();

    let event = pi::session::migrate_jsonl_to_v2(&jsonl, "forensic-check-001")?;

    // Check every field of the forensic event.
    assert_eq!(event.schema, "pi.session_store_v2.migration_event.v1");
    assert!(!event.migration_id.is_empty(), "migration_id must be set");
    assert_eq!(event.phase, "completed");
    assert!(!event.at.is_empty(), "timestamp must be set");
    // Validate the timestamp is parseable as RFC 3339.
    assert!(
        chrono::DateTime::parse_from_rfc3339(&event.at).is_ok(),
        "timestamp must be valid RFC 3339: {}",
        event.at
    );
    assert_eq!(event.source_path, jsonl_display);
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    assert_eq!(event.target_path, v2_root.display().to_string());
    assert_eq!(event.source_format, "jsonl_v3");
    assert_eq!(event.target_format, "native_v2");
    assert_eq!(event.outcome, "ok");
    assert!(event.error_class.is_none());
    assert_eq!(event.correlation_id, "forensic-check-001");

    // Verification sub-struct.
    assert!(event.verification.entry_count_match);
    assert!(event.verification.hash_chain_match);
    assert!(event.verification.index_consistent);

    Ok(())
}

/// Migration ID uniqueness across multiple migrations.
#[test]
fn e2e_migration_ids_are_unique_across_cycles() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![make_message_entry("u1", None, "data")];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    let mut seen_ids: Vec<String> = Vec::new();

    for cycle in 0..3 {
        let corr = format!("uniqueness-cycle-{cycle}");
        let event = pi::session::migrate_jsonl_to_v2(&jsonl, &corr)?;
        assert!(
            !seen_ids.contains(&event.migration_id),
            "migration_id collision at cycle {cycle}: {}",
            event.migration_id
        );
        seen_ids.push(event.migration_id);

        // Rollback for next cycle.
        pi::session::rollback_v2_sidecar(&jsonl, &corr)?;
    }

    assert_eq!(seen_ids.len(), 3);

    Ok(())
}

/// Migration state machine transitions: Unmigrated → Migrated → (corrupt) → recovered.
#[test]
fn e2e_migration_state_machine_transitions() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("sm1", None, "state"),
        make_message_entry("sm2", Some("sm1"), "machine"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    // State 1: Unmigrated.
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Unmigrated
    );

    // State 2: Migrated.
    pi::session::migrate_jsonl_to_v2(&jsonl, "sm-forward")?;
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Migrated
    );

    // State 3: Corrupt (tamper with segment data).
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let seg_path = v2_root.join("segments").join("0000000000000001.seg");
    if seg_path.exists() {
        fs::write(&seg_path, "corrupted segment data\n")?;
    }
    match pi::session::migration_status(&jsonl) {
        MigrationState::Corrupt { error } => {
            assert!(!error.is_empty(), "corrupt error message must be non-empty");
        }
        other => panic!("Expected Corrupt after segment tampering, got {other:?}"),
    }

    // State 4: Recovered via recover_partial_migration (with re-migration).
    let state = pi::session::recover_partial_migration(&jsonl, "sm-recovery", true)?;
    assert_eq!(state, MigrationState::Migrated);

    // Verify integrity post-recovery.
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    store.validate_integrity()?;
    assert_eq!(store.entry_count(), 2);

    Ok(())
}

/// Verify V2 store hash chain + integrity after migration of large session.
#[test]
fn e2e_large_session_migration_integrity_and_chain() -> PiResult<()> {
    let dir = tempdir()?;
    let mut entries = Vec::new();
    for i in 0..200 {
        let parent = if i == 0 {
            None
        } else {
            Some(format!("big{}", i - 1))
        };
        entries.push(make_message_entry(
            &format!("big{i}"),
            parent.as_deref(),
            &format!(
                "message body for entry {i} with padding: {}",
                "x".repeat(50)
            ),
        ));
    }
    let jsonl = build_test_jsonl(dir.path(), &entries);

    // Dry-run first.
    let dry = pi::session::migrate_dry_run(&jsonl)?;
    assert!(dry.entry_count_match);
    assert!(dry.hash_chain_match);
    assert!(dry.index_consistent);

    // Real migration.
    let event = pi::session::migrate_jsonl_to_v2(&jsonl, "large-e2e")?;
    assert_eq!(event.outcome, "ok");

    // Verify V2 store fully.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    assert_eq!(store.entry_count(), 200);
    store.validate_integrity()?;

    // Verify chain hash is non-genesis.
    assert_ne!(
        store.chain_hash(),
        "0000000000000000000000000000000000000000000000000000000000000000"
    );

    // Verify index is complete.
    let index = store.read_index()?;
    assert_eq!(index.len(), 200);
    for (i, row) in index.iter().enumerate() {
        assert_eq!(
            row.entry_seq,
            u64::try_from(i + 1).unwrap(),
            "index entry_seq mismatch at position {i}"
        );
    }

    // Verify frame round-trip for first and last entries.
    let first = store.lookup_entry(1)?.expect("first entry");
    assert_eq!(first.entry_id, "big0");
    let last = store.lookup_entry(200)?.expect("last entry");
    assert_eq!(last.entry_id, "big199");

    Ok(())
}

/// Swarm-oriented stress gate for large V2 recovery, stale fallback, and index concurrency.
#[test]
fn large_session_store_v2_recovery_swarm_profile_emits_evidence() -> PiResult<()> {
    const BASE_ENTRIES: usize = 384;
    const PAYLOAD_BYTES: usize = 768;
    const TAIL_ENTRIES: usize = 16;
    const MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

    let total_start = test_timing_start();
    let dir = tempdir()?;
    let jsonl_root = dir.path().join("jsonl");
    let index_root = dir.path().join("index-swarm");
    let save_root = dir.path().join("save-root");
    fs::create_dir_all(&jsonl_root)?;
    fs::create_dir_all(&index_root)?;
    fs::create_dir_all(&save_root)?;

    let entries = build_large_history_entries(BASE_ENTRIES, PAYLOAD_BYTES);
    let jsonl = build_test_jsonl(&jsonl_root, &entries);

    let migration =
        migrate_large_history_and_rebuild_checkpoint(&jsonl, BASE_ENTRIES, MAX_SEGMENT_BYTES)?;
    let recovery = append_tail_and_recover_truncated_frame(
        &migration.v2_root,
        BASE_ENTRIES,
        TAIL_ENTRIES,
        MAX_SEGMENT_BYTES,
    )?;
    let stale =
        assert_stale_sidecar_fallback(&jsonl, &jsonl_root, &migration.v2_root, BASE_ENTRIES)?;

    let index_start = test_timing_start();
    let indexed_rows = index_concurrent_session_snapshots(&index_root)?;
    let index_elapsed_us = elapsed_test_us(index_start);

    let save_start = test_timing_start();
    let saved_session_path = assert_crash_resilient_session_save(&save_root)?;
    let save_elapsed_us = elapsed_test_us(save_start);

    let report = json!({
        "schema": "pi.session_store_v2.recovery_swarm_profile.v1",
        "bead": "bd-07cku.6",
        "counts": {
            "base_entries": BASE_ENTRIES,
            "payload_bytes_per_entry": PAYLOAD_BYTES,
            "tail_entries_appended": TAIL_ENTRIES,
            "recovered_entries_after_truncation": recovery.recovered_count,
            "concurrent_index_rows": indexed_rows,
            "stale_jsonl_entries": stale.total_entries,
        },
        "recovery": {
            "v2_root": migration.v2_root.display().to_string(),
            "offset_index": migration.index_path.display().to_string(),
            "truncated_segment": recovery.segment_path.display().to_string(),
            "checkpoint_head_entry_seq": migration.checkpoint_head_entry_seq,
            "checkpoint_head_entry_id": migration.checkpoint_head_entry_id,
        },
        "stale_sidecar_fallback": {
            "opened_backend": stale.opened_backend,
            "sidecar_present": stale.sidecar_present,
            "sidecar_stale": stale.sidecar_stale,
        },
        "crash_resilient_save": {
            "session_path": saved_session_path.display().to_string(),
            "entries": 32,
            "deterministic_root": save_root.display().to_string(),
        },
        "timings_us": {
            "migration": migration.migration_elapsed_us,
            "checkpoint_rebuild": migration.checkpoint_rebuild_elapsed_us,
            "trailing_frame_recovery": recovery.elapsed_us,
            "stale_fallback": stale.elapsed_us,
            "concurrent_index": index_elapsed_us,
            "crash_resilient_save": save_elapsed_us,
            "total": elapsed_test_us(total_start),
        }
    });
    let evidence_path = emit_session_store_v2_recovery_evidence(&report)?;
    assert!(
        evidence_path.exists(),
        "recovery evidence was not emitted at {}",
        evidence_path.display()
    );
    println!(
        "session store v2 recovery swarm evidence: {}",
        evidence_path.display()
    );

    Ok(())
}

/// Deterministic chaos lane for concurrent save/resume, session-index refresh,
/// recoverable V2 index rebuild, verified corrupt-sidecar repair, and JSONL/V2 parity.
#[test]
fn session_index_store_v2_resume_chaos_lane_emits_evidence() -> PiResult<()> {
    let total_start = test_timing_start();
    let dir = tempdir()?;
    let jsonl_root = dir.path().join("jsonl-parity");
    let index_root = dir.path().join("index-concurrency");

    let parity_start = test_timing_start();
    let parity = assert_jsonl_v2_resume_parity_and_corrupt_sidecar_repair(&jsonl_root)?;
    let parity_elapsed_us = elapsed_test_us(parity_start);

    let index_start = test_timing_start();
    let indexed_rows = concurrent_save_resume_index_chaos(&index_root)?;
    let index_elapsed_us = elapsed_test_us(index_start);

    let report = json!({
        "schema": "pi.session_store_v2.chaos_lane.v2",
        "bead": "bd-e5le6.8",
        "status": "pass",
        "coverage": {
            "concurrent_save_resume": true,
            "session_index_stale_refresh": true,
            "store_v2_recoverable_index_rebuild": true,
            "corrupt_sidecar_verified_repair": true,
            "jsonl_v2_resume_parity": true,
        },
        "parity_and_repair": parity,
        "session_index": {
            "indexed_rows_after_concurrent_workers": indexed_rows,
            "root": index_root.display().to_string(),
        },
        "timings_us": {
            "parity_and_repair": parity_elapsed_us,
            "concurrent_index": index_elapsed_us,
            "total": elapsed_test_us(total_start),
        }
    });
    let evidence_path = emit_session_store_v2_recovery_evidence(&report)?;
    assert!(
        evidence_path.exists(),
        "chaos lane evidence was not emitted at {}",
        evidence_path.display()
    );
    println!(
        "session store v2 chaos lane evidence: {}",
        evidence_path.display()
    );

    Ok(())
}

/// Migration with branching session preserves all branches and parent chains.
#[test]
fn e2e_branching_migration_preserves_all_paths() -> PiResult<()> {
    let dir = tempdir()?;
    // Create a session with two branch points:
    //   root → a → b → c (main branch)
    //              ↘ d → e (side branch 1)
    //   root → a → f (side branch 2)
    let entries = vec![
        make_message_entry("root", None, "genesis"),
        make_message_entry("a", Some("root"), "step A"),
        make_message_entry("b", Some("a"), "main B"),
        make_message_entry("c", Some("b"), "main C"),
        make_message_entry("d", Some("b"), "side1 D"),
        make_message_entry("e", Some("d"), "side1 E"),
        make_message_entry("f", Some("a"), "side2 F"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    let event = pi::session::migrate_jsonl_to_v2(&jsonl, "branch-e2e")?;
    assert_eq!(event.outcome, "ok");
    assert!(event.verification.entry_count_match);

    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    assert_eq!(store.entry_count(), 7);

    // Verify each branch path.
    let main_path = store.read_active_path("c")?;
    let main_ids: Vec<&str> = main_path.iter().map(|f| f.entry_id.as_str()).collect();
    assert_eq!(main_ids, vec!["root", "a", "b", "c"]);

    let side1_path = store.read_active_path("e")?;
    let side1_ids: Vec<&str> = side1_path.iter().map(|f| f.entry_id.as_str()).collect();
    assert_eq!(side1_ids, vec!["root", "a", "b", "d", "e"]);

    let side2_path = store.read_active_path("f")?;
    let side2_ids: Vec<&str> = side2_path.iter().map(|f| f.entry_id.as_str()).collect();
    assert_eq!(side2_ids, vec!["root", "a", "f"]);

    // Rollback preserves JSONL intact.
    pi::session::rollback_v2_sidecar(&jsonl, "branch-e2e-rollback")?;
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Unmigrated
    );
    assert!(jsonl.exists());

    Ok(())
}

/// Correlation ID propagation — same correlation ID links related events.
#[test]
fn e2e_correlation_id_propagation() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![make_message_entry("ci1", None, "corr")];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    let corr_id = "CORR-2026-0215-001";
    let event = pi::session::migrate_jsonl_to_v2(&jsonl, corr_id)?;
    assert_eq!(event.correlation_id, corr_id);

    // Verify it's in the ledger.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    let ledger = store.read_migration_events()?;
    assert_eq!(ledger[0].correlation_id, corr_id);

    Ok(())
}

/// Recovery from partial state is idempotent on already-unmigrated sessions.
#[test]
fn e2e_recover_unmigrated_is_noop() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![make_message_entry("n1", None, "noop")];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    // Already unmigrated — recover should be a noop.
    let state = pi::session::recover_partial_migration(&jsonl, "noop-test", true)?;
    assert_eq!(state, MigrationState::Unmigrated);

    // Still unmigrated (recover doesn't migrate an unmigrated session).
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Unmigrated
    );

    Ok(())
}

/// Recovery from already-migrated state is also a noop.
#[test]
fn e2e_recover_migrated_is_noop() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![make_message_entry("m1", None, "already")];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    pi::session::migrate_jsonl_to_v2(&jsonl, "pre-migrate")?;
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Migrated
    );

    // Recover on already-migrated — should be a noop.
    let state = pi::session::recover_partial_migration(&jsonl, "noop-migrated", false)?;
    assert_eq!(state, MigrationState::Migrated);

    Ok(())
}

/// Migration of session with multiple entry types (custom + message).
#[test]
fn e2e_migration_mixed_entry_types() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("msg1", None, "hello"),
        make_custom_entry("cust1", Some("msg1")),
        make_message_entry("msg2", Some("cust1"), "world"),
        make_custom_entry("cust2", Some("msg2")),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    let event = pi::session::migrate_jsonl_to_v2(&jsonl, "mixed-types")?;
    assert_eq!(event.outcome, "ok");
    assert!(event.verification.entry_count_match);

    // Verify all entry types round-trip.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    let frames = store.read_all_entries()?;
    assert_eq!(frames.len(), 4);
    assert_eq!(frames[0].entry_type, "message");
    assert_eq!(frames[1].entry_type, "custom");
    assert_eq!(frames[2].entry_type, "message");
    assert_eq!(frames[3].entry_type, "custom");

    // Verify conversion back to SessionEntry works for all types.
    for frame in &frames {
        let recovered = pi::session_store_v2::frame_to_session_entry(frame)?;
        assert!(recovered.base_id().is_some());
    }

    Ok(())
}

/// Rollback on non-existent sidecar is safe (idempotent).
#[test]
fn e2e_rollback_nonexistent_sidecar_is_safe() -> PiResult<()> {
    let dir = tempdir()?;
    let jsonl = build_test_jsonl(dir.path(), &[make_message_entry("x", None, "data")]);

    // No sidecar exists.
    assert!(!pi::session_store_v2::has_v2_sidecar(&jsonl));

    // Rollback should succeed silently.
    pi::session::rollback_v2_sidecar(&jsonl, "phantom-rollback")?;

    // Still no sidecar, JSONL intact.
    assert!(!pi::session_store_v2::has_v2_sidecar(&jsonl));
    assert!(jsonl.exists());

    Ok(())
}

/// Migrate and verify the published manifest against both stores of identity.
#[test]
fn e2e_migration_manifest_consistency() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("mf1", None, "manifest"),
        make_message_entry("mf2", Some("mf1"), "test"),
        make_message_entry("mf3", Some("mf2"), "data"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    pi::session::migrate_jsonl_to_v2(&jsonl, "manifest-e2e")?;

    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let store = SessionStoreV2::open_for_inspection(&v2_root, 64 * 1024 * 1024)?;
    let header: SessionHeader = serde_json::from_str(
        fs::read_to_string(&jsonl)?
            .lines()
            .next()
            .expect("JSONL header"),
    )?;

    // Validate the manifest emitted by migration; do not replace the artifact
    // under test with a newly generated manifest.
    let manifest = store
        .validate_manifest_against_store()?
        .expect("migration must publish a manifest");
    assert_eq!(manifest.store_version, 2);
    assert_eq!(manifest.session_id, header.id);
    assert_eq!(manifest.source_format, "jsonl_v3");
    let expected_entry_count = u64::try_from(entries.len()).expect("fixture length fits u64");
    assert_eq!(manifest.counters.entries_total, expected_entry_count);
    assert_eq!(manifest.head.entry_seq, expected_entry_count);
    assert_eq!(manifest.head.entry_id, "mf3");
    assert!(manifest.invariants.hash_chain_valid);
    assert!(manifest.invariants.monotonic_entry_seq);
    assert_eq!(
        frame_ids(&store.read_all_entries()?),
        vec!["mf1".to_string(), "mf2".to_string(), "mf3".to_string()]
    );

    // A second read must preserve the exact identity and integrity evidence.
    let read_back = store.read_manifest()?.expect("manifest should exist");
    assert_eq!(read_back.session_id, header.id);
    assert_eq!(read_back.source_format, manifest.source_format);
    assert_eq!(read_back.head.entry_id, manifest.head.entry_id);
    assert_eq!(read_back.head.entry_seq, manifest.head.entry_seq);
    assert_eq!(
        read_back.integrity.chain_hash,
        manifest.integrity.chain_hash
    );
    assert_eq!(
        read_back.integrity.manifest_hash,
        manifest.integrity.manifest_hash
    );
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Migrated
    );

    Ok(())
}

/// Verify that forensic ledger events are persisted as valid JSONL.
#[test]
fn e2e_forensic_ledger_is_valid_jsonl() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("jl1", None, "jsonl"),
        make_message_entry("jl2", Some("jl1"), "ledger"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    pi::session::migrate_jsonl_to_v2(&jsonl, "jsonl-ledger-test")?;

    // Read the raw ledger file and verify each line is valid JSON.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let ledger_path = v2_root.join("migrations").join("ledger.jsonl");
    assert!(
        ledger_path.exists(),
        "ledger file must exist after migration"
    );

    let ledger_content = fs::read_to_string(&ledger_path)?;
    let mut line_count = 0;
    for line in ledger_content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("Ledger line is not valid JSON: {e}\nLine: {line}"));
        // Each entry must have the schema field.
        assert_eq!(
            parsed["schema"].as_str(),
            Some("pi.session_store_v2.migration_event.v1")
        );
        line_count += 1;
    }
    assert_eq!(line_count, 1);

    Ok(())
}

/// Multiple rapid migrate/rollback cycles don't leave stale artifacts.
#[test]
fn e2e_rapid_migrate_rollback_cycles_clean() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("rc1", None, "rapid"),
        make_message_entry("rc2", Some("rc1"), "cycle"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);

    for cycle in 0..5 {
        let corr = format!("rapid-cycle-{cycle}");

        // Migrate.
        let event = pi::session::migrate_jsonl_to_v2(&jsonl, &corr)?;
        assert_eq!(event.outcome, "ok", "cycle {cycle} migration failed");
        assert_eq!(
            pi::session::migration_status(&jsonl),
            MigrationState::Migrated
        );

        // Verify store.
        let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
        assert_eq!(store.entry_count(), 2);
        store.validate_integrity()?;
        drop(store);

        // Rollback.
        pi::session::rollback_v2_sidecar(&jsonl, &corr)?;
        assert!(
            !v2_root.exists(),
            "V2 root should not exist after rollback at cycle {cycle}"
        );
    }

    // Final state is unmigrated, JSONL intact.
    assert_eq!(
        pi::session::migration_status(&jsonl),
        MigrationState::Unmigrated
    );
    assert!(jsonl.exists());

    Ok(())
}

/// Verification detects entry count mismatch when JSONL is modified post-migration.
#[test]
fn e2e_verification_detects_post_migration_jsonl_modification() -> PiResult<()> {
    let dir = tempdir()?;
    let entries = vec![
        make_message_entry("vm1", None, "verify"),
        make_message_entry("vm2", Some("vm1"), "me"),
    ];
    let jsonl = build_test_jsonl(dir.path(), &entries);

    // Migrate.
    pi::session::migrate_jsonl_to_v2(&jsonl, "verify-mod")?;

    // Append an extra entry to the JSONL (simulating a post-migration write).
    let extra = make_message_entry("vm3", Some("vm2"), "sneaky");
    let mut file = fs::OpenOptions::new().append(true).open(&jsonl)?;
    serde_json::to_writer(&mut file, &extra)?;
    file.write_all(b"\n")?;

    // Re-verify — should detect mismatch because V2 has 2 entries but JSONL now has 3.
    let v2_root = pi::session_store_v2::v2_sidecar_path(&jsonl);
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
    let verification = pi::session::verify_v2_against_jsonl(&jsonl, &store)?;

    assert!(
        !verification.entry_count_match,
        "entry count should NOT match after JSONL modification"
    );

    Ok(())
}
