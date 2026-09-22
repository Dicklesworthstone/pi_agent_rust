//! Hub admission and cancellation are execution authority, not best-effort logging.
//!
//! Each OS child must have a registered owner before launch. A killed starting
//! entry cannot be resurrected by a later spawn callback, and a running child
//! observes hub cancellation at the same checkpoints as parent cancellation.
//! Cleanup may recover a poisoned mutex to retire an existing lease, but must
//! not clear the poison or authorize new work from potentially damaged state.

use super::{SubagentResult, SubagentStatus, cancel};
use crate::agent_hub::{AgentHubRegistry, ChildEntry, ChildKind, ChildStatus, registry};
use crate::error::{Error, Result};
use std::sync::Mutex;

const OPERATOR_KILLED: &str = "Child was killed by the operator.";
const HUB_CANCELLED: &str = "Child was cancelled through the agent hub.";
const HUB_POISONED: &str = "PI_SUBAGENT_HUB: registry is poisoned; refusing child execution";
const HUB_MISSING: &str = "PI_SUBAGENT_HUB: registered child ownership was lost";
const HUB_SETTLED: &str = "PI_SUBAGENT_HUB: child was already settled before result acceptance";
const HUB_ACTIVATED: &str = "PI_SUBAGENT_HUB: child lease already activated";

fn register_in(
    hub: &Mutex<AgentHubRegistry>,
    name: &str,
    task: &str,
    kind: ChildKind,
) -> Result<ChildEntry> {
    let mut hub = hub
        .lock()
        .map_err(|_| Error::tool("subagent", HUB_POISONED))?;
    hub.register_kind(name, task, kind).map_err(|error| {
        Error::tool(
            "subagent",
            format!("PI_SUBAGENT_HUB: cannot register child: {error}"),
        )
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Admission {
    Live,
    Cancelled(&'static str),
    Refused(&'static str),
}

fn disposition(status: Option<ChildStatus>) -> Admission {
    match status {
        Some(ChildStatus::Starting | ChildStatus::Running) => Admission::Live,
        Some(ChildStatus::Killed) => Admission::Cancelled(OPERATOR_KILLED),
        Some(ChildStatus::Cancelled) => Admission::Cancelled(HUB_CANCELLED),
        Some(ChildStatus::Done | ChildStatus::Failed) => Admission::Refused(HUB_SETTLED),
        None => Admission::Refused(HUB_MISSING),
    }
}

fn admission(hub: &Mutex<AgentHubRegistry>, id: &str) -> Admission {
    let Ok(hub) = hub.lock() else {
        return Admission::Refused(HUB_POISONED);
    };
    disposition(hub.get(id).map(|entry| entry.status))
}

fn activate_in(hub: &Mutex<AgentHubRegistry>, id: &str, pid: u32) -> Admission {
    let Ok(mut hub) = hub.lock() else {
        return Admission::Refused(HUB_POISONED);
    };
    match hub.get(id).map(|entry| entry.status) {
        Some(ChildStatus::Starting) => {
            hub.mark_running(id, pid);
            Admission::Live
        }
        Some(ChildStatus::Running) => Admission::Refused(HUB_ACTIVATED),
        status => disposition(status),
    }
}

fn apply_admission(result: &mut SubagentResult, admission: Admission) -> bool {
    match admission {
        Admission::Live => {}
        Admission::Cancelled(reason) => cancel(result, reason),
        Admission::Refused(reason) if !result.is_error => result.fail(reason.to_string()),
        Admission::Refused(_) => {}
    }
    !result.is_error
}

/// A missing id is legitimate only before registration (input validation and
/// setup errors). The runner assigns it before any process may be spawned.
pub(super) fn checkpoint(result: &mut SubagentResult) -> bool {
    if let Some(id) = &result.hub_id {
        let admission = admission(registry(), id);
        return apply_admission(result, admission);
    }
    !result.is_error
}

fn settle_in(hub: &Mutex<AgentHubRegistry>, id: &str, status: ChildStatus) {
    // AgentHubRegistry::settle latches the first terminal outcome, so this
    // cannot turn Killed into Done or overwrite another terminal disposition.
    hub.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .settle(id, status);
}

/// Independent of process ownership: cancellation can drop a future before
/// spawn or after process exit but before result acceptance and writeback.
pub(super) struct HubLease {
    id: Option<String>,
}

impl HubLease {
    pub(super) const fn empty() -> Self {
        Self { id: None }
    }

    pub(super) fn register(
        &mut self,
        name: &str,
        task: &str,
        kind: ChildKind,
    ) -> Result<ChildEntry> {
        if self.id.is_some() {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_HUB: child lease already registered",
            ));
        }
        let entry = register_in(registry(), name, task, kind)?;
        self.id = Some(entry.id.clone());
        Ok(entry)
    }

    /// Serialize Starting -> Running with operator kill under the same hub
    /// lock. A late OS spawn cannot silently fail activation and keep running.
    pub(super) fn mark_running(&self, pid: u32, result: &mut SubagentResult) -> bool {
        if result.is_error {
            return false;
        }
        let admission = self
            .id
            .as_deref()
            .map_or(Admission::Refused(HUB_MISSING), |id| {
                activate_in(registry(), id, pid)
            });
        if !apply_admission(result, admission) {
            return false;
        }
        result.status = SubagentStatus::Running;
        true
    }

    pub(super) fn settle(&mut self, result: &SubagentResult) {
        if let Some(id) = self.id.take() {
            let status = match result.status {
                SubagentStatus::Cancelled => ChildStatus::Cancelled,
                SubagentStatus::Completed if !result.is_error => ChildStatus::Done,
                _ => ChildStatus::Failed,
            };
            settle_in(registry(), &id, status);
        }
    }
}

impl Drop for HubLease {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            settle_in(registry(), &id, ChildStatus::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_hub(dir: &std::path::Path) -> Mutex<AgentHubRegistry> {
        let mut hub = AgentHubRegistry::default();
        hub.set_dir_for_tests(dir.to_path_buf());
        Mutex::new(hub)
    }

    #[test]
    fn failed_registration_installs_no_entry_and_does_not_spend_a_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_directory = dir.path().join("file");
        std::fs::write(&not_a_directory, b"not a directory").unwrap();
        let hub = local_hub(&not_a_directory.join("children"));
        let error =
            register_in(&hub, "worker", "private assignment", ChildKind::Subagent).unwrap_err();
        assert!(error.to_string().contains("PI_SUBAGENT_HUB"));
        assert!(!error.to_string().contains("private assignment"));
        assert!(hub.lock().unwrap().roster().is_empty());
        hub.lock()
            .unwrap()
            .set_dir_for_tests(dir.path().join("valid"));
        let child = register_in(&hub, "worker", "assignment", ChildKind::Subagent).unwrap();
        assert_eq!(child.id, "worker-1");
        assert_eq!(admission(&hub, &child.id), Admission::Live);
    }

    #[test]
    fn only_starting_and_running_entries_authorize_execution() {
        let dir = tempfile::tempdir().unwrap();
        let hub = local_hub(dir.path());
        for terminal in [
            ChildStatus::Killed,
            ChildStatus::Cancelled,
            ChildStatus::Done,
            ChildStatus::Failed,
        ] {
            let child = register_in(&hub, "worker", "assignment", ChildKind::Subagent).unwrap();
            assert_eq!(admission(&hub, &child.id), Admission::Live);
            hub.lock().unwrap().mark_running(&child.id, 123);
            assert_eq!(admission(&hub, &child.id), Admission::Live);
            settle_in(&hub, &child.id, terminal);
            let expected = match terminal {
                ChildStatus::Killed => Admission::Cancelled(OPERATOR_KILLED),
                ChildStatus::Cancelled => Admission::Cancelled(HUB_CANCELLED),
                _ => Admission::Refused(HUB_SETTLED),
            };
            assert_eq!(admission(&hub, &child.id), expected);
            hub.lock().unwrap().mark_running(&child.id, 456);
            settle_in(&hub, &child.id, ChildStatus::Done);
            assert_eq!(
                admission(&hub, &child.id),
                expected,
                "terminal state was resurrected"
            );
        }
        assert_eq!(admission(&hub, "missing"), Admission::Refused(HUB_MISSING));
    }

    #[test]
    fn poisoned_registry_refuses_new_work_but_existing_leases_can_be_retired() {
        let dir = tempfile::tempdir().unwrap();
        let hub = local_hub(dir.path());
        let child = register_in(&hub, "worker", "assignment", ChildKind::Tan).unwrap();
        let poison = std::panic::catch_unwind(|| {
            let _guard = hub.lock().unwrap();
            panic!("injected hub panic");
        });
        assert!(poison.is_err());
        assert_eq!(admission(&hub, &child.id), Admission::Refused(HUB_POISONED));
        assert!(register_in(&hub, "other", "assignment", ChildKind::Tan).is_err());
        settle_in(&hub, &child.id, ChildStatus::Cancelled);
        let guard = hub
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(guard.get(&child.id).unwrap().status, ChildStatus::Cancelled);
        drop(guard);
        assert!(
            hub.is_poisoned(),
            "cleanup must not silently heal admission authority"
        );
    }

    #[test]
    fn killing_one_registered_child_does_not_revoke_its_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let hub = local_hub(dir.path());
        let first = register_in(&hub, "worker", "first", ChildKind::Subagent).unwrap();
        let second = register_in(&hub, "worker", "second", ChildKind::Subagent).unwrap();
        hub.lock().unwrap().mark_killed(&first.id);
        assert_eq!(
            admission(&hub, &first.id),
            Admission::Cancelled(OPERATOR_KILLED)
        );
        assert_eq!(admission(&hub, &second.id), Admission::Live);
    }

    #[test]
    fn activation_is_once_only_and_cannot_resurrect_a_killed_start() {
        let dir = tempfile::tempdir().unwrap();
        let hub = local_hub(dir.path());
        let killed = register_in(&hub, "worker", "first", ChildKind::Subagent).unwrap();
        hub.lock().unwrap().mark_killed(&killed.id);
        assert_eq!(
            activate_in(&hub, &killed.id, 123),
            Admission::Cancelled(OPERATOR_KILLED)
        );
        assert!(hub.lock().unwrap().get(&killed.id).unwrap().pid.is_none());
        let live = register_in(&hub, "worker", "second", ChildKind::Subagent).unwrap();
        assert_eq!(activate_in(&hub, &live.id, 456), Admission::Live);
        assert_eq!(
            activate_in(&hub, &live.id, 789),
            Admission::Refused(HUB_ACTIVATED)
        );
        assert_eq!(hub.lock().unwrap().get(&live.id).unwrap().pid, Some(456));
    }

    #[cfg(unix)]
    mod processes {
        use super::super::super::{ChildRunner, Deadline, UpdateCallback};
        use super::*;
        use crate::subagents::SubagentTask;
        use serde_json::json;
        use std::collections::BTreeMap;
        use std::os::unix::fs::PermissionsExt as _;
        use std::sync::Arc;
        use std::time::Duration;

        fn fixture(script: &str) -> (tempfile::TempDir, ChildRunner) {
            let dir = tempfile::tempdir().unwrap();
            let child = dir.path().join("child.sh");
            std::fs::write(&child, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
            let deadline = Deadline::for_request(Some(Duration::from_secs(5)), None).unwrap();
            let runner = ChildRunner::new(
                dir.path().to_path_buf(),
                dir.path().join("global"),
                child,
                None,
                ChildKind::Subagent,
                deadline,
            );
            (dir, runner)
        }

        fn task(schema: bool) -> SubagentTask {
            let mut value = json!({
                "agent":"tan",
                "task":format!("ownership-fixture-{}", uuid::Uuid::new_v4())
            });
            if schema {
                value["outputSchema"] = json!({"type":"object"});
            }
            serde_json::from_value(value).unwrap()
        }

        fn kill_on(status: &'static str, output: Option<&'static str>) -> UpdateCallback {
            Arc::new(move |update| {
                let Some(result) = update.details.as_ref().and_then(|v| v.get("result")) else {
                    return;
                };
                if result["status"] == status
                    && output.is_none_or(|expected| result["output"] == expected)
                {
                    // Only latch the control request. The runner itself must
                    // stop/reap the process; this callback sends no OS signal.
                    // hub_id is not in the progress schema. Resolve this
                    // test's unique assignment through the real hub roster.
                    let mut hub = registry().lock().unwrap();
                    let entry = hub
                        .roster()
                        .into_iter()
                        .find(|entry| {
                            result["task"].as_str() == Some(entry.task.as_str())
                                && result["agent"].as_str() == Some(entry.name.as_str())
                        })
                        .expect("progress must have a registered owner");
                    hub.mark_killed(&entry.id);
                }
            })
        }

        fn assert_killed(result: &SubagentResult) {
            assert!(matches!(result.status, SubagentStatus::Cancelled));
            assert!(result.is_error);
            assert_eq!(result.error.as_deref(), Some(OPERATOR_KILLED));
            let id = result.hub_id.as_deref().unwrap();
            assert_eq!(
                registry().lock().unwrap().get(id).unwrap().status,
                ChildStatus::Killed
            );
            if let Some(pid) = result.pid {
                let status = std::process::Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .unwrap();
                assert!(!status.success(), "child was not reaped");
            }
        }

        #[test]
        fn operator_kill_at_registration_prevents_os_spawn() {
            let (dir, runner) = fixture("printf launched > sentinel\nexec sleep 30");
            let agents =
                BTreeMap::from([("tan".to_string(), crate::subagents::tan_agent_definition())]);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let result = runtime.block_on(runner.run_one(
                &agents,
                task(false),
                None,
                Some(kill_on("starting", None)),
            ));
            assert_killed(&result);
            assert!(result.pid.is_none(), "a killed starting child was spawned");
            assert!(!dir.path().join("sentinel").exists());
        }

        #[test]
        fn operator_kill_at_running_reaps_before_the_next_async_wait() {
            let (_dir, runner) = fixture("exec sleep 30");
            let agents =
                BTreeMap::from([("tan".to_string(), crate::subagents::tan_agent_definition())]);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let result = runtime.block_on(async {
                let mut run = Box::pin(runner.run_one(
                    &agents,
                    task(false),
                    None,
                    Some(kill_on("running", None)),
                ));
                match futures::poll!(&mut run) {
                    std::task::Poll::Ready(result) => result,
                    std::task::Poll::Pending => {
                        panic!("hub kill waited on a live child instead of reaping")
                    }
                }
            });
            assert!(result.pid.is_some(), "test must reach the actual OS spawn");
            assert_killed(&result);
        }

        #[test]
        fn a_kill_from_streaming_progress_stops_later_frames_and_result_acceptance() {
            let script = concat!(
                "printf '%s\\n' '{\"type\":\"message_update\",\"assistantMessageEvent\":{\"type\":\"text_delta\",\"delta\":\"partial\"}}'\n",
                "printf '%s\\n' '{\"type\":\"message_update\",\"assistantMessageEvent\":{\"type\":\"text_delta\",\"delta\":\" forbidden tail\"}}'\n",
                "exec sleep 30"
            );
            let (_dir, runner) = fixture(script);
            let agents =
                BTreeMap::from([("tan".to_string(), crate::subagents::tan_agent_definition())]);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let result = runtime.block_on(runner.run_one(
                &agents,
                task(false),
                None,
                Some(kill_on("running", Some("partial"))),
            ));
            assert_killed(&result);
            assert_eq!(
                result.output, "partial",
                "post-kill output was still consumed"
            );
        }

        #[test]
        fn killed_schema_failure_cannot_launch_a_corrective_child() {
            let script = concat!(
                "printf 'launch\\n' >> launches\n",
                "printf '%s\\n' '{\"type\":\"agent_end\",\"messages\":[{\"role\":\"assistant\",\"stopReason\":\"stop\",\"content\":[{\"type\":\"text\",\"text\":\"not JSON\"}]}]}'\n"
            );
            let (dir, runner) = fixture(script);
            let agents =
                BTreeMap::from([("tan".to_string(), crate::subagents::tan_agent_definition())]);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let result = runtime.block_on(runner.run_one(
                &agents,
                task(true),
                None,
                Some(kill_on("running", Some("not JSON"))),
            ));
            assert_killed(&result);
            assert_eq!(
                std::fs::read_to_string(dir.path().join("launches")).unwrap(),
                "launch\n"
            );
            assert_ne!(result.schema_retries, Some(1));
        }

        #[test]
        fn killed_isolated_child_preserves_edits_without_applying_them_to_parent() {
            let script = concat!(
                "printf 'child edit\\n' > tracked.txt\n",
                "printf '%s\\n' '{\"type\":\"agent_end\",\"messages\":[{\"role\":\"assistant\",\"stopReason\":\"stop\",\"content\":[{\"type\":\"text\",\"text\":\"finished edit\"}]}]}'\n"
            );
            let (dir, runner) = fixture(script);
            std::fs::write(dir.path().join("tracked.txt"), "parent original\n").unwrap();
            for args in [
                vec!["init", "--quiet"],
                vec!["add", "tracked.txt"],
                vec![
                    "-c",
                    "user.name=Pi Test",
                    "-c",
                    "user.email=pi@example.invalid",
                    "-c",
                    "commit.gpgSign=false",
                    "commit",
                    "--quiet",
                    "-m",
                    "fixture",
                ],
            ] {
                assert!(
                    std::process::Command::new("git")
                        .args(args)
                        .current_dir(dir.path())
                        .status()
                        .unwrap()
                        .success()
                );
            }
            let agents =
                BTreeMap::from([("tan".to_string(), crate::subagents::tan_agent_definition())]);
            let mut request = task(false);
            request.isolation = Some("worktree".to_string());
            request.iso_apply = Some("apply".to_string());
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let result = runtime.block_on(runner.run_one(
                &agents,
                request,
                None,
                Some(kill_on("running", Some("finished edit"))),
            ));
            assert_killed(&result);
            assert_eq!(
                std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
                "parent original\n"
            );
            let iso = result
                .iso
                .as_ref()
                .expect("cancelled worktree must remain inspectable");
            assert!(!iso.applied);
            assert_eq!(iso.apply_mode, "keep");
            let retained = std::path::Path::new(&iso.worktree_path).join("tracked.txt");
            assert_eq!(std::fs::read_to_string(retained).unwrap(), "child edit\n");
        }
    }
}
