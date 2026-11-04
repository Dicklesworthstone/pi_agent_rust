//! Byte identity for validation inputs; unavailable or oversized snapshots fail closed.
//!
//! The manifest covers all cwd files except `.git`, `target_dir`, and `tmpdir`.
//! Keep broker stores and generated logs in those output directories or outside
//! cwd; other output writes intentionally invalidate reuse. The executable is
//! the immediate command, not an attestation of remote workers or external
//! dependencies. DSR remains the validation authority.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest as _, Sha256};

use super::{ValidationSlotLease, ValidationSlotRequest, fingerprint_json, hex_lower};
use crate::error::{Error, Result};
use crate::file_identity::FileIdentity;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationInputIdentity {
    pub workspace_sha256: String,
    pub executable_path: PathBuf,
    pub executable_sha256: String,
    pub executable_search_path: Option<String>,
}

impl ValidationInputIdentity {
    pub(super) fn capture(request: &ValidationSlotRequest) -> Result<Self> {
        // Use the effective inherited search path when the request does not override it.
        let executable_search_path = request.environment.get("PATH").cloned()
            .or_else(|| std::env::var("PATH").ok());
        let executable_path = resolve_executable(
            &request.cwd,
            request
                .command
                .first()
                .ok_or_else(|| Error::validation("missing validation command"))?,
            executable_search_path.as_deref(),
        )?;
        Ok(Self {
            workspace_sha256: workspace_fingerprint(
                &request.cwd,
                &request.target_dir,
                &request.tmpdir,
            )?,
            executable_sha256: hash_file(&executable_path, &mut (256 * 1024 * 1024))?,
            executable_path,
            executable_search_path,
        })
    }

    pub(super) fn is_current(&self, lease: &ValidationSlotLease) -> Result<bool> {
        let current = (|| -> Result<Self> {
            let executable_path = resolve_executable(
                &lease.cwd,
                lease
                    .command
                    .first()
                    .ok_or_else(|| Error::validation("missing validation command"))?,
                self.executable_search_path.as_deref(),
            )?;
            Ok(Self {
                workspace_sha256: workspace_fingerprint(
                    &lease.cwd,
                    &lease.target_dir,
                    &lease.tmpdir,
                )?,
                executable_sha256: hash_file(&executable_path, &mut (256 * 1024 * 1024))?,
                executable_path,
                executable_search_path: self.executable_search_path.clone(),
            })
        })();
        Ok(current.as_ref().is_ok_and(|identity| identity == self))
    }
}

fn resolve_executable(cwd: &str, command: &str, search_path: Option<&str>) -> Result<PathBuf> {
    let executable = Path::new(command);
    let mut candidates = Vec::new();
    if executable.is_absolute() || executable.components().count() > 1 {
        candidates.push(Path::new(cwd).join(executable));
    } else {
        let search_path = search_path.ok_or_else(|| {
            Error::validation("bare validation executable requires an explicit PATH")
        })?;
        for directory in std::env::split_paths(search_path) {
            let directory = Path::new(cwd).join(directory);
            #[cfg(windows)]
            if executable.extension().is_none() {
                // Windows native process launch adds .exe; shell scripts need an explicit interpreter.
                candidates.push(directory.join(executable).with_extension("exe"));
                continue;
            }
            candidates.push(directory.join(executable));
        }
    }
    for candidate in candidates {
        if let Ok(metadata) = fs::metadata(&candidate)
            && metadata.is_file()
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                if metadata.permissions().mode() & 0o111 == 0 {
                    continue;
                }
            }
            return fs::canonicalize(candidate).map_err(Into::into);
        }
    }
    Err(Error::validation("validation executable cannot be resolved"))
}

fn safe_output_exclusion(root: &Path, output: &Path) -> bool {
    if !output.starts_with(root) { return true; }
    let Ok(relative) = output.strip_prefix(root) else { return false };
    let mut components = relative.components();
    let first = components.next().and_then(|part| part.as_os_str().to_str());
    // A caller-controlled path must never exclude an ordinary source tree.
    matches!(first, Some("target" | ".target" | "tmp" | ".tmp" | "build" | "dist" | "out" | ".pi-validation"))
}

fn workspace_fingerprint(cwd: &str, target_dir: &str, tmpdir: &str) -> Result<String> {
    let root = fs::canonicalize(cwd)?;
    let outputs = [target_dir, tmpdir].map(|path| {
        let path = root.join(path);
        fs::canonicalize(&path).unwrap_or(path)
    });
    if outputs.iter().any(|path| path == &root || !safe_output_exclusion(&root, path)) {
        return Err(Error::validation(
            "validation output directory overlaps the workspace or an untrusted source path",
        ));
    }
    let mut files = BTreeMap::new();
    let mut directories = vec![root.clone()];
    let mut remaining_bytes = 512 * 1024 * 1024;
    let mut entries = 0;
    // shortcut: snapshots stop at 100000 entries or 512 MiB, add a declared input manifest for larger workspaces.
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if path == root.join(".git") || outputs.iter().any(|output| path == *output) {
                continue;
            }
            entries += 1;
            if entries > 100_000 {
                return Err(Error::validation(
                    "validation workspace snapshot entry limit exceeded",
                ));
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(Error::validation("validation workspace contains a symlink"));
            }
            let relative = path
                .strip_prefix(&root)
                .map_err(|error| Error::validation(error.to_string()))?;
            let relative = relative
                .to_str()
                .ok_or_else(|| Error::validation("non-UTF8 validation input path"))?
                .to_owned();
            if kind.is_dir() {
                files.insert(relative, json!({"kind":"directory"}));
                directories.push(path);
            } else if kind.is_file() {
                let metadata = fs::symlink_metadata(&path)?;
                let entry = json!({"kind":"file", "sha256":hash_file(&path, &mut remaining_bytes)?,
                    "read_only":metadata.permissions().readonly()});
                #[cfg(unix)]
                let entry = {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mut entry = entry;
                    entry["mode"] = json!(metadata.permissions().mode() & 0o7777);
                    entry
                };
                files.insert(relative, entry);
            } else {
                return Err(Error::validation("unsupported validation input file type"));
            }
        }
    }
    fingerprint_json(&json!({"root": root, "files": files}))
}

fn hash_file(path: &Path, remaining_bytes: &mut u64) -> Result<String> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > *remaining_bytes {
        return Err(Error::validation(
            "validation inputs require bounded regular files",
        ));
    }
    let identity = FileIdentity::of_path_nofollow(path)?;
    #[cfg(unix)]
    let mut file = fs::File::from(rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )?);
    #[cfg(not(unix))]
    let mut file = {
        let mut options = fs::OpenOptions::new();
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
    if !opened.is_file() || FileIdentity::of_open_file(&file)? != identity {
        return Err(Error::validation("validation input changed during open"));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            let after = file.metadata()?;
            if FileIdentity::of_path_nofollow(path)? != identity
                || opened.len() != after.len()
                || opened.modified().ok() != after.modified().ok()
            {
                return Err(Error::validation("validation input changed during read"));
            }
            return Ok(hex_lower(&hash.finalize()));
        }
        *remaining_bytes = remaining_bytes.checked_sub(count as u64).ok_or_else(|| {
            Error::validation("validation workspace snapshot byte limit exceeded")
        })?;
        hash.update(&buffer[..count]);
    }
}

#[cfg(test)]
mod output_exclusion_tests {
    use super::*;
    #[test]
    fn caller_cannot_exclude_source_tree_from_input_digest() {
        let root = Path::new("/work/project");
        assert!(!safe_output_exclusion(root, &root.join("src")));
        assert!(!safe_output_exclusion(root, &root.join("src/main.rs")));
        assert!(!safe_output_exclusion(root, root));
        assert!(safe_output_exclusion(root, &root.join("target/debug")));
        assert!(safe_output_exclusion(root, Path::new("/data/tmp/pi-validation")));
    }
}
