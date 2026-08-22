//! Workspace isolation for subagents (bd-cv653.5.2).
//!
//! `worktree` mode: `git worktree add` a temp branch at HEAD into
//! `<tmp>/pi-iso-<id>`; the child runs there; on completion the parent
//! gets {worktree_path, diff_stat, patch} and applies per mode:
//! `keep` (leave for the user), `apply` (git apply into the parent tree,
//! serialized process-wide so sibling patches land in task order), `drop`
//! (remove after reporting the patch). A failing apply reports the
//! conflicting files and leaves the worktree — never force.
//!
//! Dirty-tree strategy (round-7 correctness): `git worktree add` starts
//! from HEAD, so uncommitted parent changes would be invisible to isolated
//! children. `isolate` materializes the working-tree state into the copy:
//! a tracked-diff patch (`git diff HEAD`) applied into the worktree plus
//! an untracked-file copy list. CoW backends (APFS clonefile, btrfs
//! reflink, overlayfs) are the documented full-fidelity path when they
//! land — same interface.
//!
//! Cleanup: only `pi-iso-` prefix-tagged worktrees are ever reaped;
//! foreign worktrees are untouchable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::error::{Error, Result};

/// Tool-result schema tag for isolation outcomes.
pub const ISO_SCHEMA: &str = "pi.worktree_iso.v1";

/// Branch/path prefix that marks worktrees created by us. The reaper only
/// ever touches this prefix.
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
    /// Commit recording the materialized parent state (HEAD + dirty
    /// materialization). The child's own work is the diff from here.
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
    /// The full unified diff (worktree state vs parent HEAD).
    pub patch: String,
    /// Files that failed to apply (conflict path only).
    pub conflicted_files: Vec<String>,
    pub apply_mode: String,
    pub applied: bool,
}

fn git(repo: &Path, args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
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
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn sanitize_id(task_id: &str) -> String {
    let mut out = String::with_capacity(task_id.len().min(32));
    for ch in task_id.chars() {
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
        trimmed.chars().take(32).collect()
    }
}

/// Create an isolated worktree for a child task, materializing the parent's
/// uncommitted state into it.
///
/// # Errors
static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Named `PI_ISO_NOT_GIT` for non-git directories; git failures otherwise.
pub fn isolate(repo_root: &Path, task_id: &str) -> Result<IsoHandle> {
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

    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = format!(
        "{ISO_PREFIX}{}-{}-{seq}",
        sanitize_id(task_id),
        std::process::id()
    );
    let path = std::env::temp_dir().join(&id);

    // Branch from HEAD into the temp path.
    git_ok(
        repo_root,
        &["worktree", "add", &path.to_string_lossy(), "-b", &id],
    )?;

    // Dirty-tree materialization: tracked diff + untracked files.
    let tracked_patch = git_ok(repo_root, &["diff", "--binary", "HEAD"])?;
    if !tracked_patch.trim().is_empty() {
        let patch_file = path.join(".pi-iso-parent.patch");
        std::fs::write(&patch_file, &tracked_patch)
            .map_err(|e| Error::tool("subagent", format!("Failed to stage parent patch: {e}")))?;
        git_ok(&path, &["apply", &patch_file.to_string_lossy()])?;
        let _ = std::fs::remove_file(&patch_file);
    }
    let untracked = git_ok(repo_root, &["ls-files", "--others", "--exclude-standard"])?;
    for relative in untracked.lines().filter(|line| !line.is_empty()) {
        let source = repo_root.join(relative);
        let target = path.join(relative);
        if source.is_file() {
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::copy(&source, &target);
        }
    }

    // Commit the materialized baseline so the child's own work is a clean
    // diff against it (and the parent's dirty state never double-applies).
    git_ok(&path, &["add", "-A"])?;
    git_ok(
        &path,
        &[
            "commit",
            "--allow-empty",
            "--no-verify",
            "-m",
            &format!("pi-iso baseline {id}"),
        ],
    )?;
    let baseline = git_ok(&path, &["rev-parse", "HEAD"])?.trim().to_string();

    Ok(IsoHandle {
        branch: id.clone(),
        id,
        path,
        repo_root: repo_root.to_path_buf(),
        baseline,
    })
}

/// Collect the child's worktree state as a patch vs the materialized
/// baseline (so the parent's dirty state never double-applies).
///
/// # Errors
/// git failures.
pub fn collect_diff(handle: &IsoHandle) -> Result<(String, String)> {
    // Stage everything so untracked work appears in the diff.
    git_ok(&handle.path, &["add", "-A"])?;
    let patch = git_ok(
        &handle.path,
        &["diff", "--binary", "--cached", &handle.baseline],
    )?;
    let diff_stat = git_ok(
        &handle.path,
        &["diff", "--cached", "--stat", &handle.baseline],
    )?;
    Ok((patch, diff_stat))
}

/// Serialize parent-tree application across sibling children (they apply
/// in task order, never concurrently).
fn parent_apply_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(()));
    &LOCK
}

/// Apply the collected patch into the parent tree. On conflict, report the
/// conflicting files and leave the worktree for manual resolution — never
/// force.
///
/// # Errors
/// Named `PI_ISO_CONFLICT` listing conflicted files.
pub fn apply_to_parent(handle: &IsoHandle, patch: &str) -> Result<()> {
    if patch.trim().is_empty() {
        return Ok(());
    }
    let _guard = parent_apply_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // --check first so a failing apply reports files without partial writes.
    let patch_file = handle.path.join(".pi-iso-outgoing.patch");
    std::fs::write(&patch_file, patch)
        .map_err(|e| Error::tool("subagent", format!("Failed to stage patch: {e}")))?;
    let check = git(
        &handle.repo_root,
        &["apply", "--check", &patch_file.to_string_lossy()],
    )?;
    if !check.status.success() {
        let stderr = String::from_utf8_lossy(&check.stderr).into_owned();
        let mut files: Vec<String> = stderr
            .lines()
            .filter_map(|line| {
                line.strip_prefix("error: ")
                    .and_then(|rest| rest.split(':').nth(1))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            })
            .collect();
        files.sort();
        files.dedup();
        let _ = std::fs::remove_file(&patch_file);
        return Err(Error::tool(
            "subagent",
            format!(
                "PI_ISO_CONFLICT: patch from {} does not apply cleanly to the parent tree. \
                 Conflicted files: {}. The worktree is left at {} for manual resolution.",
                handle.branch,
                if files.is_empty() {
                    "(see git apply output)".to_string()
                } else {
                    files.join(", ")
                },
                handle.path.display()
            ),
        ));
    }
    git_ok(&handle.repo_root, &["apply", &patch_file.to_string_lossy()])?;
    let _ = std::fs::remove_file(&patch_file);
    Ok(())
}

/// Remove the worktree and its branch.
///
/// # Errors
/// git failures.
pub fn drop_worktree(handle: &IsoHandle) -> Result<()> {
    git_ok(
        &handle.repo_root,
        &[
            "worktree",
            "remove",
            "--force",
            &handle.path.to_string_lossy(),
        ],
    )?;
    // Branch deletion is best-effort (the worktree removal usually drops it).
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
        if !path.contains(ISO_PREFIX) {
            return;
        }
        let age_ms = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|mtime| mtime.elapsed().ok())
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
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

/// Reap our stale worktrees (prefix-tagged only; foreign worktrees are
/// never touched).
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
        let err = isolate(&dir, "x").unwrap_err();
        assert!(
            err.to_string().contains("PI_ISO_NOT_GIT"),
            "expected named refusal: {err}"
        );
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
}
