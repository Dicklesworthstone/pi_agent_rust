//! Real-Git regression cases for launch snapshots and non-mutating diff collection.

use super::*;
use std::fs;
use tempfile::TempDir;

fn repository_at(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git_ok(path, &["init", "-b", "main"]).unwrap();
    git_ok(path, &["config", "user.name", "Isolation Fixture"]).unwrap();
    git_ok(path, &["config", "user.email", "isolation@localhost"]).unwrap();
    git_ok(path, &["config", "commit.gpgSign", "false"]).unwrap();
    fs::write(path.join("base.txt"), "original\n").unwrap();
    git_ok(path, &["add", "."]).unwrap();
    git_ok(path, &["commit", "-m", "initial"]).unwrap();
}

fn repository() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    repository_at(dir.path());
    dir
}

fn unborn_repository() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    git_ok(dir.path(), &["init", "-b", "main"]).unwrap();
    dir
}

fn index_bytes(repo: &Path) -> Vec<u8> {
    let index = git_ok(repo, &["rev-parse", "--git-path", "index"]).unwrap();
    let index = PathBuf::from(index.trim_end_matches('\n'));
    fs::read(if index.is_absolute() {
        index
    } else {
        repo.join(index)
    })
    .unwrap()
}

#[test]
fn an_empty_unborn_repository_can_isolate_and_apply_its_first_files() {
    let repo = unborn_repository();
    git_ok(repo.path(), &["config", "user.name", ""]).unwrap();
    git_ok(repo.path(), &["config", "user.email", ""]).unwrap();
    git_ok(repo.path(), &["config", "user.useConfigOnly", "true"]).unwrap();
    git_ok(repo.path(), &["config", "commit.gpgSign", "true"]).unwrap();
    git_ok(
        repo.path(),
        &["config", "gpg.program", "/not/a/real/signer"],
    )
    .unwrap();
    let head = fs::read(repo.path().join(".git/HEAD")).unwrap();
    assert!(!repo.path().join(".git/index").exists());

    let handle = isolate(repo.path(), "empty-unborn").unwrap();
    assert_eq!(
        git_ok(
            &handle.path,
            &["rev-list", "--parents", "--max-count=1", &handle.baseline],
        )
        .unwrap(),
        format!("{}\n", handle.baseline),
        "the internal snapshot is a root commit"
    );
    assert!(collect_diff(&handle).unwrap().0.is_empty());
    fs::create_dir(handle.path.join("src")).unwrap();
    fs::write(handle.path.join("src/main.rs"), "fn main() {}\n").unwrap();
    assert!(!repo.path().join("src").exists());
    let (patch, _) = collect_diff(&handle).unwrap();
    assert!(patch.contains("src/main.rs"));
    apply_to_parent(&handle, &patch).unwrap();
    assert_eq!(
        fs::read(repo.path().join("src/main.rs")).unwrap(),
        b"fn main() {}\n"
    );
    drop_worktree(&handle).unwrap();

    assert_eq!(fs::read(repo.path().join(".git/HEAD")).unwrap(), head);
    assert!(git_ok(repo.path(), &["rev-parse", "--verify", "HEAD"]).is_err());
    assert!(!repo.path().join(".git/index").exists());
    assert!(!repo.path().join(".git/refs/heads/main").exists());
}

#[test]
fn an_unborn_scaffold_preserves_staging_and_shares_a_frozen_launch_baseline() {
    let repo = unborn_repository();
    fs::create_dir(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/main.rs"), "staged parent\n").unwrap();
    fs::write(repo.path().join(".gitignore"), "*.private\n").unwrap();
    fs::write(repo.path().join("tracked.private"), "tracked\n").unwrap();
    fs::write(repo.path().join("secret.private"), "ignored\n").unwrap();
    git_ok(repo.path(), &["add", "src/main.rs", ".gitignore"]).unwrap();
    git_ok(repo.path(), &["add", "--force", "tracked.private"]).unwrap();
    fs::write(repo.path().join("src/main.rs"), "working parent\n").unwrap();
    fs::write(repo.path().join("untracked.bin"), [0, 255, 1, 128]).unwrap();
    let parent_index = index_bytes(repo.path());
    let parent_head = fs::read(repo.path().join(".git/HEAD")).unwrap();
    let baseline = IsoBaseline::capture(isolation_root(repo.path()).unwrap()).unwrap();
    let first = baseline.isolate("unborn-first").unwrap();
    assert_eq!(
        fs::read(first.path.join("src/main.rs")).unwrap(),
        b"working parent\n"
    );
    assert_eq!(
        fs::read(first.path.join("untracked.bin")).unwrap(),
        [0, 255, 1, 128]
    );
    assert_eq!(
        fs::read(first.path.join("tracked.private")).unwrap(),
        b"tracked\n"
    );
    assert!(!first.path.join("secret.private").exists());
    assert!(collect_diff(&first).unwrap().0.is_empty());
    assert_eq!(index_bytes(repo.path()), parent_index);

    fs::write(first.path.join("src/main.rs"), "staged child\n").unwrap();
    git_ok(&first.path, &["add", "src/main.rs"]).unwrap();
    fs::write(first.path.join("src/main.rs"), "working child\n").unwrap();
    fs::write(first.path.join("untracked.bin"), [0, 254, 2, 127]).unwrap();
    fs::write(first.path.join("new.txt"), "child addition\n").unwrap();
    let child_index = index_bytes(&first.path);
    let (patch, _) = collect_diff(&first).unwrap();
    assert_eq!(index_bytes(&first.path), child_index);
    assert_eq!(
        fs::read(repo.path().join("src/main.rs")).unwrap(),
        b"working parent\n"
    );
    apply_to_parent(&first, &patch).unwrap();
    assert_eq!(
        fs::read(repo.path().join("src/main.rs")).unwrap(),
        b"working child\n"
    );
    assert_eq!(
        fs::read(repo.path().join("untracked.bin")).unwrap(),
        [0, 254, 2, 127]
    );
    assert_eq!(
        fs::read(repo.path().join("new.txt")).unwrap(),
        b"child addition\n"
    );
    assert_eq!(
        fs::read(repo.path().join("secret.private")).unwrap(),
        b"ignored\n"
    );
    assert_eq!(index_bytes(repo.path()), parent_index);
    assert_eq!(
        git_ok(repo.path(), &["show", ":src/main.rs"]).unwrap(),
        "staged parent\n"
    );

    let second = baseline.isolate("unborn-second").unwrap();
    assert_eq!(second.baseline, first.baseline);
    assert_eq!(
        fs::read(second.path.join("src/main.rs")).unwrap(),
        b"working parent\n"
    );
    assert_eq!(
        fs::read(second.path.join("untracked.bin")).unwrap(),
        [0, 255, 1, 128]
    );
    assert!(!second.path.join("new.txt").exists());
    assert!(collect_diff(&second).unwrap().0.is_empty());
    drop_worktree(&first).unwrap();
    drop_worktree(&second).unwrap();
    assert_eq!(
        fs::read(repo.path().join(".git/HEAD")).unwrap(),
        parent_head
    );
    assert_eq!(index_bytes(repo.path()), parent_index);
    assert!(git_ok(repo.path(), &["rev-parse", "--verify", "HEAD"]).is_err());
}

#[test]
fn invalid_parent_references_are_never_treated_as_unborn_branches() {
    for kind in [
        "missing-object",
        "malformed-branch",
        "non-commit-object",
        "dangling-symbolic-branch",
        "non-branch-head",
        "detached-missing-head",
        "malformed-head",
    ] {
        let repo = unborn_repository();
        fs::write(repo.path().join("scaffold.txt"), "staged scaffold\n").unwrap();
        git_ok(repo.path(), &["add", "scaffold.txt"]).unwrap();
        let index = index_bytes(repo.path());
        let missing_object = format!("{}\n", "1".repeat(40));
        let (reference, contents) = match kind {
            "missing-object" => ("refs/heads/main", missing_object),
            "malformed-branch" => ("refs/heads/main", "not-an-object\n".to_string()),
            "non-commit-object" => (
                "refs/heads/main",
                git_ok(repo.path(), &["hash-object", "scaffold.txt"]).unwrap(),
            ),
            "dangling-symbolic-branch" => (
                "refs/heads/main",
                "ref: refs/heads/absent\n".to_string(),
            ),
            "non-branch-head" => ("HEAD", "ref: refs/tags/absent\n".to_string()),
            "detached-missing-head" => ("HEAD", missing_object),
            "malformed-head" => ("HEAD", "not-a-valid-head\n".to_string()),
            _ => unreachable!("fixture kind is enumerated above"),
        };
        fs::write(repo.path().join(".git").join(reference), &contents).unwrap();
        let head = fs::read(repo.path().join(".git/HEAD")).unwrap();
        let error = isolate(repo.path(), kind).unwrap_err();
        assert!(
            error.to_string().contains("PI_ISO_SNAPSHOT")
                || error.to_string().contains("PI_ISO_NOT_GIT"),
            "{kind}: {error}"
        );
        assert_eq!(
            fs::read(repo.path().join(".git/HEAD")).unwrap(),
            head,
            "{kind}"
        );
        assert_eq!(
            fs::read(repo.path().join(".git").join(reference)).unwrap(),
            contents.as_bytes(),
            "{kind}"
        );
        // Even reading the index through Git can fail with a malformed HEAD.
        assert_eq!(
            fs::read(repo.path().join(".git/index")).unwrap(),
            index,
            "{kind}"
        );
        assert_eq!(
            fs::read(repo.path().join("scaffold.txt")).unwrap(),
            b"staged scaffold\n"
        );
        assert!(!repo.path().join(".git/worktrees").exists(), "{kind}");
    }

    let non_git = tempfile::tempdir().unwrap();
    fs::write(non_git.path().join("scaffold.txt"), "not a repository\n").unwrap();
    let error = isolate(non_git.path(), "non-git").unwrap_err();
    assert!(error.to_string().contains("PI_ISO_NOT_GIT"), "{error}");
    assert_eq!(
        fs::read(non_git.path().join("scaffold.txt")).unwrap(),
        b"not a repository\n"
    );
    assert!(!non_git.path().join(".git").exists());
}

#[cfg(unix)]
#[test]
fn switching_parent_branches_during_capture_is_detected_even_without_a_commit_change() {
    for unborn in [true, false] {
        let repo = if unborn {
            unborn_repository()
        } else {
            repository()
        };
        fs::write(repo.path().join("staged.txt"), "staged\n").unwrap();
        git_ok(repo.path(), &["add", "staged.txt"]).unwrap();
        if !unborn {
            git_ok(repo.path(), &["branch", "during-snapshot"]).unwrap();
        }
        let index = index_bytes(repo.path());
        fs::write(
            repo.path().join(".gitattributes"),
            "trigger.txt filter=switch-head\n",
        )
        .unwrap();
        fs::write(repo.path().join("trigger.txt"), "trigger\n").unwrap();
        git_ok(
            repo.path(),
            &[
                "config",
                "filter.switch-head.clean",
                "git symbolic-ref HEAD refs/heads/during-snapshot && cat",
            ],
        )
        .unwrap();
        git_ok(
            repo.path(),
            &["config", "filter.switch-head.required", "true"],
        )
        .unwrap();
        let error = isolate(repo.path(), "head-race").unwrap_err();
        assert!(
            error.to_string().contains("PI_ISO_PARENT_CHANGED"),
            "{error}"
        );
        assert_eq!(
            git_ok(repo.path(), &["symbolic-ref", "HEAD"]).unwrap(),
            "refs/heads/during-snapshot\n",
            "do not undo the external branch change"
        );
        assert_eq!(index_bytes(repo.path()), index);
        assert!(!repo.path().join(".git/worktrees").exists());
    }
}

#[test]
fn dirty_snapshot_and_collection_preserve_both_real_indexes() {
    let repo = repository();
    fs::write(repo.path().join("base.txt"), "staged parent\n").unwrap();
    git_ok(repo.path(), &["add", "base.txt"]).unwrap();
    fs::write(repo.path().join("base.txt"), "working parent\n").unwrap();
    fs::write(repo.path().join("untracked.bin"), [0, 255, 1, 128]).unwrap();
    let parent_index = index_bytes(repo.path());
    let parent_head = git_ok(repo.path(), &["rev-parse", "HEAD"]).unwrap();
    let handle = isolate(repo.path(), "preserve-staging").unwrap();
    assert_eq!(
        fs::read(handle.path.join("base.txt")).unwrap(),
        b"working parent\n"
    );
    assert_eq!(
        fs::read(handle.path.join("untracked.bin")).unwrap(),
        [0, 255, 1, 128]
    );
    assert_eq!(index_bytes(repo.path()), parent_index);
    assert!(collect_diff(&handle).unwrap().0.is_empty());

    fs::write(handle.path.join("base.txt"), "staged child\n").unwrap();
    git_ok(&handle.path, &["add", "base.txt"]).unwrap();
    fs::write(handle.path.join("base.txt"), "working child\n").unwrap();
    let child_index = index_bytes(&handle.path);
    let (patch, _) = collect_diff(&handle).unwrap();
    assert_eq!(index_bytes(&handle.path), child_index);
    apply_to_parent(&handle, &patch).unwrap();
    assert_eq!(
        fs::read(repo.path().join("base.txt")).unwrap(),
        b"working child\n"
    );
    assert_eq!(index_bytes(repo.path()), parent_index);
    assert_eq!(
        git_ok(repo.path(), &["rev-parse", "HEAD"]).unwrap(),
        parent_head
    );
    drop_worktree(&handle).unwrap();
}

#[test]
fn tracked_ignored_additions_survive_without_copying_ignored_untracked_files() {
    let repo = repository();
    fs::write(repo.path().join(".gitignore"), "*.ignored\n").unwrap();
    fs::write(repo.path().join("tracked.ignored"), "tracked\n").unwrap();
    fs::write(
        repo.path().join("private.ignored"),
        "not part of the snapshot\n",
    )
    .unwrap();
    git_ok(repo.path(), &["add", "-f", "tracked.ignored"]).unwrap();
    let before = index_bytes(repo.path());
    let handle = isolate(repo.path(), "ignored").unwrap();
    assert_eq!(
        fs::read(handle.path.join("tracked.ignored")).unwrap(),
        b"tracked\n"
    );
    assert!(!handle.path.join("private.ignored").exists());
    assert_eq!(index_bytes(repo.path()), before);
    drop_worktree(&handle).unwrap();
}

#[cfg(unix)]
#[test]
fn unusual_untracked_names_and_binary_diffs_round_trip_without_loss() -> std::io::Result<()> {
    use std::os::unix::ffi::OsStringExt as _;

    let repo = repository();
    let mut names = vec![
        std::ffi::OsString::from("line\nbreak.bin"),
        std::ffi::OsString::from("tab\tand\"quote.bin"),
        std::ffi::OsString::from("日本語 space.bin"),
    ];
    let original: Vec<u8> = (0..=255).collect();
    for name in &names {
        fs::write(repo.path().join(name), &original).unwrap();
    }
    let non_utf8 = std::ffi::OsString::from_vec(b"nonutf8-\xff.bin".to_vec());
    match fs::write(repo.path().join(&non_utf8), &original) {
        Ok(()) => names.push(non_utf8),
        Err(error) if error.raw_os_error() == Some(rustix::io::Errno::ILSEQ.raw_os_error()) => {
            eprintln!("SKIP non-UTF-8 filename {non_utf8:?}: filesystem returned {error}");
        }
        Err(error) => return Err(error),
    }
    eprintln!("Exercising snapshot filenames: {names:?}");
    let handle = isolate(repo.path(), "filenames").unwrap();
    let changed = [0, 254, 127, 1, 0];
    for name in &names {
        assert_eq!(fs::read(handle.path.join(name)).unwrap(), original);
        fs::write(handle.path.join(name), changed).unwrap();
    }
    let (patch, _) = collect_diff(&handle).unwrap();
    assert!(patch.contains("GIT binary patch"));
    apply_to_parent(&handle, &patch).unwrap();
    for name in &names {
        assert_eq!(fs::read(repo.path().join(name)).unwrap(), changed);
    }
    drop_worktree(&handle).unwrap();
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlinks_dangling_links_and_executable_modes_are_not_flattened() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let repo = repository();
    symlink("base.txt", repo.path().join("alias")).unwrap();
    symlink("missing-target", repo.path().join("dangling")).unwrap();
    fs::write(repo.path().join("run.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(
        repo.path().join("run.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let handle = isolate(repo.path(), "file-types").unwrap();
    assert_eq!(
        fs::read_link(handle.path.join("alias")).unwrap(),
        Path::new("base.txt")
    );
    assert_eq!(
        fs::read_link(handle.path.join("dangling")).unwrap(),
        Path::new("missing-target")
    );
    assert_ne!(
        fs::metadata(handle.path.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert!(collect_diff(&handle).unwrap().0.is_empty());
    drop_worktree(&handle).unwrap();
}

#[test]
fn requesting_isolation_from_a_subdirectory_still_captures_the_whole_repo() {
    let repo = repository();
    fs::create_dir(repo.path().join("nested")).unwrap();
    fs::write(repo.path().join("nested/inside.txt"), "inside\n").unwrap();
    fs::write(repo.path().join("outside.txt"), "outside\n").unwrap();
    let handle = isolate(&repo.path().join("nested"), "subdirectory").unwrap();
    assert_eq!(handle.repo_root, repo.path().canonicalize().unwrap());
    assert_eq!(
        fs::read(handle.path.join("outside.txt")).unwrap(),
        b"outside\n"
    );
    assert_eq!(
        fs::read(handle.path.join("nested/inside.txt")).unwrap(),
        b"inside\n"
    );
    drop_worktree(&handle).unwrap();
}

#[test]
fn isolation_control_filenames_are_ordinary_user_files() {
    let repo = repository();
    let names = [".pi-iso-parent.patch", ".pi-iso-outgoing.patch"];
    for name in names {
        fs::write(repo.path().join(name), "user-owned contents\n").unwrap();
    }
    fs::write(repo.path().join("base.txt"), "parent dirty\n").unwrap();
    let handle = isolate(repo.path(), "control-names").unwrap();
    for name in names {
        assert_eq!(
            fs::read(handle.path.join(name)).unwrap(),
            b"user-owned contents\n"
        );
    }
    fs::write(handle.path.join("child.txt"), "child\n").unwrap();
    let (patch, _) = collect_diff(&handle).unwrap();
    assert!(!patch.contains(".pi-iso-"));
    apply_to_parent(&handle, &patch).unwrap();
    for name in names {
        assert_eq!(
            fs::read(repo.path().join(name)).unwrap(),
            b"user-owned contents\n"
        );
        assert_eq!(
            fs::read(handle.path.join(name)).unwrap(),
            b"user-owned contents\n"
        );
    }
    assert_eq!(
        collect_diff(&handle).unwrap().0,
        patch,
        "collection must not introduce its own patch file"
    );
    drop_worktree(&handle).unwrap();
}

#[cfg(unix)]
#[test]
fn internal_baselines_do_not_require_user_identity_signing_or_commit_hooks() {
    use std::os::unix::fs::PermissionsExt as _;

    let repo = repository();
    let hooks = tempfile::tempdir().unwrap();
    for name in ["pre-commit", "post-commit", "post-checkout"] {
        let path = hooks.path().join(name);
        fs::write(&path, "#!/bin/sh\nexit 77\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    git_ok(
        repo.path(),
        &["config", "core.hooksPath", hooks.path().to_str().unwrap()],
    )
    .unwrap();
    git_ok(repo.path(), &["config", "user.name", ""]).unwrap();
    git_ok(repo.path(), &["config", "user.email", ""]).unwrap();
    git_ok(repo.path(), &["config", "user.useConfigOnly", "true"]).unwrap();
    git_ok(repo.path(), &["config", "commit.gpgSign", "true"]).unwrap();
    git_ok(
        repo.path(),
        &["config", "gpg.program", "/not/a/real/signer"],
    )
    .unwrap();
    let head = git_ok(repo.path(), &["rev-parse", "HEAD"]).unwrap();
    let handle = isolate(repo.path(), "internal-baseline").unwrap();
    fs::write(handle.path.join("child.txt"), "child\n").unwrap();
    assert!(collect_diff(&handle).unwrap().0.contains("child.txt"));
    assert_eq!(git_ok(repo.path(), &["rev-parse", "HEAD"]).unwrap(), head);
    drop_worktree(&handle).unwrap();
}

#[test]
fn unsupported_submodules_and_sparse_checkouts_fail_before_creating_worktrees() {
    let repo = repository();
    let head = git_ok(repo.path(), &["rev-parse", "HEAD"]).unwrap();
    git_ok(
        repo.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},vendor", head.trim()),
        ],
    )
    .unwrap();
    let before = git_ok(repo.path(), &["worktree", "list", "--porcelain"]).unwrap();
    let index = index_bytes(repo.path());
    let error = isolate(repo.path(), "gitlink").unwrap_err();
    assert!(
        error.to_string().contains("PI_ISO_SUBMODULE_UNSUPPORTED"),
        "{error}"
    );
    assert_eq!(
        git_ok(repo.path(), &["worktree", "list", "--porcelain"]).unwrap(),
        before
    );
    assert_eq!(index_bytes(repo.path()), index);

    let sparse = repository();
    git_ok(sparse.path(), &["config", "core.sparseCheckout", "true"]).unwrap();
    let before = git_ok(sparse.path(), &["worktree", "list", "--porcelain"]).unwrap();
    let error = isolate(sparse.path(), "sparse").unwrap_err();
    assert!(
        error.to_string().contains("PI_ISO_SPARSE_UNSUPPORTED"),
        "{error}"
    );
    assert_eq!(
        git_ok(sparse.path(), &["worktree", "list", "--porcelain"]).unwrap(),
        before
    );
}

#[test]
fn a_linked_parent_worktree_keeps_its_own_index_and_head() {
    let repo = repository();
    let outer = tempfile::tempdir().unwrap();
    let linked = outer.path().join("linked");
    git_ok(
        repo.path(),
        &[
            "worktree",
            "add",
            "-b",
            "linked-parent",
            linked.to_str().unwrap(),
        ],
    )
    .unwrap();
    fs::write(linked.join("base.txt"), "linked staged\n").unwrap();
    git_ok(&linked, &["add", "base.txt"]).unwrap();
    fs::write(linked.join("base.txt"), "linked actual\n").unwrap();
    let index = index_bytes(&linked);
    let head = git_ok(&linked, &["rev-parse", "HEAD"]).unwrap();
    let handle = isolate(&linked, "linked-child").unwrap();
    assert_eq!(
        fs::read(handle.path.join("base.txt")).unwrap(),
        b"linked actual\n"
    );
    assert_eq!(index_bytes(&linked), index);
    assert_eq!(git_ok(&linked, &["rev-parse", "HEAD"]).unwrap(), head);
    assert_eq!(
        fs::read(repo.path().join("base.txt")).unwrap(),
        b"original\n"
    );
    drop_worktree(&handle).unwrap();
    git_ok(
        repo.path(),
        &["worktree", "remove", "--force", linked.to_str().unwrap()],
    )
    .unwrap();
}

#[test]
fn non_utf8_text_diffs_fail_instead_of_replacing_bytes_lossily() {
    let repo = repository();
    let handle = isolate(repo.path(), "text-encoding").unwrap();
    fs::write(handle.path.join("base.txt"), [b'a', 255, b'\n']).unwrap();
    let error = collect_diff(&handle).unwrap_err();
    assert!(
        error.to_string().contains("PI_ISO_TEXT_ENCODING"),
        "{error}"
    );
    assert_eq!(
        fs::read(repo.path().join("base.txt")).unwrap(),
        b"original\n"
    );
    assert_eq!(
        fs::read(handle.path.join("base.txt")).unwrap(),
        [b'a', 255, b'\n']
    );
    drop_worktree(&handle).unwrap();
}

#[test]
fn foreign_branch_in_a_prefix_named_directory_is_not_reaped() {
    let repo = repository();
    let outer = tempfile::tempdir().unwrap();
    let foreign = outer.path().join("pi-iso-user-owned");
    git_ok(
        repo.path(),
        &[
            "worktree",
            "add",
            "-b",
            "user-owned",
            foreign.to_str().unwrap(),
        ],
    )
    .unwrap();
    let reaped = reap_stale(repo.path(), Duration::ZERO).unwrap();
    assert!(!reaped.iter().any(|path| Path::new(path) == foreign));
    assert!(foreign.join("base.txt").exists());
    git_ok(
        repo.path(),
        &["worktree", "remove", "--force", foreign.to_str().unwrap()],
    )
    .unwrap();
}
