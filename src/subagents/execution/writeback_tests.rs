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

fn gated_schema_repair_fixture() -> (tempfile::TempDir, ChildRunner, SubagentTask) {
    let (root, runner, mut task) = fixture();
    fs::create_dir_all(&runner.global_dir).unwrap();
    fs::write(
        &runner.child_binary,
        r#"#!/bin/sh
set -eu
repair=false
for argument in "$@"; do
    if [ "$argument" = "--no-tools" ]; then repair=true; fi
done
if [ "$repair" = true ]; then
    printf '%s\n' "$$" > "$PI_CODING_AGENT_DIR/repair.pid"
    printf '%s\n' "$PI_SUBAGENT_RUN_ID" > "$PI_CODING_AGENT_DIR/repair.hub"
    : > "$PI_CODING_AGENT_DIR/repair.started"
    while [ ! -f "$PI_CODING_AGENT_DIR/repair.release" ]; do sleep 0.01; done
    printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"{\"verdict\":\"pass\"}"}]}]}'
else
    printf 'child edit\n' > tracked.txt
    pwd > "$PI_CODING_AGENT_DIR/original.worktree"
    printf '%s\n' "$PI_SUBAGENT_RUN_ID" > "$PI_CODING_AGENT_DIR/original.hub"
    printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"completed edit"}]}]}'
fi
"#,
    )
    .unwrap();
    task.output_schema = Some(json!({
        "type":"object", "required":["verdict"],
        "properties":{"verdict":{"type":"string"}},
    }));
    task.schema_mode = SchemaMode::Strict;
    (root, runner, task)
}

#[test]
#[allow(clippy::too_many_lines)]
fn interrupted_schema_repair_reaps_child_and_preserves_unapplied_original_worktree() {
    for interruption in ["parent cancellation", "hub kill", "future drop"] {
        let (_root, runner, request) = gated_schema_repair_fixture();
        let agents = BTreeMap::from([("tan".to_string(), super::super::tan_agent_definition())]);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let owner = AgentCx::for_request();
            let run = runner.run_one(&agents, request, None, None);
            let mut running = Box::pin(owner.with_current(run));
            let ready = runner.global_dir.join("repair.started");
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ready.exists() {
                assert!(
                    futures::poll!(&mut running).is_pending(),
                    "{interruption}: run settled before its schema-only repair started"
                );
                assert!(Instant::now() < deadline, "schema-only repair never started");
                poll_pause(&owner).await;
            }
            let original_id = fs::read_to_string(runner.global_dir.join("original.hub"))
                .unwrap()
                .trim()
                .to_string();
            let repair_id = fs::read_to_string(runner.global_dir.join("repair.hub"))
                .unwrap()
                .trim()
                .to_string();
            assert_eq!(
                repair_id, original_id,
                "repair must retain the original lease"
            );
            let worktree = PathBuf::from(
                fs::read_to_string(runner.global_dir.join("original.worktree"))
                    .unwrap()
                    .trim(),
            );
            let pid = fs::read_to_string(runner.global_dir.join("repair.pid"))
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap();
            {
                let hub = registry().lock().unwrap();
                assert_eq!(
                    hub.get(&original_id).unwrap().status,
                    ChildStatus::Running
                );
                assert_eq!(hub.control_pid(&original_id), Some(pid));
            }
            assert_eq!(
                fs::read(runner.cwd.join("tracked.txt")).unwrap(),
                b"original\n"
            );
            assert_eq!(
                fs::read(worktree.join("tracked.txt")).unwrap(),
                b"child edit\n"
            );
            if interruption == "future drop" {
                drop(running);
            } else {
                if interruption == "hub kill" {
                    registry().lock().unwrap().mark_killed(&original_id);
                } else {
                    owner.cancel_with(
                        asupersync::types::CancelKind::User,
                        Some("cancel schema repair"),
                    );
                }
                let result = running.await;
                assert!(matches!(result.status, SubagentStatus::Cancelled));
                assert!(result.is_error);
                assert!(result.preserved_worktrees.is_empty());
                let isolation = result.iso.unwrap();
                assert_eq!(PathBuf::from(isolation.worktree_path), worktree);
                assert_eq!(isolation.apply_mode, "keep");
                assert!(!isolation.applied);
                assert!(isolation.patch.contains("child edit"));
            }
            {
                let hub = registry().lock().unwrap();
                assert_eq!(
                    hub.get(&original_id).unwrap().status,
                    if interruption == "hub kill" {
                        ChildStatus::Killed
                    } else {
                        ChildStatus::Cancelled
                    },
                );
                assert!(hub.control_pid(&original_id).is_none());
            }
            let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
            assert!(
                rustix::process::test_kill_process(pid).is_err(),
                "{interruption}: repair process survived cleanup"
            );
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
fn isolated_roots_map_into_the_capture_and_disclose_external_and_missing_exclusions() {
    let (root, runner, mut request) = fixture();
    let nested = runner.cwd.join("library");
    let empty = runner.cwd.join("empty-library");
    let external = root.path().join("other-repository");
    for directory in [&nested, &empty, &external] {
        fs::create_dir(directory).unwrap();
    }
    fs::write(nested.join("source.txt"), "captured library\n").unwrap();
    fs::write(external.join("source.txt"), "live external repository\n").unwrap();
    let mut workspace = crate::workspace::WorkspaceHandle::single(&runner.cwd);
    for directory in [
        nested.as_path(),
        external.as_path(),
        empty.as_path(),
        root.path(),
    ] {
        workspace.add_root(directory);
    }
    let privacy = crate::text_completion::AuxiliaryPrivacySnapshot::from_privacy(
        crate::text_completion::AuxiliaryPrivacy::from_settings(None),
    );
    let runner = runner.with_workspace_roots(
        crate::subagents::workspace::CapturedRoots::capture(&workspace, privacy).unwrap(),
    );
    fs::write(
        &runner.child_binary,
        concat!(
            "#!/bin/sh\nset -eu\n",
            "while [ \"$#\" -gt 0 ]; do\n",
            "case \"$1\" in\n",
            "--workspace-roots-file) shift; cat \"$1\" > child-roots.json ;;\n",
            "--append-system-prompt) shift; cat \"$1\" > child-system.txt ;;\n",
            "esac\nshift\ndone\n",
            "printf 'child edit\\n' > tracked.txt\n",
            "printf '%s\\n' '{\"type\":\"agent_end\",\"messages\":[{\"role\":\"assistant\",\"stopReason\":\"stop\",\"content\":[{\"type\":\"text\",\"text\":\"finished edit\"}]}]}'\n",
        ),
    )
    .unwrap();
    request.iso_apply = Some("keep".to_string());
    let agents = BTreeMap::from([("tan".to_string(), super::super::tan_agent_definition())]);
    let result = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
        .block_on(runner.run_one(&agents, request, None, None));
    assert!(!result.is_error, "{:?}", result.error);
    let metadata = serde_json::to_value(result.workspace_roots.as_ref().unwrap()).unwrap();
    let captured = PathBuf::from(&result.iso.as_ref().unwrap().worktree_path);
    let mapped = crate::extensions::safe_canonicalize(&captured.join("library"));
    let source = crate::extensions::safe_canonicalize(&nested);
    assert_eq!(metadata["inherited"], json!([]));
    assert_eq!(metadata["mapped"], json!([{"source":source,"path":mapped}]));
    let excluded = metadata["excluded"].as_array().unwrap();
    assert_eq!(excluded.len(), 3);
    assert!(excluded.iter().any(|entry| {
        entry["source"] == json!(crate::extensions::safe_canonicalize(&empty))
            && entry["reason"] == "absent_from_captured_workspace"
    }));
    assert_eq!(
        excluded
            .iter()
            .filter(|entry| entry["reason"] == "outside_isolated_repository")
            .count(),
        2
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(captured.join("child-roots.json")).unwrap()).unwrap();
    assert_eq!(manifest["roots"], json!([mapped]));
    let mut child_workspace = crate::workspace::WorkspaceHandle::single(&captured);
    child_workspace
        .load_child_workspace_roots(&captured.join("child-roots.json"))
        .unwrap();
    assert!(child_workspace.is_within(&captured.join("library/source.txt")));
    assert!(!child_workspace.is_within(&nested.join("source.txt")));
    assert!(!child_workspace.is_within(&external.join("source.txt")));
    let prompt = fs::read_to_string(captured.join("child-system.txt")).unwrap();
    assert!(prompt.contains("Delegated workspace access"));
    assert!(prompt.contains("outside_isolated_repository"));
    assert_eq!(
        fs::read(runner.cwd.join("tracked.txt")).unwrap(),
        b"original\n"
    );
}

#[test]
fn revoked_and_regranted_root_cannot_authorize_queued_isolated_writeback() {
    let (_root, runner, request) = fixture();
    let nested = runner.cwd.join("library");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("source.txt"), "captured library\n").unwrap();
    let mut workspace = crate::workspace::WorkspaceHandle::single(&runner.cwd);
    workspace.add_root(&nested);
    let privacy = crate::text_completion::AuxiliaryPrivacySnapshot::from_privacy(
        crate::text_completion::AuxiliaryPrivacy::from_settings(None),
    );
    let runner = runner.with_workspace_roots(
        crate::subagents::workspace::CapturedRoots::capture(&workspace, privacy).unwrap(),
    );
    let agents = BTreeMap::from([("tan".to_string(), super::super::tan_agent_definition())]);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let pending = runner.run_pending(&agents, request, None, None, None).await;
        let (_, worktree) = identity(&pending);
        let held = HeldApply::new();
        let mut finishing = Box::pin(pending.finish(None));
        assert!(futures::poll!(&mut finishing).is_pending());
        assert!(workspace.remove_root(&nested));
        workspace.add_root(&nested);
        let result = finishing.await;
        held.release();
        assert!(result.is_error);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("PI_SUBAGENT_WORKSPACE_REVOKED")
        );
        let isolation = result.iso.unwrap();
        assert!(!isolation.applied);
        assert_eq!(isolation.apply_mode, "keep");
        assert!(isolation.patch.contains("child edit"));
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
                owner.cancel_with(
                    asupersync::types::CancelKind::User,
                    Some("cancel writeback"),
                );
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
