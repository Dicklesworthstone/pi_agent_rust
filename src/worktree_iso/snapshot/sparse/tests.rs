use super::super::{capture, repository_path};
use super::*;

fn repository() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    run(
        command(repo.path()).args(["init", "-b", "main"]),
        "init fixture",
    )
    .unwrap();
    fs::write(repo.path().join("base.txt"), b"base\n").unwrap();
    run(command(repo.path()).args(["add", "."]), "stage fixture").unwrap();
    run(
        command(repo.path()).args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-m",
            "baseline",
        ]),
        "commit fixture",
    )
    .unwrap();
    repo
}

fn index_only(repo: &Path, name: &std::ffi::OsStr, mode: &str) -> String {
    let oid = object_id(&run(command(repo).args(["rev-parse", "HEAD:base.txt"]), "blob").unwrap())
        .unwrap();
    let mut entry = std::ffi::OsString::from(format!("{mode},{oid},"));
    entry.push(name);
    run(
        command(repo)
            .args(["update-index", "--add", "--cacheinfo"])
            .arg(entry),
        "index-only fixture",
    )
    .unwrap();
    run(
        command(repo)
            .args(["update-index", "--skip-worktree", "--"])
            .arg(name),
        "skip fixture",
    )
    .unwrap();
    oid
}

#[test]
fn absent_indexed_content_and_modes_survive_without_touching_parent_index() {
    let repo = repository();
    let name = std::ffi::OsStr::new("excluded/tool.sh");
    let oid = index_only(repo.path(), name, "100755");
    let index = repository_path(repo.path(), "--absolute-git-dir")
        .unwrap()
        .join("index");
    let before = fs::read(&index).unwrap();
    let head = run(command(repo.path()).args(["rev-parse", "HEAD"]), "head").unwrap();
    fs::write(repo.path().join("base.txt"), b"effective unstaged\n").unwrap();
    fs::write(repo.path().join("new.txt"), b"new\n").unwrap();
    let snapshot = capture(repo.path(), "absent-index").unwrap();
    let entry = run(
        command(repo.path())
            .args(["ls-tree", &snapshot.baseline, "--"])
            .arg(name),
        "snapshot entry",
    )
    .unwrap();
    assert!(entry.starts_with(format!("100755 blob {oid}\t").as_bytes()));
    let body = run(
        command(repo.path()).args(["show", &format!("{}:base.txt", snapshot.baseline)]),
        "effective content",
    )
    .unwrap();
    assert_eq!(body, b"effective unstaged\n");
    assert_eq!(
        run(
            command(repo.path()).args(["show", &format!("{}:new.txt", snapshot.baseline)]),
            "new content"
        )
        .unwrap(),
        b"new\n"
    );
    assert_eq!(fs::read(index).unwrap(), before);
    assert_eq!(
        run(command(repo.path()).args(["rev-parse", "HEAD"]), "head").unwrap(),
        head
    );
    assert!(!repo.path().join("excluded").exists());
}

#[test]
fn present_skip_worktree_files_are_refreshed_instead_of_reusing_stale_blobs() {
    let repo = repository();
    run(
        command(repo.path()).args(["update-index", "--skip-worktree", "--", "base.txt"]),
        "skip",
    )
    .unwrap();
    fs::write(repo.path().join("base.txt"), b"present and changed\n").unwrap();
    let index = repository_path(repo.path(), "--absolute-git-dir")
        .unwrap()
        .join("index");
    let before = fs::read(&index).unwrap();
    let snapshot = capture(repo.path(), "present-skip").unwrap();
    let body = run(
        command(repo.path()).args(["show", &format!("{}:base.txt", snapshot.baseline)]),
        "content",
    )
    .unwrap();
    assert_eq!(body, b"present and changed\n");
    assert_eq!(fs::read(index).unwrap(), before);
}

#[test]
fn stage_record_checks_preserve_tabs_and_reject_missing_or_conflicting_entries() {
    let name = b"excluded/a\tb\nc".to_vec();
    let wanted = BTreeSet::from([name.clone()]);
    let mut record = format!("100644 {} 0\t", "a".repeat(40)).into_bytes();
    record.extend_from_slice(&name);
    record.push(0);
    assert_eq!(selected_entries(&record, &wanted).unwrap().len(), 1);
    assert!(selected_entries(&[], &wanted).is_err());
    assert!(selected_entries(&[record.clone(), record.clone()].concat(), &wanted).is_err());
    let mut changed = record.clone();
    changed[0] = b'0';
    assert!(selected_entries(&changed, &wanted).is_err());
    for bytes in [b"missing terminator".as_slice(), b"\0", b"x\0\0"] {
        assert!(records(bytes).is_err());
    }
    let skipped = Skipped {
        flags: Vec::new(),
        absent: selected_entries(&record, &wanted).unwrap(),
    };
    changed = record;
    changed[7] = b'b';
    assert!(skipped.verify_materialized(&changed).is_err());
}

#[test]
fn source_materialization_and_flag_changes_invalidate_the_capture() {
    let repo = repository();
    index_only(repo.path(), std::ffi::OsStr::new("excluded.txt"), "100644");
    let scratch = Scratch::new(repo.path()).unwrap();
    let entries = scratch.entries(command(repo.path()), "parent").unwrap();
    let skipped = Skipped::capture(repo.path(), &scratch, &entries).unwrap();
    fs::write(repo.path().join("excluded.txt"), b"appeared\n").unwrap();
    assert!(skipped.verify_source(repo.path(), &scratch).is_err());
    run(
        command(repo.path()).args(["update-index", "--no-skip-worktree", "--", "excluded.txt"]),
        "change flags",
    )
    .unwrap();
    assert!(skipped.verify_source(repo.path(), &scratch).is_err());
}

#[test]
fn path_admission_distinguishes_absence_from_invalid_or_obstructed_paths() {
    let repo = repository();
    assert!(absent(repo.path(), b"missing/deep/file").unwrap());
    assert!(!absent(repo.path(), b"base.txt").unwrap());
    assert!(absent(repo.path(), b"base.txt/child").is_err());
    for bytes in [
        b"".as_slice(),
        b"../outside",
        b"/absolute",
        b"tree/",
        b"a\0b",
    ] {
        assert!(path(bytes).is_err());
    }
}

#[cfg(unix)]
#[test]
fn raw_unix_names_are_not_split_on_lines_or_reinterpreted_as_options() {
    use std::os::unix::ffi::OsStringExt as _;
    let repo = repository();
    let raw = b"-excluded\tline\n\xff".to_vec();
    let name = std::ffi::OsString::from_vec(raw.clone());
    index_only(repo.path(), &name, "100644");
    let snapshot = capture(repo.path(), "raw-path").unwrap();
    let names = run(
        command(repo.path()).args(["ls-tree", "-r", "--name-only", "-z", &snapshot.baseline]),
        "names",
    )
    .unwrap();
    assert!(
        names
            .split(|byte| *byte == 0)
            .any(|entry| entry == raw.as_slice())
    );
}

#[cfg(unix)]
#[test]
fn excluded_paths_do_not_follow_intermediate_symlinks() {
    let repo = repository();
    let other = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(other.path(), repo.path().join("link")).unwrap();
    assert!(absent(repo.path(), b"link/absent.txt").is_err());
    assert!(!absent(repo.path(), b"link").unwrap());
}

fn configure_sparse(repo: &Path, cone: bool, sparse_index: bool) {
    run(
        command(repo).args(["config", "extensions.worktreeConfig", "true"]),
        "worktree config",
    )
    .unwrap();
    for (key, value) in [
        ("core.sparseCheckout", "true"),
        (
            "core.sparseCheckoutCone",
            if cone { "true" } else { "false" },
        ),
        ("index.sparse", if sparse_index { "true" } else { "false" }),
    ] {
        run(
            command(repo).args(["config", "--worktree", key, value]),
            "sparse fixture mode",
        )
        .unwrap();
    }
    let info = repository_path(repo, "--absolute-git-dir")
        .unwrap()
        .join("info");
    fs::create_dir_all(&info).unwrap();
    fs::write(
        info.join("sparse-checkout"),
        if cone {
            "/*\n!/*/\n/included/\n"
        } else {
            "/*\n!/excluded/\n"
        },
    )
    .unwrap();
    if sparse_index {
        // Rebuild the same staged tree without -u; no working files are removed.
        let tree =
            object_id(&run(command(repo).arg("write-tree"), "staged tree").unwrap()).unwrap();
        run(
            command(repo).args(["read-tree", "-m", &tree]),
            "compress fixture index",
        )
        .unwrap();
        let entries = run(
            command(repo).args(["ls-files", "--sparse", "--stage", "-z"]),
            "check sparse index",
        )
        .unwrap();
        assert!(
            entries
                .split(|byte| *byte == 0)
                .any(|entry| entry.starts_with(b"040000 "))
        );
    }
}

#[test]
fn sparse_sources_create_full_children_without_expanding_or_reconfiguring_the_parent() {
    for (cone, sparse_index) in [(true, false), (false, false), (true, true)] {
        let repo = repository();
        index_only(
            repo.path(),
            std::ffi::OsStr::new("excluded/kept.txt"),
            "100644",
        );
        fs::write(repo.path().join("base.txt"), b"staged parent\n").unwrap();
        run(
            command(repo.path()).args(["add", "--", "base.txt"]),
            "staged parent",
        )
        .unwrap();
        configure_sparse(repo.path(), cone, sparse_index);
        fs::write(repo.path().join("base.txt"), b"unstaged parent\n").unwrap();
        let admin = repository_path(repo.path(), "--absolute-git-dir").unwrap();
        let index = fs::read(admin.join("index")).unwrap();
        let common = fs::read(admin.join("config")).unwrap();
        let local = fs::read(admin.join("config.worktree")).unwrap();
        let head = run(command(repo.path()).args(["rev-parse", "HEAD"]), "head").unwrap();
        let snapshot = capture(repo.path(), "sparse-full").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("child");
        snapshot
            .checkout(repo.path(), &child, "sparse-full-child")
            .unwrap();
        assert_eq!(
            fs::read(child.join("excluded/kept.txt")).unwrap(),
            b"base\n"
        );
        assert_eq!(
            fs::read(child.join("base.txt")).unwrap(),
            b"unstaged parent\n"
        );
        assert!(
            run(
                command(&child).args(["status", "--porcelain"]),
                "child status"
            )
            .unwrap()
            .is_empty()
        );
        assert!(!configured_bool(&child, "core.sparseCheckout").unwrap());
        assert!(!configured_bool(&child, "index.sparse").unwrap());
        assert_eq!(fs::read(admin.join("index")).unwrap(), index);
        assert_eq!(fs::read(admin.join("config")).unwrap(), common);
        assert_eq!(fs::read(admin.join("config.worktree")).unwrap(), local);
        assert_eq!(
            run(command(repo.path()).args(["rev-parse", "HEAD"]), "head").unwrap(),
            head
        );
        assert!(!repo.path().join("excluded").exists());
        assert!(configured_bool(repo.path(), "core.sparseCheckout").unwrap());
    }
}

#[test]
fn linked_sparse_source_keeps_its_own_index_and_configuration() {
    let repo = repository();
    let outer = tempfile::tempdir().unwrap();
    let source = outer.path().join("source");
    run(
        command(repo.path())
            .args(["worktree", "add", "--no-checkout", "-b", "linked-source"])
            .arg(&source)
            .arg("HEAD"),
        "linked source",
    )
    .unwrap();
    run(
        command(&source).args(["read-tree", "HEAD"]),
        "seed linked index",
    )
    .unwrap();
    run(
        command(&source).args(["checkout-index", "--", "base.txt"]),
        "materialize base",
    )
    .unwrap();
    index_only(&source, std::ffi::OsStr::new("excluded/kept.txt"), "100644");
    configure_sparse(&source, true, true);
    let source_admin = repository_path(&source, "--absolute-git-dir").unwrap();
    let main_admin = repository_path(repo.path(), "--absolute-git-dir").unwrap();
    let source_index = fs::read(source_admin.join("index")).unwrap();
    let source_config = fs::read(source_admin.join("config.worktree")).unwrap();
    let main_index = fs::read(main_admin.join("index")).unwrap();
    let main_config = fs::read(main_admin.join("config")).unwrap();
    let snapshot = capture(&source, "linked-sparse").unwrap();
    let child = outer.path().join("child");
    snapshot
        .checkout(&source, &child, "linked-sparse-child")
        .unwrap();
    assert_eq!(
        fs::read(child.join("excluded/kept.txt")).unwrap(),
        b"base\n"
    );
    assert_eq!(fs::read(source_admin.join("index")).unwrap(), source_index);
    assert_eq!(
        fs::read(source_admin.join("config.worktree")).unwrap(),
        source_config
    );
    assert_eq!(fs::read(main_admin.join("index")).unwrap(), main_index);
    assert_eq!(fs::read(main_admin.join("config")).unwrap(), main_config);
    assert!(configured_bool(&source, "core.sparseCheckout").unwrap());
    assert!(!configured_bool(&child, "core.sparseCheckout").unwrap());
}

#[test]
fn legacy_shared_sparse_config_and_mode_changes_are_refused_before_checkout() {
    let repo = repository();
    run(
        command(repo.path()).args(["config", "core.sparseCheckout", "true"]),
        "legacy config",
    )
    .unwrap();
    assert!(
        CheckoutMode::read(repo.path())
            .unwrap_err()
            .to_string()
            .contains("PI_ISO_SPARSE_UNSUPPORTED")
    );
    configure_sparse(repo.path(), true, false);
    let snapshot = capture(repo.path(), "mode-change").unwrap();
    run(
        command(repo.path()).args(["config", "--worktree", "core.sparseCheckout", "false"]),
        "change mode",
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let child = directory.path().join("never-created");
    assert!(
        snapshot
            .checkout(repo.path(), &child, "never-created")
            .is_err()
    );
    assert!(!child.exists());
}

#[test]
fn child_configuration_never_falls_back_to_shared_config_when_extension_is_missing() {
    let repo = repository();
    let config = repository_path(repo.path(), "--absolute-git-dir")
        .unwrap()
        .join("config");
    let before = fs::read(&config).unwrap();
    let mode = CheckoutMode {
        sparse: true,
        worktree_config: true,
    };
    assert!(mode.configure_child(repo.path()).is_err());
    assert_eq!(fs::read(config).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn public_subagent_runs_with_excluded_files_and_applies_only_its_materialized_change() {
    use crate::subagents::SubagentTool;
    use crate::tools::Tool as _;
    use std::os::unix::fs::PermissionsExt as _;
    let repo = repository();
    index_only(
        repo.path(),
        std::ffi::OsStr::new("excluded/kept.txt"),
        "100644",
    );
    configure_sparse(repo.path(), true, true);
    let admin = repository_path(repo.path(), "--absolute-git-dir").unwrap();
    let index = fs::read(admin.join("index")).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("child.sh");
    fs::write(&binary, r#"#!/bin/sh
set -eu
test -f excluded/kept.txt
printf 'delegated change\n' > base.txt
printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"done"}]}]}'
"#).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let tool = SubagentTool::with_paths(
        repo.path().to_path_buf(),
        directory.path().join("global"),
        binary,
    );
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap();
    let output = runtime.block_on(tool.execute("sparse-task", serde_json::json!({
        "agent":"task", "task":"update the included file", "isolation":"worktree", "isoApply":"apply",
    }), None)).unwrap();
    assert!(!output.is_error, "{output:?}");
    let details = output.details.unwrap();
    assert_eq!(details["results"][0]["iso"]["applied"], true);
    let patch = details["results"][0]["iso"]["patch"].as_str().unwrap();
    assert!(patch.contains("base.txt"));
    assert!(!patch.contains("excluded/"));
    assert_eq!(
        fs::read(repo.path().join("base.txt")).unwrap(),
        b"delegated change\n"
    );
    assert_eq!(fs::read(admin.join("index")).unwrap(), index);
    assert!(!repo.path().join("excluded").exists());
}
