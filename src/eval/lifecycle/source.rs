//! Host-side admission for JavaScript `%load` cells. No shell or module loader.

use crate::error::{Error, Result};
use std::io::Read as _;
use std::path::{Path, PathBuf};

const MAX_SCRIPT_BYTES: u64 = 2 * 1024 * 1024;

pub(super) struct Source {
    pub code: String,
    pub path: String,
}

fn failure(kind: &str, message: impl std::fmt::Display) -> Error {
    Error::tool("eval", format!("EVAL_LOAD_{kind}: {message}"))
}

/// One path, optionally enclosed in matching single or double quotes. Quotes
/// are delimiters, not shell syntax: backslashes and dollar signs stay literal.
/// Forward slashes work on Windows; no shell expansion is performed anywhere.
pub(super) fn load_path(code: &str) -> Result<Option<PathBuf>> {
    let command = code.trim();
    if !command.starts_with('%') {
        return Ok(None);
    }
    let usage = || failure("USAGE", "use one standalone %load <path> cell; quote paths with spaces");
    if command.contains(['\r', '\n', '\0']) {
        return Err(usage());
    }
    let Some(tail) = command.strip_prefix("%load") else {
        return Err(usage());
    };
    if !tail.starts_with(char::is_whitespace) {
        return Err(usage());
    }
    let argument = tail.trim();
    let path = match argument.chars().next() {
        Some(quote @ ('\'' | '"')) => {
            let inner = argument.get(1..).and_then(|rest| rest.strip_suffix(quote)).ok_or_else(usage)?;
            if inner.contains(quote) { return Err(usage()); }
            inner
        }
        Some(_) if !argument.chars().any(char::is_whitespace) => argument,
        _ => return Err(usage()),
    };
    if path.is_empty() { return Err(usage()); }
    if path.contains("://") {
        return Err(failure("PATH", "URLs and virtual documents cannot be executed; inspect and save the script locally first"));
    }
    Ok(Some(PathBuf::from(path)))
}

pub(super) async fn load(cwd: PathBuf, path: PathBuf) -> Result<Source> {
    // This worker only reads a bounded regular file. Abandoning the await
    // cannot execute the script or reset the caller's retained kernel.
    asupersync::runtime::spawn_blocking_io(move || load_sync(&cwd, &path)).await
}

fn load_sync(cwd: &Path, path: &Path) -> Result<Source> {
    let root = std::fs::canonicalize(cwd).map_err(|error| failure("PATH", error))?;
    let requested = if path.is_absolute() { path.to_path_buf() } else { root.join(path) };
    let path = std::fs::canonicalize(requested).map_err(|error| failure("PATH", error))?;
    if !path.starts_with(&root) {
        return Err(failure("PATH", "script is outside the primary workspace"));
    }
    let before = std::fs::symlink_metadata(&path).map_err(|error| failure("PATH", error))?;
    if !before.is_file() {
        return Err(failure("PATH", "script must be a regular file"));
    }
    if before.len() > MAX_SCRIPT_BYTES {
        return Err(failure("LIMIT", "script exceeds 2 MiB"));
    }
    let file = open_script(&path).map_err(|error| failure("PATH", error))?;
    let opened = file.metadata().map_err(|error| failure("PATH", error))?;
    if !opened.is_file() || !same_snapshot(&before, &opened) {
        return Err(failure("CHANGED", "script changed while opening"));
    }
    // Identity/stamp checks detect observed races, not a filesystem snapshot
    // against an adversarial parent-directory writer. Eval is not a sandbox.
    let mut bytes = Vec::new();
    (&file).take(MAX_SCRIPT_BYTES + 1).read_to_end(&mut bytes)
        .map_err(|error| failure("READ", error))?;
    let after = file.metadata().map_err(|error| failure("READ", error))?;
    if !same_snapshot(&opened, &after) {
        return Err(failure("CHANGED", "script changed while reading"));
    }
    if bytes.len() as u64 > MAX_SCRIPT_BYTES {
        return Err(failure("LIMIT", "script exceeds 2 MiB"));
    }
    let code = String::from_utf8(bytes).map_err(|_| failure("ENCODING", "JavaScript source must be UTF-8"))?;
    // A UTF-8 BOM is an encoding signature. Removing it also allows a
    // following hashbang to occupy the first position expected by QuickJS.
    let code = code.strip_prefix('\u{feff}').map_or_else(|| code.clone(), str::to_owned);
    let path = path.to_str().ok_or_else(|| failure("PATH", "script path must be UTF-8"))?.to_owned();
    Ok(Source { code, path })
}

#[cfg(unix)]
fn open_script(path: &Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    rustix::fs::open(path, OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK, Mode::empty())
        .map(std::fs::File::from).map_err(std::io::Error::from)
}

#[cfg(not(unix))]
fn open_script(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

fn same_snapshot(first: &std::fs::Metadata, second: &std::fs::Metadata) -> bool {
    if first.len() != second.len() || first.modified().ok() != second.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        first.dev() == second.dev() && first.ino() == second.ino()
            && first.ctime() == second.ctime() && first.ctime_nsec() == second.ctime_nsec()
    }
    #[cfg(not(unix))]
    { first.created().ok() == second.created().ok() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_literals_not_commands() {
        assert_eq!(load_path("  %load 'analysis/setup data.js'\n").unwrap(), Some("analysis/setup data.js".into()));
        assert_eq!(load_path(r#"%load "C:\work\setup.js""#).unwrap(), Some(r"C:\work\setup.js".into()));
        assert_eq!(load_path("%load $(touch-owned).js").unwrap(), Some("$(touch-owned).js".into()));
        assert!(load_path("globalThis.x = 1 % 2").unwrap().is_none());
    }

    #[test]
    fn malformed_and_remote_commands_are_rejected() {
        for command in ["%load", "%loader a.js", "%load a.js b.js", "%load ''", "%load 'a.js", "%load 'a' 'b'", "%load a.js\n42", "%load a\0.js", "%bun add x", "%load https://host/code.js", "%load local://code.js"] {
            assert!(load_path(command).is_err(), "{command:?}");
        }
    }

    #[test]
    fn local_source_is_bounded_utf8_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("setup.js");
        std::fs::write(&path, "\u{feff}globalThis.value = 1").unwrap();
        assert_eq!(load_sync(dir.path(), Path::new("setup.js")).unwrap().code, "globalThis.value = 1");
        std::fs::write(&path, "globalThis.value = 2").unwrap();
        assert_eq!(load_sync(dir.path(), &path).unwrap().code, "globalThis.value = 2");
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(load_sync(dir.path(), &path).err().unwrap().to_string().contains("ENCODING"));
        std::fs::File::create(&path).unwrap().set_len(MAX_SCRIPT_BYTES + 1).unwrap();
        assert!(load_sync(dir.path(), &path).err().unwrap().to_string().contains("LIMIT"));
    }

    #[test]
    fn directories_and_parent_workspace_are_denied() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(dir.path().join("outside.js"), "42").unwrap();
        assert!(load_sync(&root, Path::new("../outside.js")).is_err());
        assert!(load_sync(&root, Path::new(".")).is_err());
        assert!(load_sync(&root, Path::new("missing.js")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_and_fifo_are_denied_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(dir.path().join("outside.js"), "42").unwrap();
        std::os::unix::fs::symlink(dir.path().join("outside.js"), root.join("escape.js")).unwrap();
        assert!(load_sync(&root, Path::new("escape.js")).is_err());
        rustix::fs::mkfifoat(rustix::fs::CWD, root.join("pipe.js"), rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR).unwrap();
        assert!(load_sync(&root, Path::new("pipe.js")).is_err());
    }
}
