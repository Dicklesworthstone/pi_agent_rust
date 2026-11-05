//! File-mutation recording with `/undo` and `/redo` (bd-cv653.3.13).
//!
//! The [`FileMutationRecorder`] snapshots file content around every
//! successful `write`/`edit`/`hashline_edit`/`ast_edit`/`lsp` tool call so agent
//! edits can be rolled back without git. Snapshots live in an in-memory
//! content-addressed store (dedup by SHA-256) bounded by a byte budget with
//! oldest-unit eviction. Session-backed recorders also persist a private journal.
//! Native LSP file edits, including admitted command callbacks, share one unit
//! per call. Directory moves create a blocking omission marker because this
//! recorder restores files, not directory topology. Subagent processes and
//! language-server writes outside native workspace edits are not recorded.
//!
//! Recording contract: one mutation unit per tool call (`begin_file` before
//! persisting, `commit` after success, `abort` on failure). Only tool-path
//! mutations are recorded; user or external edits are invisible here, and
//! `/undo` refuses (without `force`) when a file changed externally since the
//! recorded post-state. Force overrides content conflicts, never unsafe file
//! types or a replaced parent directory. Restores replace individual files
//! atomically; a multi-file unit is not a filesystem transaction.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::file_identity::FileIdentity;

mod journal;

/// Schema tag for undo/redo session entries and tool-result details.
pub const UNDO_SCHEMA: &str = "pi.undo.v1";

/// Default byte budget for the snapshot store.
pub const DEFAULT_STORE_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// Files larger than this are not snapshotted; the unit records the gap and
/// blocks undo past it (a partial restore would corrupt interleaved state).
pub const MAX_SNAPSHOT_FILE_BYTES: u64 = 8 * 1024 * 1024;

type BlobId = [u8; 32];

/// Recorded state of one file at one point in time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum FileState {
    Absent,
    Present(BlobId),
    /// The file exceeded [`MAX_SNAPSHOT_FILE_BYTES`], was unreadable, or was
    /// not a regular file that could be safely snapshotted.
    Unrecorded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RecordedParent {
    path: PathBuf,
    identity: FileIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileMutation {
    path: PathBuf,
    parent: Option<RecordedParent>,
    pre: FileState,
    post: FileState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub durable: bool,
    pub durability_error: Option<&'static str>,
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
    durability_error: Option<&'static str>,
    journal_identity: Option<FileIdentity>,
}

/// Records tool-path file mutations and applies `/undo` / `/redo`.
///
/// All methods take `&self`; internal state is mutex-guarded. An unavailable
/// snapshot becomes an unrecorded marker. A durable recorder must save that
/// intent before authorizing a mutation: callers propagate `begin_file` errors
/// before writing. Post-write persistence errors retain the pending intent.
#[derive(Debug)]
pub struct FileMutationRecorder {
    inner: Mutex<RecorderInner>,
    budget_bytes: u64,
    journal: Option<journal::Journal>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn snapshot_error(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Keep publication tied to the directory that was validated, even if an
/// ancestor is renamed while bytes are being staged.
fn open_directory_nofollow(path: &Path) -> io::Result<std::fs::File> {
    #[cfg(unix)]
    let file = {
        // Checking only the final component still follows a substituted
        // ancestor. Walk the canonical path through held directory handles.
        let absolute = std::path::absolute(path)?;
        let flags = rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC;
        let mut directory =
            std::fs::File::from(rustix::fs::open("/", flags, rustix::fs::Mode::empty())?);
        for component in absolute.components() {
            match component {
                std::path::Component::RootDir | std::path::Component::CurDir => {}
                std::path::Component::Normal(name) => {
                    directory = std::fs::File::from(rustix::fs::openat(
                        &directory,
                        name,
                        flags,
                        rustix::fs::Mode::empty(),
                    )?);
                }
                _ => return Err(snapshot_error("undo directory path must be canonical")),
            }
        }
        directory
    };
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt as _;

        std::fs::OpenOptions::new()
            .read(true)
            // No delete sharing: retain this directory while publishing.
            .share_mode(0x0000_0001 | 0x0000_0002)
            .custom_flags(0x0200_0000 | 0x0020_0000)
            .open(path)?
    };
    #[cfg(not(any(unix, windows)))]
    let file = std::fs::File::open(path)?;
    validate_open_directory(path, &file)?;
    Ok(file)
}

fn validate_open_directory(path: &Path, file: &std::fs::File) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || !file.metadata()?.is_dir()
        || FileIdentity::of_path_nofollow(path)? != FileIdentity::of_open_file(file)?
    {
        return Err(snapshot_error("undo parent directory changed"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(snapshot_error("undo parent directory is a reparse point"));
        }
    }
    Ok(())
}

/// An exclusively created sibling. Unix creation, publication and cleanup all
/// use the retained directory descriptor instead of re-resolving its pathname.
struct StagedWrite {
    #[cfg(unix)]
    directory: std::fs::File,
    #[cfg(unix)]
    name: std::ffi::OsString,
    #[cfg(unix)]
    file: std::fs::File,
    #[cfg(unix)]
    published: bool,
    #[cfg(not(unix))]
    temporary: tempfile::NamedTempFile,
}

impl StagedWrite {
    fn new(directory: &std::fs::File, parent: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let _ = parent;
            let directory = directory.try_clone()?;
            for _ in 0..16 {
                let name /* ubs:ignore 16 collision retries */ = std::ffi::OsString::from(format!(
                    ".pi-undo-{}.pending",
                    uuid::Uuid::new_v4().simple(),
                ));
                match rustix::fs::openat(
                    &directory,
                    &name,
                    rustix::fs::OFlags::RDWR
                        | rustix::fs::OFlags::CREATE
                        | rustix::fs::OFlags::EXCL
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
                ) {
                    Ok(file) => {
                        return Ok(Self {
                            directory,
                            name,
                            file: std::fs::File::from(file),
                            published: false,
                        });
                    }
                    Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate an undo staging file",
            ))
        }
        #[cfg(not(unix))]
        {
            validate_open_directory(parent, directory)?;
            Ok(Self {
                temporary: tempfile::NamedTempFile::new_in(parent)?,
            })
        }
    }

    fn file(&self) -> &std::fs::File {
        #[cfg(unix)]
        {
            &self.file
        }
        #[cfg(not(unix))]
        {
            self.temporary.as_file()
        }
    }

    fn file_mut(&mut self) -> &mut std::fs::File {
        #[cfg(unix)]
        {
            &mut self.file
        }
        #[cfg(not(unix))]
        {
            self.temporary.as_file_mut()
        }
    }

    fn persist(self, target: &Path, replace: bool) -> io::Result<()> {
        #[cfg(unix)]
        {
            let mut staged = self;
            let name = target
                .file_name()
                .ok_or_else(|| snapshot_error("undo target has no name"))?;
            if FileIdentity::of_directory_member_nofollow(&staged.directory, &staged.name)?
                != FileIdentity::of_open_file(&staged.file)?
            {
                return Err(snapshot_error("undo staging file changed"));
            }
            if replace {
                rustix::fs::renameat(&staged.directory, &staged.name, &staged.directory, name)?;
            } else {
                // A previously absent target must remain absent at publication.
                // A late creator wins instead of being silently overwritten.
                rustix::fs::linkat(
                    &staged.directory,
                    &staged.name,
                    &staged.directory,
                    name,
                    rustix::fs::AtFlags::empty(),
                )?;
                rustix::fs::unlinkat(
                    &staged.directory,
                    &staged.name,
                    rustix::fs::AtFlags::empty(),
                )?;
            }
            staged.published = true;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let result = if replace {
                self.temporary.persist(target)
            } else {
                self.temporary.persist_noclobber(target)
            };
            result.map(|_| ()).map_err(|error| error.error)
        }
    }
}

#[cfg(unix)]
impl Drop for StagedWrite {
    fn drop(&mut self) {
        if !self.published
            && FileIdentity::of_directory_member_nofollow(&self.directory, &self.name).is_ok_and(
                |identity| {
                    FileIdentity::of_open_file(&self.file).is_ok_and(|opened| opened == identity)
                },
            )
        {
            let _ = rustix::fs::unlinkat(&self.directory, &self.name, rustix::fs::AtFlags::empty());
        }
    }
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

/// Read through the parent authority captured for this mutation. Missing
/// parents can establish absence, but can never authorize a content read.
fn read_snapshot(path: &Path, parent: Option<&RecordedParent>) -> io::Result<Option<Vec<u8>>> {
    let Some(parent) = parent else {
        return match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            _ => Err(snapshot_error("undo snapshot parent was not recorded")),
        };
    };
    let name = path
        .file_name()
        .ok_or_else(|| snapshot_error("undo snapshot file name required"))?;
    let directory = open_directory_nofollow(&parent.path)?;
    if FileIdentity::of_open_file(&directory)? != parent.identity {
        return Err(snapshot_error("undo snapshot parent changed"));
    }
    #[cfg(unix)]
    let content = read_snapshot_in_directory(&directory, name)?;
    #[cfg(not(unix))]
    let content = read_snapshot_by_path(&parent.path.join(name))?; // ubs:ignore file_name is one component under an identity-checked parent; durable journals are unsupported on this platform
    validate_open_directory(&parent.path, &directory)?;
    Ok(content)
}

#[cfg(unix)]
fn read_snapshot_in_directory(
    directory: &std::fs::File,
    name: &std::ffi::OsStr,
) -> io::Result<Option<Vec<u8>>> {
    if Path::new(name).file_name() != Some(name) {
        return Err(snapshot_error("undo snapshot requires one file name"));
    }
    let metadata = match rustix::fs::statat(directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
    {
        Ok(metadata) => metadata,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if rustix::fs::FileType::from_raw_mode(metadata.st_mode) != rustix::fs::FileType::RegularFile
        || u64::try_from(metadata.st_size).unwrap_or(u64::MAX) > MAX_SNAPSHOT_FILE_BYTES
    {
        return Err(snapshot_error(
            "undo snapshots require bounded regular files",
        ));
    }
    let identity = FileIdentity::of_directory_member_nofollow(directory, name)?;
    // Neither the metadata check nor opening the file re-resolves an ancestor.
    // NONBLOCK also refuses a raced special file without hanging on a FIFO.
    let mut file = std::fs::File::from(rustix::fs::openat(
        directory,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )?);
    let opened = file.metadata()?;
    if !opened.is_file()
        || opened.len() > MAX_SNAPSHOT_FILE_BYTES
        || FileIdentity::of_open_file(&file)? != identity
    {
        return Err(snapshot_error("undo snapshot file changed during open"));
    }
    let content = read_bounded_snapshot(&mut file, MAX_SNAPSHOT_FILE_BYTES)?;
    let after = file.metadata()?;
    if FileIdentity::of_directory_member_nofollow(directory, name)? != identity
        || opened.len() != after.len()
        || opened.modified().ok() != after.modified().ok()
        || u64::try_from(content.len()).unwrap_or(u64::MAX) != after.len()
    {
        return Err(snapshot_error("undo snapshot file changed during read"));
    }
    Ok(Some(content))
}

#[cfg(not(unix))]
fn read_snapshot_by_path(path: &Path) -> io::Result<Option<Vec<u8>>> {
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

fn capture_state(
    store: &mut BlobStore,
    path: &Path,
    parent: Option<&RecordedParent>,
    unrecorded: &mut u64,
) -> FileState {
    match read_snapshot(path, parent) {
        Ok(Some(content)) => FileState::Present(store.insert(content)),
        Ok(None) => FileState::Absent,
        Err(_) => {
            *unrecorded = unrecorded.saturating_add(1);
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
    let directory = open_directory_nofollow(&parent)?;
    Ok(RecordedParent {
        identity: FileIdentity::of_open_file(&directory)?,
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
fn state_matches_disk(state: &FileState, path: &Path, parent: Option<&RecordedParent>) -> bool {
    match state {
        FileState::Absent => read_snapshot(path, parent).is_ok_and(|content| content.is_none()),
        FileState::Present(id) => read_snapshot(path, parent).is_ok_and(|content| {
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
    if !force && !state_matches_disk(expected, &file.path, file.parent.as_ref()) {
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
    let directory = open_directory_nofollow(&parent.path)?;
    if FileIdentity::of_open_file(&directory)? != parent.identity {
        return Err(snapshot_error(
            "undo parent directory changed before staging",
        ));
    }
    let current_bytes = match expected {
        FileState::Present(id) => store.get(id).map(<[u8]>::to_vec),
        FileState::Absent | FileState::Unrecorded => {
            read_snapshot(&target, Some(parent)).ok().flatten()
        }
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
            let mut staged = StagedWrite::new(&directory, &parent.path)?;
            staged.file_mut().write_all(content)?;
            if let Some(metadata) = metadata {
                staged.file().set_permissions(metadata.permissions())?;
            } else {
                // A recreated file gets the write tool's new-file mode, not
                // the temp file's private 0600.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    staged
                        .file()
                        .set_permissions(std::fs::Permissions::from_mode(0o644))?;
                }
            }
            staged.file().sync_all()?;
            validate_replay_state(file, expected, force)?;
            staged.persist(&target, force || !matches!(expected, FileState::Absent))?;
            sync_restored_directory(&directory)?;
            validate_open_directory(&parent.path, &directory)?;
            Ok(report)
        }
        FileState::Absent => {
            validate_replay_state(file, expected, force)?;
            #[cfg(unix)]
            let removed = rustix::fs::unlinkat(&directory, name, rustix::fs::AtFlags::empty())
                .map_err(io::Error::from);
            #[cfg(not(unix))]
            let removed = std::fs::remove_file(&target);
            match removed {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            sync_restored_directory(&directory)?;
            validate_open_directory(&parent.path, &directory)?;
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

#[cfg(all(test, unix))]
std::thread_local! {
    static FAIL_RESTORE_DIRECTORY_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn sync_restored_directory(directory: &std::fs::File) -> io::Result<()> {
    #[cfg(unix)]
    {
        #[cfg(test)]
        if FAIL_RESTORE_DIRECTORY_SYNC.replace(false) {
            return Err(io::Error::other("injected undo directory sync failure"));
        }
        // Publish the workspace directory entry before the journal can claim a
        // completed restore. A failure keeps the persisted restoring intent.
        directory.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
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
            journal: None,
        }
    }

    /// Snapshot `path`'s pre-mutation state under `tool_call_id`. Call before
    /// persisting; repeated calls with the same id accumulate a multi-file
    /// unit (first snapshot per path wins).
    ///
    /// # Errors
    /// Returns an error when durable intent cannot be persisted or recovery is
    /// unavailable. The caller must not perform the file mutation in that case.
    pub fn begin_file(&self, tool_call_id: &str, tool_name: &str, path: &Path) -> io::Result<()> {
        self.begin_files(tool_call_id, tool_name, std::slice::from_ref(&path))
    }

    /// Snapshot a complete batch before authorizing any of its file mutations.
    /// First preimages win across duplicates and earlier batches of the same
    /// call. Capture runs under one recorder lock, then the complete pending
    /// intent is persisted once, avoiding one journal rewrite per target.
    /// Empty batches do not create history or write a journal checkpoint.
    ///
    /// # Errors
    /// Returns an error when durable intent cannot be persisted or recovery is
    /// unavailable. The caller must not mutate any path in the batch on error;
    /// retained pending intent must be finalized with `abort` as usual.
    pub fn begin_files<P: AsRef<Path>>(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        paths: &[P],
    ) -> io::Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut inner = self.inner.lock().expect("undo recorder lock"); // ubs:ignore poisoned lock means a prior snapshot panicked; propagating cannot help
        let inner = &mut *inner;
        if let Some(error) = inner.durability_error {
            return Err(io::Error::other(error));
        }
        let unit = inner
            .pending
            .entry(tool_call_id.to_string())
            .or_insert_with(|| MutationUnit {
                tool_name: tool_name.to_string(),
                at_ms: now_ms(),
                files: Vec::new(),
            });
        let mut recorded: HashSet<_> = unit.files.iter().map(|file| file.path.clone()).collect();
        for path in paths {
            let path = path.as_ref();
            let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
            let parent = capture_parent(&path).ok();
            let path = if self.journal.is_some() {
                parent
                    .as_ref()
                    .and_then(|parent| path.file_name().map(|name| parent.path.join(name))) // ubs:ignore file_name is one component under the canonical captured parent; journal save independently enforces workspace roots
                    .unwrap_or_else(|| {
                        // Missing parents still require the canonical workspace prefix.
                        path.ancestors()
                            .skip(1)
                            .find_map(|ancestor| {
                                std::fs::canonicalize(ancestor).ok().and_then(|canonical| {
                                    path.strip_prefix(ancestor)
                                        .ok()
                                        .map(|suffix| canonical.join(suffix))
                                })
                            })
                            .unwrap_or_else(|| path.clone())
                    })
            } else {
                path
            };
            if !recorded.insert(path.clone()) {
                continue;
            }
            let pre = capture_state(
                &mut inner.store,
                &path,
                parent.as_ref(),
                &mut inner.unrecorded_files,
            );
            unit.files.push(FileMutation {
                path,
                parent,
                pre,
                post: FileState::Unrecorded,
            });
        }
        enforce_budget(inner, self.budget_bytes);
        if self.persist(inner, false) {
            Ok(())
        } else {
            Err(io::Error::other(
                "Undo intent could not be saved; file mutation was not authorized",
            ))
        }
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
            // A write may have created its parent directories after begin.
            // Never replace an identity that was already recorded, however.
            if file.parent.is_none() {
                file.parent = capture_parent(&file.path).ok();
                if self.journal.is_some()
                    && let Some(parent) = &file.parent
                    && let Some(name) = file.path.file_name()
                {
                    file.path = parent.path.join(name); // ubs:ignore file_name is one component under the newly captured canonical parent; journal save independently enforces workspace roots
                }
            }
            file.post = capture_state(
                &mut inner.store,
                &file.path,
                file.parent.as_ref(),
                &mut inner.unrecorded_files,
            );
        }
        for old_unit in inner.redo_stack.drain(..) {
            release_unit(&mut inner.store, &old_unit);
        }
        inner.undo_stack.push(unit);
        enforce_budget(inner, self.budget_bytes);
        self.persist(inner, false);
    }

    /// A failed call is discardable only after its pre-state is restored.
    /// Failed rollback or a post-publication error leaves an explicit gap,
    /// including across restart, instead of permitting undo through it.
    pub fn abort(&self, tool_call_id: &str) {
        let mut inner = self.inner.lock().expect("undo recorder lock"); // ubs:ignore poisoned lock means a prior snapshot panicked; propagating cannot help
        let inner = &mut *inner;
        if let Some(unit) = inner.pending.remove(tool_call_id) {
            let unchanged = unit.files.iter().all(|file| {
                state_matches_disk(&file.pre, &file.path, file.parent.as_ref())
                    && file.parent.as_ref().is_none_or(|parent| {
                        capture_parent(&file.path).is_ok_and(|current| current == *parent)
                    })
            });
            if unchanged {
                release_unit(&mut inner.store, &unit);
            } else {
                inner.unrecorded_files = inner
                    .unrecorded_files
                    .saturating_add(u64::try_from(unit.files.len()).unwrap_or(u64::MAX));
                for old_unit in inner.redo_stack.drain(..) {
                    release_unit(&mut inner.store, &old_unit);
                }
                inner.undo_stack.push(unit);
                enforce_budget(inner, self.budget_bytes);
            }
            self.persist(inner, false);
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
        if let Some(error) = &inner.durability_error {
            return UndoOutcome {
                applied: Vec::new(),
                stopped: Some(UndoStop::RestoreFailed {
                    path: "undo journal".into(),
                    error: (*error).to_owned(),
                }),
            };
        }
        if !inner.pending.is_empty() {
            // A tool mutates outside this mutex between begin and commit.
            // Replaying older edits while it is in flight would cross an
            // unknown state and could invalidate its pre-write snapshot.
            return UndoOutcome {
                applied: Vec::new(),
                stopped: Some(UndoStop::SnapshotOmitted {
                    paths: inner
                        .pending
                        .values()
                        .flat_map(|unit| &unit.files)
                        .map(|file| file.path.display().to_string())
                        .collect(),
                }),
            };
        }
        let inner = &mut *inner;
        let mut applied = Vec::new();
        let mut stopped = None;

        for _ in 0..n {
            let source = match direction {
                Direction::Undo => &inner.undo_stack,
                Direction::Redo => &inner.redo_stack,
            };
            let Some(unit) = source.last().cloned() else {
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
                    .filter(|f| !state_matches_disk(&pick(f).0, &f.path, f.parent.as_ref()))
                    .map(|f| f.path.display().to_string())
                    .collect();
                if !changed.is_empty() {
                    stopped = Some(UndoStop::ExternalChange { paths: changed });
                    break;
                }
            }

            if !self.persist(inner, true) {
                stopped = Some(UndoStop::RestoreFailed {
                    path: "undo journal".into(),
                    error: "Could not persist undo intent; no restore started".into(),
                });
                break;
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
                if self.journal.is_some() {
                    inner.durability_error = Some(
                        "Interrupted restore; inspect the files before recovering the journal",
                    );
                }
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
            if !self.persist(inner, false) {
                stopped = Some(UndoStop::RestoreFailed {
                    path: "undo journal".into(),
                    error:
                        "Restore applied, but its journal update failed; inspect before retrying"
                            .into(),
                });
                break;
            }
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
            durable: self.journal.is_some() && inner.durability_error.is_none(),
            durability_error: inner.durability_error,
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

/// Keep both byte and durable-journal unit counts bounded, including pending
/// edits. Evict unreachable history before a journal save can poison durability.
fn enforce_budget(inner: &mut RecorderInner, budget_bytes: u64) {
    loop {
        let total_units = inner.undo_stack.len() + inner.redo_stack.len() + inner.pending.len();
        // Each history unit can contain multiple file mutations. The journal
        // bounds both unit count and total file entries, so eviction must too.
        let total_files: usize = inner
            .undo_stack
            .iter()
            .chain(&inner.redo_stack)
            .chain(inner.pending.values())
            .map(|unit| unit.files.len())
            .sum();
        let over_units = total_units > journal::MAX_UNITS || total_files > journal::MAX_UNITS;
        let over_bytes = inner.store.bytes > budget_bytes;
        if !over_units && !over_bytes {
            break;
        }
        if inner.undo_stack.len() > usize::from(over_bytes && !over_units) {
            let unit = inner.undo_stack.remove(0);
            release_unit(&mut inner.store, &unit);
            inner.evicted_units = inner.evicted_units.saturating_add(1);
        } else if !inner.redo_stack.is_empty() {
            let unit = inner.redo_stack.remove(0);
            release_unit(&mut inner.store, &unit);
            inner.evicted_units = inner.evicted_units.saturating_add(1);
        } else {
            // One enormous pending unit is rejected by the journal itself.
            break;
        }
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
        recorder.begin_file("call-1", "edit", &file).unwrap();
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
    fn batch_admission_keeps_first_preimages_across_duplicates_and_later_batches() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        let created = dir.path().join("created.txt");
        write(&first, "original first");
        write(&second, "original second");
        let recorder = FileMutationRecorder::default();
        recorder
            .begin_files("batch", "lsp", &[&first, &second, &first, &created])
            .unwrap();
        write(&first, "intermediate first");
        recorder
            .begin_files("batch", "lsp", &[&first, &second])
            .unwrap();
        write(&first, "final first");
        write(&second, "final second");
        write(&created, "created content");
        recorder.commit("batch");

        assert_eq!(recorder.stats().undo_depth, 1);
        let undone = recorder.undo(1, false);
        assert_eq!(undone.applied.len(), 1);
        assert_eq!(undone.applied[0].files.len(), 3);
        assert_eq!(read(&first), "original first");
        assert_eq!(read(&second), "original second");
        assert!(!created.exists());
        assert_eq!(recorder.redo(1, false).applied.len(), 1);
        assert_eq!(read(&first), "final first");
        assert_eq!(read(&second), "final second");
        assert_eq!(read(&created), "created content");
    }

    #[test]
    fn undo_of_file_creation_removes_and_redo_recreates() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("new.txt");

        let recorder = FileMutationRecorder::default();
        recorder.begin_file("call-1", "write", &file).unwrap();
        write(&file, "created\n");
        recorder.commit("call-1");

        let outcome = recorder.undo(1, false);
        assert_eq!(outcome.applied[0].files[0].action, "removed");
        assert!(!file.exists());

        let outcome = recorder.redo(1, false);
        assert_eq!(outcome.applied[0].files[0].action, "recreated");
        assert_eq!(read(&file), "created\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&file)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o644, "not the temp file's 0600");
        }
    }

    #[test]
    fn external_change_blocks_undo_unless_forced() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        write(&file, "one\n");

        let recorder = FileMutationRecorder::default();
        recorder.begin_file("call-1", "edit", &file).unwrap();
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
        recorder.begin_file("call-1", "edit", &file).unwrap();
        write(&file, "two\n");
        recorder.commit("call-1");
        recorder.undo(1, false);

        recorder.begin_file("call-2", "edit", &file).unwrap();
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
        recorder.begin_file("call-1", "edit", &file).unwrap();
        recorder.abort("call-1");
        let outcome = recorder.undo(1, false);
        assert!(matches!(outcome.stopped, Some(UndoStop::Exhausted)));
    }

    #[test]
    fn pending_mutation_blocks_replay_until_it_is_completed() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("file.txt");
        write(&file, "before");
        let recorder = FileMutationRecorder::default();
        recorder.begin_file("old", "write", &file).unwrap();
        write(&file, "after");
        recorder.commit("old");
        assert_eq!(recorder.undo(1, false).applied.len(), 1);
        recorder.begin_file("pending", "write", &file).unwrap();
        for force in [false, true] {
            assert!(matches!(
                recorder.undo(1, force).stopped,
                Some(UndoStop::SnapshotOmitted { .. })
            ));
            assert!(matches!(
                recorder.redo(1, force).stopped,
                Some(UndoStop::SnapshotOmitted { .. })
            ));
            assert_eq!(read(&file), "before");
        }
        recorder.abort("pending");
        assert_eq!(recorder.redo(1, false).applied.len(), 1);
        assert_eq!(read(&file), "after");
    }

    #[cfg(unix)]
    #[test]
    fn staging_publication_cannot_be_redirected_by_replacing_the_parent() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("parent");
        let moved = dir.path().join("original-parent");
        std::fs::create_dir(&parent).unwrap();
        let directory = open_directory_nofollow(&parent).unwrap();
        let mut staged = StagedWrite::new(&directory, &parent).unwrap();
        staged.file_mut().write_all(b"restored").unwrap();
        staged.file().sync_all().unwrap();
        std::fs::rename(&parent, &moved).unwrap();
        std::fs::create_dir(&parent).unwrap();
        let target = parent.join("file.txt");
        std::fs::write(&target, "unrelated").unwrap();
        staged.persist(&target, true).unwrap();
        assert_eq!(read(&target), "unrelated");
        assert_eq!(read(&moved.join("file.txt")), "restored");
        assert!(validate_open_directory(&parent, &directory).is_err());
    }

    #[test]
    fn absent_target_publication_never_overwrites_a_late_creator() {
        let dir = tempdir().unwrap();
        let parent = std::fs::canonicalize(dir.path()).unwrap();
        let directory = open_directory_nofollow(&parent).unwrap();
        let mut staged = StagedWrite::new(&directory, &parent).unwrap();
        staged.file_mut().write_all(b"restored").unwrap();
        let target = parent.join("file.txt");
        std::fs::write(&target, "late external edit").unwrap();
        assert!(staged.persist(&target, false).is_err());
        assert_eq!(read(&target), "late external edit");
    }

    #[test]
    fn multi_step_undo_is_lifo() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        write(&file, "v1\n");

        let recorder = FileMutationRecorder::default();
        for (id, content) in [("c1", "v2\n"), ("c2", "v3\n"), ("c3", "v4\n")] {
            recorder.begin_file(id, "edit", &file).unwrap();
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
            recorder.begin_file(id, "edit", &file).unwrap();
            write(&file, &fill.repeat(1024));
            recorder.commit(id);
        }
        let stats = recorder.stats();
        assert!(stats.evicted_units > 0, "{stats:?}");
    }

    #[test]
    fn unit_count_eviction_prevents_small_edit_journal_exhaustion() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("tiny.txt");
        write(&file, "a");
        let recorder = FileMutationRecorder::new(DEFAULT_STORE_BUDGET_BYTES);
        for i in 0..=journal::MAX_UNITS {
            let id = format!("tiny-{i}");
            recorder.begin_file(&id, "edit", &file).unwrap();
            write(&file, if i % 2 == 0 { "b" } else { "a" });
            recorder.commit(&id);
        }
        assert!(recorder.stats().undo_depth <= journal::MAX_UNITS);
        assert!(recorder.stats().evicted_units > 0);
    }

    #[test]
    fn multi_file_units_evict_before_journal_file_count_limit() {
        let dir = tempdir().unwrap();
        let mut inner = RecorderInner::default();
        for unit_id in 0..3 {
            inner.undo_stack.push(MutationUnit {
                tool_name: "workspace_edit".to_string(),
                at_ms: i64::from(unit_id),
                files: (0..(journal::MAX_UNITS / 2))
                    .map(|file_id| FileMutation {
                        path: dir.path().join(format!("{unit_id}-{file_id}.txt")),
                        parent: None,
                        pre: FileState::Absent,
                        post: FileState::Absent,
                    })
                    .collect(),
            });
        }
        enforce_budget(&mut inner, DEFAULT_STORE_BUDGET_BYTES);
        assert_eq!(inner.undo_stack.len(), 2);
        assert_eq!(inner.evicted_units, 1);
        assert!(
            inner
                .undo_stack
                .iter()
                .map(|unit| unit.files.len())
                .sum::<usize>()
                <= journal::MAX_UNITS
        );
    }

    #[test]
    fn blob_dedup_shares_identical_content() {
        let dir = tempdir().expect("tempdir");
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        write(&a, "same content\n");
        write(&b, "same content\n");

        let recorder = FileMutationRecorder::default();
        recorder.begin_file("c1", "edit", &a).unwrap();
        write(&a, "changed a\n");
        recorder.commit("c1");
        recorder.begin_file("c2", "edit", &b).unwrap();
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
        recorder.begin_file("large", "write", &file).unwrap();
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
        recorder
            .begin_file("directory", "edit", dir.path())
            .unwrap();
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
        recorder.begin_file("edit", "edit", &file).unwrap();
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
        recorder.begin_file("multi", "edit", &first).unwrap();
        recorder.begin_file("multi", "edit", &second).unwrap();
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
        recorder.begin_file("create", "write", &file).unwrap();
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
    fn snapshots_stay_with_the_held_parent_after_ancestor_substitution() {
        let directory = tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let ancestor = root.join("ancestor");
        let moved = root.join("moved");
        let outside = root.join("outside");
        std::fs::create_dir_all(ancestor.join("nested")).unwrap();
        std::fs::create_dir_all(outside.join("nested")).unwrap();
        let file = ancestor.join("nested").join("file.txt");
        let external_file = outside.join("nested").join("file.txt");
        write(&file, "recorded workspace bytes");
        write(&external_file, "private outside bytes");
        let parent = capture_parent(&file).unwrap();
        let held = open_directory_nofollow(&parent.path).unwrap();

        std::fs::rename(&ancestor, &moved).unwrap();
        std::os::unix::fs::symlink(&outside, &ancestor).unwrap();

        // A read already authorized by the held parent cannot follow the new
        // ancestor. A fresh capture using the historical identity refuses it.
        assert_eq!(
            read_snapshot_in_directory(&held, file.file_name().unwrap()).unwrap(),
            Some(b"recorded workspace bytes".to_vec())
        );
        assert!(read_snapshot(&file, Some(&parent)).is_err());
        assert!(open_directory_nofollow(&parent.path).is_err());
        let mut store = BlobStore::default();
        let mut omitted = 0;
        assert_eq!(
            capture_state(&mut store, &file, Some(&parent), &mut omitted),
            FileState::Unrecorded
        );
        assert_eq!(
            capture_state(&mut store, &file, None, &mut omitted),
            FileState::Unrecorded
        );
        assert_eq!(store.bytes, 0);
        assert_eq!(omitted, 2);
        assert_eq!(read(&external_file), "private outside bytes");
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
            recorder.begin_file(id, "edit", path).unwrap();
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
        recorder.begin_file("edit", "edit", &file).unwrap();
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
        recorder.begin_file("create", "write", &file).unwrap();
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
            recorder.begin_file("edit", "edit", &file).unwrap();
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
        recorder.begin_file("edit", "edit", &file).unwrap();
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
        recorder.begin_file("edit", "edit", &file).unwrap();
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
