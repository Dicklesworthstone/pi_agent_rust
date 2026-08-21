//! File-mutation recording with `/undo` and `/redo` (bd-cv653.3.13).
//!
//! The [`FileMutationRecorder`] snapshots file content around every
//! successful `write`/`edit`/`hashline_edit` tool call so agent edits can be
//! rolled back without git. Snapshots live in an in-memory content-addressed
//! store (dedup by SHA-256) bounded by a byte budget with oldest-unit
//! eviction; history therefore dies with the process — git remains the
//! durable layer.
//!
//! Recording contract: one mutation unit per tool call (`begin_file` before
//! persisting, `commit` after success, `abort` on failure). Only tool-path
//! mutations are recorded; user or external edits are invisible here, and
//! `/undo` refuses (without `force`) when a file changed externally since the
//! recorded post-state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use sha2::Digest as _;

/// Schema tag for undo/redo session entries and tool-result details.
pub const UNDO_SCHEMA: &str = "pi.undo.v1";

/// Default byte budget for the snapshot store.
pub const DEFAULT_STORE_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// Files larger than this are not snapshotted; the unit records the gap and
/// blocks undo past it (a partial restore would corrupt interleaved state).
pub const MAX_SNAPSHOT_FILE_BYTES: u64 = 8 * 1024 * 1024;

type BlobId = [u8; 32];

/// Recorded state of one file at one point in time.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileState {
    Absent,
    Present(BlobId),
    /// The file existed but exceeded [`MAX_SNAPSHOT_FILE_BYTES`] (or was
    /// unreadable), so its content was not captured.
    Unrecorded,
}

#[derive(Debug, Clone)]
struct FileMutation {
    path: PathBuf,
    pre: FileState,
    post: FileState,
}

#[derive(Debug, Clone)]
struct MutationUnit {
    tool_name: String,
    at_ms: i64,
    files: Vec<FileMutation>,
}

/// One line of `/undo`/`/redo` reporting.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoredFile {
    pub path: String,
    pub action: String,
    pub lines_added: usize,
    pub lines_removed: usize,
}

/// Outcome of applying one mutation unit in an undo/redo direction.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedUnit {
    pub tool_name: String,
    pub files: Vec<RestoredFile>,
}

/// Why an undo/redo run stopped before applying `n` units.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum UndoStop {
    /// Nothing (more) to apply.
    Exhausted,
    /// A file changed externally since the recorded state; pass `force`.
    ExternalChange { paths: Vec<String> },
    /// The unit's snapshot was omitted (file too large/unreadable).
    SnapshotOmitted { paths: Vec<String> },
    /// Restoring a file failed mid-unit (reported, unit left on the stack).
    RestoreFailed { path: String, error: String },
}

/// Result of an `/undo` or `/redo` invocation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoOutcome {
    pub applied: Vec<AppliedUnit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped: Option<UndoStop>,
}

/// A single history row for display.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnitSummary {
    pub tool_name: String,
    pub at_ms: i64,
    pub paths: Vec<String>,
}

/// Store counters for display and eviction notices.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderStats {
    pub undo_depth: usize,
    pub redo_depth: usize,
    pub store_bytes: u64,
    pub budget_bytes: u64,
    pub evicted_units: u64,
    pub unrecorded_files: u64,
}

#[derive(Debug, Default)]
struct BlobStore {
    blobs: HashMap<BlobId, Vec<u8>>,
    refcounts: HashMap<BlobId, u64>,
    bytes: u64,
}

impl BlobStore {
    fn insert(&mut self, content: Vec<u8>) -> BlobId {
        let id: BlobId = sha2::Sha256::digest(&content).into();
        let count = self.refcounts.entry(id).or_insert(0);
        *count += 1;
        if *count == 1 {
            self.bytes += content.len() as u64;
            self.blobs.insert(id, content);
        }
        id
    }

    fn release(&mut self, id: &BlobId) {
        if let Some(count) = self.refcounts.get_mut(id) {
            *count -= 1;
            if *count == 0 {
                self.refcounts.remove(id);
                if let Some(content) = self.blobs.remove(id) {
                    self.bytes -= content.len() as u64;
                }
            }
        }
    }

    fn get(&self, id: &BlobId) -> Option<&[u8]> {
        self.blobs.get(id).map(Vec::as_slice)
    }
}

#[derive(Debug, Default)]
struct RecorderInner {
    store: BlobStore,
    undo_stack: Vec<MutationUnit>,
    redo_stack: Vec<MutationUnit>,
    pending: HashMap<String, MutationUnit>,
    evicted_units: u64,
    unrecorded_files: u64,
}

/// Records tool-path file mutations and applies `/undo` / `/redo`.
///
/// All methods take `&self`; internal state is mutex-guarded. Recording is
/// deliberately infallible from the caller's perspective — a snapshot that
/// cannot be taken degrades to an unrecorded marker instead of failing the
/// tool call it observes.
#[derive(Debug)]
pub struct FileMutationRecorder {
    inner: Mutex<RecorderInner>,
    budget_bytes: u64,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn capture_state(store: &mut BlobStore, path: &Path, unrecorded: &mut u64) -> FileState {
    match std::fs::metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return FileState::Absent,
        Err(_) => {
            *unrecorded += 1;
            return FileState::Unrecorded;
        }
        Ok(meta) => {
            if meta.len() > MAX_SNAPSHOT_FILE_BYTES {
                *unrecorded += 1;
                return FileState::Unrecorded;
            }
        }
    }
    std::fs::read(path).map_or_else(
        |_| {
            *unrecorded += 1;
            FileState::Unrecorded
        },
        |content| FileState::Present(store.insert(content)),
    )
}

/// Current on-disk state matches a recorded state? `Unrecorded` never
/// matches: units with omitted snapshots refuse replay instead of guessing.
fn state_matches_disk(state: &FileState, path: &Path) -> bool {
    match state {
        FileState::Absent => !path.exists(),
        FileState::Present(id) => std::fs::read(path)
            .is_ok_and(|content| BlobId::from(sha2::Sha256::digest(&content)) == *id),
        FileState::Unrecorded => false,
    }
}

#[allow(clippy::naive_bytecount)] // cold path: only runs during /undo reporting
fn line_count(bytes: &[u8]) -> usize {
    bytes.last().map_or(0, |last| {
        bytes.iter().filter(|b| **b == b'\n').count() + usize::from(*last != b'\n')
    })
}

impl Default for FileMutationRecorder {
    fn default() -> Self {
        Self::new(DEFAULT_STORE_BUDGET_BYTES)
    }
}

// The guard-reborrow (`let inner = &mut *inner`) trips clippy's
// significant-drop-tightening heuristic, but every method here genuinely
// needs the lock for its whole body.
#[allow(clippy::significant_drop_tightening)]
impl FileMutationRecorder {
    #[must_use]
    pub fn new(budget_bytes: u64) -> Self {
        Self {
            inner: Mutex::new(RecorderInner::default()),
            budget_bytes,
        }
    }

    /// Snapshot `path`'s pre-mutation state under `tool_call_id`. Call before
    /// persisting; repeated calls with the same id accumulate a multi-file
    /// unit (first snapshot per path wins).
    pub fn begin_file(&self, tool_call_id: &str, tool_name: &str, path: &Path) {
        let mut inner = self.inner.lock().expect("undo recorder lock"); // ubs:ignore poisoned lock means a prior snapshot panicked; propagating cannot help
        let inner = &mut *inner;
        let unit = inner
            .pending
            .entry(tool_call_id.to_string())
            .or_insert_with(|| MutationUnit {
                tool_name: tool_name.to_string(),
                at_ms: now_ms(),
                files: Vec::new(),
            });
        if unit.files.iter().any(|f| f.path == path) {
            return;
        }
        let pre = capture_state(&mut inner.store, path, &mut inner.unrecorded_files);
        unit.files.push(FileMutation {
            path: path.to_path_buf(),
            pre,
            post: FileState::Unrecorded,
        });
    }

    /// Finalize the unit for `tool_call_id` after a successful persist:
    /// capture post-state, push onto the undo stack, clear the redo stack.
    pub fn commit(&self, tool_call_id: &str) {
        let mut inner = self.inner.lock().expect("undo recorder lock"); // ubs:ignore poisoned lock means a prior snapshot panicked; propagating cannot help
        let inner = &mut *inner;
        let Some(mut unit) = inner.pending.remove(tool_call_id) else {
            return;
        };
        for file in &mut unit.files {
            file.post = capture_state(&mut inner.store, &file.path, &mut inner.unrecorded_files);
        }
        for old_unit in inner.redo_stack.drain(..) {
            release_unit(&mut inner.store, &old_unit);
        }
        inner.undo_stack.push(unit);
        enforce_budget(inner, self.budget_bytes);
    }

    /// Drop the pending unit for a failed tool call.
    pub fn abort(&self, tool_call_id: &str) {
        let mut inner = self.inner.lock().expect("undo recorder lock"); // ubs:ignore poisoned lock means a prior snapshot panicked; propagating cannot help
        let inner = &mut *inner;
        if let Some(unit) = inner.pending.remove(tool_call_id) {
            release_unit(&mut inner.store, &unit);
        }
    }

    /// Undo up to `n` mutation units, newest first.
    pub fn undo(&self, n: usize, force: bool) -> UndoOutcome {
        self.apply(n, force, Direction::Undo)
    }

    /// Re-apply up to `n` previously undone units.
    pub fn redo(&self, n: usize, force: bool) -> UndoOutcome {
        self.apply(n, force, Direction::Redo)
    }

    #[allow(clippy::too_many_lines)]
    fn apply(&self, n: usize, force: bool, direction: Direction) -> UndoOutcome {
        let mut inner = self.inner.lock().expect("undo recorder lock"); // ubs:ignore poisoned lock means a prior snapshot panicked; propagating cannot help
        let inner = &mut *inner;
        let mut applied = Vec::new();
        let mut stopped = None;

        for _ in 0..n {
            let source = match direction {
                Direction::Undo => &inner.undo_stack,
                Direction::Redo => &inner.redo_stack,
            };
            let Some(unit) = source.last() else {
                if applied.is_empty() {
                    stopped = Some(UndoStop::Exhausted);
                }
                break;
            };

            // `expected` must match disk before replay; `restore_to` is what
            // lands on disk afterwards.
            let pick = |file: &FileMutation| match direction {
                Direction::Undo => (file.post.clone(), file.pre.clone()),
                Direction::Redo => (file.pre.clone(), file.post.clone()),
            };

            let omitted: Vec<String> = unit
                .files
                .iter()
                .filter(|f| {
                    let (expected, restore_to) = pick(f);
                    expected == FileState::Unrecorded || restore_to == FileState::Unrecorded
                })
                .map(|f| f.path.display().to_string())
                .collect();
            if !omitted.is_empty() {
                stopped = Some(UndoStop::SnapshotOmitted { paths: omitted });
                break;
            }

            if !force {
                let changed: Vec<String> = unit
                    .files
                    .iter()
                    .filter(|f| !state_matches_disk(&pick(f).0, &f.path))
                    .map(|f| f.path.display().to_string())
                    .collect();
                if !changed.is_empty() {
                    stopped = Some(UndoStop::ExternalChange { paths: changed });
                    break;
                }
            }

            let mut restored = Vec::new();
            let mut failure = None;
            for file in &unit.files {
                let (expected, restore_to) = pick(file);
                let current_bytes = match &expected {
                    FileState::Present(id) => inner.store.get(id).map(<[u8]>::to_vec),
                    FileState::Absent | FileState::Unrecorded => std::fs::read(&file.path).ok(),
                }
                .unwrap_or_default();
                let result = match &restore_to {
                    FileState::Present(id) => {
                        let content = inner.store.get(id).map(<[u8]>::to_vec).unwrap_or_default();
                        let report = RestoredFile {
                            path: file.path.display().to_string(),
                            action: if file.path.exists() {
                                "restored"
                            } else {
                                "recreated"
                            }
                            .to_string(),
                            lines_added: line_count(&content),
                            lines_removed: line_count(&current_bytes),
                        };
                        file.path
                            .parent()
                            .map_or(Ok(()), std::fs::create_dir_all)
                            .and_then(|()| std::fs::write(&file.path, &content))
                            .map(|()| report)
                    }
                    FileState::Absent => {
                        let report = RestoredFile {
                            path: file.path.display().to_string(),
                            action: "removed".to_string(),
                            lines_added: 0,
                            lines_removed: line_count(&current_bytes),
                        };
                        match std::fs::remove_file(&file.path) {
                            Ok(()) => Ok(report),
                            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(report),
                            Err(err) => Err(err),
                        }
                    }
                    FileState::Unrecorded => unreachable!("omitted snapshots filtered above"),
                };
                match result {
                    Ok(report) => restored.push(report),
                    Err(err) => {
                        failure = Some(UndoStop::RestoreFailed {
                            path: file.path.display().to_string(),
                            error: err.to_string(),
                        });
                        break;
                    }
                }
            }

            if let Some(failure) = failure {
                stopped = Some(failure);
                break;
            }

            let (source, target) = match direction {
                Direction::Undo => (&mut inner.undo_stack, &mut inner.redo_stack),
                Direction::Redo => (&mut inner.redo_stack, &mut inner.undo_stack),
            };
            let unit = source.pop().expect("unit checked above");
            applied.push(AppliedUnit {
                tool_name: unit.tool_name.clone(),
                files: restored,
            });
            target.push(unit);
        }

        UndoOutcome { applied, stopped }
    }

    /// Newest-first summaries of undoable units.
    #[must_use]
    pub fn history(&self) -> Vec<UnitSummary> {
        let inner = self.inner.lock().expect("undo recorder lock"); // ubs:ignore poisoned lock means a prior snapshot panicked; propagating cannot help
        inner
            .undo_stack
            .iter()
            .rev()
            .map(|unit| UnitSummary {
                tool_name: unit.tool_name.clone(),
                at_ms: unit.at_ms,
                paths: unit
                    .files
                    .iter()
                    .map(|f| f.path.display().to_string())
                    .collect(),
            })
            .collect()
    }

    #[must_use]
    pub fn stats(&self) -> RecorderStats {
        let inner = self.inner.lock().expect("undo recorder lock"); // ubs:ignore poisoned lock means a prior snapshot panicked; propagating cannot help
        RecorderStats {
            undo_depth: inner.undo_stack.len(),
            redo_depth: inner.redo_stack.len(),
            store_bytes: inner.store.bytes,
            budget_bytes: self.budget_bytes,
            evicted_units: inner.evicted_units,
            unrecorded_files: inner.unrecorded_files,
        }
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Undo,
    Redo,
}

fn release_unit(store: &mut BlobStore, unit: &MutationUnit) {
    for file in &unit.files {
        if let FileState::Present(id) = &file.pre {
            store.release(id);
        }
        if let FileState::Present(id) = &file.post {
            store.release(id);
        }
    }
}

/// Evict oldest undo units until the store fits the budget. Redo units were
/// already cleared by the commit that grew the store.
fn enforce_budget(inner: &mut RecorderInner, budget_bytes: u64) {
    while inner.store.bytes > budget_bytes && inner.undo_stack.len() > 1 {
        let unit = inner.undo_stack.remove(0);
        release_unit(&mut inner.store, &unit);
        inner.evicted_units += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, content: &str) {
        std::fs::write(path, content).expect("write fixture");
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read fixture")
    }

    #[test]
    fn undo_and_redo_round_trip_an_edit() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        write(&file, "one\n");

        let recorder = FileMutationRecorder::default();
        recorder.begin_file("call-1", "edit", &file);
        write(&file, "two\n");
        recorder.commit("call-1");

        let outcome = recorder.undo(1, false);
        assert_eq!(outcome.applied.len(), 1, "{outcome:?}");
        assert!(outcome.stopped.is_none());
        assert_eq!(read(&file), "one\n");
        assert_eq!(outcome.applied[0].files[0].action, "restored");

        let outcome = recorder.redo(1, false);
        assert_eq!(outcome.applied.len(), 1, "{outcome:?}");
        assert_eq!(read(&file), "two\n");
    }

    #[test]
    fn undo_of_file_creation_removes_and_redo_recreates() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("new.txt");

        let recorder = FileMutationRecorder::default();
        recorder.begin_file("call-1", "write", &file);
        write(&file, "created\n");
        recorder.commit("call-1");

        let outcome = recorder.undo(1, false);
        assert_eq!(outcome.applied[0].files[0].action, "removed");
        assert!(!file.exists());

        let outcome = recorder.redo(1, false);
        assert_eq!(outcome.applied[0].files[0].action, "recreated");
        assert_eq!(read(&file), "created\n");
    }

    #[test]
    fn external_change_blocks_undo_unless_forced() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        write(&file, "one\n");

        let recorder = FileMutationRecorder::default();
        recorder.begin_file("call-1", "edit", &file);
        write(&file, "two\n");
        recorder.commit("call-1");

        write(&file, "external change\n");

        let outcome = recorder.undo(1, false);
        assert!(outcome.applied.is_empty());
        assert!(
            matches!(outcome.stopped, Some(UndoStop::ExternalChange { .. })),
            "{outcome:?}"
        );
        assert_eq!(read(&file), "external change\n");

        let outcome = recorder.undo(1, true);
        assert_eq!(outcome.applied.len(), 1);
        assert_eq!(read(&file), "one\n");
    }

    #[test]
    fn new_mutation_clears_redo_stack() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        write(&file, "one\n");

        let recorder = FileMutationRecorder::default();
        recorder.begin_file("call-1", "edit", &file);
        write(&file, "two\n");
        recorder.commit("call-1");
        recorder.undo(1, false);

        recorder.begin_file("call-2", "edit", &file);
        write(&file, "three\n");
        recorder.commit("call-2");

        let outcome = recorder.redo(1, false);
        assert!(outcome.applied.is_empty());
        assert!(matches!(outcome.stopped, Some(UndoStop::Exhausted)));
        assert_eq!(recorder.stats().redo_depth, 0);
    }

    #[test]
    fn abort_discards_pending_unit() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        write(&file, "one\n");

        let recorder = FileMutationRecorder::default();
        recorder.begin_file("call-1", "edit", &file);
        recorder.abort("call-1");
        let outcome = recorder.undo(1, false);
        assert!(matches!(outcome.stopped, Some(UndoStop::Exhausted)));
    }

    #[test]
    fn multi_step_undo_is_lifo() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        write(&file, "v1\n");

        let recorder = FileMutationRecorder::default();
        for (id, content) in [("c1", "v2\n"), ("c2", "v3\n"), ("c3", "v4\n")] {
            recorder.begin_file(id, "edit", &file);
            write(&file, content);
            recorder.commit(id);
        }

        let outcome = recorder.undo(2, false);
        assert_eq!(outcome.applied.len(), 2);
        assert_eq!(read(&file), "v2\n");
        assert_eq!(recorder.stats().undo_depth, 1);
        assert_eq!(recorder.stats().redo_depth, 2);

        let history = recorder.history();
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn eviction_drops_oldest_units_and_counts() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        write(&file, "start\n");

        // Budget fits roughly one ~1KiB blob pair; three distinct commits
        // must evict the oldest.
        let recorder = FileMutationRecorder::new(3 * 1024);
        for (id, fill) in [("c1", "a"), ("c2", "b"), ("c3", "c")] {
            recorder.begin_file(id, "edit", &file);
            write(&file, &fill.repeat(1024));
            recorder.commit(id);
        }
        let stats = recorder.stats();
        assert!(stats.evicted_units > 0, "{stats:?}");
    }

    #[test]
    fn blob_dedup_shares_identical_content() {
        let dir = tempdir().expect("tempdir");
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        write(&a, "same content\n");
        write(&b, "same content\n");

        let recorder = FileMutationRecorder::default();
        recorder.begin_file("c1", "edit", &a);
        write(&a, "changed a\n");
        recorder.commit("c1");
        recorder.begin_file("c2", "edit", &b);
        write(&b, "changed b\n");
        recorder.commit("c2");

        // "same content\n" is stored once even though two units reference it.
        let stats = recorder.stats();
        let unique = "same content\n".len() + "changed a\n".len() + "changed b\n".len();
        assert_eq!(stats.store_bytes, unique as u64, "{stats:?}");
    }
}
