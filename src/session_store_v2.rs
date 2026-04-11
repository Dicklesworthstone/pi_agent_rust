//! Session Store V2 segmented append log + sidecar index primitives.
//!
//! This module provides the storage core requested by Phase-2 performance work:
//! - Segment append writer
//! - Sidecar offset index rows
//! - Reader helpers
//! - Integrity validation (checksum + payload hash)

use crate::error::{Error, Result};
use crate::session::SessionEntry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

fn secure_open_options() -> OpenOptions {
    let mut opts = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts
}

pub const SEGMENT_FRAME_SCHEMA: &str = "pi.session_store_v2.segment_frame.v1";
pub const OFFSET_INDEX_SCHEMA: &str = "pi.session_store_v2.offset_index.v1";
pub const CHECKPOINT_SCHEMA: &str = "pi.session_store_v2.checkpoint.v1";
pub const MANIFEST_SCHEMA: &str = "pi.session_store_v2.manifest.v1";
pub const MIGRATION_EVENT_SCHEMA: &str = "pi.session_store_v2.migration_event.v1";

/// Maximum size for a single frame line (100MB) to prevent OOM on corrupted files.
const MAX_FRAME_READ_BYTES: u64 = 100 * 1024 * 1024;

/// Initial chain hash before any frames are appended.
const GENESIS_CHAIN_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentFrame {
    pub schema: Cow<'static, str>,
    pub segment_seq: u64,
    pub frame_seq: u64,
    pub entry_seq: u64,
    pub entry_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_entry_id: Option<String>,
    pub entry_type: String,
    pub timestamp: String,
    pub payload_sha256: String,
    pub payload_bytes: u64,
    pub payload: Box<RawValue>,
}

impl SegmentFrame {
    fn new(
        segment_seq: u64,
        frame_seq: u64,
        entry_seq: u64,
        entry_id: String,
        parent_entry_id: Option<String>,
        entry_type: String,
        payload: Box<RawValue>,
    ) -> Result<Self> {
        let (payload_sha256, payload_bytes) = payload_hash_and_size(&payload)?;
        Ok(Self {
            schema: Cow::Borrowed(SEGMENT_FRAME_SCHEMA),
            segment_seq,
            frame_seq,
            entry_seq,
            entry_id,
            parent_entry_id,
            entry_type,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            payload_sha256,
            payload_bytes,
            payload,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OffsetIndexEntry {
    pub schema: Cow<'static, str>,
    pub entry_seq: u64,
    pub entry_id: String,
    pub segment_seq: u64,
    pub frame_seq: u64,
    pub byte_offset: u64,
    pub byte_length: u64,
    pub crc32c: String,
    pub state: Cow<'static, str>,
}

/// Current head position of the store (last written entry).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StoreHead {
    pub segment_seq: u64,
    pub entry_seq: u64,
    pub entry_id: String,
}

/// Periodic checkpoint snapshot metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub schema: String,
    pub checkpoint_seq: u64,
    pub at: String,
    pub head_entry_seq: u64,
    pub head_entry_id: String,
    pub snapshot_ref: String,
    pub compacted_before_entry_seq: u64,
    pub chain_hash: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema: String,
    pub store_version: u8,
    pub session_id: String,
    pub source_format: String,
    pub created_at: String,
    pub updated_at: String,
    pub head: StoreHead,
    pub counters: ManifestCounters,
    pub files: ManifestFiles,
    pub integrity: ManifestIntegrity,
    pub invariants: ManifestInvariants,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestCounters {
    pub entries_total: u64,
    pub messages_total: u64,
    pub branches_total: u64,
    pub compactions_total: u64,
    pub bytes_total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFiles {
    pub segment_dir: String,
    pub segment_count: u64,
    pub index_path: String,
    pub checkpoint_dir: String,
    pub migration_ledger_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestIntegrity {
    pub chain_hash: String,
    pub manifest_hash: String,
    pub last_crc32c: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)] // invariants are naturally boolean checks
pub struct ManifestInvariants {
    pub parent_links_closed: bool,
    pub monotonic_entry_seq: bool,
    pub monotonic_segment_seq: bool,
    pub index_within_segment_bounds: bool,
    pub branch_heads_indexed: bool,
    pub checkpoints_monotonic: bool,
    pub hash_chain_valid: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MigrationVerification {
    pub entry_count_match: bool,
    pub hash_chain_match: bool,
    pub index_consistent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MigrationEvent {
    pub schema: String,
    pub migration_id: String,
    pub phase: String,
    pub at: String,
    pub source_path: String,
    pub target_path: String,
    pub source_format: String,
    pub target_format: String,
    pub verification: MigrationVerification,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
    pub correlation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IndexSummary {
    pub entry_count: u64,
    pub first_entry_seq: u64,
    pub last_entry_seq: u64,
    pub last_entry_id: String,
}

#[derive(Debug, Clone)]
pub struct SessionStoreV2 {
    root: PathBuf,
    max_segment_bytes: u64,
    next_segment_seq: u64,
    next_frame_seq: u64,
    next_entry_seq: u64,
    current_segment_bytes: u64,
    /// Running SHA-256 hash chain: `H(prev_chain || payload_sha256)`.
    chain_hash: String,
    /// Total bytes written across all segments.
    total_bytes: u64,
    /// Last entry ID written (for head tracking).
    last_entry_id: Option<String>,
    /// Last CRC32-C written (for integrity checkpoints).
    last_crc32c: String,
}

impl SessionStoreV2 {
    /// Open a store handle for read-only inspection without bootstrap recovery.
    pub fn open_for_inspection(root: impl AsRef<Path>, max_segment_bytes: u64) -> Result<Self> {
        if max_segment_bytes == 0 {
            return Err(Error::validation("max_segment_bytes must be > 0"));
        }

        Ok(Self {
            root: root.as_ref().to_path_buf(),
            max_segment_bytes,
            next_segment_seq: 1,
            next_frame_seq: 1,
            next_entry_seq: 1,
            current_segment_bytes: 0,
            chain_hash: GENESIS_CHAIN_HASH.to_string(),
            total_bytes: 0,
            last_entry_id: None,
            last_crc32c: "00000000".to_string(),
        })
    }

    pub fn create(root: impl AsRef<Path>, max_segment_bytes: u64) -> Result<Self> {
        if max_segment_bytes == 0 {
            return Err(Error::validation("max_segment_bytes must be > 0"));
        }

        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("segments"))?;
        fs::create_dir_all(root.join("index"))?;
        fs::create_dir_all(root.join("checkpoints"))?;
        fs::create_dir_all(root.join("migrations"))?;
        fs::create_dir_all(root.join("tmp"))?;

        let mut store = Self {
            root,
            max_segment_bytes,
            next_segment_seq: 1,
            next_frame_seq: 1,
            next_entry_seq: 1,
            current_segment_bytes: 0,
            chain_hash: GENESIS_CHAIN_HASH.to_string(),
            total_bytes: 0,
            last_entry_id: None,
            last_crc32c: "00000000".to_string(),
        };
        if let Err(err) = store.bootstrap_from_disk() {
            if is_recoverable_index_error(&err) {
                tracing::warn!(
                    root = %store.root.display(),
                    error = %err,
                    "SessionStoreV2 bootstrap failed with recoverable index error; attempting index rebuild"
                );
                store.rebuild_index()?;
                store.bootstrap_from_disk()?;
            } else {
                return Err(err);
            }
        }

        // Recovery path: segments exist but index file is missing or empty.
        // Rebuild from segment frames so resume does not appear as an empty session.
        if store.entry_count() == 0 && store.segments_exist_with_data()? {
            tracing::warn!(
                root = %store.root.display(),
                "SessionStoreV2 detected segment data with empty index; rebuilding index"
            );
            store.rebuild_index()?;
            store.bootstrap_from_disk()?;
        }

        if let Err(err) = store.validate_integrity() {
            if is_recoverable_index_error(&err) {
                tracing::warn!(
                    root = %store.root.display(),
                    error = %err,
                    "SessionStoreV2 integrity validation failed with recoverable error; rebuilding index"
                );
                store.rebuild_index()?;
                store.bootstrap_from_disk()?;
                store.validate_integrity()?;
            } else {
                return Err(err);
            }
        }
        Ok(store)
    }

    pub fn segment_file_path(&self, segment_seq: u64) -> PathBuf {
        self.root
            .join("segments")
            .join(format!("{segment_seq:016}.seg"))
    }

    pub fn index_file_path(&self) -> PathBuf {
        self.root.join("index").join("offsets.jsonl")
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    fn migration_ledger_path(&self) -> PathBuf {
        self.root.join("migrations").join("ledger.jsonl")
    }

    fn list_segment_files(&self) -> Result<Vec<(u64, PathBuf)>> {
        let segments_dir = self.root.join("segments");
        if !segments_dir.exists() {
            return Ok(Vec::new());
        }

        let mut segment_files = Vec::new();
        for entry in fs::read_dir(segments_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("seg") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(segment_seq) = stem.parse::<u64>() else {
                continue;
            };
            segment_files.push((segment_seq, path));
        }
        segment_files.sort_by_key(|(segment_seq, _)| *segment_seq);
        Ok(segment_files)
    }

    fn segments_exist_with_data(&self) -> Result<bool> {
        for (_, path) in self.list_segment_files()? {
            if fs::metadata(path)?.len() > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn append_entry(
        &mut self,
        entry_id: impl Into<String>,
        parent_entry_id: Option<String>,
        entry_type: impl Into<String>,
        payload: Value,
    ) -> Result<OffsetIndexEntry> {
        let entry_id = entry_id.into();
        let entry_type = entry_type.into();

        // Convert the generic Value into a RawValue (string slice) to avoid
        // re-serializing the payload when writing the full frame.
        // We do this by first serializing the Value to a string, then
        // creating a Box<RawValue> from it.
        let raw_string = serde_json::to_string(&payload)?;
        let raw_payload = RawValue::from_string(raw_string)
            .map_err(|e| Error::session(format!("failed to convert payload to RawValue: {e}")))?;

        let mut frame = SegmentFrame::new(
            self.next_segment_seq,
            self.next_frame_seq,
            self.next_entry_seq,
            entry_id,
            parent_entry_id,
            entry_type,
            raw_payload,
        )?;
        let mut encoded = serde_json::to_vec(&frame)?;
        let mut line_len = line_length_u64(&encoded)?;

        if self.current_segment_bytes > 0
            && self.current_segment_bytes.saturating_add(line_len) > self.max_segment_bytes
        {
            self.next_segment_seq = self
                .next_segment_seq
                .checked_add(1)
                .ok_or_else(|| Error::session("segment sequence overflow"))?;
            self.next_frame_seq = 1;
            self.current_segment_bytes = 0;

            frame = SegmentFrame::new(
                self.next_segment_seq,
                self.next_frame_seq,
                self.next_entry_seq,
                frame.entry_id.clone(),
                frame.parent_entry_id.clone(),
                frame.entry_type.clone(),
                frame.payload.clone(),
            )?;
            encoded = serde_json::to_vec(&frame)?;
            line_len = line_length_u64(&encoded)?;
        }

        let segment_path = self.segment_file_path(self.next_segment_seq);

        // Prepare the write buffer by appending the newline to the encoded JSON
        let mut write_buf = encoded;
        write_buf.push(b'\n');

        let is_new_segment = self.next_frame_seq == 1;
        let mut segment = secure_open_options()
            .create(true)
            .write(true)
            .truncate(is_new_segment)
            .open(&segment_path)?;

        let byte_offset = segment.seek(SeekFrom::End(0))?;
        if let Err(e) = segment.write_all(&write_buf) {
            let _ = segment.set_len(byte_offset);
            return Err(Error::from(e));
        }

        // Use write_buf (which includes the newline) for CRC calculation
        let crc = crc32c_upper(&write_buf);
        let index_entry = OffsetIndexEntry {
            schema: Cow::Borrowed(OFFSET_INDEX_SCHEMA),
            entry_seq: frame.entry_seq,
            entry_id: frame.entry_id.clone(),
            segment_seq: frame.segment_seq,
            frame_seq: frame.frame_seq,
            byte_offset,
            byte_length: line_len,
            crc32c: crc.clone(),
            state: Cow::Borrowed("active"),
        };

        if let Err(e) = append_jsonl_line(&self.index_file_path(), &index_entry) {
            // Rollback: truncate segment to remove the unindexed frame.
            let _ = segment.set_len(byte_offset);
            return Err(e);
        }

        self.chain_hash = chain_hash_step(&self.chain_hash, &frame.payload_sha256);
        self.total_bytes = self.total_bytes.saturating_add(line_len);
        self.last_entry_id = Some(frame.entry_id);
        self.last_crc32c = crc;

        self.next_entry_seq = self
            .next_entry_seq
            .checked_add(1)
            .ok_or_else(|| Error::session("entry sequence overflow"))?;
        self.next_frame_seq = self
            .next_frame_seq
            .checked_add(1)
            .ok_or_else(|| Error::session("frame sequence overflow"))?;
        self.current_segment_bytes = self.current_segment_bytes.saturating_add(line_len);

        Ok(index_entry)
    }

    pub fn read_segment(&self, segment_seq: u64) -> Result<Vec<SegmentFrame>> {
        let path = self.segment_file_path(segment_seq);
        if !path.exists() {
            return Ok(Vec::new());
        }
        read_jsonl::<SegmentFrame>(&path)
    }

    pub fn read_index(&self) -> Result<Vec<OffsetIndexEntry>> {
        let path = self.index_file_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        read_jsonl::<OffsetIndexEntry>(&path)
    }

    /// Seek to a specific entry by `entry_seq` using the offset index.
    /// Returns `None` if the entry is not found.
    pub fn lookup_entry(&self, target_entry_seq: u64) -> Result<Option<SegmentFrame>> {
        let index_rows = self.read_index()?;
        let row = index_rows.iter().find(|r| r.entry_seq == target_entry_seq);
        let Some(row) = row else {
            return Ok(None);
        };
        SegmentFileReader::new(self).read_frame(row)
    }

    /// Read all entries with `entry_seq >= from_entry_seq` (tail reading).
    pub fn read_entries_from(&self, from_entry_seq: u64) -> Result<Vec<SegmentFrame>> {
        let index_rows = self.read_index()?;
        let mut frames = Vec::new();
        let mut reader = SegmentFileReader::new(self);
        for row in &index_rows {
            if row.entry_seq < from_entry_seq {
                continue;
            }
            if let Some(frame) = reader.read_frame(row)? {
                frames.push(frame);
            }
        }
        Ok(frames)
    }

    /// Read all entries across all segments in entry_seq order.
    pub fn read_all_entries(&self) -> Result<Vec<SegmentFrame>> {
        self.read_entries_from(1)
    }

    /// Read the last `count` entries by entry_seq using the offset index.
    pub fn read_tail_entries(&self, count: u64) -> Result<Vec<SegmentFrame>> {
        let index_rows = self.read_index()?;
        let total = index_rows.len();
        let skip = total.saturating_sub(usize::try_from(count).unwrap_or(usize::MAX));
        let mut frames = Vec::with_capacity(total.saturating_sub(skip));
        let mut reader = SegmentFileReader::new(self);
        for row in &index_rows[skip..] {
            if let Some(frame) = reader.read_frame(row)? {
                frames.push(frame);
            }
        }
        Ok(frames)
    }

    /// Read entries on the active branch from `leaf_entry_id` back to root.
    /// Returns frames in root→leaf order.
    pub fn read_active_path(&self, leaf_entry_id: &str) -> Result<Vec<SegmentFrame>> {
        let index_rows = self.read_index()?;
        let mut id_to_row: std::collections::HashMap<&str, &OffsetIndexEntry> =
            std::collections::HashMap::with_capacity(index_rows.len());
        for row in &index_rows {
            if id_to_row.insert(row.entry_id.as_str(), row).is_some() {
                return Err(Error::session(format!(
                    "duplicate entry_id detected while reading active path: {}",
                    row.entry_id
                )));
            }
        }

        let mut frames = Vec::new();
        let mut current_id: Option<String> = Some(leaf_entry_id.to_string());
        let mut reader = SegmentFileReader::new(self);
        let mut visited = std::collections::HashSet::new();
        while let Some(ref entry_id) = current_id {
            if !visited.insert(entry_id.clone()) {
                return Err(Error::session(format!(
                    "cyclic parent chain detected while reading active path at entry_id={entry_id}"
                )));
            }
            let Some(&row) = id_to_row.get(entry_id.as_str()) else {
                if frames.is_empty() {
                    break;
                }
                return Err(Error::session(format!(
                    "missing parent entry detected while reading active path at entry_id={entry_id}"
                )));
            };
            match reader.read_frame(row)? {
                Some(frame) => {
                    if frame.entry_id != row.entry_id {
                        return Err(Error::session(format!(
                            "active path index/frame mismatch for entry_id={} frame={}",
                            row.entry_id, frame.entry_id
                        )));
                    }
                    current_id.clone_from(&frame.parent_entry_id);
                    frames.push(frame);
                }
                None => {
                    return Err(Error::session(format!(
                        "index references missing frame while reading active path at entry_id={entry_id}"
                    )));
                }
            }
        }
        frames.reverse();
        Ok(frames)
    }

    /// Total number of entries appended so far.
    pub const fn entry_count(&self) -> u64 {
        self.next_entry_seq.saturating_sub(1)
    }

    /// Current head position, or `None` if the store is empty.
    pub fn head(&self) -> Option<StoreHead> {
        self.last_entry_id.as_ref().map(|entry_id| StoreHead {
            segment_seq: self.next_segment_seq,
            entry_seq: self.next_entry_seq.saturating_sub(1),
            entry_id: entry_id.clone(),
        })
    }

    fn checkpoint_path(&self, checkpoint_seq: u64) -> PathBuf {
        self.root
            .join("checkpoints")
            .join(format!("{checkpoint_seq:016}.json"))
    }

    /// Create a checkpoint snapshot at the current head.
    pub fn create_checkpoint(&self, checkpoint_seq: u64, reason: &str) -> Result<Checkpoint> {
        let head = self.head().unwrap_or(StoreHead {
            segment_seq: 0,
            entry_seq: 0,
            entry_id: String::new(),
        });
        let snapshot_ref = format!("checkpoints/{checkpoint_seq:016}.json");
        let checkpoint = Checkpoint {
            schema: CHECKPOINT_SCHEMA.to_string(),
            checkpoint_seq,
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            head_entry_seq: head.entry_seq,
            head_entry_id: head.entry_id,
            snapshot_ref,
            compacted_before_entry_seq: 0,
            chain_hash: self.chain_hash.clone(),
            reason: reason.to_string(),
        };
        let tmp_path = self
            .root
            .join("tmp")
            .join(format!("{checkpoint_seq:016}.json.tmp"));

        let write_result: Result<()> = (|| {
            let mut file = secure_open_options()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)?;
            file.write_all(&serde_json::to_vec_pretty(&checkpoint)?)?;
            file.sync_all()?;
            Ok(())
        })();

        if let Err(err) = write_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(err);
        }

        let target_path = self.checkpoint_path(checkpoint_seq);
        fs::rename(&tmp_path, &target_path)?;
        sync_parent_dir(&target_path)?;
        Ok(checkpoint)
    }

    /// Read a checkpoint by sequence number.
    pub fn read_checkpoint(&self, checkpoint_seq: u64) -> Result<Option<Checkpoint>> {
        let path = self.checkpoint_path(checkpoint_seq);
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(&path)?;
        let cp: Checkpoint = serde_json::from_str(&data)?;
        Ok(Some(cp))
    }

    pub fn append_migration_event(&self, mut event: MigrationEvent) -> Result<()> {
        if event.schema.is_empty() {
            event.schema = MIGRATION_EVENT_SCHEMA.to_string();
        }
        if event.at.is_empty() {
            event.at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        }
        append_jsonl_line(&self.migration_ledger_path(), &event)
    }

    pub fn read_migration_events(&self) -> Result<Vec<MigrationEvent>> {
        let path = self.migration_ledger_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        read_jsonl::<MigrationEvent>(&path)
    }

    #[allow(clippy::too_many_lines)]
    pub fn rollback_to_checkpoint(
        &mut self,
        checkpoint_seq: u64,
        migration_id: impl Into<String>,
        correlation_id: impl Into<String>,
    ) -> Result<MigrationEvent> {
        let migration_id = migration_id.into();
        let correlation_id = correlation_id.into();

        let rollback_result: Result<MigrationEvent> = (|| {
            let checkpoint = self
                .read_checkpoint(checkpoint_seq)?
                .ok_or_else(|| Error::session(format!("checkpoint {checkpoint_seq} not found")))?;

            let mut index_rows = self.read_index()?;
            index_rows.retain(|row| row.entry_seq <= checkpoint.head_entry_seq);

            let mut keep_len_by_segment: std::collections::HashMap<u64, u64> =
                std::collections::HashMap::new();
            for row in &index_rows {
                let end = row
                    .byte_offset
                    .checked_add(row.byte_length)
                    .ok_or_else(|| Error::session("index byte range overflow during rollback"))?;
                keep_len_by_segment
                    .entry(row.segment_seq)
                    .and_modify(|current| *current = (*current).max(end))
                    .or_insert(end);
            }

            let segments_dir = self.root.join("segments");
            if segments_dir.exists() {
                let mut segment_files: Vec<(u64, PathBuf)> = Vec::new();
                for entry in fs::read_dir(&segments_dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("seg") {
                        continue;
                    }
                    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    let Ok(segment_seq) = stem.parse::<u64>() else {
                        continue;
                    };
                    segment_files.push((segment_seq, path));
                }
                segment_files.sort_by_key(|(segment_seq, _)| *segment_seq);

                for (segment_seq, path) in segment_files {
                    match keep_len_by_segment.get(&segment_seq).copied() {
                        Some(keep_len) if keep_len > 0 => {
                            let current_len = fs::metadata(&path)?.len();
                            if keep_len < current_len {
                                truncate_file_to(&path, keep_len)?;
                            }
                        }
                        _ => {
                            fs::remove_file(&path)?;
                        }
                    }
                }
            }

            let index_path = self.index_file_path();
            let index_tmp = self.root.join("tmp").join("offsets.rollback.tmp");
            if let Some(parent) = index_tmp.parent() {
                let _ = fs::create_dir_all(parent);
            }
            write_jsonl_lines(&index_tmp, &index_rows)?;
            fs::rename(&index_tmp, &index_path)?;
            let _ = sync_parent_dir(&index_path);

            self.next_segment_seq = 1;
            self.next_frame_seq = 1;
            self.next_entry_seq = 1;
            self.current_segment_bytes = 0;
            self.bootstrap_from_disk()?;

            let verification = MigrationVerification {
                entry_count_match: self.entry_count() == checkpoint.head_entry_seq,
                hash_chain_match: self.chain_hash == checkpoint.chain_hash,
                index_consistent: self.validate_integrity().is_ok(),
            };

            let (outcome, error_class) = if verification.entry_count_match
                && verification.hash_chain_match
                && verification.index_consistent
            {
                ("ok".to_string(), None)
            } else if verification.index_consistent {
                (
                    "recoverable_error".to_string(),
                    Some("integrity_mismatch".to_string()),
                )
            } else {
                (
                    "fatal_error".to_string(),
                    Some("index_corruption".to_string()),
                )
            };

            let event = MigrationEvent {
                schema: MIGRATION_EVENT_SCHEMA.to_string(),
                migration_id: migration_id.clone(),
                phase: "rollback".to_string(),
                at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                source_path: checkpoint.snapshot_ref,
                target_path: self.root.display().to_string(),
                source_format: "native_v2".to_string(),
                target_format: "native_v2".to_string(),
                verification,
                outcome: outcome.clone(),
                error_class,
                correlation_id: correlation_id.clone(),
            };
            self.append_migration_event(event.clone())?;

            if outcome == "ok" {
                Ok(event)
            } else {
                Err(Error::session(format!(
                    "rollback verification failed for checkpoint {checkpoint_seq}"
                )))
            }
        })();

        match rollback_result {
            Ok(event) => Ok(event),
            Err(error) => {
                if !rollback_failure_event_already_recorded(&error) {
                    let failure_event = MigrationEvent {
                        schema: MIGRATION_EVENT_SCHEMA.to_string(),
                        migration_id,
                        phase: "rollback".to_string(),
                        at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        source_path: self.checkpoint_path(checkpoint_seq).display().to_string(),
                        target_path: self.root.display().to_string(),
                        source_format: "native_v2".to_string(),
                        target_format: "native_v2".to_string(),
                        verification: MigrationVerification {
                            entry_count_match: false,
                            hash_chain_match: false,
                            index_consistent: false,
                        },
                        outcome: "fatal_error".to_string(),
                        error_class: Some(classify_rollback_error(&error).to_string()),
                        correlation_id,
                    };
                    let _ = self.append_migration_event(failure_event);
                }
                Err(error)
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn write_manifest(
        &self,
        session_id: impl Into<String>,
        source_format: impl Into<String>,
    ) -> Result<Manifest> {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let created_at = self
            .read_manifest()?
            .map_or_else(|| now.clone(), |m| m.created_at);
        let session_id = session_id.into();
        let source_format = source_format.into();
        let index_rows = self.read_index()?;

        let mut parent_counts: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        let mut message_count = 0u64;
        let mut compaction_count = 0u64;
        let mut entry_ids = std::collections::HashSet::with_capacity(index_rows.len());

        let mut recomputed_chain = GENESIS_CHAIN_HASH.to_string();
        let mut parent_links_closed = true;
        let mut reader = SegmentFileReader::new(self);

        for row in &index_rows {
            if let Some(frame) = reader.read_frame(row)? {
                entry_ids.insert(frame.entry_id.clone());

                if frame.entry_type == "message" {
                    message_count = message_count.saturating_add(1);
                }
                if frame.entry_type == "compaction" {
                    compaction_count = compaction_count.saturating_add(1);
                }

                if let Some(parent_id) = frame.parent_entry_id.as_deref() {
                    *parent_counts.entry(parent_id.to_string()).or_insert(0) += 1;

                    // In a valid append-only log, the parent must have appeared
                    // (and thus been added to entry_ids) before the child.
                    if !entry_ids.contains(parent_id) {
                        parent_links_closed = false;
                    }
                }

                recomputed_chain = chain_hash_step(&recomputed_chain, &frame.payload_sha256);
            }
        }

        let branches_total = u64::try_from(parent_counts.values().filter(|&&n| n > 1).count())
            .map_err(|_| Error::session("branch count exceeds u64"))?;

        let mut monotonic_entry_seq = true;
        let mut monotonic_segment_seq = true;
        let mut last_entry_seq = 0u64;
        let mut last_segment_seq = 0u64;
        for row in &index_rows {
            if row.entry_seq <= last_entry_seq {
                monotonic_entry_seq = false;
            }
            if row.segment_seq < last_segment_seq {
                monotonic_segment_seq = false;
            }
            last_entry_seq = row.entry_seq;
            last_segment_seq = row.segment_seq;
        }

        let hash_chain_valid = recomputed_chain == self.chain_hash;

        let head = self.head().unwrap_or(StoreHead {
            segment_seq: 0,
            entry_seq: 0,
            entry_id: String::new(),
        });
        let segment_count = u64::try_from(
            index_rows
                .iter()
                .map(|row| row.segment_seq)
                .collect::<BTreeSet<_>>()
                .len(),
        )
        .map_err(|_| Error::session("segment count exceeds u64"))?;

        let mut manifest = Manifest {
            schema: MANIFEST_SCHEMA.to_string(),
            store_version: 2,
            session_id,
            source_format,
            created_at,
            updated_at: now,
            head,
            counters: ManifestCounters {
                entries_total: u64::try_from(index_rows.len())
                    .map_err(|_| Error::session("entry count exceeds u64"))?,
                messages_total: message_count,
                branches_total,
                compactions_total: compaction_count,
                bytes_total: self.total_bytes,
            },
            files: ManifestFiles {
                segment_dir: "segments/".to_string(),
                segment_count,
                index_path: "index/offsets.jsonl".to_string(),
                checkpoint_dir: "checkpoints/".to_string(),
                migration_ledger_path: "migrations/ledger.jsonl".to_string(),
            },
            integrity: ManifestIntegrity {
                chain_hash: self.chain_hash.clone(),
                manifest_hash: String::new(),
                last_crc32c: self.last_crc32c.clone(),
            },
            invariants: ManifestInvariants {
                parent_links_closed,
                monotonic_entry_seq,
                monotonic_segment_seq,
                index_within_segment_bounds: self.validate_integrity().is_ok(),
                branch_heads_indexed: true,
                checkpoints_monotonic: true,
                hash_chain_valid,
            },
        };
        manifest.integrity.manifest_hash = manifest_hash_hex(&manifest)?;

        let tmp = self.root.join("tmp").join("manifest.json.tmp");

        let write_result: Result<()> = (|| {
            let mut file = secure_open_options()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            file.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
            file.sync_all()?;
            Ok(())
        })();

        if let Err(err) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }

        let target_path = self.manifest_path();
        fs::rename(&tmp, &target_path)?;
        sync_parent_dir(&target_path)?;
        Ok(manifest)
    }

    pub fn read_manifest(&self) -> Result<Option<Manifest>> {
        let path = self.manifest_path();
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&path)?;
        let manifest: Manifest = serde_json::from_str(&content).map_err(|err| {
            Error::session(format!(
                "Failed to parse manifest {}: {err}",
                path.display()
            ))
        })?;
        Ok(Some(manifest))
    }

    pub fn chain_hash(&self) -> &str {
        &self.chain_hash
    }

    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn index_summary(&self) -> Result<Option<IndexSummary>> {
        let rows = self.read_index()?;
        let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
            return Ok(None);
        };
        Ok(Some(IndexSummary {
            entry_count: u64::try_from(rows.len())
                .map_err(|_| Error::session("entry count exceeds u64"))?,
            first_entry_seq: first.entry_seq,
            last_entry_seq: last.entry_seq,
            last_entry_id: last.entry_id.clone(),
        }))
    }

    /// Rebuild the offset index by scanning all segment files.
    /// This is the recovery path when the index is missing or corrupted.
    #[allow(clippy::too_many_lines)]
    pub fn rebuild_index(&mut self) -> Result<u64> {
        let mut rebuilt_count = 0u64;
        let index_path = self.index_file_path();
        let index_tmp_path = self.root.join("tmp").join("offsets.rebuild.tmp");

        // Ensure tmp dir exists
        if let Some(parent) = index_tmp_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Start fresh with the temp file
        if index_tmp_path.exists() {
            fs::remove_file(&index_tmp_path)?;
        }

        let mut index_writer = std::io::BufWriter::new(
            secure_open_options()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&index_tmp_path)?,
        );

        self.chain_hash = GENESIS_CHAIN_HASH.to_string();
        self.total_bytes = 0;
        self.last_entry_id = None;
        self.last_crc32c = "00000000".to_string();

        let segment_files = self.list_segment_files()?;
        let mut last_observed_seq = 0u64;

        'segments: for (i, (segment_seq, seg_path)) in segment_files.iter().enumerate() {
            let file = File::open(seg_path)?;
            let mut reader = BufReader::new(file);
            let mut byte_offset = 0u64;
            let mut line_number = 0u64;
            let mut expected_frame_seq = 1u64;
            let mut line = String::new();

            loop {
                line.clear();
                // Use bounded read to prevent OOM on corrupted files (e.g. missing newlines)
                let bytes_read =
                    match read_line_with_limit(&mut reader, &mut line, MAX_FRAME_READ_BYTES) {
                        Ok(n) => n,
                        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                            return Err(Error::session(format!(
                                "failed to read segment frame while rebuilding index: \
                             segment={} line={}: {e}",
                                seg_path.display(),
                                line_number.saturating_add(1),
                            )));
                        }
                        Err(e) => return Err(Error::Io(Box::new(e))),
                    };

                if bytes_read == 0 {
                    break;
                }
                line_number = line_number.saturating_add(1);
                let mut line_len = u64::try_from(bytes_read)
                    .map_err(|_| Error::session("line length exceeds u64"))?;

                if line.trim().is_empty() {
                    byte_offset = byte_offset.saturating_add(line_len);
                    continue;
                }

                let missing_newline = !line.ends_with('\n');
                let json_line = line.trim_end_matches('\n').trim_end_matches('\r');
                let frame: SegmentFrame = match serde_json::from_str(json_line) {
                    Ok(frame) => {
                        if missing_newline {
                            use std::io::{Read, Write};
                            tracing::warn!(
                                segment = %seg_path.display(),
                                line_number,
                                "SessionStoreV2 encountered valid frame missing trailing newline; healing segment"
                            );
                            let mut f = secure_open_options().append(true).open(seg_path)?;
                            f.write_all(b"\n")?;
                            line.push('\n');
                            line_len += 1;
                            // Consume the healed newline so the reader and offset accounting stay aligned.
                            let mut healed_newline = [0u8; 1];
                            reader.read_exact(&mut healed_newline).map_err(|err| {
                                Error::session(format!(
                                    "failed to consume healed newline while rebuilding index: \
                                     segment={} line={line_number}: {err}",
                                    seg_path.display()
                                ))
                            })?;
                            if healed_newline[0] != b'\n' {
                                return Err(Error::session(format!(
                                    "healed newline read back as non-newline byte while rebuilding index: \
                                     segment={} line={line_number}: 0x{:02X}",
                                    seg_path.display(),
                                    healed_newline[0]
                                )));
                            }
                        }
                        frame
                    }
                    Err(err) => {
                        let at_eof = reader.fill_buf().is_ok_and(<[u8]>::is_empty);
                        if !at_eof || !missing_newline {
                            return Err(Error::session(format!(
                                "failed to parse segment frame while rebuilding index: \
                                 segment={} line={line_number}: {err}",
                                seg_path.display()
                            )));
                        }
                        tracing::warn!(
                            segment = %seg_path.display(),
                            line_number,
                            error = %err,
                            at_eof,
                            missing_newline,
                            "SessionStoreV2 dropping corrupted frame during index rebuild; truncating segment and quarantining subsequent segments"
                        );
                        // Trim the incomplete tail so subsequent reads and appends remain valid.
                        drop(reader);
                        truncate_file_to(seg_path, byte_offset)?;
                        quarantine_segment_tail(&segment_files[i + 1..])?;
                        break 'segments;
                    }
                };

                if frame.segment_seq != *segment_seq || frame.frame_seq != expected_frame_seq {
                    tracing::warn!(
                        segment = %seg_path.display(),
                        line_number,
                        expected_segment_seq = *segment_seq,
                        actual_segment_seq = frame.segment_seq,
                        expected_frame_seq,
                        actual_frame_seq = frame.frame_seq,
                        "SessionStoreV2 detected mismatched embedded frame coordinates during rebuild; truncating segment and quarantining subsequent segments"
                    );
                    drop(reader);
                    truncate_file_to(seg_path, byte_offset)?;
                    quarantine_segment_tail(&segment_files[i + 1..])?;
                    break 'segments;
                }

                if frame.entry_seq <= last_observed_seq {
                    tracing::warn!(
                        segment = %seg_path.display(),
                        line_number,
                        entry_seq = frame.entry_seq,
                        last_seq = last_observed_seq,
                        "SessionStoreV2 detected non-monotonic entry sequence during rebuild; truncating segment and quarantining subsequent segments"
                    );
                    drop(reader);
                    truncate_file_to(seg_path, byte_offset)?;
                    quarantine_segment_tail(&segment_files[i + 1..])?;
                    break 'segments;
                }
                last_observed_seq = frame.entry_seq;

                let crc = crc32c_upper(line.as_bytes());

                let index_entry = OffsetIndexEntry {
                    schema: Cow::Borrowed(OFFSET_INDEX_SCHEMA),
                    entry_seq: frame.entry_seq,
                    entry_id: frame.entry_id.clone(),
                    segment_seq: *segment_seq,
                    frame_seq: expected_frame_seq,
                    byte_offset,
                    byte_length: line_len,
                    crc32c: crc.clone(),
                    state: Cow::Borrowed("active"),
                };
                serde_json::to_writer(&mut index_writer, &index_entry)?;
                index_writer.write_all(b"\n")?;

                self.chain_hash = chain_hash_step(&self.chain_hash, &frame.payload_sha256);
                self.total_bytes = self.total_bytes.saturating_add(line_len);
                self.last_entry_id = Some(frame.entry_id);
                self.last_crc32c = crc;

                byte_offset = byte_offset.saturating_add(line_len);
                rebuilt_count = rebuilt_count.saturating_add(1);
                expected_frame_seq = expected_frame_seq
                    .checked_add(1)
                    .ok_or_else(|| Error::session("frame sequence overflow during rebuild"))?;
            }
        }

        index_writer.flush()?;
        let file = index_writer
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)?;
        file.sync_all()?;
        drop(file); // Close the file handle before renaming (fixes Windows ERROR_SHARING_VIOLATION)

        // Atomically replace the old index with the rebuilt one
        fs::rename(&index_tmp_path, &index_path)?;
        sync_parent_dir(&index_path)?;

        self.next_segment_seq = 1;
        self.next_frame_seq = 1;
        self.next_entry_seq = 1;
        self.current_segment_bytes = 0;
        self.bootstrap_from_disk()?;

        Ok(rebuilt_count)
    }

    pub fn validate_integrity(&self) -> Result<()> {
        let index_rows = self.read_index()?;
        let mut last_entry_seq = 0;
        let mut parent_by_entry: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::with_capacity(index_rows.len());

        // Group rows by segment to minimize file opens
        let mut rows_by_segment: std::collections::BTreeMap<u64, Vec<&OffsetIndexEntry>> =
            std::collections::BTreeMap::new();
        for row in &index_rows {
            if row.entry_seq <= last_entry_seq {
                return Err(Error::session(format!(
                    "entry sequence is not strictly increasing at entry_seq={}",
                    row.entry_seq
                )));
            }
            last_entry_seq = row.entry_seq;
            rows_by_segment
                .entry(row.segment_seq)
                .or_default()
                .push(row);
        }

        for (segment_seq, rows) in rows_by_segment {
            let segment_path = self.segment_file_path(segment_seq);
            let mut file = File::open(&segment_path).map_err(|err| {
                Error::session(format!(
                    "failed to open segment {}: {err}",
                    segment_path.display()
                ))
            })?;
            let segment_len = file.metadata()?.len();

            for row in rows {
                let end = row
                    .byte_offset
                    .checked_add(row.byte_length)
                    .ok_or_else(|| Error::session("index byte range overflow"))?;
                if end > segment_len {
                    return Err(Error::session(format!(
                        "index out of bounds for segment {}: end={} len={segment_len}",
                        segment_path.display(),
                        end
                    )));
                }

                file.seek(SeekFrom::Start(row.byte_offset))?;
                let mut record_bytes = vec![
                    0u8;
                    usize::try_from(row.byte_length).map_err(|_| {
                        Error::session(format!("byte length too large: {}", row.byte_length))
                    })?
                ];
                file.read_exact(&mut record_bytes)?;

                let checksum = crc32c_upper(&record_bytes);
                if checksum != row.crc32c {
                    return Err(Error::session(format!(
                        "checksum mismatch for entry_seq={} expected={} actual={checksum}",
                        row.entry_seq, row.crc32c
                    )));
                }

                if record_bytes.last() == Some(&b'\n') {
                    record_bytes.pop();
                }
                let frame: SegmentFrame = serde_json::from_slice(&record_bytes)?;

                if frame.entry_seq != row.entry_seq
                    || frame.entry_id != row.entry_id
                    || frame.segment_seq != row.segment_seq
                    || frame.frame_seq != row.frame_seq
                {
                    return Err(Error::session(format!(
                        "index/frame mismatch at entry_seq={}",
                        row.entry_seq
                    )));
                }

                let (payload_hash, payload_bytes) = payload_hash_and_size(&frame.payload)?;
                if frame.payload_sha256 != payload_hash || frame.payload_bytes != payload_bytes {
                    return Err(Error::session(format!(
                        "payload integrity mismatch at entry_seq={}",
                        row.entry_seq
                    )));
                }

                if parent_by_entry
                    .insert(frame.entry_id.clone(), frame.parent_entry_id.clone())
                    .is_some()
                {
                    return Err(Error::session(format!(
                        "duplicate entry_id detected in session store: {}",
                        frame.entry_id
                    )));
                }
            }
        }

        validate_parent_graph_links(&parent_by_entry)?;
        validate_parent_graph_acyclic(&parent_by_entry)?;

        Ok(())
    }

    fn bootstrap_from_disk(&mut self) -> Result<()> {
        let index_rows = self.read_index()?;
        if let Some(last) = index_rows.last() {
            self.next_entry_seq = last
                .entry_seq
                .checked_add(1)
                .ok_or_else(|| Error::session("entry sequence overflow while bootstrapping"))?;
            self.next_segment_seq = last.segment_seq;
            self.next_frame_seq = last
                .frame_seq
                .checked_add(1)
                .ok_or_else(|| Error::session("frame sequence overflow while bootstrapping"))?;
            let segment_path = self.segment_file_path(last.segment_seq);
            let expected_segment_bytes = last.byte_offset.saturating_add(last.byte_length);
            let actual_segment_bytes =
                fs::metadata(&segment_path)
                    .map(|meta| meta.len())
                    .map_err(|err| {
                        Error::session(format!(
                            "failed to stat active segment {} while bootstrapping: {err}",
                            segment_path.display()
                        ))
                    })?;

            if actual_segment_bytes > expected_segment_bytes {
                tracing::warn!(
                    segment = %segment_path.display(),
                    expected = expected_segment_bytes,
                    actual = actual_segment_bytes,
                    "SessionStoreV2 truncating unindexed trailing bytes from active segment after crash recovery"
                );
                truncate_file_to(&segment_path, expected_segment_bytes)?;
            }
            self.current_segment_bytes = expected_segment_bytes;
            self.last_entry_id = Some(last.entry_id.clone());
            self.last_crc32c.clone_from(&last.crc32c);

            let mut chain = GENESIS_CHAIN_HASH.to_string();
            let mut total = 0u64;
            let mut reader = SegmentFileReader::new(self);
            for row in &index_rows {
                let frame = reader.read_frame(row)?.ok_or_else(|| {
                    Error::session(format!(
                        "index references missing frame during bootstrap: entry_seq={}, segment={}",
                        row.entry_seq, row.segment_seq
                    ))
                })?;
                chain = chain_hash_step(&chain, &frame.payload_sha256);
                total = total.saturating_add(row.byte_length);
            }
            self.chain_hash = chain;
            self.total_bytes = total;
        } else {
            self.chain_hash = GENESIS_CHAIN_HASH.to_string();
            self.total_bytes = 0;
            self.last_entry_id = None;
            self.last_crc32c = "00000000".to_string();
        }
        Ok(())
    }
}

fn rollback_failure_event_already_recorded(error: &Error) -> bool {
    matches!(error, Error::Session(message) if message.contains("rollback verification failed"))
}

fn classify_rollback_error(error: &Error) -> &'static str {
    match error {
        Error::Session(message) => {
            if message.contains("checkpoint") && message.contains("not found") {
                "checkpoint_not_found"
            } else if message.contains("index byte range overflow") {
                "index_range_overflow"
            } else if message.contains("rollback verification failed") {
                "rollback_verification_failed"
            } else {
                "session_error"
            }
        }
        _ => error.category_code(),
    }
}

fn is_recoverable_index_error(error: &Error) -> bool {
    match error {
        Error::Json(_) => true,
        Error::Io(err) => matches!(
            err.kind(),
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::InvalidData
        ),
        Error::Session(message) => {
            let lower = message.to_ascii_lowercase();
            lower.contains("checksum mismatch")
                || lower.contains("index out of bounds")
                || lower.contains("index/frame mismatch")
                || lower.contains("index references missing frame")
                || lower.contains("payload integrity mismatch")
                || lower.contains("entry sequence is not strictly increasing")
                || lower.contains("index byte range overflow")
                || lower.contains("failed to stat active segment")
        }
        _ => false,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParentGraphVisitState {
    Visiting,
    Visited,
}

fn validate_parent_graph_links(
    parent_by_entry: &std::collections::HashMap<String, Option<String>>,
) -> Result<()> {
    for (entry_id, parent_id) in parent_by_entry {
        if let Some(parent_id) = parent_id.as_deref()
            && !parent_by_entry.contains_key(parent_id)
        {
            return Err(Error::session(format!(
                "missing parent entry detected in session store: entry_id={entry_id} parent_id={parent_id}"
            )));
        }
    }

    Ok(())
}

fn validate_parent_graph_acyclic(
    parent_by_entry: &std::collections::HashMap<String, Option<String>>,
) -> Result<()> {
    let mut visit_state: std::collections::HashMap<&str, ParentGraphVisitState> =
        std::collections::HashMap::with_capacity(parent_by_entry.len());

    for entry_id in parent_by_entry.keys() {
        if visit_state.get(entry_id.as_str()) == Some(&ParentGraphVisitState::Visited) {
            continue;
        }

        let mut stack = vec![(entry_id.as_str(), false)];
        while let Some((current_id, expanded)) = stack.pop() {
            if expanded {
                visit_state.insert(current_id, ParentGraphVisitState::Visited);
                continue;
            }

            match visit_state.get(current_id).copied() {
                Some(ParentGraphVisitState::Visited) => continue,
                Some(ParentGraphVisitState::Visiting) => {
                    return Err(Error::session(format!(
                        "cyclic parent chain detected in session store at entry_id={current_id}"
                    )));
                }
                None => {}
            }

            visit_state.insert(current_id, ParentGraphVisitState::Visiting);
            stack.push((current_id, true));

            if let Some(parent_id) = parent_by_entry
                .get(current_id)
                .and_then(std::option::Option::as_deref)
                && parent_by_entry.contains_key(parent_id)
            {
                stack.push((parent_id, false));
            }
        }
    }

    Ok(())
}

/// Convert a V2 `SegmentFrame` payload back into a `SessionEntry`.
pub fn frame_to_session_entry(frame: &SegmentFrame) -> Result<SessionEntry> {
    // Deserialize directly from the RawValue to avoid extra allocation/copying.
    // serde_json::from_str works on RawValue.get() which is &str.
    let entry: SessionEntry = serde_json::from_str(frame.payload.get()).map_err(|e| {
        Error::session(format!(
            "failed to deserialize SessionEntry from frame entry_id={}: {e}",
            frame.entry_id
        ))
    })?;

    if let Some(base_id) = entry.base_id() {
        if base_id != &frame.entry_id {
            return Err(Error::session(format!(
                "frame entry_id mismatch: frame={} entry={}",
                frame.entry_id, base_id
            )));
        }
    }

    Ok(entry)
}

/// Extract the V2 frame arguments from a `SessionEntry`.
pub fn session_entry_to_frame_args(
    entry: &SessionEntry,
) -> Result<(String, Option<String>, String, Value)> {
    let base = entry.base();
    let entry_id = base
        .id
        .clone()
        .ok_or_else(|| Error::session("SessionEntry has no id"))?;
    let parent_entry_id = base.parent_id.clone();

    let entry_type = match entry {
        SessionEntry::Message(_) => "message",
        SessionEntry::ModelChange(_) => "model_change",
        SessionEntry::ThinkingLevelChange(_) => "thinking_level_change",
        SessionEntry::Compaction(_) => "compaction",
        SessionEntry::BranchSummary(_) => "branch_summary",
        SessionEntry::Label(_) => "label",
        SessionEntry::SessionInfo(_) => "session_info",
        SessionEntry::Custom(_) => "custom",
    };

    let payload = serde_json::to_value(entry).map_err(|e| {
        Error::session(format!(
            "failed to serialize SessionEntry to frame payload: {e}"
        ))
    })?;

    Ok((entry_id, parent_entry_id, entry_type.to_string(), payload))
}

/// Helper to cache the file descriptor when reading multiple frames sequentially.
struct SegmentFileReader<'a> {
    store: &'a SessionStoreV2,
    current_segment_seq: Option<u64>,
    current_file: Option<File>,
    current_len: u64,
}

impl<'a> SegmentFileReader<'a> {
    const fn new(store: &'a SessionStoreV2) -> Self {
        Self {
            store,
            current_segment_seq: None,
            current_file: None,
            current_len: 0,
        }
    }

    fn read_frame(&mut self, row: &OffsetIndexEntry) -> Result<Option<SegmentFrame>> {
        if self.current_segment_seq != Some(row.segment_seq) {
            self.current_segment_seq = Some(row.segment_seq);
            let path = self.store.segment_file_path(row.segment_seq);
            if path.exists() {
                let file = File::open(&path)?;
                self.current_len = file.metadata()?.len();
                self.current_file = Some(file);
            } else {
                self.current_file = None;
            }
        }

        let Some(file) = self.current_file.as_mut() else {
            return Ok(None);
        };

        let end_offset = row
            .byte_offset
            .checked_add(row.byte_length)
            .ok_or_else(|| Error::session("index byte range overflow"))?;

        if end_offset > self.current_len {
            return Err(Error::session(format!(
                "index out of bounds for segment {}: end={} len={}",
                self.store.segment_file_path(row.segment_seq).display(),
                end_offset,
                self.current_len
            )));
        }

        file.seek(SeekFrom::Start(row.byte_offset))?;
        let byte_len = usize::try_from(row.byte_length)
            .map_err(|_| Error::session(format!("byte length too large: {}", row.byte_length)))?;

        if row.byte_length > self.store.max_segment_bytes.max(100 * 1024 * 1024) {
            return Err(Error::session(format!(
                "frame byte length {byte_len} exceeds limit"
            )));
        }

        let mut buf = vec![0u8; byte_len];
        file.read_exact(&mut buf)?;
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        let frame: SegmentFrame = serde_json::from_slice(&buf)?;
        Ok(Some(frame))
    }
}

/// Compute next hash chain value: `SHA-256(prev_chain_hex || payload_sha256_hex)`.
fn chain_hash_step(prev_chain: &str, payload_sha256: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev_chain.as_bytes());
    hasher.update(payload_sha256.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn manifest_hash_hex(manifest: &Manifest) -> Result<String> {
    let encoded = serde_json::to_vec(manifest)?;
    Ok(format!("{:x}", Sha256::digest(&encoded)))
}

/// Derive the V2 sidecar store root from a JSONL session file path.
pub fn v2_sidecar_path(jsonl_path: &Path) -> PathBuf {
    let stem = jsonl_path.file_stem().map_or_else(
        || "session".to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    let parent = jsonl_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{stem}.v2"))
}

/// Check whether a V2 sidecar store exists for the given JSONL session.
pub fn has_v2_sidecar(jsonl_path: &Path) -> bool {
    let root = v2_sidecar_path(jsonl_path);
    root.join("manifest.json").exists() || root.join("index").join("offsets.jsonl").exists()
}

fn append_jsonl_line<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let file = secure_open_options().create(true).append(true).open(path)?;
    let mut writer = std::io::BufWriter::new(file);
    // Serialize directly to buffered file — avoids intermediate Vec<u8> allocation
    // while preventing excessive write syscalls.
    serde_json::to_writer(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn truncate_file_to(path: &Path, len: u64) -> Result<()> {
    let file = secure_open_options()
        .write(true)
        .truncate(false)
        .open(path)?;
    file.set_len(len)?;
    file.sync_all()?;
    Ok(())
}

fn quarantine_segment_file(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::session(format!("segment has no parent: {}", path.display())))?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| Error::session(format!("segment has no filename: {}", path.display())))?;

    for suffix in 0u32..10_000 {
        let backup_name = if suffix == 0 {
            format!("{file_name}.bak")
        } else {
            format!("{file_name}.bak.{suffix}")
        };
        let backup_path = parent.join(backup_name);
        if backup_path.exists() {
            continue;
        }

        fs::rename(path, &backup_path).map_err(|err| {
            Error::session(format!(
                "failed to quarantine segment {} -> {}: {err}",
                path.display(),
                backup_path.display()
            ))
        })?;
        return Ok(backup_path);
    }

    Err(Error::session(format!(
        "failed to quarantine segment {}: exhausted backup suffixes",
        path.display()
    )))
}

fn quarantine_segment_tail(segment_files: &[(u64, PathBuf)]) -> Result<()> {
    for (_, path) in segment_files {
        let backup_path = quarantine_segment_file(path)?;
        tracing::warn!(
            segment = %path.display(),
            backup = %backup_path.display(),
            "SessionStoreV2 quarantined trailing segment during rebuild"
        );
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn write_jsonl_lines<T: Serialize>(path: &Path, rows: &[T]) -> Result<()> {
    let file = secure_open_options()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    let mut writer = std::io::BufWriter::new(file);
    for row in rows {
        serde_json::to_writer(&mut writer, row)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    let file = writer
        .into_inner()
        .map_err(std::io::IntoInnerError::into_error)?;
    file.sync_all()?;
    Ok(())
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = read_line_with_limit(&mut reader, &mut line, MAX_FRAME_READ_BYTES)
            .map_err(|e| Error::Io(Box::new(e)))?;
        if bytes_read == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let json_line = line.trim_end_matches('\n').trim_end_matches('\r');
        out.push(serde_json::from_str::<T>(json_line)?);
    }
    Ok(out)
}

fn payload_hash_and_size(payload: &RawValue) -> Result<(String, u64)> {
    // For RawValue, we can just get the string content directly.
    let bytes = payload.get().as_bytes();
    let payload_bytes = u64::try_from(bytes.len())
        .map_err(|_| Error::session(format!("payload is too large: {} bytes", bytes.len())))?;
    let hash = format!("{:x}", Sha256::digest(bytes));
    Ok((hash, payload_bytes))
}

fn line_length_u64(encoded: &[u8]) -> Result<u64> {
    let line_len = encoded
        .len()
        .checked_add(1)
        .ok_or_else(|| Error::session("line length overflow"))?;
    u64::try_from(line_len).map_err(|_| Error::session("line length exceeds u64"))
}

fn crc32c_upper(data: &[u8]) -> String {
    let crc = crc32c::crc32c(data);
    format!("{crc:08X}")
}

fn read_line_with_limit<R: BufRead>(
    reader: &mut R,
    buf: &mut String,
    limit: u64,
) -> std::io::Result<usize> {
    let mut take = reader.take(limit);
    let n = take.read_line(buf)?;
    if n > 0 && take.limit() == 0 && !buf.ends_with('\n') {
        // We reached the limit, but this might just be the exact end of the file.
        // Check if there is more data in the underlying reader.
        let is_eof = take.into_inner().fill_buf()?.is_empty();
        if !is_eof {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Line length exceeds limit of {limit} bytes"),
            ));
        }
    }
    Ok(n)
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn quarantine_segment_file_moves_segment_to_backup() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let segment = tmp.path().join("0000000000000002.seg");
        fs::write(&segment, b"hello").expect("write segment");

        let backup = quarantine_segment_file(&segment).expect("quarantine segment");

        assert_eq!(backup, tmp.path().join("0000000000000002.seg.bak"));
        assert!(!segment.exists(), "original segment should be moved away");
        assert_eq!(fs::read(&backup).expect("read backup"), b"hello");
    }

    #[test]
    fn quarantine_segment_file_uses_next_available_backup_suffix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let segment = tmp.path().join("0000000000000002.seg");
        let existing_backup = tmp.path().join("0000000000000002.seg.bak");
        fs::write(&segment, b"new").expect("write segment");
        fs::write(&existing_backup, b"old").expect("write existing backup");

        let backup = quarantine_segment_file(&segment).expect("quarantine segment");

        assert_eq!(backup, tmp.path().join("0000000000000002.seg.bak.1"));
        assert_eq!(
            fs::read(&existing_backup).expect("read existing backup"),
            b"old"
        );
        assert_eq!(fs::read(&backup).expect("read new backup"), b"new");
    }

    #[test]
    fn create_recovers_from_index_row_that_references_missing_segment() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("store");
        let mut store = SessionStoreV2::create(&root, 4096).expect("create store");
        store
            .append_entry("entry-1", None, "message", json!({"n": 1}))
            .expect("append entry");

        let mut rows = store.read_index().expect("read index");
        assert_eq!(rows.len(), 1);
        rows[0].segment_seq = 999;
        write_jsonl_lines(&store.index_file_path(), &rows).expect("write corrupted index");
        drop(store);

        let reopened = SessionStoreV2::create(&root, 4096).expect("reopen store");
        assert_eq!(reopened.entry_count(), 1);

        let rebuilt_rows = reopened.read_index().expect("read rebuilt index");
        assert_eq!(rebuilt_rows.len(), 1);
        assert_eq!(rebuilt_rows[0].segment_seq, 1);
        assert!(reopened.lookup_entry(1).expect("lookup entry").is_some());
    }

    #[test]
    fn create_drops_frame_with_mismatched_embedded_segment_seq_during_rebuild() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("store");
        let mut store = SessionStoreV2::create(&root, 4096).expect("create store");
        store
            .append_entry("entry-1", None, "message", json!({"n": 1}))
            .expect("append first entry");
        store
            .append_entry(
                "entry-2",
                Some("entry-1".to_string()),
                "message",
                json!({"n": 2}),
            )
            .expect("append second entry");

        let segment_path = store.segment_file_path(1);
        let mut frames = store.read_segment(1).expect("read segment");
        assert_eq!(frames.len(), 2);
        frames[1].segment_seq = 77;
        write_jsonl_lines(&segment_path, &frames).expect("write corrupted segment");
        fs::remove_file(store.index_file_path()).expect("remove index");
        drop(store);

        let reopened = SessionStoreV2::create(&root, 4096).expect("reopen store");
        assert_eq!(reopened.entry_count(), 1);

        let rebuilt_rows = reopened.read_index().expect("read rebuilt index");
        assert_eq!(rebuilt_rows.len(), 1);
        assert_eq!(rebuilt_rows[0].entry_seq, 1);
        assert_eq!(reopened.read_segment(1).expect("read segment").len(), 1);
        assert!(reopened.lookup_entry(2).expect("lookup entry").is_none());
    }

    // ====================================================================
    // chain_hash_step
    // ====================================================================

    proptest! {
        #[test]
        fn chain_hash_output_is_64_hex(
            a in "[0-9a-f]{64}",
            b in "[0-9a-f]{64}",
        ) {
            let result = chain_hash_step(&a, &b);
            assert_eq!(result.len(), 64);
            assert!(result.chars().all(|c| c.is_ascii_hexdigit()));
        }

        #[test]
        fn chain_hash_deterministic(
            a in "[0-9a-f]{64}",
            b in "[0-9a-f]{64}",
        ) {
            assert_eq!(chain_hash_step(&a, &b), chain_hash_step(&a, &b));
        }

        #[test]
        fn chain_hash_non_commutative(
            a in "[0-9a-f]{64}",
            b in "[0-9a-f]{64}",
        ) {
            if a != b {
                assert_ne!(chain_hash_step(&a, &b), chain_hash_step(&b, &a));
            }
        }

        #[test]
        fn chain_hash_genesis_differs_from_step(payload in "[0-9a-f]{64}") {
            let step1 = chain_hash_step(GENESIS_CHAIN_HASH, &payload);
            assert_ne!(step1, GENESIS_CHAIN_HASH);
        }
    }

    // ====================================================================
    // crc32c_upper
    // ====================================================================

    proptest! {
        #[test]
        fn crc32c_output_is_8_uppercase_hex(data in prop::collection::vec(any::<u8>(), 0..500)) {
            let result = crc32c_upper(&data);
            assert_eq!(result.len(), 8);
            assert!(result.chars().all(|c| matches!(c, '0'..='9' | 'A'..='F')));
        }

        #[test]
        fn crc32c_deterministic(data in prop::collection::vec(any::<u8>(), 0..500)) {
            assert_eq!(crc32c_upper(&data), crc32c_upper(&data));
        }

        #[test]
        fn crc32c_single_bit_sensitivity(byte in any::<u8>()) {
            let a = crc32c_upper(&[byte]);
            let b = crc32c_upper(&[byte ^ 1]);
            if byte != byte ^ 1 {
                assert_ne!(a, b, "flipping LSB should change CRC");
            }
        }
    }

    // ====================================================================
    // payload_hash_and_size
    // ====================================================================

    proptest! {
        #[test]
        fn payload_hash_is_64_hex(s in "[a-z]{0,50}") {
            let val = json!(s);
            let raw_string = serde_json::to_string(&val).unwrap();
            let raw = RawValue::from_string(raw_string).unwrap();
            let (hash, _size) = payload_hash_and_size(&raw).unwrap();
            assert_eq!(hash.len(), 64);
            assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        }

        #[test]
        fn payload_size_matches_serialization(s in "[a-z]{0,50}") {
            let val = json!(s);
            let raw_string = serde_json::to_string(&val).unwrap();
            let raw = RawValue::from_string(raw_string).unwrap();
            let (_, size) = payload_hash_and_size(&raw).unwrap();
            let expected = serde_json::to_vec(&val).unwrap().len() as u64;
            assert_eq!(size, expected);
        }

        #[test]
        fn payload_hash_deterministic(n in 0i64..10000) {
            let val = json!(n);
            let raw_string = serde_json::to_string(&val).unwrap();
            let raw = RawValue::from_string(raw_string).unwrap();
            let (h1, s1) = payload_hash_and_size(&raw).unwrap();
            let (h2, s2) = payload_hash_and_size(&raw).unwrap();
            assert_eq!(h1, h2);
            assert_eq!(s1, s2);
        }
    }

    // ====================================================================
    // line_length_u64
    // ====================================================================

    proptest! {
        #[test]
        fn line_length_is_len_plus_one(data in prop::collection::vec(any::<u8>(), 0..1000)) {
            let result = line_length_u64(&data).unwrap();
            assert_eq!(result, data.len() as u64 + 1);
        }

        #[test]
        fn line_length_never_zero(data in prop::collection::vec(any::<u8>(), 0..100)) {
            let result = line_length_u64(&data).unwrap();
            assert!(result >= 1);
        }
    }

    // ====================================================================
    // v2_sidecar_path
    // ====================================================================

    proptest! {
        #[test]
        fn sidecar_path_ends_with_v2(stem in "[a-z]{1,10}") {
            let input = PathBuf::from(format!("/tmp/{stem}.jsonl"));
            let result = v2_sidecar_path(&input);
            let name = result.file_name().unwrap().to_str().unwrap();
            assert_eq!(
                Path::new(name).extension().and_then(|ext| ext.to_str()),
                Some("v2"),
                "expected .v2 suffix, got {name}"
            );
        }

        #[test]
        fn sidecar_path_preserves_parent(stem in "[a-z]{1,10}", dir in "[a-z]{1,8}") {
            let input = PathBuf::from(format!("/tmp/{dir}/{stem}.jsonl"));
            let result = v2_sidecar_path(&input);
            assert_eq!(
                result.parent().unwrap(),
                Path::new(&format!("/tmp/{dir}"))
            );
        }

        #[test]
        fn sidecar_path_deterministic(stem in "[a-z]{1,10}") {
            let input = PathBuf::from(format!("/sessions/{stem}.jsonl"));
            assert_eq!(v2_sidecar_path(&input), v2_sidecar_path(&input));
        }

        #[test]
        fn sidecar_path_contains_stem(stem in "[a-z]{1,10}") {
            let input = PathBuf::from(format!("/tmp/{stem}.jsonl"));
            let result = v2_sidecar_path(&input);
            let name = result.file_name().unwrap().to_str().unwrap();
            assert_eq!(name, format!("{stem}.v2"));
        }
    }
}
