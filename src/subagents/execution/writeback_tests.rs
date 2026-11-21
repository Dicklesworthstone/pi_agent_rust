//! Accepted native children must retain cancellation authority while their
//! isolated patches wait behind another caller's parent writeback.

use super::*;
use crate::agent_hub::{ChildStatus, registry};
use crate::worktree_iso::writeback_tests::HeldApply;
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;

fn fixture() -> (tempfile::TempDir, ChildRunner, SubagentTask) {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repository");
    fs::create_dir(&repo).unwrap();
    fs::write(repo.join("tracked.txt"), "original\n").unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "tracked.txt"],
        vec![
            "-c",
            "user.name=Writeback Fixture",
            "-c",
            "user.email=writeback@localhost",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    ] {
        let output = Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let child = root.path().join("child.sh");
    fs::write(
        &child,
        concat!(
            "#!/bin/sh\n",
            "printf 'child edit\\n' > tracked.txt\n",
            "printf '%s\\n' '{\"type\":\"agent_end\",\"messages\":[{\"role\":\"assistant\",\"stopReason\":\"stop\",\"content\":[{\"type\":\"text\",\"text\":\"finished edit\"}]}]}'\n",
        ),
    )
    .unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();
    let runner = ChildRunner::new(
        repo,
        root.path().join("global"),
        child,
        None,
        ChildKind::Subagent,
        Deadline::for_request(Some(Duration::from_secs(60)), None).unwrap(),
    );
    let task = serde_json::from_value(json!({
        "agent":"tan", "task":format!("writeback-fixture-{}", uuid::Uuid::new_v4()),
        "isolation":"worktree", "isoApply":"apply",
    }))
    .unwrap();
    (root, runner, task)
}

fn identity(pending: &PendingRun) -> (String, PathBuf) {
    let PendingRun::AwaitingWriteback { attempt, .. } = pending else {
        panic!("the native child must finish successfully before writeback is tested");
    };
    assert!(matches!(attempt.result.status, SubagentStatus::Completed));
    assert_eq!(attempt.result.output, "finished edit");
    let id = attempt.result.hub_id.clone().unwrap();
    let hub = registry().lock().unwrap();
    assert_eq!(hub.get(&id).unwrap().status, ChildStatus::Running);
    assert!(
        hub.control_pid(&id).is_none(),
        "child must already be reaped"
    );
    (id, attempt.isolation.as_ref().unwrap().0.path.clone())
}

#[test]
fn parent_cancel_and_hub_kill_reject_queued_native_writeback_and_preserve_edits() {
    for operator_killed in [false, true] {
        let (_root, runner, request) = fixture();
        let agents = BTreeMap::from([("tan".to_string(), super::super::tan_agent_definition())]);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let owner = AgentCx::for_request();
            let pending = owner
                .with_current(runner.run_pending(&agents, request, None, None, None))
                .await;
            let (id, worktree) = identity(&pending);
            let held = HeldApply::new();
            let mut finishing = Box::pin(pending.finish(None));
            assert!(futures::poll!(&mut finishing).is_pending());
            if operator_killed {
                registry().lock().unwrap().mark_killed(&id);
            } else {
                owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel writeback"));
            }
            let result = finishing.await;
            // Cancellation must finish while the unrelated writer still owns
            // the lock; releasing first would conceal a blocked executor.
            held.release();
            assert!(matches!(result.status, SubagentStatus::Cancelled));
            assert!(result.is_error);
            assert_eq!(
                registry().lock().unwrap().get(&id).unwrap().status,
                if operator_killed {
                    ChildStatus::Killed
                } else {
                    ChildStatus::Cancelled
                },
            );
            let iso = result.iso.unwrap();
            assert!(!iso.applied);
            assert_eq!(iso.apply_mode, "keep");
            assert!(iso.conflicted_files.is_empty());
            assert!(iso.patch.contains("child edit"));
            assert_eq!(PathBuf::from(iso.worktree_path), worktree);
            assert_eq!(
                fs::read(runner.cwd.join("tracked.txt")).unwrap(),
                b"original\n"
            );
            assert_eq!(
                fs::read(worktree.join("tracked.txt")).unwrap(),
                b"child edit\n"
            );
        });
    }
}

#[test]
fn dropping_queued_native_writeback_settles_ownership_without_installing_edits() {
    let (_root, runner, request) = fixture();
    let agents = BTreeMap::from([("tan".to_string(), super::super::tan_agent_definition())]);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let pending = runner.run_pending(&agents, request, None, None, None).await;
        let (id, worktree) = identity(&pending);
        let held = HeldApply::new();
        let mut finishing = Box::pin(pending.finish(None));
        assert!(futures::poll!(&mut finishing).is_pending());
        drop(finishing);
        assert_eq!(
            registry().lock().unwrap().get(&id).unwrap().status,
            ChildStatus::Cancelled
        );
        held.release();
        assert_eq!(
            fs::read(runner.cwd.join("tracked.txt")).unwrap(),
            b"original\n"
        );
        assert_eq!(
            fs::read(worktree.join("tracked.txt")).unwrap(),
            b"child edit\n"
        );
    });
}

#[test]
fn live_queued_native_writeback_applies_and_reports_cleanup_after_release() {
    let (_root, runner, request) = fixture();
    let agents = BTreeMap::from([("tan".to_string(), super::super::tan_agent_definition())]);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let pending = runner.run_pending(&agents, request, None, None, None).await;
        let (id, worktree) = identity(&pending);
        let held = HeldApply::new();
        let mut finishing = Box::pin(pending.finish(None));
        assert!(futures::poll!(&mut finishing).is_pending());
        held.release();
        let result = finishing.await;
        assert!(matches!(result.status, SubagentStatus::Completed));
        assert!(!result.is_error, "{:?}", result.error);
        assert_eq!(
            registry().lock().unwrap().get(&id).unwrap().status,
            ChildStatus::Done
        );
        let iso = result.iso.unwrap();
        assert!(iso.applied);
        assert_eq!(iso.apply_mode, "apply");
        assert!(iso.patch.contains("child edit"));
        assert_eq!(
            fs::read(runner.cwd.join("tracked.txt")).unwrap(),
            b"child edit\n"
        );
        assert!(!worktree.exists());
    });
}
