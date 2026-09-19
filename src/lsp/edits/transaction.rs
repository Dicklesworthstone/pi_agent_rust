//! Bounded regular-file transactions for LSP edits.
//!
//! Stage every operation before changing a target. Commit final file images,
//! retaining sibling backups until completion. This is rollback on reported
//! failure, not a multi-file filesystem transaction or a power-loss guarantee.
//! Directories, symlinks, shared hard links, special files and path-shape changes are rejected
//! before commit; they need a different snapshot/restore contract.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{File, Permissions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use super::{ApplyOutcome, FileOp, WorkspaceEditPlan, describe_file_op, plan_error};
use crate::error::Result;
use crate::lsp::text::{TextEdit, apply_text_edits, content_hash_for_drift};

const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TRANSACTION_BYTES: usize = 64 * 1024 * 1024;
const MAX_TRANSACTION_FILES: usize = 1024;

pub(super) mod evidence;

#[derive(Clone, PartialEq, Eq)]
struct Image {
    bytes: Arc<[u8]>,
    permissions: Option<Permissions>,
}

struct StagedFile {
    before: Option<Image>,
    after: Option<Image>,
}

#[derive(Default)]
pub(super) struct Transaction {
    files: BTreeMap<PathBuf, StagedFile>,
    text_paths: BTreeSet<PathBuf>,
    operations: Vec<String>,
    bytes: usize,
}

fn conflict(message: impl Into<String>) -> crate::error::Error {
    plan_error("LSP_EDIT_CONFLICT", message)
}

fn io_context(path: &Path, error: &io::Error) -> crate::error::Error {
    conflict(format!("{}: {error}", path.display()))
}

/// Resolve directory aliases once, including a not-yet-created suffix. A
/// final symlink is deliberately NOT followed. Parent traversal is rejected
/// instead of erasing '..' before the existing-prefix resolution.
fn target_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if absolute
        .components()
        .any(|part| part == Component::ParentDir)
    {
        return Err(io::Error::other("parent traversal in edit path"));
    }
    let name = absolute
        .file_name()
        .ok_or_else(|| io::Error::other("invalid file path"))?;
    let mut parent = absolute
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    let mut suffix = Vec::new();
    loop {
        match std::fs::metadata(parent) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => return Err(io::Error::other("edit parent is not a directory")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                suffix.push(
                    parent
                        .file_name()
                        .ok_or_else(|| io::Error::other("invalid parent"))?,
                );
                parent = parent
                    .parent()
                    .ok_or_else(|| io::Error::other("missing ancestor"))?;
            }
            Err(error) => return Err(error),
        }
    }
    let mut resolved = parent.canonicalize()?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    resolved.push(name);
    Ok(resolved)
}

fn read_image(path: &Path) -> io::Result<Option<Image>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::other(
            "workspace edits require regular files, not directories or symlinks",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(io::Error::other(
                "workspace edits cannot replace shared hard links",
            ));
        }
    }
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(io::Error::other("workspace edit file exceeds 16 MiB"));
    }
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(io::Error::other("workspace edit file exceeds 16 MiB"));
    }
    Ok(Some(Image {
        bytes: bytes.into(),
        permissions: Some(metadata.permissions()),
    }))
}

impl Transaction {
    fn spend(&mut self, bytes: usize) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|total| *total <= MAX_TRANSACTION_BYTES)
            .ok_or_else(|| {
                plan_error(
                    "LSP_EDIT_LIMIT",
                    "workspace edit snapshots and replacements exceed 64 MiB",
                )
            })?;
        Ok(())
    }

    fn load(&mut self, path: &Path) -> Result<PathBuf> {
        let path = target_path(path).map_err(|error| io_context(path, &error))?;
        if !self.files.contains_key(&path) {
            if self.files.len() >= MAX_TRANSACTION_FILES {
                return Err(plan_error(
                    "LSP_EDIT_LIMIT",
                    "workspace edit exceeds 1024 files",
                ));
            }
            // A regular-file transaction cannot safely turn a target into an
            // ancestor directory (or the reverse), even via a delete first.
            if self
                .files
                .keys()
                .any(|other| path.starts_with(other) || other.starts_with(&path))
            {
                return Err(conflict(
                    "workspace edit has overlapping file/directory paths",
                ));
            }
            let before = read_image(&path).map_err(|error| io_context(&path, &error))?;
            self.spend(before.as_ref().map_or(0, |image| image.bytes.len()))?;
            self.files.insert(
                path.clone(),
                StagedFile {
                    after: before.clone(),
                    before,
                },
            );
        }
        Ok(path)
    }

    pub(super) fn edit(&mut self, path: &Path, edits: &[TextEdit]) -> Result<()> {
        let normalized = self.load(path)?;
        let image = self.files[&normalized].after.as_ref().ok_or_else(|| {
            conflict(format!(
                "cannot read {}: file missing at this edit step",
                path.display()
            ))
        })?;
        let original = std::str::from_utf8(&image.bytes)
            .map_err(|_| conflict(format!("{}: text edit requires UTF-8", path.display())))?;
        // Bound output allocation before the splicer runs, even for hostile
        // insertion arrays. This conservative bound does not credit deletions.
        let projected = edits
            .iter()
            .try_fold(original.len(), |size, edit| {
                size.checked_add(edit.new_text.len())
                    .filter(|size| *size <= MAX_FILE_BYTES)
            })
            .ok_or_else(|| plan_error("LSP_EDIT_LIMIT", "text replacement exceeds 16 MiB"))?;
        if projected > MAX_TRANSACTION_BYTES.saturating_sub(self.bytes) {
            return Err(plan_error(
                "LSP_EDIT_LIMIT",
                "workspace edit snapshots and replacements exceed 64 MiB",
            ));
        }
        let updated = apply_text_edits(original, edits)
            .map_err(|error| conflict(format!("{}: {error}", path.display())))?;
        let permissions = image.permissions.clone();
        self.spend(projected)?;
        self.files
            .get_mut(&normalized)
            .expect("loaded target")
            .after = Some(Image {
            bytes: updated.into_bytes().into(),
            permissions,
        });
        self.text_paths.insert(path.to_path_buf());
        Ok(())
    }

    pub(super) fn file_op(
        &mut self,
        operation: &FileOp,
        ignore_exists: bool,
        ignore_missing: bool,
    ) -> Result<()> {
        match operation {
            FileOp::Create { path, overwrite } => {
                let path = self.load(path)?;
                let current = &self.files[&path].after;
                if current.is_some() && !overwrite {
                    if ignore_exists {
                        return Ok(());
                    }
                    return Err(conflict(format!(
                        "create target exists: {}",
                        path.display()
                    )));
                }
                let permissions = current.as_ref().and_then(|image| image.permissions.clone());
                self.files.get_mut(&path).expect("loaded target").after = Some(Image {
                    bytes: Arc::from([]),
                    permissions,
                });
            }
            FileOp::Rename {
                old_path,
                new_path,
                overwrite,
            } => {
                let old = self.load(old_path)?;
                let source = self.files[&old].after.clone().ok_or_else(|| {
                    conflict(format!("rename source missing: {}", old_path.display()))
                })?;
                let new = self.load(new_path)?;
                if old == new {
                    return Ok(());
                }
                if self.files[&new].after.is_some() && !overwrite {
                    if ignore_exists {
                        return Ok(());
                    }
                    return Err(conflict(format!(
                        "rename target exists: {}",
                        new_path.display()
                    )));
                }
                self.files.get_mut(&old).expect("loaded source").after = None;
                self.files.get_mut(&new).expect("loaded target").after = Some(source);
            }
            FileOp::Delete { path } => {
                let path = self.load(path)?;
                if self.files[&path].after.is_none() {
                    if ignore_missing {
                        return Ok(());
                    }
                    return Err(conflict(format!(
                        "delete target missing: {}",
                        path.display()
                    )));
                }
                self.files.get_mut(&path).expect("loaded target").after = None;
            }
        }
        self.operations.push(describe_file_op(operation));
        Ok(())
    }

    pub(super) fn check_hashes(&self, hashes: Option<&HashMap<PathBuf, u64>>) -> Result<()> {
        if let Some(hashes) = hashes {
            for (path, expected) in hashes {
                let normalized = target_path(path).map_err(|error| io_context(path, &error))?;
                let Some(staged) = self.files.get(&normalized) else {
                    continue;
                };
                let actual = staged
                    .before
                    .as_ref()
                    .and_then(|image| std::str::from_utf8(&image.bytes).ok())
                    .map(content_hash_for_drift);
                if actual != Some(*expected) {
                    return Err(conflict(format!(
                        "{} changed on disk since the edit was computed; re-run the request",
                        path.display()
                    )));
                }
            }
        }
        Ok(())
    }

    pub(super) fn commit(self) -> Result<ApplyOutcome> {
        self.commit_with(|_, _| Ok(()))
    }

    // The callback is a deterministic failure/interleaving seam for real-file
    // tests. Production uses the same commit path with a no-op callback.
    fn commit_with(
        mut self,
        mut after_change: impl FnMut(usize, &Path) -> io::Result<()>,
    ) -> Result<ApplyOutcome> {
        // Validate ALL read preimages, including no-op targets, before commit.
        for (path, staged) in &self.files {
            verify_image(path, staged.before.as_ref()).map_err(|error| io_context(path, &error))?;
        }
        self.files.retain(|_, staged| staged.before != staged.after);
        let mut changes: Vec<_> = self
            .files
            .into_iter()
            .map(|(path, staged)| Change {
                path,
                staged,
                backup: None,
                replacement: None,
                applied: false,
            })
            .collect();
        // Publish destinations before removing sources. All final images were
        // computed from the ordered virtual state, not from this commit order.
        changes.sort_by_key(|change| change.staged.after.is_none());
        let mut directories = Vec::new();
        let result = (|| -> io::Result<()> {
            for change in &mut changes {
                change.prepare(&mut directories)?;
            }
            for (index, change) in changes.iter_mut().enumerate() {
                change.apply()?;
                after_change(index, &change.path)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let mut failures = Vec::new();
            for change in changes.iter_mut().rev().filter(|change| change.applied) {
                if let Err(rollback) = change.rollback() {
                    let recovery = change.retain_backup();
                    failures.push(format!("{}: {rollback}{recovery}", change.path.display()));
                }
            }
            // Drop scratch files before attempting to remove owned empty dirs.
            drop(changes);
            for directory in directories.iter().rev() {
                if let Err(error) = std::fs::remove_dir(directory) {
                    failures.push(format!(
                        "cannot remove created directory {}: {error}",
                        directory.display()
                    ));
                }
            }
            return Err(if failures.is_empty() {
                plan_error(
                    "LSP_EDIT_APPLY",
                    format!("workspace edit failed; original files restored: {error}"),
                )
            } else {
                plan_error(
                    "LSP_EDIT_ROLLBACK",
                    format!(
                        "workspace edit failed: {error}; rollback incomplete: {}",
                        failures.join("; ")
                    ),
                )
            });
        }
        Ok(ApplyOutcome {
            files_changed: self.text_paths.into_iter().collect(),
            file_ops_applied: self.operations,
        })
    }
}

fn verify_image(path: &Path, expected: Option<&Image>) -> io::Result<()> {
    if target_path(path)? != path || read_image(path)?.as_ref() != expected {
        return Err(io::Error::other(
            "file or parent changed during workspace edit",
        ));
    }
    Ok(())
}

fn ensure_parents(path: &Path, created: &mut Vec<PathBuf>) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    while !parent.try_exists()? {
        missing.push(parent.to_path_buf());
        parent = parent
            .parent()
            .ok_or_else(|| io::Error::other("missing ancestor"))?;
    }
    for directory in missing.into_iter().rev() {
        match std::fs::create_dir(&directory) {
            Ok(()) => created.push(directory),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && directory.is_dir() => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn scratch(path: &Path, image: &Image, prefix: &str) -> io::Result<tempfile::NamedTempFile> {
    let mut file = tempfile::Builder::new().prefix(prefix).tempfile_in(
        path.parent()
            .ok_or_else(|| io::Error::other("missing parent"))?,
    )?;
    file.write_all(&image.bytes)?;
    // Backups stay private and writable until needed for restoration. In
    // particular, a Windows read-only preimage must not make its successful
    // transaction's temporary backup undeletable.
    file.as_file().sync_all()?;
    Ok(file)
}

struct Change {
    path: PathBuf,
    staged: StagedFile,
    backup: Option<tempfile::NamedTempFile>,
    replacement: Option<tempfile::NamedTempFile>,
    applied: bool,
}

impl Change {
    fn prepare(&mut self, directories: &mut Vec<PathBuf>) -> io::Result<()> {
        if self.staged.after.is_some() {
            ensure_parents(&self.path, directories)?;
        }
        if let Some(image) = &self.staged.before {
            self.backup = Some(scratch(&self.path, image, ".pi-lsp-backup-")?);
        }
        if let Some(image) = &self.staged.after {
            self.replacement = Some(scratch(&self.path, image, ".pi-lsp-edit-")?);
            if let Some(permissions) = &image.permissions {
                let file = self
                    .replacement
                    .as_ref()
                    .expect("replacement file")
                    .as_file();
                file.set_permissions(permissions.clone())?;
                file.sync_all()?;
            }
            // Newly created files have tempfile's restrictive permissions.
            // Capture those actual permissions for drift-safe rollback checks.
            if image.permissions.is_none() {
                self.staged
                    .after
                    .as_mut()
                    .expect("replacement image")
                    .permissions = Some(
                    self.replacement
                        .as_ref()
                        .expect("replacement file")
                        .as_file()
                        .metadata()?
                        .permissions(),
                );
            }
        }
        Ok(())
    }

    fn apply(&mut self) -> io::Result<()> {
        verify_image(&self.path, self.staged.before.as_ref())?;
        if let Some(file) = self.replacement.take() {
            let result = if self.staged.before.is_none() {
                file.persist_noclobber(&self.path)
            } else {
                file.persist(&self.path)
            };
            result.map_err(|error| error.error)?;
        } else {
            std::fs::remove_file(&self.path)?;
        }
        self.applied = true;
        Ok(())
    }

    fn rollback(&mut self) -> io::Result<()> {
        // Do not overwrite a concurrent editor's work while undoing ours.
        verify_image(&self.path, self.staged.after.as_ref())?;
        if let Some(backup) = &self.backup
            && let Some(permissions) = self
                .staged
                .before
                .as_ref()
                .and_then(|image| image.permissions.as_ref())
        {
            backup.as_file().set_permissions(permissions.clone())?;
            backup.as_file().sync_all()?;
        }
        if let Some(backup) = self.backup.take() {
            let result = if self.staged.after.is_none() {
                backup.persist_noclobber(&self.path)
            } else {
                backup.persist(&self.path)
            };
            if let Err(error) = result {
                self.backup = Some(error.file);
                return Err(error.error);
            }
        } else {
            std::fs::remove_file(&self.path)?;
        }
        self.applied = false;
        Ok(())
    }

    fn retain_backup(&mut self) -> String {
        let Some(backup) = self.backup.take() else {
            return String::new();
        };
        match backup.keep() {
            Ok((file, path)) => {
                drop(file);
                format!("; original retained at {}", path.display())
            }
            Err(error) => {
                let path = error.file.path().to_path_buf();
                // On this exceptional recovery path, do not let RAII delete
                // the only on-disk preimage merely because keep failed.
                let (file, temporary_path) = error.file.into_parts();
                drop(file);
                std::mem::forget(temporary_path);
                format!(
                    "; could not finalize recovery file {}: {}",
                    path.display(),
                    error.error
                )
            }
        }
    }
}

pub(super) fn apply(
    plan: &WorkspaceEditPlan,
    hashes: Option<&HashMap<PathBuf, u64>>,
) -> Result<ApplyOutcome> {
    let transaction = super::sequence::stage(plan)?;
    transaction.check_hashes(hashes)?;
    transaction.commit()
}

#[cfg(test)]
mod tests;
