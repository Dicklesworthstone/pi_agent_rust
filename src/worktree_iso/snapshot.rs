//! Build an isolated checkout from the parent's effective working tree.
//!
//! Use a private index rather than applying a diff and copying newline-delimited
//! untracked paths. Git retains binary data, symlinks, executable modes and names
//! containing tabs/newlines. The parent's index, HEAD and branch are never written.
//! This is not a filesystem-wide atomic snapshot of concurrent external writers.

use crate::error::{Error, Result};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

mod sparse;

const MAX_INDEX_RECORD_BYTES: u64 = 32 * 1024 * 1024;

/// Git revision syntax that peels `HEAD` to a commit object.
/// `{commit}` is revision syntax, not a format placeholder.
#[allow(clippy::literal_string_with_formatting_args)]
const HEAD_COMMIT_REV: &str = "HEAD^{commit}";

fn failure(code: &str, message: &str) -> Error {
    Error::tool("subagent", format!("{code}: {message}"))
}

fn command(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The explicit repository is the authority, not ambient routing variables
    // inherited from a hook, another worktree, or a caller's temporary index.
    for key in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
    ] {
        command.env_remove(key);
    }
    command
}

fn output(command: &mut Command, operation: &str) -> Result<Output> {
    command
        .output()
        .map_err(|error| failure("PI_ISO_SNAPSHOT", &format!("Cannot {operation}: {error}")))
}

fn command_failure(output: &Output, operation: &str) -> Error {
    let diagnostic: String = String::from_utf8_lossy(&output.stderr)
        .chars()
        .take(4096)
        .collect();
    failure(
        "PI_ISO_SNAPSHOT",
        &format!("Cannot {operation}: {}", diagnostic.trim()),
    )
}

fn run(command: &mut Command, operation: &str) -> Result<Vec<u8>> {
    let output = output(command, operation)?;
    if !output.status.success() {
        return Err(command_failure(&output, operation));
    }
    Ok(output.stdout)
}

fn object_id(bytes: &[u8]) -> Result<String> {
    let id = std::str::from_utf8(bytes)
        .map_err(|_| failure("PI_ISO_SNAPSHOT", "Invalid Git object id"))?
        .trim();
    if !matches!(id.len(), 40 | 64) || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(failure("PI_ISO_SNAPSHOT", "Invalid Git object id"));
    }
    Ok(id.to_string())
}

/// Strip only Git's record terminator, never whitespace belonging to a path or
/// a reference. Preserve non-UTF-8 names on platforms that support them.
fn terminated_os_string(mut bytes: Vec<u8>) -> Result<OsString> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    #[cfg(windows)]
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    #[cfg(unix)]
    let value = {
        use std::os::unix::ffi::OsStringExt as _;
        OsString::from_vec(bytes)
    };
    #[cfg(not(unix))]
    let value = OsString::from(String::from_utf8(bytes).map_err(|_| {
        failure(
            "PI_ISO_SNAPSHOT",
            "Git path or reference is not representable on this platform",
        )
    })?);
    Ok(value)
}

fn symbolic_reference(repo: &Path, reference: &OsStr) -> Result<Option<OsString>> {
    let output = output(
        command(repo)
            .args(["symbolic-ref", "--quiet", "--no-recurse"])
            .arg(reference),
        "read symbolic reference",
    )?;
    match output.status.code() {
        Some(0) => terminated_os_string(output.stdout).map(Some),
        Some(1) => Ok(None),
        _ => Err(command_failure(&output, "read symbolic reference")),
    }
}

/// A missing commit is valid only for a symbolic HEAD whose branch does not
/// exist yet. Keep its exact identity so switching unborn branches during a
/// capture is detected just like creating the first commit.
#[derive(Debug, PartialEq, Eq)]
struct ParentHead {
    branch: Option<OsString>,
    commit: Option<String>,
}

impl ParentHead {
    fn read(repo: &Path) -> Result<Self> {
        let branch = symbolic_reference(repo, OsStr::new("HEAD"))?;
        let resolved = output(
            command(repo).args(["rev-parse", "--verify", "--quiet", HEAD_COMMIT_REV]),
            "resolve parent commit",
        )?;
        if resolved.status.success() {
            return Ok(Self {
                branch,
                commit: Some(object_id(&resolved.stdout)?),
            });
        }
        let error = command_failure(&resolved, "resolve parent commit");
        let Some(branch) = branch else {
            return Err(error);
        };
        if resolved.status.code() != Some(1)
            || !branch.as_encoded_bytes().starts_with(b"refs/heads/")
        {
            return Err(error);
        }
        run(
            command(repo).arg("check-ref-format").arg(&branch),
            "validate unborn branch",
        )?;
        // A malformed loose ref can look absent to show-ref --verify, and
        // a dangling symbolic branch can look absent to revision resolution.
        // symbolic-ref reports malformed refs as errors; reject aliases too.
        if symbolic_reference(repo, &branch)?.is_some() {
            return Err(error);
        }
        let existing = output(
            command(repo)
                .args(["show-ref", "--verify", "--quiet", "--"])
                .arg(&branch),
            "verify unborn branch",
        )?;
        if existing.status.code() != Some(1) {
            // This includes existing refs to missing or non-commit objects.
            // Never turn repository corruption into an empty launch history.
            return Err(error);
        }
        Ok(Self {
            branch: Some(branch),
            commit: None,
        })
    }
}

fn repository_path(cwd: &Path, flag: &str) -> Result<PathBuf> {
    let path = PathBuf::from(terminated_os_string(run(
        command(cwd).args(["rev-parse", flag]),
        "locate repository path",
    )?)?);
    path.canonicalize().map_err(|error| {
        failure(
            "PI_ISO_SNAPSHOT",
            &format!("Cannot resolve repository root: {error}"),
        )
    })
}

/// Resolve the actual repository root, including subdirectory callers.
pub(super) fn repository_root(cwd: &Path) -> Result<PathBuf> {
    repository_path(cwd, "--show-toplevel")
}

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(repo: &Path) -> Result<Self> {
        // TMPDIR can itself be inside the working tree. Snapshot internals
        // must never be staged into the very tree they are recording.
        let root = repository_path(repo, "--absolute-git-dir")?
            .join(format!("pi-iso-index-{}", uuid::Uuid::new_v4().simple()));
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(&root).map_err(|error| {
            failure(
                "PI_ISO_SNAPSHOT",
                &format!("Cannot create private index directory: {error}"),
            )
        })?;
        let scratch = Self { root };
        fs::create_dir(scratch.root.join("hooks")).map_err(|error| {
            failure(
                "PI_ISO_SNAPSHOT",
                &format!("Cannot create empty hook directory: {error}"),
            )
        })?;
        Ok(scratch)
    }

    fn command(&self, repo: &Path) -> Command {
        let mut command = command(repo);
        command
            .env("GIT_INDEX_FILE", self.root.join("index"))
            .args([
                "-c",
                "core.splitIndex=false",
                "-c",
                "core.ignorestat=false",
                "-c",
                "core.sparseCheckout=false",
                "-c",
                "index.sparse=false",
            ]);
        command
    }

    fn entries(&self, mut command: Command, name: &str) -> Result<Vec<u8>> {
        command.args(["ls-files", "--stage", "--full-name", "-z"]);
        self.records(command, name)
    }

    fn records(&self, mut command: Command, name: &str) -> Result<Vec<u8>> {
        let path = self.root.join(name);
        let file = File::create(&path).map_err(|error| {
            failure(
                "PI_ISO_SNAPSHOT",
                &format!("Cannot stage index records: {error}"),
            )
        })?;
        run(command.stdout(file), "read index records")?;
        let mut bytes = Vec::new();
        File::open(&path)
            .and_then(|file| {
                file.take(MAX_INDEX_RECORD_BYTES + 1)
                    .read_to_end(&mut bytes)
            })
            .map_err(|error| {
                failure(
                    "PI_ISO_SNAPSHOT",
                    &format!("Cannot read index records: {error}"),
                )
            })?;
        if bytes.len() as u64 > MAX_INDEX_RECORD_BYTES {
            return Err(failure(
                "PI_ISO_SNAPSHOT_LIMIT",
                "Index records exceed the 32 MiB isolation limit",
            ));
        }
        Ok(bytes)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Remove only our known scratch files, not arbitrary descendants.
        for name in [
            "index",
            "index.lock",
            "parent",
            "verify",
            "materialized",
            "flags",
            "verify-flags",
            "skip",
        ] {
            let _ = fs::remove_file(self.root.join(name));
        }
        let _ = fs::remove_dir(self.root.join("hooks"));
        let _ = fs::remove_dir(&self.root);
    }
}

fn reject_gitlinks(records: &[u8]) -> Result<()> {
    if records
        .split(|byte| *byte == 0)
        .any(|record| record.starts_with(b"160000 "))
    {
        return Err(failure(
            "PI_ISO_SUBMODULE_UNSUPPORTED",
            "Isolation cannot materialize submodules or embedded repositories without a separate checkout; no incomplete child was launched",
        ));
    }
    Ok(())
}

/// An internal commit plus the private hook directory used during checkout.
pub(super) struct Snapshot {
    pub(super) baseline: String,
    scratch: Scratch,
    checkout_mode: sparse::CheckoutMode,
}

impl Snapshot {
    pub(super) fn checkout(&self, repo: &Path, path: &Path, branch: &str) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| failure("PI_ISO_WORKTREE_PATH", "Missing worktree parent directory"))?
            .canonicalize()
            .map_err(|_| {
                failure(
                    "PI_ISO_WORKTREE_PATH",
                    "Cannot resolve worktree parent directory",
                )
            })?;
        if !path.is_absolute() || parent.starts_with(repository_root(repo)?) {
            return Err(failure(
                "PI_ISO_NESTED_TEMP",
                "Choose an absolute temporary directory outside the source repository; a nested child checkout would contaminate later snapshots",
            ));
        }
        let mut hook_setting = std::ffi::OsString::from("core.hooksPath=");
        hook_setting.push(self.scratch.root.join("hooks"));
        self.checkout_mode.require_current(repo)?;
        run(
            command(repo)
                .args([
                    "-c",
                    "core.sparseCheckout=false",
                    "-c",
                    "index.sparse=false",
                ])
                .arg("-c")
                .arg(hook_setting)
                .args(["worktree", "add", "-b", branch])
                .arg(path)
                .arg(&self.baseline),
            "create isolated checkout",
        )?;
        self.checkout_mode.configure_child(path)?;
        self.checkout_mode.require_current(repo)?;
        Ok(())
    }
}

/// Capture effective file contents while preserving the parent's staged state.
/// Seed from its index, not HEAD, so newly tracked ignored files and staged
/// additions remain tracked. Refresh only the private index from the filesystem.
pub(super) fn capture(repo: &Path, id: &str) -> Result<Snapshot> {
    let checkout_mode = sparse::CheckoutMode::read(repo)?;
    let head = ParentHead::read(repo)?;
    let scratch = Scratch::new(repo)?;
    let parent_entries = scratch.entries(command(repo), "parent")?;
    reject_gitlinks(&parent_entries)?;
    let skipped = sparse::Skipped::capture(repo, &scratch, &parent_entries)?;
    run(
        scratch.command(repo).args(["read-tree", "--empty"]),
        "initialize private index",
    )?;
    let input = File::open(scratch.root.join("parent")).map_err(|error| {
        failure(
            "PI_ISO_SNAPSHOT",
            &format!("Cannot open index records: {error}"),
        )
    })?;
    run(
        scratch
            .command(repo)
            .args(["update-index", "-z", "--index-info"])
            .stdin(input),
        "seed private index",
    )?;
    skipped.prepare(repo, &scratch)?;
    run(
        scratch.command(repo).args(["add", "--all", "--", "."]),
        "snapshot working files",
    )?;
    let materialized = scratch.entries(scratch.command(repo), "materialized")?;
    reject_gitlinks(&materialized)?;
    skipped.verify_materialized(&materialized)?;
    let tree = object_id(&run(
        scratch.command(repo).arg("write-tree"),
        "write snapshot tree",
    )?)?;

    // Plumbing does not run commit hooks, use the user's signing key, move HEAD,
    // or require a configured author. This commit belongs only to the pi-iso
    // branch and records the launch baseline, not a user-authored source commit.
    // An unborn parent gets an internal root commit without creating its first
    // source commit or changing its symbolic HEAD.
    let mut commit = command(repo);
    commit.args(["-c", "commit.gpgSign=false", "commit-tree", &tree]);
    if let Some(parent) = &head.commit {
        commit.args(["-p", parent]);
    }
    commit
        .args(["--no-gpg-sign", "-m"])
        .arg(format!("pi-iso baseline {id}"))
        .env("GIT_AUTHOR_NAME", "Pi Isolation")
        .env("GIT_AUTHOR_EMAIL", "pi-isolation@localhost")
        .env("GIT_COMMITTER_NAME", "Pi Isolation")
        .env("GIT_COMMITTER_EMAIL", "pi-isolation@localhost")
        .env_remove("GIT_AUTHOR_DATE")
        .env_remove("GIT_COMMITTER_DATE");
    let baseline = object_id(&run(&mut commit, "record snapshot baseline")?)?;
    let current_head = ParentHead::read(repo)?;
    if current_head != head || scratch.entries(command(repo), "verify")? != parent_entries {
        return Err(failure(
            "PI_ISO_PARENT_CHANGED",
            "Parent HEAD or index changed while capturing isolation; retry the delegation",
        ));
    }
    skipped.verify_source(repo, &scratch)?;
    checkout_mode.require_current(repo)?;
    Ok(Snapshot {
        baseline,
        scratch,
        checkout_mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        run(
            command(repo.path()).args(["init", "-b", "main"]),
            "initialize fixture",
        )
        .unwrap();
        fs::write(repo.path().join("base.txt"), "source\n").unwrap();
        run(command(repo.path()).args(["add", "."]), "stage fixture").unwrap();
        run(
            command(repo.path()).args([
                "-c",
                "user.name=Isolation Fixture",
                "-c",
                "user.email=isolation@localhost",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-m",
                "initial",
            ]),
            "commit fixture",
        )
        .unwrap();
        repo
    }

    #[test]
    fn private_index_files_stay_in_git_metadata_and_are_not_snapshotted() {
        let repo = repository();
        let snapshot = capture(repo.path(), "private-index-location").unwrap();
        let scratch = snapshot.scratch.root.clone();
        assert!(scratch.starts_with(repository_path(repo.path(), "--absolute-git-dir").unwrap()));
        assert!(scratch.join("index").is_file());
        let names = run(
            command(repo.path()).args(["ls-tree", "--name-only", "-r", &snapshot.baseline]),
            "inspect snapshot",
        )
        .unwrap();
        assert_eq!(names, b"base.txt\n");
        drop(snapshot);
        assert!(
            !scratch.exists(),
            "only private scratch files are cleaned up"
        );
    }

    #[test]
    fn nested_temporary_checkout_is_rejected_before_worktree_creation() {
        let repo = repository();
        let temporary = repo.path().join("temporary");
        fs::create_dir(&temporary).unwrap();
        let snapshot = capture(repo.path(), "nested-temp").unwrap();
        let path = temporary.join("pi-iso-nested");
        let error = snapshot
            .checkout(repo.path(), &path, "pi-iso-nested")
            .unwrap_err();
        assert!(error.to_string().contains("PI_ISO_NESTED_TEMP"), "{error}");
        assert!(!path.exists());
        let branches = run(
            command(repo.path()).args([
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/pi-iso-nested",
            ]),
            "inspect refs",
        )
        .unwrap();
        assert!(branches.is_empty());
    }
}
