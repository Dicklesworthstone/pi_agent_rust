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
    _lease: File,
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

fn open_private(path: &Path, create: bool) -> io::Result<File> {
    #[cfg(unix)]
    let file = {
        let access = if create { rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CREATE }
            else { rustix::fs::OFlags::RDONLY };
        File::from(rustix::fs::open(path,
            access | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR)?)
    };
    #[cfg(not(unix))]
    let file = {
        let mut options = OpenOptions::new();
        options.read(true).write(create).create(create);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            options.custom_flags(0x0020_0000); // Open the reparse point itself.
        }
        options.open(path)?
    };
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || FileIdentity::of_path_nofollow(path)? != FileIdentity::of_open_file(&file)? {
        return Err(snapshot_error("unsafe undo journal file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if meta.permissions().mode() & 0o077 != 0 || meta.nlink() != 1 {
            return Err(snapshot_error("undo journal must be private and have one link"));
        }
    }
    Ok(file)
}

impl FileMutationRecorder {
    /// Session journals live beside private agent state, never in imported source trees.
    pub fn for_session(session_id: &str, roots: &[PathBuf]) -> io::Result<Self> {
        let directory = crate::config::Config::global_dir().join("undo");
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        let setup = builder.create(&directory)
            .and_then(|()| std::fs::symlink_metadata(&directory))
            .and_then(|metadata| {
                if metadata.is_dir() && !metadata.file_type().is_symlink() { Ok(()) }
                else { Err(snapshot_error("undo directory must be a real directory")) }
            });
        if let Err(error) = setup {
            return Ok(Self::unavailable_journal(error));
        }
        let name = format!("{:x}.json", sha2::Sha256::digest(session_id.as_bytes()));
        Ok(Self::open_session_journal(&directory.join(name), session_id, roots))
    }

    fn open_session_journal(path: &Path, session_id: &str, roots: &[PathBuf]) -> Self {
        match Self::open_journal(path, session_id, roots, DEFAULT_STORE_BUDGET_BYTES) {
            Ok(recorder) => recorder,
            Err(error) => Self::unavailable_journal(error),
        }
    }

    fn unavailable_journal(error: io::Error) -> Self {
        // Recovery failures must not prevent opening the conversation or overwrite its history.
        tracing::warn!("Undo journal unavailable ({error}); undo/redo disabled; existing journal preserved for inspection");
        let recorder = Self::new(DEFAULT_STORE_BUDGET_BYTES);
        recorder.inner.lock().expect("new undo recorder lock").durability_error =
            Some("Undo recovery unavailable; existing journal preserved for inspection; undo/redo disabled");
        recorder
    }

    /// Open a session's durable recorder. Corrupt, busy, or mismatched journals fail closed.
    #[allow(clippy::too_many_lines)]
    pub fn open_journal(path: &Path, session_id: &str, roots: &[PathBuf], budget_bytes: u64) -> io::Result<Self> {
        if session_id.is_empty() || session_id.len() > 256 || budget_bytes > DEFAULT_STORE_BUDGET_BYTES {
            return Err(snapshot_error("invalid undo journal scope or budget"));
        }
        let parent = std::fs::canonicalize(path.parent().ok_or_else(|| snapshot_error("journal parent required"))?)?;
        let path = parent.join(path.file_name().ok_or_else(|| snapshot_error("journal name required"))?);
        let mut roots = roots.iter().map(std::fs::canonicalize).collect::<io::Result<Vec<_>>>()?;
        roots.sort(); roots.dedup();
        if roots.is_empty() { return Err(snapshot_error("undo journal requires a workspace")); }
        let lease = open_private(&path.with_extension("lock"), true)?;
        fs4::FileExt::try_lock_exclusive(&lease)?;
        let journal = Journal { path, session_id:session_id.into(), roots, _lease:lease };
        let mut inner = RecorderInner::default();
        match open_private(&journal.path, false) {
            Ok(file) => {
                let limit = budget_bytes.saturating_mul(2).saturating_add(4 * 1024 * 1024);
                let bytes = read_bounded_snapshot(file, limit)?;
                let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|_| snapshot_error("invalid undo journal"))?;
                if envelope.sha256 != format!("{:x}", sha2::Sha256::digest(envelope.payload.as_bytes())) {
                    return Err(snapshot_error("undo journal checksum mismatch"));
                }
                let document: Document = serde_json::from_str(&envelope.payload).map_err(|_| snapshot_error("invalid undo journal payload"))?;
                if document.schema != SCHEMA || document.session_id != session_id || document.roots != journal.roots || document.restoring {
                    return Err(snapshot_error("undo journal scope mismatch or interrupted restore; manual reconciliation required"));
                }
                if document.undo.len() + document.redo.len() + document.pending.len() > MAX_UNITS {
                    return Err(snapshot_error("undo journal unit limit exceeded"));
                }
                let mut blobs = HashMap::new();
                let mut bytes = 0_u64;
                for (id, encoded) in document.blobs {
                    if encoded.len() as u64 > MAX_SNAPSHOT_FILE_BYTES * 2 { return Err(snapshot_error("oversized undo blob")); }
                    let content = base64::engine::general_purpose::STANDARD.decode(encoded)
                        .map_err(|_| snapshot_error("invalid undo blob encoding"))?;
                    bytes = bytes.saturating_add(content.len() as u64);
                    if content.len() as u64 > MAX_SNAPSHOT_FILE_BYTES || bytes > budget_bytes
                        || BlobId::from(sha2::Sha256::digest(&content)) != id || blobs.insert(id, content).is_some()
                    { return Err(snapshot_error("invalid or oversized undo blob")); }
                }
                inner.undo_stack = document.undo;
                inner.redo_stack = document.redo;
                inner.evicted_units = document.evicted_units;
                inner.unrecorded_files = document.unrecorded_files;
                let total_files = inner.undo_stack.iter().chain(&inner.redo_stack)
                    .chain(document.pending.values()).map(|unit| unit.files.len()).sum::<usize>();
                if total_files > MAX_UNITS { return Err(snapshot_error("undo journal file limit exceeded")); }
                // A pre-write snapshot is not evidence that the edit completed.
                for (_, mut unit) in document.pending {
                    for file in &mut unit.files { file.post = FileState::Unrecorded; }
                    inner.undo_stack.push(unit);
                    inner.unrecorded_files = inner.unrecorded_files.saturating_add(1);
                }
                for unit in inner.undo_stack.iter().chain(&inner.redo_stack) {
                    if unit.files.len() > MAX_UNITS { return Err(snapshot_error("undo file limit exceeded")); }
                    for file in &unit.files {
                        if !file.path.is_absolute()
                            || file.path.components().any(|part| matches!(part, std::path::Component::ParentDir))
                            || !journal.roots.iter().any(|root| file.path.starts_with(root))
                        { return Err(snapshot_error("undo path escapes its saved workspace")); }
                        if let Some(parent) = &file.parent {
                            // Historical identifiers detect ordinary replacement, not hostile inode reuse.
                            if file.path.parent() != Some(parent.path.as_path()) || capture_parent(&file.path)? != *parent {
                                return Err(snapshot_error("undo parent changed since recording"));
                            }
                        }
                        // Missing historical parents remain gaps; never bless a newly created directory.
                        for state in [&file.pre, &file.post] {
                            if let FileState::Present(id) = state {
                                let content = blobs.get(id).ok_or_else(|| snapshot_error("undo blob missing"))?;
                                inner.store.insert(content.clone());
                            }
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(Self { inner:Mutex::new(inner), budget_bytes, journal:Some(journal) })
    }

    pub(super) fn persist(&self, inner: &mut RecorderInner, restoring: bool) -> bool {
        let Some(journal) = &self.journal else { return true; };
        if inner.durability_error.is_some() { return false; }
        match journal.save(inner, restoring, self.budget_bytes) {
            Ok(()) => true,
            Err(_) => {
                let message = "Undo journal could not be saved; restart recovery is unavailable";
                tracing::warn!("{message}");
                inner.durability_error = Some(message);
                false
            }
        }
    }
}

impl Journal {
    fn save(&self, inner: &RecorderInner, restoring: bool, budget_bytes: u64) -> io::Result<()> {
        if inner.undo_stack.len() + inner.redo_stack.len() + inner.pending.len() > MAX_UNITS {
            return Err(snapshot_error("undo journal unit limit exceeded"));
        }
        let files = inner.undo_stack.iter().chain(&inner.redo_stack).chain(inner.pending.values())
            .map(|unit| unit.files.len()).sum::<usize>();
        if files > MAX_UNITS || inner.store.bytes > budget_bytes {
            return Err(snapshot_error("undo journal file or backup byte limit exceeded"));
        }
        if inner.undo_stack.iter().chain(&inner.redo_stack).chain(inner.pending.values())
            .flat_map(|unit| &unit.files).any(|file| !file.path.is_absolute()
                || file.path.components().any(|part| matches!(part, std::path::Component::ParentDir))
                || !self.roots.iter().any(|root| file.path.starts_with(root))
                || file.parent.as_ref().is_some_and(|parent| file.path.parent() != Some(parent.path.as_path())))
        {
            return Err(snapshot_error("undo journal path outside its original workspace scope"));
        }
        let mut blobs = inner.store.blobs.iter().map(|(id, content)|
            (*id, base64::engine::general_purpose::STANDARD.encode(content))).collect::<Vec<_>>();
        blobs.sort_by_key(|(id, _)| *id);
        let document = Document { schema:SCHEMA.into(), session_id:self.session_id.clone(), roots:self.roots.clone(), blobs,
            undo:inner.undo_stack.clone(), redo:inner.redo_stack.clone(), pending:inner.pending.clone().into_iter().collect(),
            evicted_units:inner.evicted_units,unrecorded_files:inner.unrecorded_files,restoring };
        let payload = serde_json::to_string(&document).map_err(io::Error::other)?;
        let envelope = Envelope { sha256:format!("{:x}", sha2::Sha256::digest(payload.as_bytes())),payload };
        let encoded = serde_json::to_vec(&envelope).map_err(io::Error::other)?;
        if encoded.len() as u64 > budget_bytes.saturating_mul(2).saturating_add(4 * 1024 * 1024) {
            return Err(snapshot_error("undo journal byte limit exceeded"));
        }
        let parent = self.path.parent().ok_or_else(|| snapshot_error("journal parent missing"))?;
        let mut staged = tempfile::NamedTempFile::new_in(parent)?;
        staged.write_all(&encoded)?;
        staged.as_file().sync_all()?;
        staged.persist(&self.path).map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_restores_history_and_conflict_checks() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let file = root.join("file.rs");
        let journal = root.join("session.undo.json");
        std::fs::write(&file, "before").unwrap();
        let open = || FileMutationRecorder::open_journal(&journal, "session", std::slice::from_ref(&root), 1024).unwrap();
        let recorder = open();
        recorder.begin_file("edit", "edit", &file);
        std::fs::write(&file, "after").unwrap();
        recorder.commit("edit");
        assert!(recorder.stats().durable);
        drop(recorder);
        let recorder = open();
        std::fs::write(&file, "external").unwrap();
        assert!(matches!(recorder.undo(1, false).stopped, Some(UndoStop::ExternalChange { .. })));
        std::fs::write(&file, "after").unwrap();
        assert_eq!(recorder.undo(1, false).applied.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
        drop(recorder);
        let recorder = open();
        assert_eq!(recorder.redo(1, false).applied.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
    }

    #[test]
    fn pending_edits_and_corrupt_journals_never_replay() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let file = root.join("file");
        std::fs::write(&file, "before").unwrap();
        let recorder = FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).unwrap();
        assert!(FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).is_err());
        recorder.begin_file("pending","write",&file);
        std::fs::write(&file,"ambiguous").unwrap();
        drop(recorder);
        let recorder = FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).unwrap();
        assert!(matches!(recorder.undo(1,false).stopped,Some(UndoStop::SnapshotOmitted { .. })));
        drop(recorder);
        std::fs::write(&path,"corrupt").unwrap();
        assert!(FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(),"ambiguous");
    }

    #[test]
    fn pending_creation_with_missing_parent_reopens_as_a_gap() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let file = dir.path().join("new").join("nested").join("file.rs");
        let recorder = FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).unwrap();
        recorder.begin_file("pending", "write", &file);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "partial").unwrap();
        drop(recorder);
        let recorder = FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).unwrap();
        assert!(matches!(recorder.undo(1,true).stopped, Some(UndoStop::SnapshotOmitted { .. })));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "partial");
    }

    #[test]
    fn oversized_commit_preserves_a_loadable_gap_instead_of_unloadable_success() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let file = root.join("file");
        std::fs::write(&file, "before").unwrap();
        let recorder = FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),8).unwrap();
        recorder.begin_file("edit", "edit", &file);
        std::fs::write(&file, "after").unwrap();
        recorder.commit("edit");
        assert!(!recorder.stats().durable);
        assert!(recorder.stats().durability_error.is_some());
        drop(recorder);
        let recorder = FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),8).unwrap();
        assert!(matches!(recorder.undo(1,true).stopped, Some(UndoStop::SnapshotOmitted { .. })));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
    }

    #[test]
    fn excessive_files_cannot_overwrite_the_last_recoverable_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let recorder = FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).unwrap();
        let mut inner = recorder.inner.lock().unwrap();
        assert!(recorder.persist(&mut inner, false));
        let original = std::fs::read(&path).unwrap();
        inner.pending.insert("many".into(), MutationUnit {
            tool_name:"write".into(), at_ms:0,
            files:(0..=MAX_UNITS).map(|i| FileMutation {
                path:root.join(format!("file{i}")), parent:None,
                pre:FileState::Absent, post:FileState::Unrecorded,
            }).collect(),
        });
        assert!(!recorder.persist(&mut inner, false));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn edits_outside_the_original_roots_do_not_claim_durability() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let recorder = FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).unwrap();
        assert!(recorder.persist(&mut recorder.inner.lock().unwrap(), false));
        let original = std::fs::read(&path).unwrap();
        recorder.begin_file("outside", "write", &outside.path().join("file"));
        assert!(!recorder.stats().durable);
        assert!(recorder.stats().durability_error.is_some());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn session_recovery_failure_disables_replay_without_overwriting_journal() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let added = root.join("added");
        std::fs::create_dir(&added).unwrap();
        let path = root.join("journal.json");
        let recorder = FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).unwrap();
        assert!(recorder.persist(&mut recorder.inner.lock().unwrap(), false));
        let original = std::fs::read(&path).unwrap();
        let busy = FileMutationRecorder::open_session_journal(&path,"s",std::slice::from_ref(&root));
        assert!(busy.stats().durability_error.is_some());
        drop(recorder);
        let changed_scope = FileMutationRecorder::open_session_journal(&path,"s",&[root.clone(), added]);
        assert!(matches!(changed_scope.undo(1,true).stopped, Some(UndoStop::RestoreFailed { .. })));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::write(&path, "corrupt").unwrap();
        let corrupt = FileMutationRecorder::open_session_journal(&path,"s",std::slice::from_ref(&root));
        assert!(corrupt.stats().durability_error.is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "corrupt");
    }

    #[test]
    fn interrupted_restore_and_wrong_session_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let recorder = FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).unwrap();
        {
            let mut inner = recorder.inner.lock().unwrap();
            assert!(recorder.persist(&mut inner,true));
        }
        drop(recorder);
        assert!(FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).is_err());
        assert!(FileMutationRecorder::open_journal(&path,"other",std::slice::from_ref(&root),1024).is_err());
    }

    #[test]
    fn journal_failure_is_visible_and_blocks_restore() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("journal.json");
        let file = root.join("file");
        std::fs::write(&file,"before").unwrap();
        let mut recorder = FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).unwrap();
        recorder.journal.as_mut().unwrap().path = root.join("missing").join("journal.json");
        recorder.begin_file("edit","write",&file);
        assert!(recorder.stats().durability_error.is_some());
        assert!(!recorder.stats().durable);
        assert!(matches!(recorder.undo(1,false).stopped,Some(UndoStop::RestoreFailed { .. })));
        assert_eq!(std::fs::read_to_string(file).unwrap(),"before");
    }

    #[cfg(unix)]
    #[test]
    fn restart_keeps_canonical_parent_when_recording_through_alias() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let file = alias.join("file");
        let path = root.join("journal.json");
        std::fs::write(&file, "before").unwrap();
        let recorder = FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).unwrap();
        recorder.begin_file("edit","edit",&file);
        std::fs::write(&file,"after").unwrap();
        recorder.commit("edit");
        drop(recorder);
        let recorder = FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).unwrap();
        assert_eq!(recorder.undo(1,false).applied.len(),1);
        assert_eq!(std::fs::read_to_string(file).unwrap(),"before");
    }

    #[test]
    fn replaced_parent_is_not_blessed_when_loading_history() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let parent = root.join("parent");
        std::fs::create_dir(&parent).unwrap();
        let file = parent.join("file");
        let path = root.join("journal.json");
        std::fs::write(&file,"before").unwrap();
        let recorder = FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).unwrap();
        recorder.begin_file("edit","edit",&file);
        std::fs::write(&file,"after").unwrap();
        recorder.commit("edit");
        drop(recorder);
        std::fs::rename(&parent,root.join("old_parent")).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(&file,"after").unwrap();
        assert!(FileMutationRecorder::open_journal(&path,"s",std::slice::from_ref(&root),1024).is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(),"after");
    }
}
