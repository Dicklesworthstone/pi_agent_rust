//! A bounded, final directory move in an otherwise regular-file transaction.
//!
//! Keep the actual tree: do not reconstruct it from text files or delete it
//! recursively. All contents are snapshotted by the parent transaction. The
//! native rename is the last fallible operation, while import-edit backups
//! remain outside the moving tree. A failed move can therefore roll back edits.

use std::collections::BTreeMap;
use std::fs::{Metadata, Permissions};
use std::io;
use std::path::{Path, PathBuf};

use super::{MAX_TRANSACTION_FILES, target_path};

const MAX_DEPTH: usize = 64;

#[derive(PartialEq, Eq)]
struct Node {
    directory: bool,
    permissions: Permissions,
    // File replacement is expected during import editing. Directory identity,
    // where available, must remain stable until the final rename.
    directory_identity: Option<(u64, u64)>,
}

#[allow(clippy::unnecessary_wraps)] // non-unix targets have no directory identity
fn directory_identity(metadata: &Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

fn inventory(root: &Path) -> io::Result<BTreeMap<PathBuf, Node>> {
    let mut nodes = BTreeMap::new();
    let mut pending = vec![(root.to_path_buf(), 0)];
    while let Some((path, depth)) = pending.pop() {
        if nodes.len() >= MAX_TRANSACTION_FILES || depth > MAX_DEPTH {
            return Err(io::Error::other(
                "directory rename exceeds 1024 entries or 64 levels",
            ));
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !(metadata.is_dir() || metadata.is_file()) {
            return Err(io::Error::other(
                "directory rename cannot include symlinks or special files",
            ));
        }
        let directory = metadata.is_dir();
        if directory {
            // Bound enumeration itself, not just the later snapshot map. Never
            // follow ignored-file rules: a move includes untracked/hidden data.
            for child in std::fs::read_dir(&path)? {
                if nodes.len() + pending.len() + 1 >= MAX_TRANSACTION_FILES {
                    return Err(io::Error::other("directory rename exceeds 1024 entries"));
                }
                pending.push((child?.path(), depth + 1));
            }
        }
        nodes.insert(
            path,
            Node {
                directory,
                permissions: metadata.permissions(),
                directory_identity: directory.then(|| directory_identity(&metadata)).flatten(),
            },
        );
    }
    Ok(nodes)
}

fn require_absent(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(io::Error::other(
            "directory rename destination already exists",
        )),
    }
}

pub(super) struct DirectoryMove {
    pub(super) old: PathBuf,
    pub(super) new: PathBuf,
    nodes: BTreeMap<PathBuf, Node>,
}

impl DirectoryMove {
    pub(super) fn capture(old: &Path, new: &Path) -> io::Result<Self> {
        let old = target_path(old)?;
        let new = target_path(new)?;
        if old.starts_with(&new) || new.starts_with(&old) {
            return Err(io::Error::other("directory rename paths must not overlap"));
        }
        require_absent(&new)?;
        let nodes = inventory(&old)?;
        if !nodes.get(&old).is_some_and(|node| node.directory) {
            return Err(io::Error::other(
                "directory rename source is not a directory",
            ));
        }
        Ok(Self { old, new, nodes })
    }

    pub(super) fn files(&self) -> impl Iterator<Item = &PathBuf> {
        self.nodes
            .iter()
            .filter_map(|(path, node)| (!node.directory).then_some(path))
    }

    pub(super) fn verify(&self) -> io::Result<()> {
        if target_path(&self.old)? != self.old || target_path(&self.new)? != self.new {
            return Err(io::Error::other("directory rename parent changed"));
        }
        require_absent(&self.new)?;
        if inventory(&self.old)? != self.nodes {
            return Err(io::Error::other(
                "directory rename tree changed; request a new plan",
            ));
        }
        Ok(())
    }

    pub(super) fn moved_path(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.old)
            .map_or_else(|_| path.to_path_buf(), |relative| self.new.join(relative))
    }

    pub(super) fn summary(&self, cwd: &Path) -> serde_json::Value {
        serde_json::json!({
            "kind":"directoryRename",
            "from":crate::lsp::display_path(&self.old, cwd),
            "to":crate::lsp::display_path(&self.new, cwd),
            "files":self.files().count(),
            "directories":self.nodes.iter().filter(|(_, node)| node.directory)
                .map(|(path, _)| crate::lsp::display_path(path, cwd)).collect::<Vec<_>>(),
            "preservesUneditedFiles":true,
            "preservesDirectoryPermissions":true
        })
    }

    /// Caller has rechecked all staged final file images. No fallible work may
    /// follow success: backups must simply drop at their original outside paths.
    /// Like the existing file transaction, these are cooperative drift checks,
    /// not an atomic exclusion lock against hostile filesystem writers. In
    /// particular std::fs::rename is not a portable no-replace directory syscall.
    pub(super) fn finish(&self) -> io::Result<()> {
        self.verify()?;
        std::fs::rename(&self.old, &self.new)
    }
}

#[cfg(test)]
mod tests;
