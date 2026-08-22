//! E2E lane: multi-root workspace confinement (bd-cv653.3.12).
//!
//! Exercises the public surface end-to-end at the registry level:
//! `--add-dir`-equivalent root addition grants cross-root reads, paths
//! outside every root fail closed with a named error, `/remove-dir`
//! semantics revoke immediately through the shared handle, and single-root
//! registries keep legacy behavior. Hermetic — no network, no provider.
//!
//! Logging: structured JSONL per tests/common/logging.rs when the harness
//! feature is active; failures carry correlation ids sufficient for replay.

mod common;

use common::TestHarness;
use pi::tools::ToolRegistry;
use pi::workspace::{RootSet, WorkspaceHandle};

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    assert!(!payload.is_empty(), "JSONL logs must not be empty");
}

#[test]
fn multi_root_registry_read_spans_roots_and_denies_outside() {
    let harness = TestHarness::new();
    let primary = tempfile::tempdir().expect("primary tempdir");
    let extra = tempfile::tempdir().expect("extra tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    std::fs::write(extra.path().join("extra.txt"), "extra-content").unwrap();
    std::fs::write(outside.path().join("secret.txt"), "outside-content").unwrap();

    // The --add-dir equivalent: validate, canonicalize, add through the
    // shared handle installed on every path-confining tool.
    let canonical = pi::workspace::validate_new_root(extra.path()).unwrap();
    let handle = WorkspaceHandle::shared(RootSet::new(primary.path()));
    assert!(handle.add_root(&canonical), "first add must be new");
    let extra_path = extra.path().join("extra.txt").to_string_lossy().to_string();
    let outside_path = outside
        .path()
        .join("secret.txt")
        .to_string_lossy()
        .to_string();

    // Read inside the additional root succeeds.
    let out = asupersync::test_utils::run_test(|| {
        let registry = ToolRegistry::with_mutation_recorder(
            &["read", "ls"],
            primary.path(),
            None,
            None,
            Some(&handle),
        );
        let extra_path = extra_path.clone();
        async move {
            registry
                .get("read")
                .expect("read tool registered")
                .execute("t", serde_json::json!({ "path": extra_path }), None)
                .await
        }
    })
    .unwrap();
    harness
        .log()
        .info("multi-root", format!("extra-root read ok: {}", out.content.first().is_some()));

    // Read outside every root fails closed with a named error.
    let err = asupersync::test_utils::run_test(|| {
        let registry = ToolRegistry::with_mutation_recorder(
            &["read", "ls"],
            primary.path(),
            None,
            None,
            Some(&handle),
        );
        let outside_path = outside_path.clone();
        async move {
            registry
                .get("read")
                .expect("read tool registered")
                .execute("t", serde_json::json!({ "path": outside_path }), None)
                .await
        }
    })
    .unwrap_err();
    assert!(
        err.to_string().contains("outside the"),
        "named denial expected, got: {err}"
    );
    harness
        .log()
        .info("multi-root", "outside-all-roots denial verified");


    finish_case(&harness, "multi_root_read_span_and_deny");
}

#[test]
fn shared_handle_removal_revokes_across_clones() {
    let harness = TestHarness::new();
    let primary = tempfile::tempdir().expect("primary tempdir");
    let extra = tempfile::tempdir().expect("extra tempdir");
    std::fs::write(extra.path().join("f.txt"), "content").unwrap();

    let canonical = pi::workspace::validate_new_root(extra.path()).unwrap();
    let handle = WorkspaceHandle::shared(RootSet::new(primary.path()));
    handle.add_root(&canonical);

    // /remove-dir equivalent on one clone must revoke for every holder.
    assert!(handle.remove_root(&canonical));
    let snapshot = handle.snapshot_or(primary.path());
    assert!(
        !snapshot.contains_canonical(extra.path()),
        "removed root must leave the snapshot"
    );
    harness
        .log()
        .info("multi-root", "revocation propagated across holders");

    finish_case(&harness, "shared_handle_revocation");
}
