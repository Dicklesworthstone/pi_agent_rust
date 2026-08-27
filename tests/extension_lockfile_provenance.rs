//! Extension lockfile and provenance verification integration tests (bd-3br2a, SEC-2.2).
//!
//! Tests the deterministic lockfile format, digest computation, trust state
//! transitions, and fail-closed verification semantics.

mod common;

use pi::package_manager::{
    PACKAGE_LOCK_SCHEMA, PACKAGE_TRUST_AUDIT_SCHEMA, PackageEntryTrustState, PackageLockAction,
    PackageLockEntry, PackageLockfile, PackageResolvedProvenance, PackageSourceKind,
    digest_package_path, evaluate_lock_transition, read_package_lockfile, sort_lock_entries,
    write_package_lockfile_atomic,
};

// ============================================================================
// Helpers
// ============================================================================

fn npm_entry(name: &str, version: &str, digest: &str) -> PackageLockEntry {
    PackageLockEntry {
        identity: format!("npm:{name}"),
        source: format!("npm:{name}@{version}"),
        source_kind: PackageSourceKind::Npm,
        resolved: PackageResolvedProvenance::Npm {
            name: name.to_string(),
            requested_spec: format!("npm:{name}@{version}"),
            requested_version: Some(version.to_string()),
            installed_version: version.to_string(),
            pinned: true,
        },
        digest_sha256: digest.to_string(),
        trust_state: PackageEntryTrustState::Trusted,
    }
}

fn git_entry(repo: &str, commit: &str, digest: &str) -> PackageLockEntry {
    PackageLockEntry {
        identity: format!("git:{repo}"),
        source: format!("git:github.com/{repo}"),
        source_kind: PackageSourceKind::Git,
        resolved: PackageResolvedProvenance::Git {
            repo: repo.to_string(),
            host: "github.com".to_string(),
            path: ".".to_string(),
            requested_ref: Some("main".to_string()),
            resolved_commit: commit.to_string(),
            origin_url: Some(format!("https://github.com/{repo}.git")),
            pinned: false,
        },
        digest_sha256: digest.to_string(),
        trust_state: PackageEntryTrustState::Trusted,
    }
}

fn local_entry(path: &str, digest: &str) -> PackageLockEntry {
    PackageLockEntry {
        identity: format!("local:{path}"),
        source: path.to_string(),
        source_kind: PackageSourceKind::Local,
        resolved: PackageResolvedProvenance::Local {
            resolved_path: path.to_string(),
        },
        digest_sha256: digest.to_string(),
        trust_state: PackageEntryTrustState::Trusted,
    }
}

// ============================================================================
// Lockfile schema
// ============================================================================

#[test]
fn lockfile_schema_version_is_stable() {
    assert_eq!(PACKAGE_LOCK_SCHEMA, "pi.package_lock.v1");
}

#[test]
fn trust_audit_schema_version_is_stable() {
    assert_eq!(PACKAGE_TRUST_AUDIT_SCHEMA, "pi.package_trust_audit.v1");
}

// ============================================================================
// Lockfile JSON roundtrip
// ============================================================================

#[test]
fn lockfile_json_roundtrip() {
    let lockfile = PackageLockfile {
        schema: PACKAGE_LOCK_SCHEMA.to_string(),
        entries: vec![
            npm_entry("ext-a", "1.0.0", "aaa111"),
            git_entry("owner/repo", "abc123", "bbb222"),
            local_entry("/tmp/ext", "ccc333"),
        ],
    };

    let json = serde_json::to_string_pretty(&lockfile).unwrap();
    let back: PackageLockfile = serde_json::from_str(&json).unwrap();

    assert_eq!(back.schema, PACKAGE_LOCK_SCHEMA);
    assert_eq!(back.entries.len(), 3);
    assert_eq!(back.entries[0].identity, "npm:ext-a");
    assert_eq!(back.entries[1].identity, "git:owner/repo");
    assert_eq!(back.entries[2].identity, "local:/tmp/ext");
}

#[test]
fn lockfile_entry_serde_preserves_provenance_kind() {
    let entry = npm_entry("foo", "2.0.0", "deadbeef");
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains(r#""kind":"npm""#));

    let entry = git_entry("a/b", "sha1", "cafe");
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains(r#""kind":"git""#));

    let entry = local_entry("/x", "face");
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains(r#""kind":"local""#));
}

// ============================================================================
// Lock transition: first seen
// ============================================================================

#[test]
fn first_install_is_trusted() {
    let candidate = npm_entry("new-ext", "1.0.0", "abc123");
    let result = evaluate_lock_transition(None, &candidate, PackageLockAction::Install);
    let plan = result.unwrap();
    assert_eq!(plan.from_state, "untracked");
    assert_eq!(plan.to_state, "trusted");
    assert!(plan.reason_codes.contains(&"first_seen".to_string()));
}

#[test]
fn first_update_is_also_trusted() {
    let candidate = npm_entry("new-ext", "1.0.0", "abc123");
    let result = evaluate_lock_transition(None, &candidate, PackageLockAction::Update);
    let plan = result.unwrap();
    assert_eq!(plan.to_state, "trusted");
    assert!(plan.reason_codes.contains(&"first_seen".to_string()));
}

// ============================================================================
// Lock transition: verified (no changes)
// ============================================================================

#[test]
fn reinstall_same_digest_is_verified() {
    let existing = npm_entry("ext", "1.0.0", "digest1");
    let candidate = npm_entry("ext", "1.0.0", "digest1");
    let result = evaluate_lock_transition(Some(&existing), &candidate, PackageLockAction::Install);
    let plan = result.unwrap();
    assert!(plan.reason_codes.contains(&"verified".to_string()));
    assert_eq!(plan.to_state, "trusted");
}

// ============================================================================
// Lock transition: digest mismatch (fail-closed)
// ============================================================================

#[test]
fn install_digest_mismatch_fails_closed() {
    let existing = npm_entry("ext", "1.0.0", "original_digest");
    let candidate = npm_entry("ext", "1.0.0", "tampered_digest");
    let result = evaluate_lock_transition(Some(&existing), &candidate, PackageLockAction::Install);
    let err = result.unwrap_err();
    assert_eq!(err.code, "digest_mismatch");
    assert!(err.reason.contains("tampered_digest"));
    assert!(err.reason.contains("original_digest"));
    assert_ne!(err.remediation, "");
}

#[test]
fn install_digest_mismatch_includes_remediation() {
    let existing = npm_entry("ext", "1.0.0", "old");
    let candidate = npm_entry("ext", "1.0.0", "new");
    let err = evaluate_lock_transition(Some(&existing), &candidate, PackageLockAction::Install)
        .unwrap_err();
    assert!(
        err.remediation.contains("remove") || err.remediation.contains("install"),
        "remediation should suggest remove+install: {}",
        err.remediation
    );
}

// ============================================================================
// Lock transition: provenance mismatch (fail-closed)
// ============================================================================

#[test]
fn install_source_kind_change_fails_closed() {
    let existing = npm_entry("ext", "1.0.0", "digest1");
    let mut candidate = local_entry("/some/path", "digest1");
    candidate.identity = "npm:ext".to_string(); // Same identity, different source kind.
    let result = evaluate_lock_transition(Some(&existing), &candidate, PackageLockAction::Install);
    let err = result.unwrap_err();
    assert_eq!(err.code, "provenance_mismatch");
}

#[test]
fn install_resolved_provenance_change_fails_closed() {
    let existing = git_entry("owner/repo", "commit_a", "digest1");
    let candidate = git_entry("owner/repo", "commit_b", "digest1");
    let result = evaluate_lock_transition(Some(&existing), &candidate, PackageLockAction::Install);
    // Git is pinned=false, but this is an install (not update), so it should fail.
    // Actually, with pinned=false + Install, allow_lock_entry_update returns false.
    let err = result.unwrap_err();
    assert_eq!(err.code, "provenance_mismatch");
}

// ============================================================================
// Lock transition: update allows changes for unpinned
// ============================================================================

#[test]
fn update_unpinned_git_allows_provenance_change() {
    let existing = git_entry("owner/repo", "commit_a", "digest_old");
    let candidate = git_entry("owner/repo", "commit_b", "digest_new");
    let result = evaluate_lock_transition(Some(&existing), &candidate, PackageLockAction::Update);
    let plan = result.unwrap();
    assert!(
        plan.reason_codes
            .contains(&"provenance_changed".to_string())
    );
    assert!(plan.reason_codes.contains(&"digest_changed".to_string()));
    assert_eq!(plan.to_state, "trusted");
}

#[test]
fn update_pinned_npm_rejects_digest_change() {
    let existing = npm_entry("ext", "1.0.0", "old_digest");
    let candidate = npm_entry("ext", "1.0.0", "new_digest");
    // NPM with exact version is pinned=true, so updates also fail-close.
    let result = evaluate_lock_transition(Some(&existing), &candidate, PackageLockAction::Update);
    let err = result.unwrap_err();
    assert_eq!(err.code, "digest_mismatch");
}

// ============================================================================
// Deterministic digest computation
// ============================================================================

#[test]
fn digest_single_file_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ext.js");
    std::fs::write(&file, "export default function init() {}").unwrap();

    let d1 = digest_package_path(&file).unwrap();
    let d2 = digest_package_path(&file).unwrap();
    assert_eq!(d1, d2);
    assert_eq!(d1.len(), 64); // SHA-256 hex = 64 chars.
}

#[test]
fn digest_directory_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.js"), "console.log('main');").unwrap();
    std::fs::write(root.join("util.js"), "console.log('util');").unwrap();

    let d1 = digest_package_path(root).unwrap();
    let d2 = digest_package_path(root).unwrap();
    assert_eq!(d1, d2);
}

#[test]
fn digest_changes_when_content_changes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ext.js");

    std::fs::write(&file, "version 1").unwrap();
    let d1 = digest_package_path(&file).unwrap();

    std::fs::write(&file, "version 2").unwrap();
    let d2 = digest_package_path(&file).unwrap();

    assert_ne!(d1, d2);
}

#[test]
fn digest_ignores_git_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.js"), "code").unwrap();

    let d_without_git = digest_package_path(root).unwrap();

    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main").unwrap();

    let d_with_git = digest_package_path(root).unwrap();
    assert_eq!(d_without_git, d_with_git);
}

#[test]
fn digest_normalizes_cr() {
    // Use separate directories with the same filename so the digest input
    // (which includes the filename) matches exactly.
    let dir_unix = tempfile::tempdir().unwrap();
    let dir_win = tempfile::tempdir().unwrap();
    let unix = dir_unix.path().join("ext.js");
    let windows = dir_win.path().join("ext.js");

    std::fs::write(&unix, "line1\nline2\n").unwrap();
    std::fs::write(&windows, "line1\r\nline2\r\n").unwrap();

    let d_unix = digest_package_path(&unix).unwrap();
    let d_windows = digest_package_path(&windows).unwrap();
    assert_eq!(
        d_unix, d_windows,
        "CR normalization should produce identical digests"
    );
}

// ============================================================================
// Entry sorting
// ============================================================================

#[test]
fn sort_entries_is_deterministic() {
    let mut entries1 = vec![
        npm_entry("zzz", "1.0.0", "d1"),
        npm_entry("aaa", "1.0.0", "d2"),
        local_entry("/mmm", "d3"),
    ];
    let mut entries2 = vec![
        local_entry("/mmm", "d3"),
        npm_entry("aaa", "1.0.0", "d2"),
        npm_entry("zzz", "1.0.0", "d1"),
    ];

    sort_lock_entries(&mut entries1);
    sort_lock_entries(&mut entries2);

    let ids1: Vec<&str> = entries1.iter().map(|e| e.identity.as_str()).collect();
    let ids2: Vec<&str> = entries2.iter().map(|e| e.identity.as_str()).collect();
    assert_eq!(ids1, ids2);
}

// ============================================================================
// Lockfile read/write roundtrip
// ============================================================================

#[test]
fn lockfile_write_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("packages.lock.json");

    let lockfile = PackageLockfile {
        schema: PACKAGE_LOCK_SCHEMA.to_string(),
        entries: vec![
            npm_entry("ext-a", "1.0.0", "digest_a"),
            git_entry("owner/repo", "commit1", "digest_b"),
        ],
    };

    write_package_lockfile_atomic(&lockfile_path, &lockfile).unwrap();
    let read_back = read_package_lockfile(&lockfile_path).unwrap();

    assert_eq!(read_back.schema, PACKAGE_LOCK_SCHEMA);
    assert_eq!(read_back.entries.len(), 2);
}

#[test]
fn read_nonexistent_lockfile_returns_empty_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("does-not-exist.lock.json");
    let lockfile = read_package_lockfile(&path).unwrap();
    assert_eq!(lockfile.schema, PACKAGE_LOCK_SCHEMA);
    assert_eq!(
        lockfile.entries,
        [] as [pi::package_manager::PackageLockEntry; 0]
    );
}

#[test]
fn lockfile_output_is_byte_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let path1 = dir.path().join("lock1.json");
    let path2 = dir.path().join("lock2.json");

    let lockfile = PackageLockfile {
        schema: PACKAGE_LOCK_SCHEMA.to_string(),
        entries: vec![npm_entry("b", "1.0.0", "d2"), npm_entry("a", "1.0.0", "d1")],
    };

    write_package_lockfile_atomic(&path1, &lockfile).unwrap();
    write_package_lockfile_atomic(&path2, &lockfile).unwrap();

    let bytes1 = std::fs::read(&path1).unwrap();
    let bytes2 = std::fs::read(&path2).unwrap();
    assert_eq!(
        bytes1, bytes2,
        "Identical lockfiles must produce identical bytes"
    );
}

// ============================================================================
// Trust state serde
// ============================================================================

#[test]
fn trust_state_serde_roundtrip() {
    for state in [
        PackageEntryTrustState::Trusted,
        PackageEntryTrustState::Rejected,
    ] {
        let json = serde_json::to_string(&state).unwrap();
        let back: PackageEntryTrustState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, state);
    }
}

#[test]
fn trust_state_serde_names_stable() {
    assert_eq!(
        serde_json::to_string(&PackageEntryTrustState::Trusted).unwrap(),
        "\"trusted\""
    );
    assert_eq!(
        serde_json::to_string(&PackageEntryTrustState::Rejected).unwrap(),
        "\"rejected\""
    );
}

// ============================================================================
// Source kind serde
// ============================================================================

#[test]
fn source_kind_serde_roundtrip() {
    for kind in [
        PackageSourceKind::Npm,
        PackageSourceKind::Git,
        PackageSourceKind::Local,
    ] {
        let json = serde_json::to_string(&kind).unwrap();
        let back: PackageSourceKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }
}

// ============================================================================
// Lock action serde
// ============================================================================

#[test]
fn lock_action_variants_exist() {
    let install = PackageLockAction::Install;
    let update = PackageLockAction::Update;
    assert_ne!(install, update);
}

// ============================================================================
// Mismatch diagnostics
// ============================================================================

#[test]
fn digest_mismatch_includes_both_digests() {
    let existing = npm_entry("ext", "1.0.0", "expected_hash");
    let candidate = npm_entry("ext", "1.0.0", "actual_hash");
    let err = evaluate_lock_transition(Some(&existing), &candidate, PackageLockAction::Install)
        .unwrap_err();
    assert!(
        err.reason.contains("expected_hash"),
        "reason: {}",
        err.reason
    );
    assert!(err.reason.contains("actual_hash"), "reason: {}", err.reason);
}

#[test]
fn provenance_mismatch_identifies_change() {
    let existing = npm_entry("ext", "1.0.0", "d");
    let mut candidate = npm_entry("ext", "1.0.0", "d");
    candidate.source = "npm:ext@2.0.0".to_string(); // Source changed.
    let err = evaluate_lock_transition(Some(&existing), &candidate, PackageLockAction::Install)
        .unwrap_err();
    assert_eq!(err.code, "provenance_mismatch");
    assert_ne!(err.remediation, "");
}

// ============================================================================
// Multiple transitions compose correctly
// ============================================================================

#[test]
fn sequential_install_verify_update_cycle() {
    // Step 1: First install.
    let v1 = git_entry("owner/repo", "aaa", "digest_v1");
    let t1 = evaluate_lock_transition(None, &v1, PackageLockAction::Install).unwrap();
    assert!(t1.reason_codes.contains(&"first_seen".to_string()));

    // Step 2: Re-install same version (verify).
    let t2 = evaluate_lock_transition(Some(&v1), &v1, PackageLockAction::Install).unwrap();
    assert!(t2.reason_codes.contains(&"verified".to_string()));

    // Step 3: Update to new commit (unpinned git allows this).
    let v2 = git_entry("owner/repo", "bbb", "digest_v2");
    let t3 = evaluate_lock_transition(Some(&v1), &v2, PackageLockAction::Update).unwrap();
    assert!(t3.reason_codes.contains(&"provenance_changed".to_string()));
    assert!(t3.reason_codes.contains(&"digest_changed".to_string()));

    // Step 4: Re-install v2 should verify.
    let t4 = evaluate_lock_transition(Some(&v2), &v2, PackageLockAction::Install).unwrap();
    assert!(t4.reason_codes.contains(&"verified".to_string()));

    // Step 5: Tampered install should fail.
    let tampered = git_entry("owner/repo", "bbb", "tampered");
    let t5 = evaluate_lock_transition(Some(&v2), &tampered, PackageLockAction::Install);
    assert!(t5.is_err());
    assert_eq!(t5.unwrap_err().code, "digest_mismatch");
}
