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
//! recorded post-state. Force overrides content conflicts, never unsafe file
//! types or a replaced parent directory. Restores replace individual files
//! atomically; a multi-file unit is not a filesystem transaction.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use sha2::Digest as _;

use crate::file_identity::FileIdentity;

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
    /// The file exceeded [`MAX_SNAPSHOT_FILE_BYTES`], was unreadable, or was
    /// not a regular file that could be safely snapshotted.
    Unrecorded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedParent {
    path: PathBuf,
    identity: FileIdentity,
}

#[derive(Debug, Clone)]
struct FileMutation {
    path: PathBuf,
    parent: Option<RecordedParent>,
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
    /// A snapshot or its safe replay path is unavailable.
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

fn snapshot_error(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Read one sentinel byte beyond the limit, never an unbounded file. The
/// metadata size is only an early rejection: files can grow after stat.
fn read_bounded_snapshot(reader: impl Read, limit: u64) -> io::Result<Vec<u8>> {
    let mut content = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut content)?;
    if u64::try_from(content.len()).unwrap_or(u64::MAX) > limit {
        return Err(snapshot_error("file exceeds the undo snapshot limit"));
    }
    Ok(content)
}

fn read_snapshot(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if !metadata.is_file() || metadata.len() > MAX_SNAPSHOT_FILE_BYTES {
        return Err(snapshot_error(
            "undo snapshots require bounded regular files",
        ));
    }
    let identity = FileIdentity::of_path_nofollow(path)?;

    // NONBLOCK prevents a regular-file-to-FIFO substitution from hanging
    // the recorder while its mutex is held. NOFOLLOW rejects final symlinks
    // at open time, not just during the earlier metadata check.
    #[cfg(unix)]
    let mut file = std::fs::File::from(rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )?);
    #[cfg(not(unix))]
    let mut file = {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        options.open(path)?
    };

    let opened = file.metadata()?;
    if !opened.is_file()
        || opened.len() > MAX_SNAPSHOT_FILE_BYTES
        || FileIdentity::of_open_file(&file)? != identity
    {
        return Err(snapshot_error("undo snapshot file changed during open"));
    }
    let content = read_bounded_snapshot(&mut file, MAX_SNAPSHOT_FILE_BYTES)?;
    let after = file.metadata()?;
    if FileIdentity::of_path_nofollow(path)? != identity
        || opened.len() != after.len()
        || opened.modified().ok() != after.modified().ok()
        || u64::try_from(content.len()).unwrap_or(u64::MAX) != after.len()
    {
        return Err(snapshot_error("undo snapshot file changed during read"));
    }
    Ok(Some(content))
}

fn capture_state(store: &mut BlobStore, path: &Path, unrecorded: &mut u64) -> FileState {
    match read_snapshot(path) {
        Ok(Some(content)) => FileState::Present(store.insert(content)),
        Ok(None) => FileState::Absent,
        Err(_) => {
            *unrecorded += 1;
            FileState::Unrecorded
        }
    }
}

fn capture_parent(path: &Path) -> io::Result<RecordedParent> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = std::fs::canonicalize(parent)?;
    if !std::fs::symlink_metadata(&parent)?.is_dir() {
        return Err(snapshot_error("undo parent is not a directory"));
    }
    Ok(RecordedParent {
        identity: FileIdentity::of_path_nofollow(&parent)?,
        path: parent,
    })
}

/// Force can waive a content mismatch, not permission failures, symlinks,
/// special files, or a parent directory replaced since the tool mutation.
fn validate_mutation_path(file: &FileMutation) -> io::Result<Option<std::fs::Metadata>> {
    let parent = file
        .parent
        .as_ref()
        .ok_or_else(|| snapshot_error("undo parent directory was not recorded"))?;
    if capture_parent(&file.path)? != *parent {
        return Err(snapshot_error(
            "undo parent directory changed; refusing replay",
        ));
    }
    match std::fs::symlink_metadata(&file.path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata)),
        Ok(_) => Err(snapshot_error(
            "undo/redo refuses symbolic links and special files, even with force",
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Current on-disk state matches a recorded state? `Unrecorded` never
/// matches: units with omitted snapshots refuse replay instead of guessing.
fn state_matches_disk(state: &FileState, path: &Path) -> bool {
    match state {
        FileState::Absent => {
            std::fs::symlink_metadata(path).is_err_and(|err| err.kind() == io::ErrorKind::NotFound)
        }
        FileState::Present(id) => read_snapshot(path).is_ok_and(|content| {
            content.is_some_and(|content| BlobId::from(sha2::Sha256::digest(&content)) == *id)
        }),
        FileState::Unrecorded => false,
    }
}

fn snapshot_available(store: &BlobStore, state: &FileState) -> bool {
    match state {
        FileState::Absent => true,
        FileState::Present(id) => store.get(id).is_some(),
        FileState::Unrecorded => false,
    }
}

fn validate_replay_state(file: &FileMutation, expected: &FileState, force: bool) -> io::Result<()> {
    validate_mutation_path(file)?;
    if !force && !state_matches_disk(expected, &file.path) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "file changed while preparing undo/redo; no replacement was applied",
        ));
    }
    Ok(())
}

/// Stage complete bytes before replacing a destination. In-place writes
/// truncate before reporting a write failure and also modify every hard
/// link to that inode; neither is appropriate for restoring one path.
fn restore_file(
    store: &BlobStore,
    file: &FileMutation,
    expected: &FileState,
    restore_to: &FileState,
    force: bool,
) -> io::Result<RestoredFile> {
    let metadata = validate_mutation_path(file)?;
    let parent = file
        .parent
        .as_ref()
        .ok_or_else(|| snapshot_error("undo parent directory was not recorded"))?;
    let name = file
        .path
        .file_name()
        .ok_or_else(|| snapshot_error("undo target has no file name"))?;
    let target = parent.path.join(name);
    let current_bytes = match expected {
        FileState::Present(id) => store.get(id).map(<[u8]>::to_vec),
        FileState::Absent | FileState::Unrecorded => read_snapshot(&target).ok().flatten(),
    }
    .unwrap_or_default();

    match restore_to {
        FileState::Present(id) => {
            // A missing blob is never an empty file. Check again here even
            // though the whole unit was preflighted before its first write.
            let content = store
                .get(id)
                .ok_or_else(|| snapshot_error("undo snapshot bytes are unavailable"))?;
            let report = RestoredFile {
                path: file.path.display().to_string(),
                action: if metadata.is_some() {
                    "restored"
                } else {
                    "recreated"
                }
                .to_string(),
                lines_added: line_count(content),
                lines_removed: line_count(&current_bytes),
            };
            let mut staged = tempfile::NamedTempFile::new_in(&parent.path)?;
            staged.write_all(content)?;
            if let Some(metadata) = metadata {
                staged.as_file().set_permissions(metadata.permissions())?;
            }
            staged.as_file().sync_all()?;
            validate_replay_state(file, expected, force)?;
            staged.persist(&target).map_err(|error| error.error)?;
            Ok(report)
        }
        FileState::Absent => {
            validate_replay_state(file, expected, force)?;
            match std::fs::remove_file(&target) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            Ok(RestoredFile {
                path: file.path.display().to_string(),
                action: "removed".to_string(),
                lines_added: 0,
                lines_removed: line_count(&current_bytes),
            })
        }
        FileState::Unrecorded => Err(snapshot_error("undo snapshot was omitted")),
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
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
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
        let parent = capture_parent(&path).ok();
        let pre = capture_state(&mut inner.store, &path, &mut inner.unrecorded_files);
        unit.files.push(FileMutation {
            path,
            parent,
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
            // A write may have created its parent directories after begin.
            // Never replace an identity that was already recorded, however.
            if file.parent.is_none() {
                file.parent = capture_parent(&file.path).ok();
            }
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
                    f.parent.is_none()
                        || !snapshot_available(&inner.store, &expected)
                        || !snapshot_available(&inner.store, &restore_to)
                })
                .map(|f| f.path.display().to_string())
                .collect();
            if !omitted.is_empty() {
                stopped = Some(UndoStop::SnapshotOmitted { paths: omitted });
                break;
            }

            // Check every path before touching any file, including forced
            // replay. A known unsafe later file must not leave earlier files
            // restored while the entire unit remains on its original stack.
            if let Some(failure) = unit.files.iter().find_map(|file| {
                validate_mutation_path(file)
                    .err()
                    .map(|err| UndoStop::RestoreFailed {
                        path: file.path.display().to_string(),
                        error: err.to_string(),
                    })
            }) {
                stopped = Some(failure);
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
                match restore_file(&inner.store, file, &expected, &restore_to, force) {
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

/// Render an undo/redo outcome as the user-facing report used by both the
/// bubbletea and ftui surfaces.
#[must_use]
pub fn render_outcome_text(outcome: &UndoOutcome, redo: bool, requested: usize) -> String {
    let verb = if redo { "redo" } else { "undo" };
    let mut lines: Vec<String> = Vec::new();
    match (outcome.applied.len(), redo) {
        (0, _) => {}
        (n, false) => lines.push(format!("Undid {n} edit(s):")),
        (n, true) => lines.push(format!("Redid {n} edit(s):")),
    }
    for unit in &outcome.applied {
        for file in &unit.files {
            lines.push(format!(
                "  {} {} (+{} -{}) [{}]",
                file.action, file.path, file.lines_added, file.lines_removed, unit.tool_name
            ));
        }
    }
    if let Some(stop) = &outcome.stopped {
        lines.push(match stop {
            UndoStop::Exhausted => format!("Nothing to {verb}."),
            UndoStop::ExternalChange { paths } => format!(
                "Stopped: file(s) changed outside the agent since the recorded state: {}. \
                 Re-run `/{verb} {requested} force` to override.",
                paths.join(", ")
            ),
            UndoStop::SnapshotOmitted { paths } => format!(
                "Stopped: snapshot was not recorded for {} (file too large, unreadable, or unsafe); \
                 use git to roll further back.",
                paths.join(", ")
            ),
            UndoStop::RestoreFailed { path, error } => {
                format!("Stopped: failed to restore {path}: {error}")
            }
        });
    }
    lines.join("\n")
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

    #[test]
    fn bounded_snapshot_reads_only_one_sentinel_byte() {
        let mut reader = io::Cursor::new(b"123456789-and-more".as_slice());
        let err = read_bounded_snapshot(&mut reader, 8).expect_err("oversized snapshot");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(reader.position(), 9);
        assert_eq!(
            read_bounded_snapshot(b"12345678".as_slice(), 8).expect("exact limit"),
            b"12345678"
        );
        assert!(
            read_bounded_snapshot(b"".as_slice(), 0)
                .expect("empty")
                .is_empty()
        );
    }

    #[test]
    fn oversized_pre_snapshot_blocks_undo_even_when_forced() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("large.bin");
        std::fs::File::create(&file)
            .expect("create sparse file")
            .set_len(MAX_SNAPSHOT_FILE_BYTES + 1)
            .expect("set sparse size");
        let recorder = FileMutationRecorder::default();
        recorder.begin_file("large", "write", &file);
        write(&file, "replacement");
        recorder.commit("large");
        for force in [false, true] {
            let outcome = recorder.undo(1, force);
            assert!(matches!(
                outcome.stopped,
                Some(UndoStop::SnapshotOmitted { .. })
            ));
            assert!(outcome.applied.is_empty());
            assert_eq!(read(&file), "replacement");
        }
        assert_eq!(recorder.stats().unrecorded_files, 1);
    }

    #[test]
    fn directories_are_unrecorded_instead_of_read_as_files() {
        let dir = tempdir().expect("tempdir");
        let recorder = FileMutationRecorder::default();
        recorder.begin_file("directory", "edit", dir.path());
        recorder.commit("directory");
        let outcome = recorder.undo(1, true);
        assert!(matches!(
            outcome.stopped,
            Some(UndoStop::SnapshotOmitted { .. })
        ));
        assert_eq!(recorder.stats().unrecorded_files, 2);
        assert!(dir.path().is_dir());
    }

    #[test]
    fn missing_blob_never_becomes_an_empty_restored_file() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        write(&file, "before");
        let recorder = FileMutationRecorder::default();
        recorder.begin_file("edit", "edit", &file);
        write(&file, "after");
        recorder.commit("edit");
        recorder.inner.lock().expect("recorder").store.blobs.clear();

        let outcome = recorder.undo(1, true);
        assert!(matches!(
            outcome.stopped,
            Some(UndoStop::SnapshotOmitted { .. })
        ));
        assert!(outcome.applied.is_empty());
        assert_eq!(read(&file), "after");
        assert_eq!(recorder.stats().undo_depth, 1);
    }

    #[test]
    fn unsafe_later_path_is_rejected_before_any_file_is_restored() {
        let dir = tempdir().expect("tempdir");
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        write(&first, "first before");
        write(&second, "second before");
        let recorder = FileMutationRecorder::default();
        recorder.begin_file("multi", "edit", &first);
        recorder.begin_file("multi", "edit", &second);
        write(&first, "first after");
        write(&second, "second after");
        recorder.commit("multi");
        std::fs::remove_file(&second).expect("replace second file");
        std::fs::create_dir(&second).expect("replacement directory");

        let outcome = recorder.undo(1, true);
        assert!(matches!(
            outcome.stopped,
            Some(UndoStop::RestoreFailed { .. })
        ));
        assert!(outcome.applied.is_empty());
        assert_eq!(read(&first), "first after");
        assert!(second.is_dir());
        assert_eq!(recorder.stats().undo_depth, 1);
        assert_eq!(recorder.stats().redo_depth, 0);
    }

    #[test]
    fn newly_created_parent_can_be_recorded_after_the_tool_write() {
        let dir = tempdir().expect("tempdir");
        let parent = dir.path().join("new-directory");
        let file = parent.join("new.txt");
        let recorder = FileMutationRecorder::default();
        recorder.begin_file("create", "write", &file);
        std::fs::create_dir(&parent).expect("create parent");
        write(&file, "created");
        recorder.commit("create");
        assert_eq!(recorder.undo(1, false).applied.len(), 1);
        assert!(!file.exists());
        assert_eq!(recorder.redo(1, false).applied.len(), 1);
        assert_eq!(read(&file), "created");
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_symlinks_and_sockets_without_following_them() {
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;

        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        let socket = dir.path().join("socket");
        write(&target, "private target");
        symlink(&target, &link).expect("symlink");
        let _listener = UnixListener::bind(&socket).expect("socket");
        let recorder = FileMutationRecorder::default();
        for (id, path) in [("link", &link), ("socket", &socket)] {
            recorder.begin_file(id, "edit", path);
            recorder.commit(id);
        }
        assert_eq!(recorder.stats().unrecorded_files, 4);
        assert_eq!(recorder.stats().store_bytes, 0);
        assert_eq!(read(&target), "private target");
    }

    #[cfg(unix)]
    #[test]
    fn replacement_symlink_is_not_followed_even_with_force() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("edit.txt");
        let target = dir.path().join("unrelated.txt");
        write(&file, "before");
        let recorder = FileMutationRecorder::default();
        recorder.begin_file("edit", "edit", &file);
        write(&file, "after");
        recorder.commit("edit");
        write(&target, "after");
        std::fs::remove_file(&file).expect("replace file");
        symlink(&target, &file).expect("replacement symlink");
        for force in [false, true] {
            let outcome = recorder.undo(1, force);
            assert!(matches!(
                outcome.stopped,
                Some(UndoStop::RestoreFailed { .. })
            ));
            assert!(outcome.applied.is_empty());
            assert_eq!(read(&target), "after");
            assert!(
                std::fs::symlink_metadata(&file)
                    .expect("link")
                    .file_type()
                    .is_symlink()
            );
        }
        assert_eq!(recorder.stats().undo_depth, 1);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_is_not_an_absent_redo_target() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("created.txt");
        let target = dir.path().join("must-stay-absent.txt");
        let recorder = FileMutationRecorder::default();
        recorder.begin_file("create", "write", &file);
        write(&file, "created");
        recorder.commit("create");
        assert_eq!(recorder.undo(1, false).applied.len(), 1);
        symlink(&target, &file).expect("dangling symlink");
        for force in [false, true] {
            let outcome = recorder.redo(1, force);
            assert!(matches!(
                outcome.stopped,
                Some(UndoStop::RestoreFailed { .. })
            ));
            assert!(outcome.applied.is_empty());
            assert!(!target.exists());
        }
        assert_eq!(recorder.stats().redo_depth, 1);
    }

    #[cfg(unix)]
    #[test]
    fn changed_parent_directory_blocks_replay_even_with_matching_bytes() {
        use std::os::unix::fs::symlink;

        for use_symlink in [false, true] {
            let dir = tempdir().expect("tempdir");
            let parent = dir.path().join("work");
            let moved = dir.path().join("original-work");
            let outside = dir.path().join("outside");
            std::fs::create_dir(&parent).expect("parent");
            std::fs::create_dir(&outside).expect("outside");
            let file = parent.join("edit.txt");
            write(&file, "before");
            let recorder = FileMutationRecorder::default();
            recorder.begin_file("edit", "edit", &file);
            write(&file, "after");
            recorder.commit("edit");
            std::fs::rename(&parent, &moved).expect("move original directory");
            if use_symlink {
                symlink(&outside, &parent).expect("replacement parent symlink");
            } else {
                std::fs::create_dir(&parent).expect("replacement parent directory");
            }
            write(&file, "after");
            for force in [false, true] {
                let outcome = recorder.undo(1, force);
                assert!(matches!(
                    outcome.stopped,
                    Some(UndoStop::RestoreFailed { .. })
                ));
                assert!(outcome.applied.is_empty());
                assert_eq!(read(&file), "after");
                assert_eq!(read(&moved.join("edit.txt")), "after");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn atomic_restore_does_not_modify_an_unrelated_hard_link() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("edit.txt");
        let alias = dir.path().join("external-alias.txt");
        write(&file, "before");
        let recorder = FileMutationRecorder::default();
        recorder.begin_file("edit", "edit", &file);
        write(&file, "after");
        recorder.commit("edit");
        std::fs::hard_link(&file, &alias).expect("hard link");
        assert_eq!(recorder.undo(1, false).applied.len(), 1);
        assert_eq!(read(&file), "before");
        assert_eq!(read(&alias), "after");
        assert_eq!(recorder.redo(1, false).applied.len(), 1);
        assert_eq!(read(&file), "after");
        assert_eq!(read(&alias), "after");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_restore_preserves_existing_file_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("script.sh");
        write(&file, "before\n");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o751))
            .expect("set executable mode");
        let recorder = FileMutationRecorder::default();
        recorder.begin_file("edit", "edit", &file);
        write(&file, "after\n");
        recorder.commit("edit");
        assert_eq!(recorder.undo(1, false).applied.len(), 1);
        assert_eq!(
            std::fs::metadata(&file).expect("mode").permissions().mode() & 0o777,
            0o751
        );
        assert_eq!(recorder.redo(1, false).applied.len(), 1);
        assert_eq!(
            std::fs::metadata(&file).expect("mode").permissions().mode() & 0o777,
            0o751
        );
    }
}
