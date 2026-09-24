//! Staged filesystem writes with a single publication point.
//!
//! Never truncate the live file. An exclusive sibling holds all bytes and is
//! synced before publication. Unix publication and cleanup use the same pinned
//! directory as creation. A directory-sync error is a *committed* write with
//! uncertain crash durability, never permission to retry automatically.
//!
//! Replacement changes inode identity: open readers and other hard links keep
//! the old contents. Ordinary Unix mode bits are retained, set-id bits are not;
//! owner/group changes are rejected. Linux xattrs (including POSIX ACLs) are
//! copied and verified, except executable capabilities; integrity-signed files
//! fail closed. Other platforms' extended metadata and timestamps are not copied.
//! Non-Unix uses the platform rename operation and reports file-only sync, not
//! directory crash durability or protection from parent reparse races. This is
//! not a compare-and-swap against non-cooperating writers or a defense against
//! malicious directory relocation. A process crash may leave an unpublished stage.

use super::{
    FS_WRITE_MAX_BYTES, HostCallError, HostCallErrorCode, hash_path, regular_file_metadata,
    write_data,
};
use serde_json::{Value, json};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io::{self, Write};
use std::path::Path;
#[cfg(any(not(unix), test))]
use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod metadata;

const WRITE_CHUNK_BYTES: usize = 64 * 1024;
const STAGE_PREFIX: &str = ".pi-fs-write-";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Prepare,
    Stage,
    Write,
    Metadata,
    FileSync,
    Publish,
    DirectorySync,
}

impl Phase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Stage => "stage",
            Self::Write => "write",
            Self::Metadata => "metadata",
            Self::FileSync => "file_sync",
            Self::Publish => "publish",
            Self::DirectorySync => "directory_sync",
        }
    }
}

fn write_error(path: &Path, phase: Phase, committed: bool, error: &io::Error) -> HostCallError {
    HostCallError {
        code: if error.kind() == io::ErrorKind::InvalidInput {
            HostCallErrorCode::InvalidRequest
        } else {
            HostCallErrorCode::Io
        },
        message: format!("FS_ATOMIC_WRITE: {}: {error}", phase.as_str()),
        details: Some(json!({
            "path_hash": hash_path(path),
            "phase": phase.as_str(),
            "commit_state": if committed { "committed" } else { "not_committed" },
            "durability": if committed { "uncertain" } else { "not_published" },
        })),
        // Even an unpublished failed attempt may have raced an external edit.
        // Require the caller to inspect the result rather than blindly replay.
        retryable: Some(false),
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn conflict() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "destination changed while staging write",
    )
}

struct Parent {
    #[cfg(unix)]
    directory: File,
    #[cfg(not(unix))]
    directory: PathBuf,
    leaf: OsString,
}

impl Parent {
    fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let (directory, leaf) = super::open_io_parent(path, true)?;
            Ok(Self {
                directory,
                leaf: leaf.to_os_string(),
            })
        }
        #[cfg(not(unix))]
        {
            let directory = path
                .parent()
                .ok_or_else(|| invalid("file needs a parent"))?;
            let leaf = path
                .file_name()
                .ok_or_else(|| invalid("file needs a name"))?;
            fs::create_dir_all(directory)?;
            Ok(Self {
                directory: directory.to_path_buf(),
                leaf: leaf.to_os_string(),
            })
        }
    }

    fn open_file(&self, name: &OsStr, create_new: bool) -> io::Result<File> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags, openat};
            let mut flags = OFlags::WRONLY
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC
                | OFlags::NOCTTY;
            if create_new {
                flags |= OFlags::CREATE | OFlags::EXCL;
            }
            Ok(File::from(openat(
                &self.directory,
                name,
                flags,
                Mode::from_raw_mode(0o600),
            )?))
        }
        #[cfg(not(unix))]
        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(create_new);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt as _;
                options.custom_flags(0x0020_0000).security_qos_flags(0);
            }
            options.open(self.directory.join(name))
        }
    }

    fn target(&self) -> io::Result<Option<(File, Metadata)>> {
        // Reject known special objects before opening; verify the actual
        // handle as well. No CREATE here: a failed new write must stay absent.
        #[cfg(unix)]
        {
            use rustix::fs::{AtFlags, FileType, statat};
            match statat(&self.directory, &self.leaf, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(meta) if FileType::from_raw_mode(meta.st_mode) != FileType::RegularFile => {
                    return Err(invalid("Path is not a regular file"));
                }
                Ok(_) => {}
                Err(rustix::io::Errno::NOENT) => return Ok(None),
                Err(error) => return Err(error.into()),
            }
        }
        #[cfg(not(unix))]
        match fs::symlink_metadata(self.directory.join(&self.leaf)) {
            Ok(meta) if !regular_file_metadata(&meta) => {
                return Err(invalid("Path is not a regular file"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        let file = self.open_file(&self.leaf, false)?;
        let meta = file.metadata()?;
        if !regular_file_metadata(&meta) {
            return Err(invalid("Opened object is not a regular file"));
        }
        if meta.permissions().readonly() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "destination is read-only",
            ));
        }
        Ok(Some((file, meta)))
    }

    fn publish(&self, stage: &OsStr, replace: bool) -> io::Result<()> {
        #[cfg(unix)]
        {
            use rustix::fs::{AtFlags, linkat, renameat};
            if replace {
                renameat(&self.directory, stage, &self.directory, &self.leaf)?;
            } else {
                // Atomic create-only publication: an unexpected new target
                // must win, even if it appears after our final absence check.
                linkat(
                    &self.directory,
                    stage,
                    &self.directory,
                    &self.leaf,
                    AtFlags::empty(),
                )?;
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let source = self.directory.join(stage);
            let destination = self.directory.join(&self.leaf);
            if replace {
                fs::rename(source, destination)
            } else {
                fs::hard_link(source, destination)
            }
        }
    }

    // Only the Unix arm has a directory handle to sync; elsewhere this reports
    // `false` without touching `self`, but keeps the shared signature.
    #[cfg_attr(
        not(unix),
        allow(
            clippy::unused_self,
            clippy::unnecessary_wraps,
            clippy::missing_const_for_fn
        )
    )]
    fn sync(&self) -> io::Result<bool> {
        #[cfg(unix)]
        {
            self.directory.sync_all()?;
            Ok(true)
        }
        #[cfg(not(unix))]
        {
            Ok(false)
        }
    }
}

fn same_snapshot(left: &Metadata, right: &Metadata) -> io::Result<bool> {
    let same = left.len() == right.len()
        && left.modified()? == right.modified()?
        && left.permissions() == right.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(same
            && left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
            && left.uid() == right.uid()
            && left.gid() == right.gid())
    }
    #[cfg(not(unix))]
    {
        Ok(same && left.created()? == right.created()?)
    }
}

struct Stage {
    parent: Parent,
    name: OsString,
    file: File,
    // Armed at creation, disarmed only by successful rename/unlink. Publication
    // and staging-name ownership are separate (link publication retains both).
    named: bool,
    committed: bool,
}

impl Stage {
    fn create(parent: Parent) -> io::Result<Self> {
        // Bound collisions without ever opening/truncating somebody else's
        // entry. UUID names also make concurrent writes independent.
        for _ in 0..8 {
            let name = OsString::from(format!("{STAGE_PREFIX}{}.tmp", uuid::Uuid::new_v4()));
            match parent.open_file(&name, true) {
                Ok(file) => {
                    return Ok(Self {
                        parent,
                        name,
                        file,
                        named: true,
                        committed: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "staging-name collisions exhausted",
        ))
    }

    fn verify_name(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            use rustix::fs::{AtFlags, FileType, fstat, statat};
            let named = statat(
                &self.parent.directory,
                &self.name,
                AtFlags::SYMLINK_NOFOLLOW,
            )?;
            let opened = fstat(&self.file)?;
            if named.st_dev != opened.st_dev
                || named.st_ino != opened.st_ino
                || FileType::from_raw_mode(named.st_mode) != FileType::RegularFile
            {
                return Err(conflict());
            }
        }
        #[cfg(not(unix))]
        {
            let named = fs::symlink_metadata(self.parent.directory.join(&self.name))?;
            if !regular_file_metadata(&named) || !same_snapshot(&named, &self.file.metadata()?)? {
                return Err(conflict());
            }
        }
        Ok(())
    }

    fn permissions(&self, existing: Option<&Metadata>) -> io::Result<()> {
        let Some(existing) = existing else {
            return Ok(());
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            let staged = self.file.metadata()?;
            if staged.uid() != existing.uid() || staged.gid() != existing.gid() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "atomic replacement would change file ownership",
                ));
            }
            self.file
                .set_permissions(fs::Permissions::from_mode(existing.mode() & 0o777))
        }
        #[cfg(not(unix))]
        {
            self.file.set_permissions(existing.permissions())
        }
    }

    fn cleanup(&mut self) -> io::Result<()> {
        if !self.named {
            return Ok(());
        }
        // Never unlink a foreign replacement on error or unwind.
        self.verify_name()?;
        #[cfg(unix)]
        {
            rustix::fs::unlinkat(
                &self.parent.directory,
                &self.name,
                rustix::fs::AtFlags::empty(),
            )?;
        }
        #[cfg(not(unix))]
        fs::remove_file(self.parent.directory.join(&self.name))?;
        self.named = false;
        Ok(())
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            tracing::warn!(
                event = "ext.fs.stage_cleanup_failed",
                committed = self.committed,
                error = %error,
                "Could not remove an owned filesystem write stage"
            );
        }
    }
}

pub(super) fn write(params: &Value, path: &Path) -> Result<Value, HostCallError> {
    // Validate *before* opening/creating any filesystem object.
    let bytes = write_data(params, FS_WRITE_MAX_BYTES)?;
    write_with_hook(path, &bytes, |_| Ok(()))
}

fn write_with_hook(
    path: &Path,
    bytes: &[u8],
    mut hook: impl FnMut(Phase) -> io::Result<()>,
) -> Result<Value, HostCallError> {
    hook(Phase::Prepare).map_err(|error| write_error(path, Phase::Prepare, false, &error))?;
    let parent =
        Parent::open(path).map_err(|error| write_error(path, Phase::Prepare, false, &error))?;
    let existing = parent
        .target()
        .map_err(|error| write_error(path, Phase::Prepare, false, &error))?;
    #[cfg(target_os = "linux")]
    let attributes = existing
        .as_ref()
        .map(|(file, _)| metadata::Snapshot::read(file))
        .transpose()
        .map_err(|error| write_error(path, Phase::Metadata, false, &error))?;
    let mut stage =
        Stage::create(parent).map_err(|error| write_error(path, Phase::Stage, false, &error))?;
    let mut phase = Phase::Stage;
    let result = (|| -> io::Result<bool> {
        hook(phase)?;
        for chunk in bytes.chunks(WRITE_CHUNK_BYTES) {
            phase = Phase::Write;
            stage.file.write_all(chunk)?;
            hook(phase)?;
        }
        phase = Phase::Metadata;
        hook(phase)?;
        stage.permissions(existing.as_ref().map(|(_, meta)| meta))?;
        #[cfg(target_os = "linux")]
        if let Some(attributes) = attributes.as_ref() {
            attributes.install(&stage.file)?;
        }
        phase = Phase::FileSync;
        hook(phase)?;
        stage.file.sync_all()?;
        phase = Phase::Publish;
        hook(phase)?;
        stage.verify_name()?;
        let current = stage.parent.target()?;
        match (existing.as_ref(), current.as_ref()) {
            (None, None) => {}
            (Some((_, old)), Some((_, new))) if same_snapshot(old, new)? => {}
            _ => return Err(conflict()),
        }
        #[cfg(target_os = "linux")]
        if let (Some(attributes), Some((file, _))) = (attributes.as_ref(), current.as_ref()) {
            attributes.verify_source(file)?;
            attributes.verify_installed(&stage.file)?;
        }
        stage.parent.publish(&stage.name, existing.is_some())?;
        // Record commit before any later fallible action. No rollback can
        // safely erase a successfully published file.
        stage.committed = true;
        if existing.is_some() {
            stage.named = false;
        }
        phase = Phase::DirectorySync;
        stage.cleanup()?;
        hook(phase)?;
        stage.parent.sync()
    })();
    match result {
        Ok(directory_synced) => Ok(json!({
            "bytes_written": bytes.len(),
            "atomic": true,
            "durability": if directory_synced { "file_and_directory_synced" } else { "file_synced" },
        })),
        Err(error) => {
            let mut error = write_error(path, phase, stage.committed, &error);
            if let Err(cleanup_error) = stage.cleanup() {
                if let Some(details) = error.details.as_mut() {
                    details["stage_cleanup_failed"] = Value::Bool(true);
                }
                tracing::warn!(
                    event = "ext.fs.stage_cleanup_failed",
                    error = %cleanup_error,
                    "Filesystem write failed and its stage could not be removed"
                );
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let path = fs::canonicalize(temp.path()).unwrap().join("target");
        (temp, path)
    }

    fn stages(root: &Path) -> Vec<PathBuf> {
        fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(STAGE_PREFIX)
            })
            .collect()
    }

    #[test]
    fn unpublished_failures_preserve_existing_bytes_and_remove_stages() {
        for fail in [
            Phase::Stage,
            Phase::Write,
            Phase::Metadata,
            Phase::FileSync,
            Phase::Publish,
        ] {
            let (temp, path) = root();
            fs::write(&path, b"original").unwrap();
            let bytes = vec![b'x'; WRITE_CHUNK_BYTES * 2 + 1];
            let mut reached = false;
            let error = write_with_hook(&path, &bytes, |phase| {
                assert_eq!(fs::read(&path).unwrap(), b"original");
                if phase == fail {
                    reached = true;
                    Err(io::Error::other("injected storage failure"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert!(reached);
            assert_eq!(error.details.unwrap()["commit_state"], "not_committed");
            assert_eq!(fs::read(&path).unwrap(), b"original");
            assert!(stages(temp.path()).is_empty());
        }
    }

    #[test]
    fn unpublished_new_writes_never_leave_an_empty_or_partial_destination() {
        for fail in [
            Phase::Stage,
            Phase::Write,
            Phase::Metadata,
            Phase::FileSync,
            Phase::Publish,
        ] {
            let (temp, path) = root();
            assert!(
                write_with_hook(&path, b"new", |phase| {
                    assert!(
                        !path.exists(),
                        "destination became visible before publication"
                    );
                    if phase == fail {
                        Err(io::Error::other("injected"))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            assert!(!path.exists());
            assert!(stages(temp.path()).is_empty());
        }
    }

    #[test]
    fn sync_failure_after_publication_reports_commit_and_never_rolls_back() {
        for existed in [false, true] {
            let (temp, path) = root();
            if existed {
                fs::write(&path, b"old").unwrap();
            }
            let error = write_with_hook(&path, b"committed", |phase| {
                if phase == Phase::DirectorySync {
                    Err(io::Error::other("fsync failed"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert_eq!(error.retryable, Some(false));
            let details = error.details.unwrap();
            assert_eq!(details["commit_state"], "committed");
            assert_eq!(details["durability"], "uncertain");
            assert_eq!(fs::read(&path).unwrap(), b"committed");
            assert!(stages(temp.path()).is_empty());
        }
    }

    #[test]
    fn successful_publication_is_complete_and_leaves_no_stage() {
        let (temp, path) = root();
        for replacement in ["a much longer initial file", "short", ""] {
            let result = write(&json!({"data": replacement}), &path).unwrap();
            assert_eq!(result["atomic"], true);
            assert_eq!(result["bytes_written"], replacement.len());
            assert_eq!(fs::read(&path).unwrap(), replacement.as_bytes());
            assert!(stages(temp.path()).is_empty());
        }
    }

    #[test]
    fn an_external_edit_during_staging_is_not_overwritten() {
        let (temp, path) = root();
        fs::write(&path, b"old").unwrap();
        let error = write_with_hook(&path, b"our new content", |phase| {
            if phase == Phase::Publish {
                fs::write(&path, b"external edit with distinct size")?;
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.details.unwrap()["commit_state"], "not_committed");
        assert_eq!(
            fs::read(&path).unwrap(),
            b"external edit with distinct size"
        );
        assert!(stages(temp.path()).is_empty());
    }

    #[test]
    fn create_only_publication_does_not_clobber_a_racing_creator() {
        let (temp, path) = root();
        let mut stage = Stage::create(Parent::open(&path).unwrap()).unwrap();
        stage.file.write_all(b"ours").unwrap();
        stage.file.sync_all().unwrap();
        assert!(stage.parent.target().unwrap().is_none());
        fs::write(&path, b"racing creator").unwrap();
        // Plant the race *after* the last absence check, at the real atomic
        // publication seam. Replacing link publication with rename fails.
        assert!(stage.parent.publish(&stage.name, false).is_err());
        drop(stage);
        assert_eq!(fs::read(&path).unwrap(), b"racing creator");
        assert!(stages(temp.path()).is_empty());
    }

    #[test]
    fn unwinding_during_staging_cleans_up_without_mutating_the_target() {
        let (temp, path) = root();
        fs::write(&path, b"old").unwrap();
        let result = std::panic::catch_unwind(|| {
            let _ = write_with_hook(&path, b"new", |phase| {
                assert_ne!(phase, Phase::Write, "injected unwind");
                Ok(())
            });
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert!(stages(temp.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn replacement_keeps_open_readers_and_hard_link_aliases_on_the_old_version() {
        use std::io::Read as _;
        let (temp, path) = root();
        fs::write(&path, b"old").unwrap();
        let mut reader = File::open(&path).unwrap();
        fs::hard_link(&path, temp.path().join("alias")).unwrap();
        write(&json!({"data": "new"}), &path).unwrap();
        let mut old = String::new();
        reader.read_to_string(&mut old).unwrap();
        assert_eq!(old, "old");
        assert_eq!(fs::read(temp.path().join("alias")).unwrap(), b"old");
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn stages_are_private_and_existing_executable_mode_is_preserved() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let (temp, path) = root();
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
        write_with_hook(&path, b"new", |phase| {
            if phase == Phase::Stage {
                assert_eq!(fs::metadata(&stages(temp.path())[0])?.mode() & 0o777, 0o600);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o750);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_swapped_in_during_staging_cannot_redirect_publication() {
        use std::os::unix::fs::symlink;
        let (temp, path) = root();
        fs::write(&path, b"old").unwrap();
        let outside = temp.path().join("outside");
        fs::write(&outside, b"secret").unwrap();
        assert!(
            write_with_hook(&path, b"new", |phase| {
                if phase == Phase::Publish {
                    fs::rename(&path, temp.path().join("original"))?;
                    symlink(&outside, &path)?;
                }
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read(&outside).unwrap(), b"secret");
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(stages(temp.path()).is_empty());
    }
}
