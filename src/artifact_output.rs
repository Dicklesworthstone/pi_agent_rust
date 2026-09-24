//! Confined create-only publication for model-selected artifact paths.
//!
//! Callers supply a workspace root plus a relative destination. Unix publication
//! retains the workspace descriptor from resolution, pins every relative
//! directory component with no-follow descriptors, and hard-links a fully-synced
//! private staging file into the final name. Path swaps cannot redirect writes
//! into a replacement workspace or replace existing user files.

use crate::error::{Error, Result};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct OutputTarget {
    root: PathBuf,
    relative: PathBuf,
    absolute: PathBuf,
    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    root_directory: std::sync::Arc<rustix::fd::OwnedFd>,
}

impl OutputTarget {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.absolute
    }
}

fn error(tool: &str, message: impl Into<String>) -> Error {
    Error::tool(tool, message)
}

pub fn resolve_new(cwd: &Path, requested: &str, tool: &str) -> Result<OutputTarget> {
    if requested.is_empty()
        || requested.len() > 4096
        || requested.contains('\0')
        || requested.contains('\\')
        || requested.chars().any(char::is_control)
    {
        return Err(error(
            tool,
            "output_path must be a nonempty control-free relative path of at most 4096 bytes using slash separators",
        ));
    }
    let relative = PathBuf::from(requested);
    if relative.is_absolute() || relative.file_name().is_none() {
        return Err(error(
            tool,
            "output_path must name a relative file inside the workspace",
        ));
    }
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(error(
            tool,
            "output_path may not contain parent traversal, a root, or a platform path prefix",
        ));
    }
    let relative: PathBuf = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect();
    if relative.as_os_str().is_empty() || relative.file_name().is_none() {
        return Err(error(tool, "output_path must name a file"));
    }
    let root = std::fs::canonicalize(cwd)
        .map_err(|failure| error(tool, format!("cannot resolve workspace root: {failure}")))?;
    // Resolve before remote generation/download work and keep this identity
    // across awaits. Reopening the pathname in publish() would let a replaced
    // ancestor redirect even an O_NOFOLLOW open of the workspace itself.
    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    let root_directory = {
        use rustix::fs::{Mode, OFlags};
        std::sync::Arc::new(
            rustix::fs::open(
                &root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(|_| {
                error(
                    tool,
                    "workspace root could not be pinned without following links",
                )
            })?,
        )
    };
    let absolute = root.join(&relative);
    // Early no-clobber check avoids expensive remote work when the destination
    // is already occupied. Final publication repeats this atomically.
    match std::fs::symlink_metadata(&absolute) {
        Ok(_) => return Err(error(tool, "output_path already exists; choose a new file")),
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {}
        Err(failure) => {
            return Err(error(
                tool,
                format!("cannot inspect output_path: {failure}"),
            ));
        }
    }
    // Existing ancestors must already be real directories, not links. Missing
    // components are created only by publish() under the pinned workspace.
    let mut cursor = root.clone();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            cursor.push(name);
            match std::fs::symlink_metadata(&cursor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(error(tool, "output_path passes through a symbolic link"));
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(error(tool, "output_path parent is not a directory"));
                }
                Ok(_) => {}
                Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => break,
                Err(failure) => {
                    return Err(error(
                        tool,
                        format!("cannot inspect output_path parent: {failure}"),
                    ));
                }
            }
        }
    }
    Ok(OutputTarget {
        root,
        relative,
        absolute,
        #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
        root_directory,
    })
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
struct Stage<'a> {
    directory: &'a rustix::fd::OwnedFd,
    name: String,
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
impl Drop for Stage<'_> {
    fn drop(&mut self) {
        let _ = rustix::fs::unlinkat(
            self.directory,
            self.name.as_str(),
            rustix::fs::AtFlags::empty(),
        );
    }
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
pub fn publish(target: &OutputTarget, bytes: &[u8], tool: &str) -> Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    use std::io::Write as _;

    if bytes.is_empty() {
        return Err(error(tool, "refusing to publish an empty artifact"));
    }
    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let mut directory = target
        .root_directory
        .as_ref()
        .try_clone()
        .map_err(|_| error(tool, "cannot retain pinned workspace directory"))?;
    let pinned_root = rustix::fs::fstat(&directory)
        .map_err(|_| error(tool, "cannot inspect pinned workspace directory"))?;
    let current_root = rustix::fs::statat(
        rustix::fs::CWD,
        &target.root,
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(|_| error(tool, "workspace root changed before artifact publication"))?;
    if pinned_root.st_dev != current_root.st_dev || pinned_root.st_ino != current_root.st_ino {
        return Err(error(
            tool,
            "workspace root changed before artifact publication",
        ));
    }
    // The identity check avoids reporting success for an already-stale path.
    // Subsequent writes use the retained descriptor, never the checked path,
    // so a later rename cannot redirect publication to a different workspace.
    if let Some(parent) = target.relative.parent() {
        for component in parent.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            let next = match rustix::fs::openat(&directory, name, directory_flags, Mode::empty()) {
                Ok(next) => next,
                Err(rustix::io::Errno::NOENT) => {
                    match rustix::fs::mkdirat(
                        &directory,
                        name,
                        Mode::RUSR | Mode::WUSR | Mode::XUSR,
                    ) {
                        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                        Err(_) => return Err(error(tool, "cannot create artifact directory")),
                    }
                    rustix::fs::openat(&directory, name, directory_flags, Mode::empty())
                        .map_err(|_| error(tool, "artifact directory is missing or symbolic"))?
                }
                Err(_) => {
                    return Err(error(
                        tool,
                        "artifact path passes through a symbolic link or non-directory",
                    ));
                }
            };
            directory = next;
        }
    }
    let name = target
        .relative
        .file_name()
        .ok_or_else(|| error(tool, "output_path does not name a file"))?;
    // Inspect without opening: a FIFO inserted after resolve_new() would block
    // an RDONLY open indefinitely, while opening a device can have side effects.
    // Every existing entry, including a dangling link, is a no-clobber conflict.
    // linkat below still enforces create-only publication after this check.
    match rustix::fs::statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => {
            return Err(error(
                tool,
                "output_path already exists; refusing to overwrite",
            ));
        }
        Err(rustix::io::Errno::NOENT) => {}
        Err(_) => return Err(error(tool, "cannot safely inspect output_path")),
    }
    let stage_name = format!(".pi-artifact-{}.tmp", uuid::Uuid::new_v4().simple());
    let stage_fd = rustix::fs::openat(
        &directory,
        stage_name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|_| error(tool, "cannot create private artifact staging file"))?;
    let _stage = Stage {
        directory: &directory,
        name: stage_name.clone(),
    };
    let mut file = std::fs::File::from(stage_fd);
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|failure| error(tool, format!("cannot write artifact: {failure}")))?;
    rustix::fs::linkat(
        &directory,
        stage_name.as_str(),
        &directory,
        name,
        AtFlags::empty(),
    )
    .map_err(|_| {
        error(
            tool,
            "artifact destination exists or cannot be published without overwrite",
        )
    })?;
    Ok(())
}

#[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
pub fn publish(target: &OutputTarget, bytes: &[u8], tool: &str) -> Result<()> {
    use std::io::Write as _;
    if bytes.is_empty() {
        return Err(error(tool, "refusing to publish an empty artifact"));
    }
    let parent = target
        .absolute
        .parent()
        .ok_or_else(|| error(tool, "output_path has no parent"))?;
    std::fs::create_dir_all(parent)
        .map_err(|failure| error(tool, format!("cannot create artifact directory: {failure}")))?;
    let canonical_parent = std::fs::canonicalize(parent).map_err(|failure| {
        error(
            tool,
            format!("cannot resolve artifact directory: {failure}"),
        )
    })?;
    if !canonical_parent.starts_with(&target.root) {
        return Err(error(
            tool,
            "output_path escaped the workspace through a linked directory",
        ));
    }
    let mut staged = tempfile::NamedTempFile::new_in(&canonical_parent)
        .map_err(|failure| error(tool, format!("cannot stage artifact: {failure}")))?;
    staged
        .write_all(bytes)
        .and_then(|()| staged.as_file().sync_all())
        .map_err(|failure| error(tool, format!("cannot write artifact: {failure}")))?;
    let canonical_parent_after = std::fs::canonicalize(parent).map_err(|failure| {
        error(
            tool,
            format!("cannot revalidate artifact directory: {failure}"),
        )
    })?;
    if canonical_parent_after != canonical_parent
        || !canonical_parent_after.starts_with(&target.root)
    {
        return Err(error(tool, "artifact directory changed before publication"));
    }
    staged.persist_noclobber(&target.absolute).map_err(|_| {
        error(
            tool,
            "artifact destination exists or cannot be published without overwrite",
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_escape_and_platform_separators_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        for path in [
            "../outside.png",
            "a/../../outside.png",
            "/tmp/outside.png",
            "C:\\outside.png",
            "a\\outside.png",
            ".",
            "",
            "bad\nname.png",
        ] {
            assert!(resolve_new(root.path(), path, "test").is_err(), "{path:?}");
        }
        assert!(resolve_new(root.path(), "nested/result.png", "test").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn existing_symlink_ancestors_and_destinations_are_rejected() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("linked")).unwrap();
        assert!(resolve_new(root.path(), "linked/result.png", "test").is_err());
        symlink(outside.path().join("missing"), root.path().join("leaf.png")).unwrap();
        assert!(resolve_new(root.path(), "leaf.png", "test").is_err());
    }

    #[test]
    fn publication_is_create_only_and_nested() {
        let root = tempfile::tempdir().unwrap();
        let target = resolve_new(root.path(), "nested/result.bin", "test").unwrap();
        publish(&target, b"first", "test").unwrap();
        assert_eq!(std::fs::read(target.path()).unwrap(), b"first");
        assert!(publish(&target, b"second", "test").is_err());
        assert_eq!(std::fs::read(target.path()).unwrap(), b"first");
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn cloned_target_rejects_replacement_workspace() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        let moved = parent.path().join("original-workspace");
        std::fs::create_dir(&root).unwrap();
        let original = resolve_new(&root, "nested/result.bin", "test").unwrap();
        let target = original.clone();
        drop(original);
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();

        let failure = publish(&target, b"private payload", "test").unwrap_err();
        assert!(failure.to_string().contains("workspace root changed"));
        assert!(!root.join("nested").exists());
        assert!(!moved.join("nested").exists());
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn swapped_workspace_ancestor_cannot_redirect_publication() {
        use std::os::unix::fs::symlink;
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ancestor = parent.path().join("ancestor");
        let root = ancestor.join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(outside.path().join("workspace")).unwrap();
        let target = resolve_new(&root, "result.bin", "test").unwrap();
        let moved = parent.path().join("original-ancestor");
        std::fs::rename(&ancestor, &moved).unwrap();
        symlink(outside.path(), &ancestor).unwrap();

        assert!(publish(&target, b"private payload", "test").is_err());
        assert!(!outside.path().join("workspace/result.bin").exists());
        assert!(!moved.join("workspace/result.bin").exists());
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn symlink_inserted_after_resolution_is_an_occupied_destination() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = resolve_new(root.path(), "result.bin", "test").unwrap();
        let missing = outside.path().join("missing.bin");
        symlink(&missing, target.path()).unwrap();

        let failure = publish(&target, b"private payload", "test").unwrap_err();
        assert!(failure.to_string().contains("output_path already exists"));
        assert_eq!(std::fs::read_link(target.path()).unwrap(), missing);
        assert!(!missing.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn fifo_inserted_after_resolution_does_not_block_publication() {
        use rustix::fs::{Mode, OFlags};
        use std::os::unix::fs::FileTypeExt as _;
        use std::sync::mpsc;
        use std::time::Duration;

        let root = tempfile::tempdir().unwrap();
        let target = resolve_new(root.path(), "result.bin", "test").unwrap();
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            target.path(),
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        let worker_target = target.clone();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = publish(&worker_target, b"payload", "test")
                .map_err(|failure| failure.to_string());
            let _ = send.send(result);
        });
        let result = receive.recv_timeout(Duration::from_secs(5));
        if result.is_err() {
            // Unblock the old RDONLY probe before failing, rather than leaving
            // a stranded test thread. No sleep or FIFO peer is needed to pass.
            let peer = rustix::fs::open(
                target.path(),
                OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .unwrap();
            worker.join().unwrap();
            drop(peer);
            panic!("publication did not reject the occupied FIFO without opening it");
        }
        worker.join().unwrap();
        let failure = result.unwrap().unwrap_err();
        assert!(failure.contains("output_path already exists"));
        assert!(std::fs::symlink_metadata(target.path()).unwrap().file_type().is_fifo());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
