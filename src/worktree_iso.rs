//! Workspace isolation for subagents (bd-cv653.5.2).
//!
//! `worktree` mode creates a private Git snapshot of the parent's effective
//! working files, then checks it out on a temporary `pi-iso-` branch. Neither
//! the parent's HEAD nor its real index is modified. Child changes are collected
//! against that baseline using another private index, preserving staged work.
//!
//! `keep` leaves the worktree for inspection; `apply` serializes patch application
//! into the parent and removes the completed worktree; `drop` removes it after
//! reporting its patch. Conflicts are never forced. Temporary patch files live
//! outside both working trees, so user files cannot collide with control files.
//!
//! Gitlinks and sparse checkouts are explicitly refused rather than launching
//! a child with a silently incomplete workspace. Ignored untracked files are not
//! part of the snapshot. Concurrent external file writes are not a filesystem-
//! wide atomic snapshot; changes to parent HEAD/index during capture are detected.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::error::{Error, Result};

mod snapshot;
#[cfg(test)]
mod snapshot_tests;

/// Tool-result schema tag for isolation outcomes.
pub const ISO_SCHEMA: &str = "pi.worktree_iso.v1";

/// Branch/path prefix that marks worktrees created by us.
const ISO_PREFIX: &str = "pi-iso-";

/// What to do with the worktree after the child completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsoApplyMode {
    /// Leave the worktree in place for the user.
    Keep,
    /// Apply the patch into the parent tree (serialized), then drop.
    Apply,
    /// Report the patch, then drop.
    Drop,
}

impl IsoApplyMode {
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        match raw.unwrap_or("apply").trim().to_ascii_lowercase().as_str() {
            "keep" => Ok(Self::Keep),
            "apply" => Ok(Self::Apply),
            "drop" => Ok(Self::Drop),
            other => Err(Error::validation(format!(
                "Unknown iso apply mode '{other}'; expected keep, apply, or drop"
            ))),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Apply => "apply",
            Self::Drop => "drop",
        }
    }
}

/// A live isolated worktree.
#[derive(Debug)]
pub struct IsoHandle {
    pub id: String,
    pub branch: String,
    pub path: PathBuf,
    pub repo_root: PathBuf,
    /// Commit recording the materialized parent state. The child's own work
    /// is the diff from here, not from the parent's possibly older HEAD.
    pub baseline: String,
}

/// The outcome of an isolated run.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IsoOutcome {
    pub schema: String,
    pub worktree_path: String,
    pub branch: String,
    pub diff_stat: String,
    /// The full unified diff against the launch baseline, including binary patches.
    pub patch: String,
    /// Files that failed to apply (conflict path only).
    pub conflicted_files: Vec<String>,
    pub apply_mode: String,
    pub applied: bool,
}

fn git_command(repo: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.quotePath=true", "-c", "core.fsmonitor=false"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
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

fn git(repo: &Path, args: &[&str]) -> Result<std::process::Output> {
    git_command(repo)
        .args(args)
        .output()
        .map_err(|e| Error::tool("subagent", format!("Failed to run git: {e}")))
}

fn git_ok(repo: &Path, args: &[&str]) -> Result<String> {
    let output = git(repo, args)?;
    if !output.status.success() {
        return Err(Error::tool(
            "subagent",
            format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    // A patch is executable file data, not display text. Lossy decoding can
    // silently replace source bytes, so an unrepresentable textual diff must
    // remain in the worktree for manual handling. Git binary patches are ASCII.
    String::from_utf8(output.stdout).map_err(|_| Error::tool(
        "subagent",
        "PI_ISO_TEXT_ENCODING: Git output contains non-UTF8 text; the worktree was preserved rather than applying a lossy patch",
    ))
}

fn sanitize_id(task_id: &str) -> String {
    let mut out = String::with_capacity(task_id.len().min(32));
    for ch in task_id.chars().take(32) {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "task".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Resolve the source checkout before sharing a baseline across parallel tasks.
pub(crate) fn isolation_root(repo_root: &Path) -> Result<PathBuf> {
    let is_git = git(repo_root, &["rev-parse", "--is-inside-work-tree"])
        .is_ok_and(|output| output.status.success());
    if !is_git {
        return Err(Error::tool(
            "subagent",
            format!(
                "PI_ISO_NOT_GIT: {} is not a git work tree; isolation requires git \
                 (run non-isolated or copy the directory explicitly)",
                repo_root.display()
            ),
        ));
    }
    snapshot::repository_root(repo_root)
}

/// Immutable launch state shared by a parallel batch and its corrective retries.
/// Capturing before any child starts prevents queued work from inheriting a
/// faster sibling's edits, including edits made by non-isolated siblings.
pub(crate) struct IsoBaseline {
    repo_root: PathBuf,
    captured: snapshot::Snapshot,
}

impl IsoBaseline {
    /// The caller supplies a root resolved by [`isolation_root`]. No worktree
    /// is created until a child actually obtains a launch slot.
    pub(crate) fn capture(repo_root: PathBuf) -> Result<Self> {
        let id = format!("{ISO_PREFIX}baseline-{}", uuid::Uuid::new_v4().simple());
        let captured = snapshot::capture(&repo_root, &id)?;
        Ok(Self {
            repo_root,
            captured,
        })
    }

    pub(crate) fn isolate(&self, task_id: &str) -> Result<IsoHandle> {
        let id = format!(
            "{ISO_PREFIX}{}-{}",
            sanitize_id(task_id),
            uuid::Uuid::new_v4().simple()
        );
        let path = std::env::temp_dir().join(&id);
        self.captured
            .checkout(&self.repo_root, &path, &id)
            .map_err(|error| {
                Error::tool(
                    "subagent",
                    format!(
                        "{error}; inspect {} for any incomplete checkout before retrying",
                        path.display()
                    ),
                )
            })?;
        Ok(IsoHandle {
            branch: id.clone(),
            id,
            path,
            repo_root: self.repo_root.clone(),
            baseline: self.captured.baseline.clone(),
        })
    }
}

/// Create an isolated worktree carrying the parent's effective working files.
///
/// # Errors
/// Named `PI_ISO_NOT_GIT` for non-git directories; snapshot/checkout errors otherwise.
pub fn isolate(repo_root: &Path, task_id: &str) -> Result<IsoHandle> {
    // Capture first: unsupported modes and read failures must not launch an
    // incomplete child or create a half-materialized worktree.
    IsoBaseline::capture(isolation_root(repo_root)?)?.isolate(task_id)
}

/// Collect the child's effective file state against its launch baseline.
/// Both staged and unstaged changes are included without modifying its index.
///
/// # Errors
/// Git/snapshot failures, including textual patches that cannot be represented
/// losslessly as UTF-8. The caller retains the worktree on those failures.
pub fn collect_diff(handle: &IsoHandle) -> Result<(String, String)> {
    let current = snapshot::capture(&handle.path, &handle.id)?;
    let patch = git_ok(
        &handle.path,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--full-index",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            &handle.baseline,
            &current.baseline,
            "--",
        ],
    )?;
    let diff_stat = git_ok(
        &handle.path,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--stat",
            &handle.baseline,
            &current.baseline,
            "--",
        ],
    )?;
    Ok((patch, diff_stat))
}

/// Serialize mutations from independent callers. The parallel child runner
/// additionally waits for execution to finish and applies in authored task
/// order; a mutex alone cannot provide that ordering or a common baseline.
fn parent_apply_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(()));
    &LOCK
}

struct PatchFile {
    path: PathBuf,
}

impl PatchFile {
    fn new(patch: &str) -> Result<Self> {
        let path =
            std::env::temp_dir().join(format!("pi-iso-patch-{}", uuid::Uuid::new_v4().simple()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|error| {
            Error::tool(
                "subagent",
                format!("Cannot create private patch file: {error}"),
            )
        })?;
        let staged = Self { path };
        let written = file.write_all(patch.as_bytes());
        drop(file);
        written.map_err(|error| Error::tool("subagent", format!("Cannot stage patch: {error}")))?;
        Ok(staged)
    }

    fn apply(&self, repo: &Path, check: bool) -> Result<std::process::Output> {
        let mut command = git_command(repo);
        command.arg("apply");
        if check {
            command.arg("--check");
        }
        command
            .arg("--")
            .arg(&self.path)
            .output()
            .map_err(|error| Error::tool("subagent", format!("Cannot run git apply: {error}")))
    }
}

impl Drop for PatchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Apply a patch without overwriting temporary/control filenames in either
/// workspace.
///
/// The real apply rechecks its preconditions after `--check`, so an external
/// edit during that gap is rejected rather than forced.
///
/// # Errors
/// Named `PI_ISO_CONFLICT` on a rejected patch; the worktree remains inspectable.
pub fn apply_to_parent(handle: &IsoHandle, patch: &str) -> Result<()> {
    if patch.trim().is_empty() {
        return Ok(());
    }
    let _guard = parent_apply_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let staged = PatchFile::new(patch)?;
    for check in [true, false] {
        let output = staged.apply(&handle.repo_root, check)?;
        if !output.status.success() {
            let diagnostic: String = String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(4096)
                .collect();
            return Err(Error::tool(
                "subagent",
                format!(
                    "PI_ISO_CONFLICT: patch from {} does not apply cleanly to the parent tree. \
                     The worktree is left at {} for manual resolution. Git apply: {}",
                    handle.branch,
                    handle.path.display(),
                    diagnostic.trim()
                ),
            ));
        }
    }
    Ok(())
}

/// Remove a matching pi-iso worktree and its branch. A basename prefix alone
/// is not enough: a foreign branch in a similarly named directory is preserved.
///
/// # Errors
/// Ownership-shape mismatch or git failures.
pub fn drop_worktree(handle: &IsoHandle) -> Result<()> {
    if !handle.id.starts_with(ISO_PREFIX)
        || handle.branch != handle.id
        || handle.path.file_name() != Some(std::ffi::OsStr::new(&handle.id))
    {
        return Err(Error::tool(
            "subagent",
            "PI_ISO_NOT_OWNED: refusing to remove a non-matching isolation worktree",
        ));
    }
    let output = git_command(&handle.repo_root)
        .args(["worktree", "remove", "--force", "--"])
        .arg(&handle.path)
        .output()
        .map_err(|error| Error::tool("subagent", format!("Cannot remove worktree: {error}")))?;
    if !output.status.success() {
        return Err(Error::tool(
            "subagent",
            format!(
                "Cannot remove isolation worktree: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    let _ = git(&handle.repo_root, &["branch", "-D", &handle.branch]);
    Ok(())
}

/// One of our worktrees with its age.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: String,
    pub age_ms: u64,
}

/// List live pi-iso worktrees under a repo.
///
/// # Errors
/// git failures.
pub fn list_mine(repo_root: &Path) -> Result<Vec<WorktreeInfo>> {
    let porcelain = git_ok(repo_root, &["worktree", "list", "--porcelain"])?;
    let mut out = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_branch = String::new();
    let flush = |path: Option<String>, branch: String, out: &mut Vec<WorktreeInfo>| {
        let Some(path) = path else { return };
        // Match on the BASENAME prefix: a foreign worktree whose path
        // merely contains "pi-iso-" somewhere must never look like ours
        // to the reaper.
        let is_ours = std::path::Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(ISO_PREFIX));
        if !is_ours {
            return;
        }
        let age_ms = worktree_age_ms(Path::new(&path));
        out.push(WorktreeInfo {
            path,
            branch,
            age_ms,
        });
    };
    for line in porcelain.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            flush(
                current_path.take(),
                std::mem::take(&mut current_branch),
                &mut out,
            );
            current_path = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
            current_branch = rest.to_string();
        }
    }
    flush(current_path, current_branch, &mut out);
    Ok(out)
}

/// Age of a worktree based on the NEWEST modification time anywhere in its
/// tree (bd-ajg8l #14): the root directory mtime only changes when entries
/// are added/removed at the top level, so a long-running child writing into
/// nested paths used to look stale and got reaped mid-run.
///
/// Bounded walk (`.git` skipped, 5_000-entry cap) — liveness detection, not
/// a full audit. Falls back to the root mtime when the walk yields nothing.
fn worktree_age_ms(path: &Path) -> u64 {
    fn mtime_elapsed_ms(path: &Path) -> Option<u128> {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|mtime| mtime.elapsed().ok())
            .map(|elapsed| elapsed.as_millis())
    }

    let root_age = mtime_elapsed_ms(path).unwrap_or(u128::MAX);
    let mut newest = root_age;
    let mut visited = 0usize;
    let walker = ignore::WalkBuilder::new(path)
        .hidden(false)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walker.flatten() {
        visited += 1;
        if visited > 5_000 {
            break;
        }
        if let Some(age) = mtime_elapsed_ms(entry.path())
            && newest > age
        {
            newest = age;
        }
    }
    if visited <= 1 {
        // Nothing walkable: fall back to the root mtime alone.
        return u64::try_from(root_age).unwrap_or(u64::MAX);
    }
    u64::try_from(newest).unwrap_or(u64::MAX)
}

/// Reap our stale worktrees (matching prefix, branch and basename only).
///
/// # Errors
/// git failures on the first worktree that cannot be listed.
pub fn reap_stale(repo_root: &Path, older_than: Duration) -> Result<Vec<String>> {
    let mut reaped = Vec::new();
    for info in list_mine(repo_root)? {
        if info.age_ms < u64::try_from(older_than.as_millis()).unwrap_or(u64::MAX) {
            continue;
        }
        let handle = IsoHandle {
            id: info.branch.clone(),
            branch: info.branch.clone(),
            path: PathBuf::from(&info.path),
            repo_root: repo_root.to_path_buf(),
            baseline: String::new(),
        };
        if drop_worktree(&handle).is_ok() {
            reaped.push(info.path);
        }
    }
    Ok(reaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-iso-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("repo dir");
        git_ok(&dir, &["init", "-b", "main"]).expect("git init");
        git_ok(&dir, &["config", "user.email", "iso@test"]).expect("config");
        git_ok(&dir, &["config", "user.name", "Iso Test"]).expect("config");
        std::fs::write(dir.join("file.txt"), "one\n").expect("write");
        git_ok(&dir, &["add", "."]).expect("add");
        git_ok(&dir, &["commit", "-m", "init"]).expect("commit");
        dir
    }

    #[test]
    fn non_git_refuses_with_named_error() {
        let dir = std::env::temp_dir().join(format!("pi-iso-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");

        // Remote-execution harnesses may resolve temp_dir INSIDE the synced
        // project tree (which is itself a git work tree). The refusal under
        // test only applies to genuinely non-git directories — self-skip
        // when the environment cannot provide one.
        let inside = git(&dir, &["rev-parse", "--is-inside-work-tree"])
            .is_ok_and(|output| output.status.success());
        if inside {
            eprintln!("skipping: temp_dir resolves inside a git work tree");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        let err = isolate(&dir, "x").unwrap_err();
        assert!(
            err.to_string().contains("PI_ISO_NOT_GIT"),
            "expected named refusal: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn isolate_collects_dirty_state_and_child_edits() {
        let repo = init_repo("dirty");
        // Dirty the parent: tracked edit + untracked file.
        std::fs::write(repo.join("file.txt"), "one\nparent-dirty\n").expect("dirty");
        std::fs::write(repo.join("untracked.txt"), "loose\n").expect("untracked");

        let handle = isolate(&repo, "task-one").expect("isolate");
        // The child sees the uncommitted content (round-7 invariant).
        let seen = std::fs::read_to_string(handle.path.join("file.txt")).expect("read");
        assert!(
            seen.contains("parent-dirty"),
            "worktree must carry dirty state: {seen}"
        );
        assert!(
            handle.path.join("untracked.txt").exists(),
            "untracked file must be copied"
        );

        // Child edits; collect the diff vs parent HEAD.
        std::fs::write(handle.path.join("child.txt"), "child work\n").expect("child write");
        let (patch, stat) = collect_diff(&handle).expect("collect");
        assert!(patch.contains("child.txt"), "patch must include child work");
        assert!(!stat.is_empty());

        // Apply mode lands it byte-identical in the parent.
        apply_to_parent(&handle, &patch).expect("apply");
        let landed = std::fs::read_to_string(repo.join("child.txt")).expect("landed");
        assert_eq!(landed, "child work\n");

        drop_worktree(&handle).expect("drop");
        assert!(!handle.path.exists());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn conflicting_apply_reports_files_and_never_forces() {
        let repo = init_repo("conflict");
        let handle = isolate(&repo, "task-two").expect("isolate");
        // Child edits file.txt one way...
        std::fs::write(handle.path.join("file.txt"), "one\nchild-version\n").expect("child edit");
        let (patch, _) = collect_diff(&handle).expect("collect");
        // ...while the parent diverges on the same lines and COMMITS, so
        // the patch's context no longer matches.
        std::fs::write(repo.join("file.txt"), "one\nparent-version\n").expect("parent edit");
        git_ok(&repo, &["add", "."]).expect("add");
        git_ok(&repo, &["commit", "-m", "parent diverges"]).expect("commit");

        let err = apply_to_parent(&handle, &patch).unwrap_err();
        assert!(
            err.to_string().contains("PI_ISO_CONFLICT"),
            "expected conflict refusal: {err}"
        );
        // Worktree left for manual resolution; parent untouched by force.
        assert!(handle.path.exists());
        let parent = std::fs::read_to_string(repo.join("file.txt")).expect("parent");
        assert!(parent.contains("parent-version"));
        drop_worktree(&handle).expect("drop");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn reaper_only_touches_our_prefix() {
        let repo = init_repo("reap");
        let ours = isolate(&repo, "task-three").expect("isolate");
        // A foreign worktree without the prefix.
        let foreign_path = std::env::temp_dir().join(format!("foreign-wt-{}", std::process::id()));
        git_ok(
            &repo,
            &[
                "worktree",
                "add",
                &foreign_path.to_string_lossy(),
                "-b",
                "foreign-branch",
            ],
        )
        .expect("foreign worktree");

        // Backdate ours so it counts as stale.
        let reaped = reap_stale(&repo, Duration::from_millis(0)).expect("reap");
        assert!(
            reaped.iter().any(|path| path.contains(ISO_PREFIX)),
            "our worktree must be reaped: {reaped:?}"
        );
        assert!(
            foreign_path.exists(),
            "foreign worktree must survive the sweep"
        );
        git_ok(
            &repo,
            &[
                "worktree",
                "remove",
                "--force",
                &foreign_path.to_string_lossy(),
            ],
        )
        .expect("foreign cleanup");
        let _ = git(&repo, &["branch", "-D", "foreign-branch"]);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = ours;
    }

    /// bd-ajg8l #14: a long-running child writing into NESTED paths must
    /// keep the worktree fresh even when the root directory mtime is old.
    #[test]
    fn worktree_age_uses_newest_nested_mtime() {
        let repo = init_repo("fresh-nested");
        let ours = isolate(&repo, "task-fresh").expect("isolate");

        // Age the root directory far into the past (as if nothing happened
        // at the top level since creation).
        let old = filetime::FileTime::from_unix_time(1_600_000_000, 0);
        filetime::set_file_mtime(&ours.path, old).expect("set root mtime");

        // Child activity: write into a nested path NOW. The freshness walk
        // must find it even though the ROOT mtime says the tree is old.
        // (The converse — a fully-aged tree reporting old — cannot be
        // asserted reliably on remote-execution harnesses whose own sync
        // touches worktree files mid-test.)
        let nested = ours.path.join("src").join("deep");
        std::fs::create_dir_all(&nested).expect("mkdir nested");
        std::fs::write(nested.join("mod.rs"), "child work").expect("write");

        let age = worktree_age_ms(&ours.path);
        assert!(
            age < 60_000,
            "nested writes must keep the worktree fresh, got {age}ms"
        );
    }

    #[test]
    fn reap_stale_spares_fresh_nested_worktrees() {
        let repo = init_repo("reap-fresh");
        let ours = isolate(&repo, "task-live").expect("isolate");

        // Root mtime aged; child keeps writing to nested paths.
        let old = filetime::FileTime::from_unix_time(1_600_000_000, 0);
        filetime::set_file_mtime(&ours.path, old).expect("set root mtime");
        let deep = ours.path.join("deeply").join("nested");
        std::fs::create_dir_all(&deep).expect("mkdir deep");
        std::fs::write(deep.join("work.txt"), "still running").expect("write");

        let reaped = reap_stale(&repo, Duration::from_secs(60)).unwrap();
        assert!(
            !reaped
                .iter()
                .any(|p| p == &ours.path.to_string_lossy().to_string()),
            "live worktree with fresh nested writes must survive the reaper"
        );
        assert!(ours.path.exists(), "worktree must still exist");
    }
}
