//! Confined create-only publication for model-selected artifact paths.
//!
//! Callers supply a workspace root plus a relative destination. Unix publication
//! pins every directory component with no-follow descriptors and hard-links a
//! fully-synced private staging file into the final name, so neither parent
//! symlink swaps nor destination races can redirect or replace user files.

use crate::error::{Error, Result};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct OutputTarget {
    root: PathBuf,
    relative: PathBuf,
    absolute: PathBuf,
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
    let mut directory =
        rustix::fs::open(&target.root, directory_flags, Mode::empty()).map_err(|_| {
            error(
                tool,
                "workspace root could not be pinned without following links",
            )
        })?;
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
    // Re-check final occupancy through the pinned parent. NOFOLLOW + EXCL on the
    // staging file and create-only link publication close both leaf and parent races.
    match rustix::fs::openat(
        &directory,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
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
}
