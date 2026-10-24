//! Preserve index-only tracked files without treating sparse absence as deletion.
//!
//! Only absent skip-worktree entries keep their indexed content. Present files
//! are refreshed normally. No skip flags or configuration are written to the
//! source index; the flags installed here belong to the disposable private index.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::Component;

use super::{
    File, Path, PathBuf, Result, Scratch, command, failure, fs, object_id, repository_path, run,
};

const MAX_SKIPPED_PATHS: usize = 65_536;

/// Modern sparse checkouts use worktree-local configuration. Legacy shared
/// sparse settings cannot safely be disabled for a child without changing the
/// parent, so keep refusing that layout instead of migrating user config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CheckoutMode {
    sparse: bool,
    worktree_config: bool,
}

fn configured_bool(repo: &Path, key: &str) -> Result<bool> {
    let output = command(repo)
        .args(["config", "--bool", "--get", key])
        .output()
        .map_err(|_| invalid())?;
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    if !output.status.success() {
        return Err(invalid());
    }
    match output.stdout.as_slice() {
        b"true\n" | b"true\r\n" => Ok(true),
        b"false\n" | b"false\r\n" => Ok(false),
        _ => Err(invalid()),
    }
}

impl CheckoutMode {
    pub(super) fn read(repo: &Path) -> Result<Self> {
        let mode = Self {
            sparse: configured_bool(repo, "core.sparseCheckout")?,
            worktree_config: configured_bool(repo, "extensions.worktreeConfig")?,
        };
        if mode.sparse && !mode.worktree_config {
            return Err(failure(
                "PI_ISO_SPARSE_UNSUPPORTED",
                "Sparse isolation requires worktree-local configuration; use a modern git sparse-checkout setup or a separate full checkout. Shared source settings were not changed",
            ));
        }
        Ok(mode)
    }

    pub(super) fn require_current(self, repo: &Path) -> Result<()> {
        if Self::read(repo)? != self {
            return Err(failure(
                "PI_ISO_PARENT_CHANGED",
                "Checkout mode changed during isolation; any created child checkout is retained",
            ));
        }
        Ok(())
    }

    pub(super) fn configure_child(self, child: &Path) -> Result<()> {
        if self.worktree_config {
            // Without the extension, --worktree degrades to shared config.
            // Target the child's exact config file instead, so a concurrent
            // extension change cannot redirect our write to shared config.
            if !configured_bool(child, "extensions.worktreeConfig")? {
                return Err(failure(
                    "PI_ISO_PARENT_CHANGED",
                    "Worktree-local configuration became unavailable; child retained without launching",
                ));
            }
            let config = repository_path(child, "--absolute-git-dir")?.join("config.worktree");
            for key in ["core.sparseCheckout", "index.sparse"] {
                run(
                    command(child)
                        .args(["config", "--file"])
                        .arg(&config)
                        .args([key, "false"]),
                    "configure the isolated child as a full checkout",
                )?;
            }
        }
        if configured_bool(child, "core.sparseCheckout")?
            || (self.worktree_config
                && (!configured_bool(child, "extensions.worktreeConfig")?
                    || configured_bool(child, "index.sparse")?))
        {
            return Err(failure(
                "PI_ISO_INDEX_STATE",
                "Child checkout is not full; child retained without launching",
            ));
        }
        Ok(())
    }
}

fn invalid() -> crate::error::Error {
    failure(
        "PI_ISO_INDEX_STATE",
        "Cannot preserve bounded skip-worktree index state",
    )
}

fn records(bytes: &[u8]) -> Result<impl Iterator<Item = &[u8]>> {
    if !bytes.is_empty()
        && (bytes.last() != Some(&0)
            || bytes.first() == Some(&0)
            || bytes.windows(2).any(|pair| pair == [0, 0]))
    {
        return Err(invalid());
    }
    Ok(bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty()))
}

fn path(bytes: &[u8]) -> Result<PathBuf> {
    if bytes.contains(&0)
        || bytes
            .split(|byte| *byte == b'/')
            .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return Err(invalid());
    }
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt as _;
        PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
    };
    #[cfg(not(unix))]
    let path = PathBuf::from(std::str::from_utf8(bytes).map_err(|_| invalid())?);
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(invalid());
    }
    Ok(path)
}

/// Do not follow an intermediate symlink while deciding whether an excluded
/// file is absent. An obstructed path is not permission to snapshot elsewhere.
fn absent(repo: &Path, bytes: &[u8]) -> Result<bool> {
    let relative = path(bytes)?;
    let mut parts = relative.components().peekable();
    let mut current = repo.to_path_buf();
    while let Some(part) = parts.next() {
        current.push(part.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if parts.peek().is_some() => {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(invalid());
                }
            }
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(_) => return Err(invalid()),
        }
    }
    Err(invalid())
}

fn tagged(scratch: &Scratch, repo: &Path, name: &str) -> Result<Vec<u8>> {
    let mut query = command(repo);
    // Omitting --sparse is intentional: sparse-directory entries must expand
    // to concrete file records before preservation, never an opaque directory.
    query.args(["ls-files", "-v", "--full-name", "-z"]);
    scratch.records(query, name)
}

fn selected_entries(
    bytes: &[u8],
    wanted: &BTreeSet<Vec<u8>>,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut selected = BTreeMap::new();
    for record in records(bytes)? {
        let separator = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(invalid)?;
        let name = &record[separator + 1..];
        if !wanted.contains(name) {
            continue;
        }
        let header = std::str::from_utf8(&record[..separator]).map_err(|_| invalid())?;
        let mut fields = header.split(' ');
        if !matches!(fields.next(), Some("100644" | "100755" | "120000")) {
            return Err(invalid());
        }
        object_id(fields.next().ok_or_else(invalid)?.as_bytes())?;
        if fields.next() != Some("0")
            || fields.next().is_some()
            || selected.insert(name.to_vec(), record.to_vec()).is_some()
        {
            return Err(invalid());
        }
    }
    if selected.len() != wanted.len() {
        return Err(invalid());
    }
    Ok(selected)
}

pub(super) struct Skipped {
    flags: Vec<u8>,
    absent: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl Skipped {
    pub(super) fn capture(repo: &Path, scratch: &Scratch, entries: &[u8]) -> Result<Self> {
        let flags = tagged(scratch, repo, "flags")?;
        let mut wanted = BTreeSet::new();
        for record in records(&flags)? {
            if record.len() < 3 || record[1] != b' ' {
                return Err(invalid());
            }
            if matches!(record[0], b'S' | b's') && absent(repo, &record[2..])? {
                if wanted.len() >= MAX_SKIPPED_PATHS || !wanted.insert(record[2..].to_vec()) {
                    return Err(invalid());
                }
            }
        }
        let absent = selected_entries(entries, &wanted)?;
        Ok(Self { flags, absent })
    }

    pub(super) fn prepare(&self, repo: &Path, scratch: &Scratch) -> Result<()> {
        if self.absent.is_empty() {
            return Ok(());
        }
        let file_path = scratch.root.join("skip");
        let mut file = File::create(&file_path).map_err(|_| invalid())?;
        for name in self.absent.keys() {
            file.write_all(name)
                .and_then(|()| file.write_all(&[0]))
                .map_err(|_| invalid())?;
        }
        drop(file);
        run(
            scratch
                .command(repo)
                .args(["update-index", "--skip-worktree", "-z", "--stdin"])
                .stdin(File::open(file_path).map_err(|_| invalid())?),
            "preserve excluded files in private index",
        )?;
        Ok(())
    }

    pub(super) fn verify_materialized(&self, entries: &[u8]) -> Result<()> {
        let wanted = self.absent.keys().cloned().collect();
        if selected_entries(entries, &wanted)? != self.absent {
            return Err(failure(
                "PI_ISO_INDEX_STATE",
                "An excluded file changed in the isolation baseline",
            ));
        }
        Ok(())
    }

    pub(super) fn verify_source(&self, repo: &Path, scratch: &Scratch) -> Result<()> {
        if tagged(scratch, repo, "verify-flags")? != self.flags {
            return Err(failure(
                "PI_ISO_PARENT_CHANGED",
                "Parent index flags changed during isolation capture",
            ));
        }
        for name in self.absent.keys() {
            if !absent(repo, name)? {
                return Err(failure(
                    "PI_ISO_PARENT_CHANGED",
                    "An excluded parent file appeared during isolation capture",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
