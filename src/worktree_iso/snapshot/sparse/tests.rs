use super::*;
use super::super::{capture, repository_path};

fn repository() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    run(command(repo.path()).args(["init", "-b", "main"]), "init fixture").unwrap();
    fs::write(repo.path().join("base.txt"), b"base\n").unwrap();
    run(command(repo.path()).args(["add", "."]), "stage fixture").unwrap();
    run(command(repo.path()).args([
        "-c", "user.name=Fixture", "-c", "user.email=fixture@example.test",
        "-c", "commit.gpgSign=false", "commit", "-m", "baseline",
    ]), "commit fixture").unwrap();
    repo
}

fn index_only(repo: &Path, name: &std::ffi::OsStr, mode: &str) -> String {
    let oid = object_id(&run(command(repo).args(["rev-parse", "HEAD:base.txt"]), "blob").unwrap()).unwrap();
    let mut entry = std::ffi::OsString::from(format!("--cacheinfo={mode},{oid},"));
    entry.push(name);
    run(command(repo).args(["update-index", "--add"]).arg(entry), "index-only fixture").unwrap();
    run(command(repo).args(["update-index", "--skip-worktree", "--"]).arg(name), "skip fixture").unwrap();
    oid
}

#[test]
fn absent_indexed_content_and_modes_survive_without_touching_parent_index() {
    let repo = repository();
    let name = std::ffi::OsStr::new("excluded/tool.sh");
    let oid = index_only(repo.path(), name, "100755");
    let index = repository_path(repo.path(), "--absolute-git-dir").unwrap().join("index");
    let before = fs::read(&index).unwrap();
    let head = run(command(repo.path()).args(["rev-parse", "HEAD"]), "head").unwrap();
    fs::write(repo.path().join("base.txt"), b"effective unstaged\n").unwrap();
    fs::write(repo.path().join("new.txt"), b"new\n").unwrap();
    let snapshot = capture(repo.path(), "absent-index").unwrap();
    let entry = run(command(repo.path()).args(["ls-tree", &snapshot.baseline, "--"])
        .arg(name), "snapshot entry").unwrap();
    assert!(entry.starts_with(format!("100755 blob {oid}\t").as_bytes()));
    let body = run(command(repo.path()).args(["show", &format!("{}:base.txt", snapshot.baseline)]), "effective content").unwrap();
    assert_eq!(body, b"effective unstaged\n");
    assert_eq!(run(command(repo.path()).args(["show", &format!("{}:new.txt", snapshot.baseline)]), "new content").unwrap(), b"new\n");
    assert_eq!(fs::read(index).unwrap(), before);
    assert_eq!(run(command(repo.path()).args(["rev-parse", "HEAD"]), "head").unwrap(), head);
    assert!(!repo.path().join("excluded").exists());
}

#[test]
fn present_skip_worktree_files_are_refreshed_instead_of_reusing_stale_blobs() {
    let repo = repository();
    run(command(repo.path()).args(["update-index", "--skip-worktree", "--", "base.txt"]), "skip").unwrap();
    fs::write(repo.path().join("base.txt"), b"present and changed\n").unwrap();
    let index = repository_path(repo.path(), "--absolute-git-dir").unwrap().join("index");
    let before = fs::read(&index).unwrap();
    let snapshot = capture(repo.path(), "present-skip").unwrap();
    let body = run(command(repo.path()).args(["show", &format!("{}:base.txt", snapshot.baseline)]), "content").unwrap();
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
    let skipped = Skipped { flags: Vec::new(), absent: selected_entries(&record, &wanted).unwrap() };
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
    run(command(repo.path()).args(["update-index", "--no-skip-worktree", "--", "excluded.txt"]), "change flags").unwrap();
    assert!(skipped.verify_source(repo.path(), &scratch).is_err());
}

#[test]
fn path_admission_distinguishes_absence_from_invalid_or_obstructed_paths() {
    let repo = repository();
    assert!(absent(repo.path(), b"missing/deep/file").unwrap());
    assert!(!absent(repo.path(), b"base.txt").unwrap());
    assert!(absent(repo.path(), b"base.txt/child").is_err());
    for bytes in [b"".as_slice(), b"../outside", b"/absolute", b"tree/", b"a\0b"] {
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
    let names = run(command(repo.path()).args(["ls-tree", "-r", "--name-only", "-z", &snapshot.baseline]), "names").unwrap();
    assert!(names.split(|byte| *byte == 0).any(|entry| entry == raw.as_slice()));
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
