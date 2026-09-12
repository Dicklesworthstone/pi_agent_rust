// The single test here builds a symlinked root, so the whole file is Unix-only.
#[cfg(unix)]
use pi::extensions_js::verify_repair_monotonicity;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;

#[test]
#[cfg(unix)]
fn test_repro_symlink_monotonicity_bug() {
    let tmp_dir = tempfile::tempdir().expect("create temp dir");
    let target_dir = tmp_dir.path().join("target");
    let link_dir = tmp_dir.path().join("link");

    fs::create_dir(&target_dir).expect("create target dir");
    symlink(&target_dir, &link_dir).expect("create symlink");

    let root = link_dir.clone();

    // Test case 1: Existing file under symlinked root
    let existing_file = link_dir.join("existing.js");
    // We must create it in the target for it to exist via link
    let real_existing_file = target_dir.join("existing.js");
    fs::write(&real_existing_file, "content").expect("write file");

    // VERIFY: Existing file passes monotonicity check
    let verdict = verify_repair_monotonicity(&root, &root, &existing_file);
    assert!(
        verdict.is_safe(),
        "Existing file under symlink should be safe. Verdict: {verdict:?}",
    );

    // Test case 2: Non-existent file under symlinked root (e.g. stub injection)
    let new_file = link_dir.join("new_stub.js");

    // VERIFY: Non-existent file fails monotonicity check due to canonicalization mismatch
    let verdict = verify_repair_monotonicity(&root, &root, &new_file);

    // This assertion CONFIRMS the bug fix. It should now be Safe.
    assert!(
        verdict.is_safe(),
        "Non-existent file under symlink SHOULD pass monotonicity check with fix. Verdict: {verdict:?}"
    );
}

#[test]
#[cfg(not(unix))]
fn test_repro_symlink_monotonicity_bug_skipped() {
    // Skip on non-unix platforms
}
