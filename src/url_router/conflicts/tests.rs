use super::*;

pub(super) const BLOCK: &str = "<<<<<<< ours\nours 😀\n||||||| base\nbase\n=======\ntheirs 雪\n>>>>>>> theirs\n";

fn event(case: &str) {
    eprintln!("{}", json!({"suite":"conflict_resolution","case":case,"phase":"start"}));
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = std::process::Command::new("git").current_dir(root).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    output.stdout
}

pub(super) fn fixture(name: &str, content: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Conflict Fixture"]);
    git(root, &["config", "user.email", "conflict@example.invalid"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join(name), "base\n").unwrap();
    git(root, &["add", "--", name]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["checkout", "-qb", "side"]);
    std::fs::write(root.join(name), "side\n").unwrap();
    git(root, &["commit", "-qam", "side"]);
    git(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join(name), "main\n").unwrap();
    git(root, &["commit", "-qam", "main"]);
    let result = std::process::Command::new("git").current_dir(root)
        .args(["merge", "side"]).output().unwrap();
    assert!(!result.status.success());
    // Keep a real unmerged index while making the working image cover byte
    // and marker variants independently of the installed Git merge algorithm.
    std::fs::write(root.join(name), content).unwrap();
    dir
}

#[test]
fn nul_delimited_index_paths_preserve_tabs_newlines_and_deduplicate_stages() {
    event("nul-index");
    let name = "spaced \"name\"\tand\nnewline.txt";
    let listing = format!("100644 deadbeef 1\t{name}\0\
        100644 deadbeef 2\t{name}\0\
        100644 deadbeef 3\t{name}\0");
    assert_eq!(paths(listing.as_bytes()).unwrap(), BTreeSet::from([name.to_owned()]));
    assert!(paths(b"100644 deadbeef 2\tfile").is_err());
    assert!(paths(b"100644 deadbeef 0\tfile\0").is_err());
    assert!(paths(b"100644 deadbeef 2\t\xff\0").is_err());
}

#[test]
fn public_read_and_single_resolution_preserve_the_other_block_index_and_head() {
    event("public-lossless-resolution");
    let content = format!("head\n{BLOCK}middle\n{BLOCK}tail");
    let dir = fixture("f.txt", &content);
    let root = dir.path();
    let index = std::fs::read(root.join(".git/index")).unwrap();
    let head = git(root, &["rev-parse", "HEAD"]);
    let regions = super::super::conflict_regions(root).unwrap();
    assert_eq!(regions.len(), 2);
    let read = super::super::resolve("conflict://1 @theirs", root).unwrap();
    assert_eq!(read.content, "theirs 雪\n");
    assert_eq!(read.metadata["hasBase"], true);
    assert_eq!(read.metadata["token"].as_str().unwrap().len(), 64);
    let resolved = super::super::write_conflict_resolution(root, 1, "base").unwrap();
    assert_eq!(resolved.index, 1);
    assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), format!("head\n{BLOCK}middle\nbase\ntail"));
    assert_eq!(super::super::conflict_regions(root).unwrap().len(), 1);
    assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
    assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
}

#[test]
fn subdirectory_calls_resolve_repository_relative_names_without_double_prefixes() {
    event("subdirectory-scope");
    let dir = fixture("f.txt", BLOCK);
    let nested = dir.path().join("nested/working");
    std::fs::create_dir_all(&nested).unwrap();
    let region = super::super::conflict_regions(&nested).unwrap().remove(0);
    assert_eq!(region.file, "f.txt");
    super::super::write_conflict_resolution(&nested, 0, "ours").unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), "ours 😀\n");
    assert!(!nested.join("f.txt").exists());
}

#[test]
fn inspection_fingerprint_and_transaction_preimage_reject_file_drift() {
    event("snapshot-drift");
    let dir = fixture("f.txt", BLOCK);
    let original = Snapshot::read(dir.path()).unwrap();
    let next = format!("new prefix\n{BLOCK}");
    std::fs::write(dir.path().join("f.txt"), &next).unwrap();
    let current = Snapshot::read(dir.path()).unwrap();
    assert_ne!(original.regions[0].token, current.regions[0].token);
    assert!(original.apply(&[(0, "ours")]).unwrap_err().to_string().contains("STALE"));
    assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), next);
}

#[test]
fn malformed_tail_and_absent_base_never_allow_partial_resolution() {
    event("invalid-marker-admission");
    let text = format!("{BLOCK}<<<<<<< truncated\n");
    let dir = fixture("f.txt", &text);
    assert!(super::super::write_conflict_resolution(dir.path(), 0, "ours").is_err());
    assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), text);
    let text = "<<<<<<< a\nours\n=======\ntheirs\n>>>>>>> b\n";
    std::fs::write(dir.path().join("f.txt"), text).unwrap();
    assert!(super::super::write_conflict_resolution(dir.path(), 0, "base").is_err());
    assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), text);
    assert!(super::super::resolve("conflict://0 @typo", dir.path()).is_err());
}

#[cfg(unix)]
#[test]
fn unusual_names_and_executable_mode_survive_resolution_but_links_do_not() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    event("file-identity");
    let name = "space\tline\n雪.txt";
    let dir = fixture(name, BLOCK);
    let path = dir.path().join(name);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    super::super::write_conflict_resolution(dir.path(), 0, "theirs").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs 雪\n");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o755);
    std::fs::remove_file(&path).unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), BLOCK).unwrap();
    symlink(outside.path(), &path).unwrap();
    assert!(super::super::write_conflict_resolution(dir.path(), 0, "ours").is_err());
    assert_eq!(std::fs::read_to_string(outside.path()).unwrap(), BLOCK);
}

#[test]
fn source_bounds_and_invalid_utf8_fail_before_marker_interpretation() {
    event("file-bounds");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("binary"), [0xff]).unwrap();
    assert!(io::read(dir.path(), Path::new("binary")).is_err());
    std::fs::File::create(dir.path().join("large")).unwrap().set_len(io::FILE_BYTES + 1).unwrap();
    assert!(io::read(dir.path(), Path::new("large")).is_err());
    assert!(io::read(dir.path(), Path::new("../outside")).is_err());
    assert!(io::read(dir.path(), Path::new("/")).is_err());
}
