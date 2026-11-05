//! Byte identity for validation inputs; unavailable or oversized snapshots fail closed.
//!
//! The manifest covers all cwd files except `.git`, `target_dir`, and `tmpdir`.
//! Keep broker stores and generated logs in those output directories or outside
//! cwd; other output writes intentionally invalidate reuse. The executable is
//! the immediate command, not an attestation of remote workers or external
//! dependencies. Captures bracket validation; they do not freeze the workspace
//! for the duration of a gate. DSR remains the validation authority.
//!
//! Descriptor-relative traversal and effective executable access checks are
//! implemented on Unix. Other platforms fail closed for result reuse until an
//! equivalent native implementation is qualified; active ownership still works.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{ValidationSlotLease, ValidationSlotRequest};
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationInputIdentity {
    pub workspace_sha256: String,
    pub executable_path: PathBuf,
    pub executable_sha256: String,
    pub executable_search_path: Option<String>,
    #[serde(default)]
    pub executable_search_path_inherited: bool,
    #[serde(default)]
    pub executable_mode: u32,
    #[serde(default)]
    pub executable_owner: (u32, u32),
}

impl ValidationInputIdentity {
    pub(super) fn capture(request: &ValidationSlotRequest) -> Result<Self> {
        // Use the effective inherited search path when the request does not override it.
        let executable_search_path_inherited = !request.environment.contains_key("PATH");
        let executable_search_path = request
            .environment
            .get("PATH")
            .cloned()
            .or_else(|| std::env::var("PATH").ok());
        capture_inputs(
            &request.cwd,
            request
                .command
                .first()
                .ok_or_else(|| Error::validation("missing validation command"))?,
            &request.target_dir,
            &request.tmpdir,
            executable_search_path,
            executable_search_path_inherited,
        )
    }

    pub(super) fn is_current(&self, lease: &ValidationSlotLease) -> Result<bool> {
        let Some(command) = lease.command.first() else {
            return Ok(false);
        };
        let current = capture_inputs(
            &lease.cwd,
            command,
            &lease.target_dir,
            &lease.tmpdir,
            if self.executable_search_path_inherited {
                std::env::var("PATH").ok()
            } else {
                self.executable_search_path.clone()
            },
            self.executable_search_path_inherited,
        );
        Ok(current.as_ref().is_ok_and(|identity| identity == self))
    }
}

#[cfg(unix)]
use filesystem::capture_inputs;

#[cfg(not(unix))]
fn capture_inputs(
    _cwd: &str,
    _command: &str,
    _target_dir: &str,
    _tmpdir: &str,
    _search_path: Option<String>,
    _search_path_inherited: bool,
) -> Result<ValidationInputIdentity> {
    Err(Error::validation(
        "native validation input capture is unavailable on this platform",
    ))
}

#[cfg(unix)]
mod filesystem {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::fs::{self, File, Metadata};
    use std::io::Read as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::path::{Component, Path, PathBuf};

    use rustix::fs::{Access, AtFlags, Mode, OFlags};
    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};

    use super::{Error, Result, ValidationInputIdentity};
    use crate::validation_broker::{fingerprint_json, hex_lower};

    pub(super) fn capture_inputs(
        cwd: &str,
        command: &str,
        target_dir: &str,
        tmpdir: &str,
        search_path: Option<String>,
        search_path_inherited: bool,
    ) -> Result<ValidationInputIdentity> {
        let root = fs::canonicalize(cwd)?;
        let executable_path = resolve_executable(&root, command, search_path.as_deref())?;
        let mut executable = open_absolute(&executable_path)?;
        let executable_metadata = executable.metadata()?;
        let executable_sha256 = hash_file(&mut executable, &mut (256 * 1024 * 1024))?;
        let workspace_sha256 = workspace_fingerprint(&root, target_dir, tmpdir)?;
        // Re-resolve the invocation path as well as the open file: PATH entries
        // and symlinks may have changed while the workspace was being scanned.
        if fs::canonicalize(cwd)? != root
            || resolve_executable(&root, command, search_path.as_deref())? != executable_path
            || !same_metadata(
                &executable_metadata,
                &open_absolute(&executable_path)?.metadata()?,
            )
        {
            return Err(Error::validation(
                "validation executable or cwd changed during capture",
            ));
        }
        Ok(ValidationInputIdentity {
            workspace_sha256,
            executable_path,
            executable_sha256,
            executable_search_path: search_path,
            executable_search_path_inherited: search_path_inherited,
            executable_mode: executable_metadata.mode() & 0o7777,
            executable_owner: (executable_metadata.uid(), executable_metadata.gid()),
        })
    }

    fn resolve_executable(cwd: &Path, command: &str, search_path: Option<&str>) -> Result<PathBuf> {
        let executable = Path::new(command);
        let candidates = if executable.is_absolute() || executable.components().count() > 1 {
            vec![cwd.join(executable)]
        } else {
            let search_path = search_path.ok_or_else(|| {
                Error::validation("bare validation executable requires an available PATH")
            })?;
            std::env::split_paths(search_path)
                .map(|directory| cwd.join(directory).join(executable))
                .collect()
        };
        for candidate in candidates {
            // A mode bit for a different user is not effective execute access.
            // Unsupported EACCESS implementations fail closed rather than
            // silently falling back to a different executable.
            match rustix::fs::accessat(
                rustix::fs::CWD,
                &candidate,
                Access::EXEC_OK,
                AtFlags::EACCESS,
            ) {
                Ok(()) => {}
                Err(
                    rustix::io::Errno::ACCESS
                    | rustix::io::Errno::NOENT
                    | rustix::io::Errno::NOTDIR,
                ) => continue,
                Err(error) => return Err(std::io::Error::from(error).into()),
            }
            let resolved = fs::canonicalize(&candidate)?;
            if open_absolute(&resolved)?.metadata()?.is_file() {
                return Ok(resolved);
            }
        }
        Err(Error::validation(
            "validation executable cannot be resolved",
        ))
    }

    fn open_at(parent: &File, name: &OsStr, directory: bool) -> Result<File> {
        let before = rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(std::io::Error::from)?;
        let kind = rustix::fs::FileType::from_raw_mode(before.st_mode);
        if !matches!(
            kind,
            rustix::fs::FileType::RegularFile | rustix::fs::FileType::Directory
        ) {
            return Err(Error::validation(
                "validation input must be a regular file or directory",
            ));
        }
        let mut flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
        if directory || kind == rustix::fs::FileType::Directory {
            flags |= OFlags::DIRECTORY;
        }
        let file = File::from(
            rustix::fs::openat(parent, name, flags, Mode::empty()).map_err(std::io::Error::from)?,
        );
        let opened = file.metadata()?;
        if i128::from(before.st_dev) != i128::from(opened.dev())
            || i128::from(before.st_ino) != i128::from(opened.ino())
        {
            return Err(Error::validation("validation input changed during open"));
        }
        Ok(file)
    }

    fn open_absolute(path: &Path) -> Result<File> {
        if !path.is_absolute() {
            return Err(Error::validation("validation input path must be absolute"));
        }
        let mut file = File::from(
            rustix::fs::open(
                "/",
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        );
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    file = open_at(&file, name, components.peek().is_some())?;
                }
                _ => return Err(Error::validation("noncanonical validation input path")),
            }
        }
        Ok(file)
    }

    fn safe_output_exclusion(root: &Path, output: &Path) -> bool {
        if !output.starts_with(root) {
            return true;
        }
        let Ok(relative) = output.strip_prefix(root) else {
            return false;
        };
        let first = relative
            .components()
            .next()
            .and_then(|part| part.as_os_str().to_str());
        // A caller-controlled path must never exclude an ordinary source tree.
        matches!(
            first,
            Some(
                "target" | ".target" | "tmp" | ".tmp" | "build" | "dist" | "out" | ".pi-validation"
            )
        )
    }

    struct WorkspaceCapture<'a> {
        root: &'a Path,
        outputs: [PathBuf; 2],
        files: BTreeMap<String, Value>,
        remaining_bytes: u64,
        entries: usize,
    }

    fn workspace_fingerprint(root: &Path, target_dir: &str, tmpdir: &str) -> Result<String> {
        let mut outputs = [PathBuf::new(), PathBuf::new()];
        for (output, raw) in outputs.iter_mut().zip([target_dir, tmpdir]) {
            let path = root.join(raw);
            match fs::metadata(&path) {
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(Error::validation("validation output is not a directory"));
                }
                Ok(_) => *output = fs::canonicalize(&path)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => *output = path,
                Err(error) => return Err(error.into()),
            }
            if !safe_output_exclusion(root, output) {
                return Err(Error::validation(
                    "validation output directory overlaps the workspace or an untrusted source path",
                ));
            }
        }
        let directory = open_absolute(root)?;
        let metadata = directory.metadata()?;
        let mut capture = WorkspaceCapture {
            root,
            outputs,
            files: BTreeMap::new(),
            remaining_bytes: 512 * 1024 * 1024,
            entries: 0,
        };
        capture.visit_directory(&directory, Path::new(""), 0)?;
        if !same_metadata(&metadata, &open_absolute(root)?.metadata()?) {
            return Err(Error::validation(
                "validation workspace root changed during capture",
            ));
        }
        fingerprint_json(&json!({
            "root":root, "root_mode":metadata.mode() & 0o7777,
            "root_owner":[metadata.uid(), metadata.gid()], "files":capture.files
        }))
    }

    impl WorkspaceCapture<'_> {
        fn visit_directory(
            &mut self,
            directory: &File,
            relative: &Path,
            depth: usize,
        ) -> Result<()> {
            if depth > 128 {
                return Err(Error::validation(
                    "validation workspace snapshot depth limit exceeded",
                ));
            }
            let before = directory.metadata()?;
            for entry in rustix::fs::Dir::read_from(directory).map_err(std::io::Error::from)? {
                let entry = entry.map_err(std::io::Error::from)?;
                let name = entry.file_name().to_bytes();
                if matches!(name, b"." | b"..") {
                    continue;
                }
                let name = OsStr::from_bytes(name);
                let child = relative.join(name);
                let path = self.root.join(&child);
                if path == self.root.join(".git") || self.outputs.contains(&path) {
                    continue;
                }
                self.entries += 1;
                if self.entries > 100_000 {
                    return Err(Error::validation(
                        "validation workspace snapshot entry limit exceeded",
                    ));
                }
                let key = child
                    .to_str()
                    .ok_or_else(|| Error::validation("non-UTF8 validation input path"))?
                    .to_owned();
                // Every descent and every file open is relative to a held
                // directory handle. NOFOLLOW on only the final absolute path
                // component would still follow a substituted ancestor symlink.
                let mut file = open_at(directory, name, false)?;
                let metadata = file.metadata()?;
                let value = if metadata.is_dir() {
                    self.visit_directory(&file, &child, depth + 1)?;
                    json!({"kind":"directory", "mode":metadata.mode() & 0o7777,
                        "owner":[metadata.uid(), metadata.gid()]})
                } else if metadata.is_file() {
                    json!({"kind":"file", "sha256":hash_file(&mut file, &mut self.remaining_bytes)?,
                        "mode":metadata.mode() & 0o7777, "owner":[metadata.uid(),metadata.gid()]})
                } else {
                    return Err(Error::validation("unsupported validation input file type"));
                };
                // Check the name against the handle as well as checking the
                // handle itself; a renamed/replaced directory must fail closed.
                if !same_metadata(&metadata, &open_at(directory, name, false)?.metadata()?) {
                    return Err(Error::validation("validation input changed during capture"));
                }
                self.files.insert(key, value);
            }
            if !same_metadata(&before, &directory.metadata()?) {
                return Err(Error::validation(
                    "validation directory changed during capture",
                ));
            }
            Ok(())
        }
    }

    fn same_metadata(before: &Metadata, after: &Metadata) -> bool {
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mode() == after.mode()
            && before.uid() == after.uid()
            && before.gid() == after.gid()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }

    fn hash_file(file: &mut File, remaining_bytes: &mut u64) -> Result<String> {
        let opened = file.metadata()?;
        if !opened.is_file() || opened.len() > *remaining_bytes {
            return Err(Error::validation(
                "validation inputs require bounded regular files",
            ));
        }
        let mut hash = Sha256::new();
        let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                if !same_metadata(&opened, &file.metadata()?) {
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
    mod tests {
        use super::*;

        #[test]
        fn caller_cannot_exclude_source_tree_from_input_digest() {
            let root = Path::new("/work/project");
            assert!(!safe_output_exclusion(root, &root.join("src")));
            assert!(!safe_output_exclusion(root, &root.join("src/main.rs")));
            assert!(!safe_output_exclusion(root, root));
            assert!(safe_output_exclusion(root, &root.join("target/debug")));
            assert!(safe_output_exclusion(
                root,
                Path::new("/data/tmp/pi-validation")
            ));
        }

        #[test]
        fn directory_handle_never_follows_a_replaced_ancestor() {
            let root = tempfile::tempdir().expect("root");
            let outside = tempfile::tempdir().expect("outside");
            let child = root.path().join("src");
            fs::create_dir(&child).expect("directory");
            fs::write(child.join("input"), "inside").expect("inside file");
            fs::write(outside.path().join("input"), "outside").expect("outside file");
            let directory = open_absolute(&child).expect("held directory");
            fs::rename(&child, root.path().join("moved")).expect("rename");
            std::os::unix::fs::symlink(outside.path(), &child).expect("replacement symlink");
            let mut file = open_at(&directory, OsStr::new("input"), false).expect("relative file");
            let mut actual = String::new();
            file.read_to_string(&mut actual)
                .expect("read through held directory");
            assert_eq!(actual, "inside");
            assert!(open_absolute(&child.join("input")).is_err());
        }
    }
}
