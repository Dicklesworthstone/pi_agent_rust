//! Private, bounded atomic snapshots for the existing mutation recorder.

use super::*;
use base64::Engine as _;
use std::collections::BTreeMap;
use std::fs::File;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::path::PathBuf;

const SCHEMA: &str = "pi.undo.journal.v1";
pub(super) const MAX_UNITS: usize = 4096;

#[derive(Debug)]
pub(super) struct Journal {
    path: PathBuf,
    session_id: String,
    roots: Vec<PathBuf>,
    directory: File,
    lease: File,
}

#[derive(Serialize, Deserialize)]
struct Document {
    schema: String,
    session_id: String,
    roots: Vec<PathBuf>,
    blobs: Vec<(BlobId, String)>,
    undo: Vec<MutationUnit>,
    redo: Vec<MutationUnit>,
    pending: BTreeMap<String, MutationUnit>,
    evicted_units: u64,
    unrecorded_files: u64,
    restoring: bool,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    sha256: String,
    payload: String,
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in sha2::Sha256::digest(bytes) {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn open_private(directory: &File, path: &Path, create: bool) -> io::Result<File> {
    #[cfg(unix)]
    let file = {
        let access = if create {
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CREATE
        } else {
            rustix::fs::OFlags::RDONLY
        };
        File::from(rustix::fs::openat(
            directory,
            path.file_name()
                .ok_or_else(|| snapshot_error("undo journal name required"))?,
            access
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?)
    };
    #[cfg(not(unix))]
    let file = {
        let _ = directory;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(create)
            .create(create)
            .truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            options.custom_flags(0x0020_0000); // Open the reparse point itself.
        }
        options.open(path)?
    };
    let meta = file.metadata()?;
    #[cfg(unix)]
    let path_identity = FileIdentity::of_directory_member_nofollow(
        directory,
        path.file_name()
            .ok_or_else(|| snapshot_error("undo journal name required"))?,
    )?;
    #[cfg(not(unix))]
    let path_identity = FileIdentity::of_path_nofollow(path)?;
    if !meta.is_file() || path_identity != FileIdentity::of_open_file(&file)? {
        return Err(snapshot_error("unsafe undo journal file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if meta.permissions().mode() & 0o077 != 0
            || meta.nlink() != 1
            || meta.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(snapshot_error(
                "undo journal must be owned, private and have one link",
            ));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(snapshot_error("undo journal must not be a reparse point"));
        }
    }
    Ok(file)
}

fn validate_private_directory(directory: &File) -> io::Result<()> {
    let metadata = directory.metadata()?;
    if !metadata.is_dir() {
        return Err(snapshot_error("undo journal parent must be a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.permissions().mode() & 0o077 != 0
            || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(snapshot_error(
                "undo journal directory must be owned and private",
            ));
        }
    }
    Ok(())
}

fn prepare_session_directory(directory: &Path) -> io::Result<()> {
    // Record every parent whose namespace gains a directory. Syncing only the
    // eventual journal directory cannot make its own newly created name durable.
    let mut parents_to_sync = Vec::new();
    let mut missing = directory;
    loop {
        match std::fs::symlink_metadata(missing) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent = missing
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                parents_to_sync.push(parent.to_path_buf());
                missing = parent;
            }
            Err(error) => return Err(error),
        }
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(directory)?;
    let directory_file = open_directory_nofollow(directory)?;
    validate_private_directory(&directory_file)?;
    #[cfg(unix)]
    for parent in parents_to_sync {
        open_directory_nofollow(&std::fs::canonicalize(parent)?)?.sync_all()?;
    }
    Ok(())
}

impl FileMutationRecorder {
    /// Session journals live beside private agent state, never in imported source trees.
    ///
    /// Durable journals require Unix filesystem protections. Other platforms
    /// return [`io::ErrorKind::Unsupported`] before reading or creating state.
    pub fn for_session(session_id: &str, roots: &[PathBuf]) -> io::Result<Self> {
        if !cfg!(unix) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "durable undo journals require Unix filesystem protections",
            ));
        }
        let directory = crate::config::Config::global_dir().join("undo");
        if let Err(error) = prepare_session_directory(&directory) {
            return Ok(Self::unavailable_journal(error));
        }
        let name = format!("{}.json", sha256_hex(session_id.as_bytes()));
        Ok(Self::open_session_journal(
            &directory.join(name),
            session_id,
            roots,
        ))
    }

    fn open_session_journal(path: &Path, session_id: &str, roots: &[PathBuf]) -> Self {
        match Self::open_journal(path, session_id, roots, DEFAULT_STORE_BUDGET_BYTES) {
            Ok(recorder) => recorder,
            Err(error) => Self::unavailable_journal(error),
        }
    }

    fn unavailable_journal(error: io::Error) -> Self {
        // Recovery failures must not prevent opening the conversation or overwrite its history.
        tracing::warn!(
            "Undo journal unavailable ({error}); undo/redo disabled; existing journal preserved for inspection"
        );
        let recorder = Self::new(DEFAULT_STORE_BUDGET_BYTES);
        recorder
            .inner
            .lock()
            .expect("new undo recorder lock")
            .durability_error = Some(
            "Undo recovery unavailable; existing journal preserved for inspection; undo/redo disabled",
        );
        recorder
    }

    /// Open a session's durable recorder. Corrupt, busy, or mismatched journals fail closed.
    ///
    /// Supported only on Unix. Other platforms return
    /// [`io::ErrorKind::Unsupported`] before reading or creating state; the
    /// default in-memory recorder remains available on every platform.
    #[allow(clippy::too_many_lines)]
    pub fn open_journal(
        path: &Path,
        session_id: &str,
        roots: &[PathBuf],
        budget_bytes: u64,
    ) -> io::Result<Self> {
        if !cfg!(unix) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "durable undo journals require Unix filesystem protections",
            ));
        }
        if session_id.is_empty()
            || session_id.len() > 256
            || budget_bytes > DEFAULT_STORE_BUDGET_BYTES
        {
            return Err(snapshot_error("invalid undo journal scope or budget"));
        }
        let parent = path
            .parent()
            .ok_or_else(|| snapshot_error("journal parent required"))?;
        if !std::fs::symlink_metadata(parent)?.is_dir() {
            return Err(snapshot_error(
                "undo journal parent must be a real directory",
            ));
        }
        let parent = std::fs::canonicalize(parent)?;
        let path = parent.join(
            path.file_name()
                .ok_or_else(|| snapshot_error("journal name required"))?,
        );
        let directory = open_directory_nofollow(&parent)?;
        validate_private_directory(&directory)?;
        let mut roots = roots
            .iter()
            .map(std::fs::canonicalize)
            .collect::<io::Result<Vec<_>>>()?;
        roots.sort();
        roots.dedup();
        if roots.is_empty() {
            return Err(snapshot_error("undo journal requires a workspace"));
        }
        for root in &roots {
            if !std::fs::symlink_metadata(root)?.is_dir() {
                return Err(snapshot_error("undo workspace root must be a directory"));
            }
        }
        let lease = open_private(&directory, &path.with_extension("lock"), true)?;
        fs4::FileExt::try_lock(&lease)?;
        let journal = Journal {
            path,
            session_id: session_id.into(),
            roots,
            directory,
            lease,
        };
        journal.validate_authority()?;
        let mut inner = RecorderInner::default();
        match open_private(&journal.directory, &journal.path, false) {
            Ok(file) => {
                inner.journal_identity = Some(FileIdentity::of_open_file(&file)?);
                let limit = budget_bytes
                    .saturating_mul(2)
                    .saturating_add(4 * 1024 * 1024);
                let bytes = read_bounded_snapshot(file, limit)?;
                let envelope: Envelope = serde_json::from_slice(&bytes)
                    .map_err(|_| snapshot_error("invalid undo journal"))?;
                if envelope.sha256 != sha256_hex(envelope.payload.as_bytes()) {
                    return Err(snapshot_error("undo journal checksum mismatch"));
                }
                let document: Document = serde_json::from_str(&envelope.payload)
                    .map_err(|_| snapshot_error("invalid undo journal payload"))?;
                if document.schema != SCHEMA
                    || document.session_id != session_id
                    || document.roots != journal.roots
                    || document.restoring
                {
                    return Err(snapshot_error(
                        "undo journal scope mismatch or interrupted restore; manual reconciliation required",
                    ));
                }
                if document.undo.len() + document.redo.len() + document.pending.len() > MAX_UNITS {
                    return Err(snapshot_error("undo journal unit limit exceeded"));
                }
                let mut blobs = HashMap::new();
                let mut bytes = 0_u64;
                for (id, encoded) in document.blobs {
                    if encoded.len() as u64 > MAX_SNAPSHOT_FILE_BYTES * 2 {
                        return Err(snapshot_error("oversized undo blob"));
                    }
                    let content = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|_| snapshot_error("invalid undo blob encoding"))?;
                    bytes = bytes.saturating_add(content.len() as u64);
                    if content.len() as u64 > MAX_SNAPSHOT_FILE_BYTES
                        || bytes > budget_bytes
                        || BlobId::from(sha2::Sha256::digest(&content)) != id
                        || blobs.insert(id, content).is_some()
                    {
                        return Err(snapshot_error("invalid or oversized undo blob"));
                    }
                }
                inner.undo_stack = document.undo;
                inner.redo_stack = document.redo;
                inner.evicted_units = document.evicted_units;
                inner.unrecorded_files = document.unrecorded_files;
                let total_files = inner
                    .undo_stack
                    .iter()
                    .chain(&inner.redo_stack)
                    .chain(document.pending.values())
                    .map(|unit| unit.files.len())
                    .sum::<usize>();
                if total_files > MAX_UNITS {
                    return Err(snapshot_error("undo journal file limit exceeded"));
                }
                if !document.pending.is_empty() {
                    // A new edit may already have changed the workspace. Redo
                    // from before that edit must not jump past its recovery gap.
                    inner.redo_stack.clear();
                }
                // A pre-write snapshot is not evidence that the edit completed.
                for (_, mut unit) in document.pending {
                    for file in &mut unit.files {
                        file.post = FileState::Unrecorded;
                    }
                    inner.unrecorded_files = inner
                        .unrecorded_files
                        .saturating_add(u64::try_from(unit.files.len()).unwrap_or(u64::MAX));
                    inner.undo_stack.push(unit);
                }
                for unit in inner.undo_stack.iter_mut().chain(&mut inner.redo_stack) {
                    if unit.files.len() > MAX_UNITS {
                        return Err(snapshot_error("undo file limit exceeded"));
                    }
                    for file in &mut unit.files {
                        if !file.path.is_absolute()
                            || file
                                .path
                                .components()
                                .any(|part| matches!(part, std::path::Component::ParentDir))
                            || !journal.roots.iter().any(|root| file.path.starts_with(root))
                        {
                            return Err(snapshot_error("undo path escapes its saved workspace"));
                        }
                        if let Some(parent) = &file.parent {
                            // Historical identifiers detect ordinary replacement, not hostile inode reuse.
                            if file.path.parent() != Some(parent.path.as_path())
                                || !capture_parent(&file.path)
                                    .is_ok_and(|current| current == *parent)
                            {
                                // A missing/replaced parent invalidates only this file's
                                // historical snapshot. Never re-trust a recreated parent.
                                file.parent = None;
                            }
                        }
                        // Missing historical parents remain gaps; never bless a newly created directory.
                        for state in [&file.pre, &file.post] {
                            if let FileState::Present(id) = state {
                                let content = blobs
                                    .get(id)
                                    .ok_or_else(|| snapshot_error("undo blob missing"))?;
                                inner.store.insert(content.clone());
                            }
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        journal.validate_authority()?;
        journal.validate_snapshot_identity(inner.journal_identity)?;
        Ok(Self {
            inner: Mutex::new(inner),
            budget_bytes,
            journal: Some(journal),
        })
    }

    pub(super) fn persist(&self, inner: &mut RecorderInner, restoring: bool) -> bool {
        let Some(journal) = &self.journal else {
            return true;
        };
        if inner.durability_error.is_some() {
            return false;
        }
        if journal.save(inner, restoring, self.budget_bytes).is_ok() {
            true
        } else {
            let message = "Undo journal could not be saved; restart recovery is unavailable";
            tracing::warn!("{message}");
            inner.durability_error = Some(message);
            false
        }
    }
}

impl Journal {
    fn validate_authority(&self) -> io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| snapshot_error("journal parent missing"))?;
        validate_open_directory(parent, &self.directory)?;
        validate_private_directory(&self.directory)?;
        if std::fs::canonicalize(parent)? != parent {
            return Err(snapshot_error("undo journal parent path changed"));
        }
        let current = open_private(&self.directory, &self.path.with_extension("lock"), false)?;
        if FileIdentity::of_open_file(&current)? != FileIdentity::of_open_file(&self.lease)? {
            return Err(snapshot_error(
                "undo journal lock was replaced; refusing to publish",
            ));
        }
        Ok(())
    }

    fn validate_snapshot_identity(&self, expected: Option<FileIdentity>) -> io::Result<()> {
        match open_private(&self.directory, &self.path, false) {
            Ok(file) if Some(FileIdentity::of_open_file(&file)?) == expected => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound && expected.is_none() => Ok(()),
            Err(error) => Err(error),
            Ok(_) => Err(snapshot_error("undo journal snapshot was replaced")),
        }
    }

    fn save(
        &self,
        inner: &mut RecorderInner,
        restoring: bool,
        budget_bytes: u64,
    ) -> io::Result<()> {
        self.validate_authority()?;
        self.validate_snapshot_identity(inner.journal_identity)?;
        if inner.undo_stack.len() + inner.redo_stack.len() + inner.pending.len() > MAX_UNITS {
            return Err(snapshot_error("undo journal unit limit exceeded"));
        }
        let files = inner
            .undo_stack
            .iter()
            .chain(&inner.redo_stack)
            .chain(inner.pending.values())
            .map(|unit| unit.files.len())
            .sum::<usize>();
        if files > MAX_UNITS || inner.store.bytes > budget_bytes {
            return Err(snapshot_error(
                "undo journal file or backup byte limit exceeded",
            ));
        }
        if inner
            .undo_stack
            .iter()
            .chain(&inner.redo_stack)
            .chain(inner.pending.values())
            .flat_map(|unit| &unit.files)
            .any(|file| {
                !file.path.is_absolute()
                    || file
                        .path
                        .components()
                        .any(|part| matches!(part, std::path::Component::ParentDir))
                    || !self.roots.iter().any(|root| file.path.starts_with(root))
                    || file
                        .parent
                        .as_ref()
                        .is_some_and(|parent| file.path.parent() != Some(parent.path.as_path()))
            })
        {
            return Err(snapshot_error(
                "undo journal path outside its original workspace scope",
            ));
        }
        let mut blobs = inner
            .store
            .blobs
            .iter()
            .map(|(id, content)| {
                (
                    *id,
                    base64::engine::general_purpose::STANDARD.encode(content),
                )
            })
            .collect::<Vec<_>>();
        blobs.sort_by_key(|(id, _)| *id);
        let document = Document {
            schema: SCHEMA.into(),
            session_id: self.session_id.clone(),
            roots: self.roots.clone(),
            blobs,
            undo: inner.undo_stack.clone(),
            redo: inner.redo_stack.clone(),
            pending: inner.pending.clone().into_iter().collect(),
            evicted_units: inner.evicted_units,
            unrecorded_files: inner.unrecorded_files,
            restoring,
        };
        let payload = serde_json::to_string(&document).map_err(io::Error::other)?;
        let envelope = Envelope {
            sha256: sha256_hex(payload.as_bytes()),
            payload,
        };
        let encoded = serde_json::to_vec(&envelope).map_err(io::Error::other)?;
        if encoded.len() as u64
            > budget_bytes
                .saturating_mul(2)
                .saturating_add(4 * 1024 * 1024)
        {
            return Err(snapshot_error("undo journal byte limit exceeded"));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| snapshot_error("journal parent missing"))?;
        let mut staged = StagedWrite::new(&self.directory, parent)?;
        staged.file_mut().write_all(&encoded)?;
        staged.file().sync_all()?;
        self.validate_authority()?;
        self.validate_snapshot_identity(inner.journal_identity)?;
        let identity = FileIdentity::of_open_file(staged.file())?;
        staged.persist(&self.path, inner.journal_identity.is_some())?;
        #[cfg(unix)]
        self.directory.sync_all()?;
        self.validate_authority()?;
        inner.journal_identity = Some(identity);
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn private_tempdir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;

        tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap()
    }

    fn reopen_after_drop(
        path: &Path,
        session_id: &str,
        roots: &[PathBuf],
        budget_bytes: u64,
    ) -> io::Result<FileMutationRecorder> {
        // Other tests spawn processes in parallel. Between fork and exec, a
        // child can briefly retain this test's flock even after its recorder
        // is dropped; CLOEXEC releases that inherited descriptor at exec.
        // Retry only expected-success reopens, with a deadline so a real
        // retained owner still fails. Busy-owner assertions use open_journal
        // directly and production lock acquisition remains nonblocking.
        let started = std::time::Instant::now();
        loop {
            let result = FileMutationRecorder::open_journal(path, session_id, roots, budget_bytes);
            if result
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock)
                && started.elapsed() < std::time::Duration::from_secs(2)
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            return result;
        }
    }

    #[test]
    fn restart_restores_history_and_conflict_checks() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let file = root.join("file.rs");
        let journal = root.join("session.undo.json");
        std::fs::write(&file, "before").unwrap();
        let open = || {
            FileMutationRecorder::open_journal(
                &journal,
                "session",
                std::slice::from_ref(&root),
                1024,
            )
            .unwrap()
        };
        let recorder = open();
        recorder.begin_file("edit", "edit", &file).unwrap();
        std::fs::write(&file, "after").unwrap();
        recorder.commit("edit");
        assert!(recorder.stats().durable);
        drop(recorder);
        let recorder =
            reopen_after_drop(&journal, "session", std::slice::from_ref(&root), 1024).unwrap();
        std::fs::write(&file, "external").unwrap();
        assert!(matches!(
            recorder.undo(1, false).stopped,
            Some(UndoStop::ExternalChange { .. })
        ));
        std::fs::write(&file, "after").unwrap();
        assert_eq!(recorder.undo(1, false).applied.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
        drop(recorder);
        let recorder =
            reopen_after_drop(&journal, "session", std::slice::from_ref(&root), 1024).unwrap();
        assert_eq!(recorder.redo(1, false).applied.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
    }

    #[test]
    fn batch_intent_contains_all_preimages_before_mutation_and_survives_restart() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let journal = root.join("journal.json");
        let files: Vec<_> = (0..32)
            .map(|index| root.join(format!("{index}.txt")))
            .collect();
        for (index, path) in files.iter().enumerate() {
            std::fs::write(path, format!("before {index}")).unwrap();
        }
        let recorder =
            FileMutationRecorder::open_journal(&journal, "s", std::slice::from_ref(&root), 4096)
                .unwrap();
        recorder
            .begin_files::<PathBuf>("empty", "lsp", &[])
            .unwrap();
        recorder.commit("empty");
        assert!(!journal.exists(), "empty batches must not checkpoint");
        assert_eq!(recorder.stats().undo_depth, 0);

        recorder.begin_files("batch", "lsp", &files).unwrap();
        let envelope: Envelope = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
        let document: Document = serde_json::from_str(&envelope.payload).unwrap();
        assert_eq!(document.pending.len(), 1);
        let pending = &document.pending["batch"];
        assert_eq!(pending.files.len(), files.len());
        for (index, file) in pending.files.iter().enumerate() {
            assert_eq!(file.path, files[index]);
            assert_eq!(file.post, FileState::Unrecorded);
            let FileState::Present(blob) = &file.pre else {
                panic!("missing batch preimage");
            };
            let encoded = &document.blobs.iter().find(|(id, _)| id == blob).unwrap().1;
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .unwrap(),
                format!("before {index}").as_bytes()
            );
            assert_eq!(
                std::fs::read_to_string(&file.path).unwrap(),
                format!("before {index}")
            );
        }
        for (index, path) in files.iter().enumerate() {
            std::fs::write(path, format!("after {index}")).unwrap();
        }
        recorder.commit("batch");
        drop(recorder);

        let recorder = reopen_after_drop(&journal, "s", std::slice::from_ref(&root), 4096).unwrap();
        let undone = recorder.undo(1, false);
        assert_eq!(undone.applied.len(), 1);
        assert_eq!(undone.applied[0].files.len(), files.len());
        for (index, path) in files.iter().enumerate() {
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                format!("before {index}")
            );
        }
    }

    #[test]
    fn pending_edits_and_corrupt_journals_never_replay() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let file = root.join("file");
        std::fs::write(&file, "before").unwrap();
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        assert!(
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .is_err()
        );
        recorder.begin_file("pending", "write", &file).unwrap();
        std::fs::write(&file, "ambiguous").unwrap();
        drop(recorder);
        let recorder = reopen_after_drop(&path, "s", std::slice::from_ref(&root), 1024).unwrap();
        assert!(matches!(
            recorder.undo(1, false).stopped,
            Some(UndoStop::SnapshotOmitted { .. })
        ));
        drop(recorder);
        std::fs::write(&path, "corrupt").unwrap();
        assert!(
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ambiguous");
    }

    #[test]
    fn removed_parent_reopens_history_as_a_gap_without_disabling_journal() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let journal = root.join("journal.json");
        let nested = root.join("removed");
        std::fs::create_dir(&nested).unwrap();
        let file = nested.join("file.txt");
        std::fs::write(&file, "before").unwrap();
        let recorder =
            FileMutationRecorder::open_journal(&journal, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        recorder.begin_file("edit", "write", &file).unwrap();
        std::fs::write(&file, "after").unwrap();
        recorder.commit("edit");
        drop(recorder);
        std::fs::remove_dir_all(&nested).unwrap();
        let reopened = reopen_after_drop(&journal, "s", std::slice::from_ref(&root), 1024).unwrap();
        assert!(reopened.stats().durable);
        assert!(matches!(
            reopened.undo(1, false).stopped,
            Some(UndoStop::SnapshotOmitted { .. })
        ));
    }

    #[test]
    fn pending_creation_with_missing_parent_reopens_as_a_gap() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let file = dir.path().join("new").join("nested").join("file.rs");
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        recorder.begin_file("pending", "write", &file).unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "partial").unwrap();
        drop(recorder);
        let recorder = reopen_after_drop(&path, "s", std::slice::from_ref(&root), 1024).unwrap();
        assert!(matches!(
            recorder.undo(1, true).stopped,
            Some(UndoStop::SnapshotOmitted { .. })
        ));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "partial");
    }

    #[test]
    fn oversized_commit_preserves_a_loadable_gap_instead_of_unloadable_success() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let file = root.join("file");
        std::fs::write(&file, "before").unwrap();
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 8).unwrap();
        recorder.begin_file("edit", "edit", &file).unwrap();
        std::fs::write(&file, "after").unwrap();
        recorder.commit("edit");
        assert!(!recorder.stats().durable);
        assert!(recorder.stats().durability_error.is_some());
        drop(recorder);
        let recorder = reopen_after_drop(&path, "s", std::slice::from_ref(&root), 8).unwrap();
        assert!(matches!(
            recorder.undo(1, true).stopped,
            Some(UndoStop::SnapshotOmitted { .. })
        ));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
    }

    #[test]
    fn excessive_files_cannot_overwrite_the_last_recoverable_snapshot() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        let mut inner = recorder.inner.lock().unwrap(); // ubs:ignore synchronous test fixture injects an oversized pending unit; poisoning must fail the test and no await occurs
        assert!(recorder.persist(&mut inner, false));
        let original = std::fs::read(&path).unwrap();
        inner.pending.insert(
            "many".into(),
            MutationUnit {
                tool_name: "write".into(),
                at_ms: 0,
                files: (0..=MAX_UNITS)
                    .map(|i| FileMutation {
                        path: root.join(format!("file{i}")),
                        parent: None,
                        pre: FileState::Absent,
                        post: FileState::Unrecorded,
                    })
                    .collect(),
            },
        );
        assert!(!recorder.persist(&mut inner, false));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn edits_outside_the_original_roots_do_not_claim_durability() {
        let dir = private_tempdir();
        let outside = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        assert!(recorder.persist(&mut recorder.inner.lock().unwrap(), false)); // ubs:ignore test synchronously persists its baseline; poisoned fixture state must fail the assertion
        let original = std::fs::read(&path).unwrap();
        assert!(
            recorder
                .begin_file("outside", "write", &outside.path().join("file"))
                .is_err()
        );
        assert!(!recorder.stats().durable);
        assert!(recorder.stats().durability_error.is_some());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn session_recovery_failure_disables_replay_without_overwriting_journal() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let added = root.join("added");
        std::fs::create_dir(&added).unwrap();
        let path = root.join("journal.json");
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        assert!(recorder.persist(&mut recorder.inner.lock().unwrap(), false)); // ubs:ignore synchronous test seeds the journal for busy and scope rejection; no async boundary exists
        let original = std::fs::read(&path).unwrap();
        let busy =
            FileMutationRecorder::open_session_journal(&path, "s", std::slice::from_ref(&root));
        assert!(busy.stats().durability_error.is_some());
        drop(recorder);
        let changed_scope =
            FileMutationRecorder::open_session_journal(&path, "s", &[root.clone(), added]);
        assert!(matches!(
            changed_scope.undo(1, true).stopped,
            Some(UndoStop::RestoreFailed { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::write(&path, "corrupt").unwrap();
        let corrupt =
            FileMutationRecorder::open_session_journal(&path, "s", std::slice::from_ref(&root));
        assert!(corrupt.stats().durability_error.is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "corrupt");
    }

    #[test]
    fn interrupted_restore_and_wrong_session_fail_closed() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        {
            let mut inner = recorder.inner.lock().unwrap(); // ubs:ignore synchronous test injects persisted restore intent; poisoning must fail this fixture and no await occurs
            assert!(recorder.persist(&mut inner, true));
        }
        drop(recorder);
        assert!(
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .is_err()
        );
        assert!(
            FileMutationRecorder::open_journal(&path, "other", std::slice::from_ref(&root), 1024)
                .is_err()
        );
    }

    #[test]
    fn journal_failure_is_visible_and_blocks_restore() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let file = root.join("file");
        let sibling = root.join("sibling");
        std::fs::write(&file, "before").unwrap();
        std::fs::write(&sibling, "sibling before").unwrap();
        let mut recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        recorder.journal.as_mut().unwrap().path = root.join("missing").join("journal.json");
        assert!(
            recorder
                .begin_files("edit", "write", &[&file, &sibling])
                .is_err()
        );
        assert!(recorder.stats().durability_error.is_some());
        assert!(!recorder.stats().durable);
        assert!(matches!(
            recorder.undo(1, false).stopped,
            Some(UndoStop::RestoreFailed { .. })
        ));
        assert_eq!(std::fs::read_to_string(file).unwrap(), "before");
        assert_eq!(std::fs::read_to_string(sibling).unwrap(), "sibling before");
    }

    #[cfg(unix)]
    #[test]
    fn restart_keeps_canonical_parent_when_recording_through_alias() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let file = alias.join("file");
        let path = root.join("journal.json");
        std::fs::write(&file, "before").unwrap();
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        recorder.begin_file("edit", "edit", &file).unwrap();
        std::fs::write(&file, "after").unwrap();
        recorder.commit("edit");
        drop(recorder);
        let recorder = reopen_after_drop(&path, "s", std::slice::from_ref(&root), 1024).unwrap();
        assert_eq!(recorder.undo(1, false).applied.len(), 1);
        assert_eq!(std::fs::read_to_string(file).unwrap(), "before");
    }

    #[test]
    fn replaced_parent_is_not_blessed_when_loading_history() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let parent = root.join("parent");
        std::fs::create_dir(&parent).unwrap();
        let file = parent.join("file");
        let path = root.join("journal.json");
        std::fs::write(&file, "before").unwrap();
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        recorder.begin_file("edit", "edit", &file).unwrap();
        std::fs::write(&file, "after").unwrap();
        recorder.commit("edit");
        drop(recorder);
        std::fs::rename(&parent, root.join("old_parent")).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(&file, "after").unwrap();
        let reopened = reopen_after_drop(&path, "s", std::slice::from_ref(&root), 1024).unwrap();
        for force in [false, true] {
            assert!(matches!(
                reopened.undo(1, force).stopped,
                Some(UndoStop::SnapshotOmitted { .. }),
            ));
        }
        assert_eq!(std::fs::read_to_string(file).unwrap(), "after");
    }

    #[test]
    fn substituted_ancestor_never_enters_the_persisted_post_snapshot() {
        let directory = private_tempdir();
        let outside = private_tempdir();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let outside_root = std::fs::canonicalize(outside.path()).unwrap();
        let ancestor = root.join("ancestor");
        let moved = root.join("original-ancestor");
        std::fs::create_dir_all(ancestor.join("nested")).unwrap();
        std::fs::create_dir_all(outside_root.join("nested")).unwrap();
        let file = ancestor.join("nested").join("file.txt");
        let external_file = outside_root.join("nested").join("file.txt");
        std::fs::write(&file, "before").unwrap();
        std::fs::write(&external_file, "private outside bytes").unwrap();
        let path = root.join("journal.json");
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        recorder.begin_file("edit", "write", &file).unwrap();
        std::fs::write(&file, "after").unwrap();
        std::fs::rename(&ancestor, &moved).unwrap();
        std::os::unix::fs::symlink(&outside_root, &ancestor).unwrap();
        recorder.commit("edit");
        assert!(recorder.stats().durable);
        assert_eq!(recorder.stats().unrecorded_files, 1);
        drop(recorder);

        let envelope: Envelope = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let document: Document = serde_json::from_str(&envelope.payload).unwrap();
        assert_eq!(document.undo[0].files[0].post, FileState::Unrecorded);
        for (_, blob) in document.blobs {
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(blob)
                    .unwrap(),
                b"before"
            );
        }
        let recorder = reopen_after_drop(&path, "s", std::slice::from_ref(&root), 1024).unwrap();
        assert!(matches!(
            recorder.undo(1, true).stopped,
            Some(UndoStop::SnapshotOmitted { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&external_file).unwrap(),
            "private outside bytes"
        );
        assert_eq!(
            std::fs::read_to_string(moved.join("nested").join("file.txt")).unwrap(),
            "after"
        );
    }

    #[test]
    fn replaced_lock_stops_the_old_writer_without_touching_new_history() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let first =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        assert!(first.persist(&mut first.inner.lock().unwrap(), false)); // ubs:ignore synchronous test seeds the first writer before lease replacement; poisoned fixture state must fail
        std::fs::rename(path.with_extension("lock"), root.join("retired.lock")).unwrap();
        let second =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        let file = root.join("file.txt");
        second.begin_file("new", "write", &file).unwrap();
        std::fs::write(&file, "second writer").unwrap();
        second.commit("new");
        let current = std::fs::read(&path).unwrap();
        assert!(!first.persist(&mut first.inner.lock().unwrap(), false)); // ubs:ignore synchronous test asserts the old writer cannot publish; no await occurs while the guard is held
        assert!(first.stats().durability_error.is_some());
        assert_eq!(std::fs::read(&path).unwrap(), current);
        assert_eq!(second.undo(1, false).applied.len(), 1);
        assert!(!file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn replaced_journal_directory_is_never_a_publication_destination() {
        use std::os::unix::fs::PermissionsExt as _;

        for symlink in [false, true] {
            let dir = private_tempdir();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            let parent = root.join("private");
            let original_parent = root.join("original");
            let replacement = root.join("replacement");
            std::fs::create_dir(&parent).unwrap();
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
            let path = parent.join("journal.json");
            let recorder =
                FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                    .unwrap();
            assert!(recorder.persist(&mut recorder.inner.lock().unwrap(), false)); // ubs:ignore synchronous test creates the original snapshot before parent substitution; poisoning must fail the fixture
            let original = std::fs::read(&path).unwrap();
            std::fs::rename(&parent, &original_parent).unwrap();
            if symlink {
                std::fs::create_dir(&replacement).unwrap();
                std::os::unix::fs::symlink(&replacement, &parent).unwrap();
            } else {
                std::fs::create_dir(&parent).unwrap();
            }
            std::fs::write(&path, "unrelated replacement").unwrap();
            assert!(!recorder.persist(&mut recorder.inner.lock().unwrap(), false)); // ubs:ignore synchronous test checks rejection of the substituted parent; guard never crosses an await
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "unrelated replacement"
            );
            assert_eq!(
                std::fs::read(original_parent.join("journal.json")).unwrap(),
                original
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn journal_directory_must_be_private_even_when_it_already_exists() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024,)
                .is_err()
        );
        assert!(!path.exists());
        assert!(!path.with_extension("lock").exists());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        assert!(recorder.persist(&mut recorder.inner.lock().unwrap(), false)); // ubs:ignore synchronous permissions fixture creates its private baseline; poisoned state must fail the test
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let original = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(!recorder.persist(&mut recorder.inner.lock().unwrap(), false)); // ubs:ignore synchronous test verifies rejection after permissions widen; no async boundary exists
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn linked_journal_or_lock_never_follows_or_overwrites_its_target() {
        use std::os::unix::fs::PermissionsExt as _;

        for lock in [false, true] {
            for hard_link in [false, true] {
                let dir = private_tempdir();
                let root = std::fs::canonicalize(dir.path()).unwrap();
                let path = root.join("journal.json");
                let member = if lock {
                    path.with_extension("lock")
                } else {
                    path.clone()
                };
                let victim = root.join("unrelated");
                std::fs::write(&victim, "must stay intact").unwrap();
                std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).unwrap();
                if hard_link {
                    std::fs::hard_link(&victim, &member).unwrap();
                } else {
                    std::os::unix::fs::symlink(&victim, &member).unwrap();
                }
                assert!(
                    FileMutationRecorder::open_journal(
                        &path,
                        "s",
                        std::slice::from_ref(&root),
                        1024,
                    )
                    .is_err()
                );
                assert_eq!(std::fs::read_to_string(victim).unwrap(), "must stay intact");
            }
        }
    }

    #[test]
    fn replaced_snapshot_is_preserved_and_disables_the_existing_writer() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        assert!(recorder.persist(&mut recorder.inner.lock().unwrap(), false)); // ubs:ignore synchronous test seeds the snapshot before replacement; poisoning must fail this fixture
        std::fs::rename(&path, root.join("original.json")).unwrap();
        let mut replacement = tempfile::NamedTempFile::new_in(&root).unwrap();
        replacement
            .write_all(b"corrupt replacement for inspection")
            .unwrap();
        replacement.persist(&path).unwrap();
        assert!(!recorder.persist(&mut recorder.inner.lock().unwrap(), false)); // ubs:ignore synchronous test verifies replacement preservation; the mutex guard never crosses an await
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "corrupt replacement for inspection"
        );
        assert!(recorder.stats().durability_error.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn failed_durable_intent_refuses_the_write_and_preserves_restart_history() {
        use std::os::unix::fs::PermissionsExt as _;

        for lost_parent in [false, true] {
            let dir = private_tempdir();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            let parent = root.join("private");
            let moved = root.join("moved-private");
            std::fs::create_dir(&parent).unwrap();
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
            let path = parent.join("journal.json");
            let old_file = root.join("old.txt");
            let next_file = root.join("next.txt");
            std::fs::write(&old_file, "before").unwrap();
            std::fs::write(&next_file, "untouched").unwrap();
            let recorder =
                FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                    .unwrap();
            recorder.begin_file("old", "write", &old_file).unwrap();
            std::fs::write(&old_file, "committed").unwrap();
            recorder.commit("old");
            let original = std::fs::read(&path).unwrap();
            if lost_parent {
                std::fs::rename(&parent, &moved).unwrap();
            } else {
                std::fs::rename(path.with_extension("lock"), parent.join("retired.lock")).unwrap();
            }
            // The same fallible boundary used by the native tools: no write
            // follows an unavailable durable intent.
            let write_result = recorder
                .begin_file("refused", "write", &next_file)
                .and_then(|()| std::fs::write(&next_file, "must not be written"));
            assert!(write_result.is_err());
            assert!(recorder.begin_file("retry", "write", &next_file).is_err());
            assert_eq!(std::fs::read_to_string(&next_file).unwrap(), "untouched");
            assert!(recorder.stats().durability_error.is_some());
            drop(recorder);
            if lost_parent {
                std::fs::rename(&moved, &parent).unwrap();
            }
            assert_eq!(std::fs::read(&path).unwrap(), original);
            let reopened =
                reopen_after_drop(&path, "s", std::slice::from_ref(&root), 1024).unwrap();
            assert_eq!(reopened.stats().undo_depth, 1);
            assert_eq!(reopened.undo(1, false).applied.len(), 1);
            assert_eq!(std::fs::read_to_string(&old_file).unwrap(), "before");
            assert_eq!(std::fs::read_to_string(&next_file).unwrap(), "untouched");
        }
    }

    #[test]
    fn persisted_maximum_counters_saturate_instead_of_panicking() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let file = root.join("old.txt");
        std::fs::write(&file, "before").unwrap();
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 16)
                .unwrap();
        recorder.begin_file("edit", "write", &file).unwrap();
        std::fs::write(&file, "after").unwrap();
        recorder.commit("edit");
        drop(recorder);
        let mut envelope: Envelope =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut document: Document = serde_json::from_str(&envelope.payload).unwrap();
        document.evicted_units = u64::MAX;
        document.unrecorded_files = u64::MAX;
        envelope.payload = serde_json::to_string(&document).unwrap();
        envelope.sha256 = sha256_hex(envelope.payload.as_bytes());
        std::fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
        let recorder = reopen_after_drop(&path, "s", std::slice::from_ref(&root), 16).unwrap();
        let next = root.join("next.txt");
        recorder.begin_file("next", "write", &next).unwrap();
        std::fs::write(&next, "new payload").unwrap();
        recorder.commit("next");
        let oversized = root.join("oversized");
        File::create(&oversized)
            .unwrap()
            .set_len(MAX_SNAPSHOT_FILE_BYTES + 1)
            .unwrap();
        recorder.begin_file("omitted", "write", &oversized).unwrap();
        recorder.commit("omitted");
        assert_eq!(recorder.stats().evicted_units, u64::MAX);
        assert_eq!(recorder.stats().unrecorded_files, u64::MAX);
        assert!(recorder.stats().durable);
    }

    #[test]
    fn partial_abort_is_a_persisted_gap_while_complete_rollback_is_discarded() {
        for rolled_back in [false, true] {
            let dir = private_tempdir();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            let path = root.join("journal.json");
            let first = root.join("first.txt");
            let second = root.join("second.txt");
            std::fs::write(&first, "before first").unwrap();
            std::fs::write(&second, "before second").unwrap();
            let recorder =
                FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                    .unwrap();
            recorder.begin_file("old", "edit", &first).unwrap();
            std::fs::write(&first, "prior edit").unwrap();
            recorder.commit("old");
            assert_eq!(recorder.undo(1, false).applied.len(), 1);
            recorder.begin_file("multi", "ast_edit", &first).unwrap();
            recorder.begin_file("multi", "ast_edit", &second).unwrap();
            std::fs::write(&first, "partly applied").unwrap();
            if rolled_back {
                std::fs::write(&first, "before first").unwrap();
            }
            recorder.abort("multi");
            drop(recorder);
            let reopened =
                reopen_after_drop(&path, "s", std::slice::from_ref(&root), 1024).unwrap();
            if rolled_back {
                assert_eq!(reopened.stats().undo_depth, 0);
                assert_eq!(reopened.redo(1, false).applied.len(), 1);
                assert_eq!(std::fs::read_to_string(&first).unwrap(), "prior edit");
            } else {
                assert_eq!(reopened.stats().redo_depth, 0);
                for force in [false, true] {
                    assert!(matches!(
                        reopened.undo(1, force).stopped,
                        Some(UndoStop::SnapshotOmitted { .. })
                    ));
                    assert!(matches!(
                        reopened.redo(1, force).stopped,
                        Some(UndoStop::Exhausted)
                    ));
                }
                assert_eq!(std::fs::read_to_string(&first).unwrap(), "partly applied");
            }
            assert_eq!(std::fs::read_to_string(&second).unwrap(), "before second");
        }
    }

    #[cfg(unix)]
    #[test]
    fn workspace_sync_failure_keeps_restore_intent_and_prevents_restart_replay() {
        for creation in [false, true] {
            let dir = private_tempdir();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            let path = root.join("journal.json");
            let file = root.join("file.txt");
            if !creation {
                std::fs::write(&file, "before").unwrap();
            }
            let recorder =
                FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                    .unwrap();
            recorder.begin_file("edit", "write", &file).unwrap();
            std::fs::write(&file, "after").unwrap();
            recorder.commit("edit");
            super::super::FAIL_RESTORE_DIRECTORY_SYNC.set(true);
            let outcome = recorder.undo(1, false);
            assert!(outcome.applied.is_empty());
            assert!(matches!(
                outcome.stopped,
                Some(UndoStop::RestoreFailed { .. })
            ));
            assert!(recorder.stats().durability_error.is_some());
            let bytes = std::fs::read(&path).unwrap();
            let envelope: Envelope = serde_json::from_slice(&bytes).unwrap();
            let document: Document = serde_json::from_str(&envelope.payload).unwrap();
            assert!(document.restoring);
            assert_eq!(document.undo.len(), 1);
            assert!(document.redo.is_empty());
            drop(recorder);
            assert!(
                FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024,)
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            if creation {
                assert!(!file.exists());
            } else {
                assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
            }
        }
    }

    #[test]
    fn checksum_blob_and_path_corruption_are_preserved_without_replay() {
        let dir = private_tempdir();
        let outside = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let file = root.join("file.txt");
        let external = outside.path().join("external.txt");
        std::fs::write(&file, "before").unwrap();
        std::fs::write(&external, "unrelated").unwrap();
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        recorder.begin_file("edit", "write", &file).unwrap();
        std::fs::write(&file, "after").unwrap();
        recorder.commit("edit");
        drop(recorder);
        let original = std::fs::read(&path).unwrap();
        for corruption in ["checksum", "blob", "missing_blob", "path"] {
            let mut envelope: Envelope = serde_json::from_slice(&original).unwrap();
            if corruption == "checksum" {
                envelope.payload.push(' ');
            } else {
                let mut document: Document = serde_json::from_str(&envelope.payload).unwrap();
                match corruption {
                    "blob" => {
                        document.blobs[0].1 =
                            base64::engine::general_purpose::STANDARD.encode(b"tampered");
                    }
                    "missing_blob" => document.blobs.clear(),
                    "path" => document.undo[0].files[0].path = external.clone(),
                    _ => unreachable!(),
                }
                envelope.payload = serde_json::to_string(&document).unwrap();
                envelope.sha256 = sha256_hex(envelope.payload.as_bytes());
            }
            let corrupt = serde_json::to_vec(&envelope).unwrap();
            std::fs::write(&path, &corrupt).unwrap();
            assert!(
                FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024,)
                    .is_err(),
                "{corruption}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), corrupt);
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
            assert_eq!(std::fs::read_to_string(&external).unwrap(), "unrelated");
        }
    }

    fn run_journal_child(root: &Path, mode: &str) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "undo::journal::tests::persisted_journal_child_process",
                "--nocapture",
            ])
            .env("PI_UNDO_JOURNAL_TEST_ROOT", root)
            .env("PI_UNDO_JOURNAL_TEST_MODE", mode)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(73),
            "child {mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// Invoked only by the subprocess regressions. `exit` deliberately skips
    /// recorder Drop, unlike reopening after an ordinary scope exit.
    #[test]
    fn persisted_journal_child_process() {
        let Ok(mode) = std::env::var("PI_UNDO_JOURNAL_TEST_MODE") else {
            return;
        };
        let root =
            std::fs::canonicalize(std::env::var_os("PI_UNDO_JOURNAL_TEST_ROOT").unwrap()).unwrap();
        let path = root.join("journal.json");
        if mode == "contended" {
            let opened =
                FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024);
            assert_eq!(opened.unwrap_err().kind(), io::ErrorKind::WouldBlock);
            std::process::exit(73);
        }
        let recorder = reopen_after_drop(&path, "s", std::slice::from_ref(&root), 1024).unwrap();
        if mode == "reopen" {
            std::process::exit(73);
        }
        let first = root.join("first.txt");
        let second = root.join("second.txt");
        recorder.begin_file("edit", "write", &first).unwrap();
        if mode == "restore_intent" {
            recorder.begin_file("edit", "write", &second).unwrap();
        }
        std::fs::write(&first, "after first").unwrap();
        if mode == "pending" {
            assert!(recorder.stats().durable);
            std::process::exit(73);
        }
        if mode == "restore_intent" {
            std::fs::write(&second, "after second").unwrap();
        }
        recorder.commit("edit");
        assert!(recorder.stats().durable);
        match mode.as_str() {
            "committed" => {}
            "undone" | "pending_redo" => {
                assert_eq!(recorder.undo(1, false).applied.len(), 1);
                if mode == "pending_redo" {
                    recorder.begin_file("interrupted", "write", &first).unwrap();
                    std::fs::write(&first, "interrupted new edit").unwrap();
                }
            }
            "restore_intent" => {
                let mut inner = recorder.inner.lock().unwrap(); // ubs:ignore synchronous child-process fixture injects partial restore state before deliberate exit; poisoned state must fail
                assert!(recorder.persist(&mut inner, true));
                let mutation = inner.undo_stack.last().unwrap().files[0].clone();
                restore_file(
                    &inner.store,
                    &mutation,
                    &mutation.post,
                    &mutation.pre,
                    false,
                )
                .unwrap();
                // A process dies with the first file restored and the second
                // still at its post-edit bytes, before stack publication.
            }
            _ => panic!("unknown journal test mode {mode}"),
        }
        std::process::exit(73);
    }

    #[test]
    fn abrupt_process_exit_recovers_committed_and_undone_history() {
        for mode in ["committed", "undone"] {
            let dir = private_tempdir();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            let path = root.join("journal.json");
            let file = root.join("first.txt");
            std::fs::write(&file, "before first").unwrap();
            run_journal_child(&root, mode);
            let recorder =
                FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                    .unwrap();
            if mode == "committed" {
                assert_eq!(std::fs::read_to_string(&file).unwrap(), "after first");
                assert_eq!(recorder.undo(1, false).applied.len(), 1);
                assert_eq!(std::fs::read_to_string(&file).unwrap(), "before first");
            }
            assert_eq!(recorder.redo(1, false).applied.len(), 1);
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "after first");
        }
    }

    #[test]
    fn abrupt_process_exit_during_an_edit_blocks_undo_and_old_redo() {
        for mode in ["pending", "pending_redo"] {
            let dir = private_tempdir();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            let path = root.join("journal.json");
            let file = root.join("first.txt");
            std::fs::write(&file, "before first").unwrap();
            run_journal_child(&root, mode);
            let before_open = std::fs::read(&file).unwrap();
            let recorder =
                FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                    .unwrap();
            assert!(recorder.stats().durable);
            assert_eq!(recorder.stats().redo_depth, 0);
            for force in [false, true] {
                assert!(matches!(
                    recorder.undo(1, force).stopped,
                    Some(UndoStop::SnapshotOmitted { .. })
                ));
                assert!(matches!(
                    recorder.redo(1, force).stopped,
                    Some(UndoStop::Exhausted)
                ));
            }
            assert_eq!(std::fs::read(&file).unwrap(), before_open);
        }
    }

    #[test]
    fn abrupt_process_exit_during_restore_preserves_the_partial_state_for_inspection() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let first = root.join("first.txt");
        let second = root.join("second.txt");
        std::fs::write(&first, "before first").unwrap();
        std::fs::write(&second, "before second").unwrap();
        run_journal_child(&root, "restore_intent");
        let journal = std::fs::read(&path).unwrap();
        assert!(
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024,)
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), journal);
        assert_eq!(std::fs::read_to_string(first).unwrap(), "before first");
        assert_eq!(std::fs::read_to_string(second).unwrap(), "after second");
    }

    #[test]
    fn journal_lease_excludes_another_process_and_is_released_on_exit() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let recorder =
            FileMutationRecorder::open_journal(&path, "s", std::slice::from_ref(&root), 1024)
                .unwrap();
        run_journal_child(&root, "contended");
        drop(recorder);
        run_journal_child(&root, "reopen");
        let reopened = reopen_after_drop(&path, "s", std::slice::from_ref(&root), 1024).unwrap();
        assert!(reopened.stats().durable);
    }

    #[test]
    fn separate_sessions_preserve_each_others_external_change_guard() {
        let dir = private_tempdir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let first = FileMutationRecorder::open_journal(
            &root.join("one.json"),
            "one",
            std::slice::from_ref(&root),
            1024,
        )
        .unwrap();
        let second = FileMutationRecorder::open_journal(
            &root.join("two.json"),
            "two",
            std::slice::from_ref(&root),
            1024,
        )
        .unwrap();
        let file = root.join("shared.txt");
        std::fs::write(&file, "before").unwrap();
        first.begin_file("first", "write", &file).unwrap();
        std::fs::write(&file, "first edit").unwrap();
        first.commit("first");
        second.begin_file("second", "write", &file).unwrap();
        std::fs::write(&file, "second edit").unwrap();
        second.commit("second");
        assert!(matches!(
            first.undo(1, false).stopped,
            Some(UndoStop::ExternalChange { .. })
        ));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "second edit");
        assert_eq!(second.undo(1, false).applied.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "first edit");
        assert_eq!(first.undo(1, false).applied.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
    }
}

#[cfg(all(test, not(unix)))]
mod unsupported_platform_tests {
    use super::*;

    #[test]
    fn durable_constructors_refuse_before_creating_or_inspecting_state() {
        let directory = tempfile::tempdir().unwrap();
        let journal = directory.path().join("missing").join("journal.json");
        let error = FileMutationRecorder::open_journal(
            &journal,
            "session",
            &[directory.path().to_path_buf()],
            1024,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(!journal.parent().unwrap().exists());
        let error = FileMutationRecorder::for_session("session", &[]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(
            FileMutationRecorder::default()
                .stats()
                .durability_error
                .is_none()
        );
    }
}
