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

#[cfg(test)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    Live,
    Cancelled(&'static str),
    Refused(&'static str),
}

const fn disposition(status: Option<ChildStatus>) -> Admission {
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
        Admission::Cancelled(reason) => cancel(result, reason),
        Admission::Refused(reason) if !result.is_error => result.fail(reason.to_string()),
        Admission::Live | Admission::Refused(_) => {}
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
    let mut hub = hub
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    hub.settle(id, status);
    hub.finish_native_execution(id);
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
        launch: crate::subagents::RevivalSpec,
        revived_from: Option<&str>,
    ) -> Result<ChildEntry> {
        if self.id.is_some() {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_HUB: child lease already registered",
            ));
        }
        let entry = registry()
            .lock()
            .map_err(|_| Error::tool("subagent", HUB_POISONED))?
            .register_native(name, task, kind, launch, revived_from)
            .map_err(|error| {
                Error::tool(
                    "subagent",
                    format!("PI_SUBAGENT_HUB: cannot register child: {error}"),
                )
            })?;
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

    /// Keep process-control retirement separate from result acceptance. The
    /// OS reaper owns this guard, including on early return or future drop;
    /// a pending ordered writeback retains only its result-acceptance lease.
    pub(super) fn process_lifetime(&self) -> ProcessLease {
        ProcessLease {
            id: self.id.clone(),
        }
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

pub(super) struct ProcessLease {
    id: Option<String>,
}

impl ProcessLease {
    /// Reaping and removing PID authority share the same lock as hub kill.
    /// Never leave a reused PID controllable while asynchronous EOF drainage
    /// or ordered result acceptance is still pending.
    pub(super) fn try_wait(
        &self,
        child: &mut std::process::Child,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        let mut hub = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let status = child.try_wait();
        if matches!(status, Ok(Some(_)))
            && let Some(id) = &self.id
        {
            hub.mark_process_reaped(id);
        }
        status
    }

    pub(super) fn terminate(&self, child: &mut super::ChildProcessGuard) {
        let mut hub = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        child.terminate_untracked();
        if let Some(id) = &self.id {
            hub.mark_process_reaped(id);
        }
    }
}

impl Drop for ProcessLease {
    fn drop(&mut self) {
        if let Some(id) = &self.id {
            registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .mark_process_reaped(id);
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
        use super::super::super::{ChildProcessGuard, ChildRunner, Deadline, UpdateCallback};
        use super::*;
        use crate::agent_cx::AgentCx;
        use crate::subagents::SubagentTask;
        use serde_json::json;
        use std::collections::BTreeMap;
        use std::os::unix::fs::PermissionsExt as _;
        use std::os::unix::process::CommandExt as _;
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
        fn dropping_a_process_guard_serializes_reaping_with_pid_retirement() {
            for operator_killed in [false, true] {
                let mut command = std::process::Command::new("sh");
                command
                    .args(["-c", "exec sleep 30"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .process_group(0);
                let child = ChildProcessGuard::spawn(&AgentCx::for_request(), &mut command)
                    .expect("spawn owned child");
                let pid = child.id();
                let os_pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
                let mut hub = registry().lock().unwrap();
                let entry = hub
                    .register_kind("drop-lease", "serialized process drop", ChildKind::Subagent)
                    .unwrap();
                hub.mark_running(&entry.id, pid);
                let child = child.with_process_lifetime(ProcessLease {
                    id: Some(entry.id.clone()),
                });
                let (starting, started) = std::sync::mpsc::channel();
                let (finishing, finished) = std::sync::mpsc::channel();
                let reaper = std::thread::spawn(move || {
                    starting.send(()).unwrap();
                    drop(child);
                    finishing.send(()).unwrap();
                });
                let drop_started = started.recv_timeout(Duration::from_secs(5)).is_ok();
                let completed_while_locked =
                    finished.recv_timeout(Duration::from_millis(100)).is_ok();
                // Waiting for the hub lock must happen before the actual reap,
                // not in a separate lease destructor after the PID is freed.
                let alive_while_locked = rustix::process::test_kill_process(os_pid).is_ok();
                let authority_while_locked = hub.control_pid(&entry.id);
                if operator_killed {
                    hub.mark_killed(&entry.id);
                }
                drop(hub);
                reaper.join().unwrap();

                let (authority_after_drop, history) = {
                    let mut hub = registry().lock().unwrap();
                    let authority = hub.control_pid(&entry.id);
                    let history = hub.get(&entry.id).unwrap().clone();
                    hub.settle(&entry.id, ChildStatus::Cancelled);
                    (authority, history)
                };
                assert!(drop_started, "the reaper thread must reach Drop");
                assert!(!completed_while_locked);
                assert!(alive_while_locked, "PID was freed before serialized retirement");
                assert_eq!(authority_while_locked, Some(pid));
                assert_eq!(authority_after_drop, None);
                assert_eq!(history.pid, Some(pid), "keep diagnostic process history");
                assert_eq!(
                    history.status,
                    if operator_killed {
                        ChildStatus::Killed
                    } else {
                        ChildStatus::Running
                    },
                    "process retirement must not settle or resurrect the result"
                );
                assert!(rustix::process::test_kill_process(os_pid).is_err());
            }
        }

        #[test]
        fn dropping_a_pending_runner_reaps_and_retires_its_bound_process_lease() {
            let (_dir, runner) = fixture("exec sleep 30");
            let agents =
                BTreeMap::from([("tan".to_string(), crate::subagents::tan_agent_definition())]);
            let request = task(false);
            let assignment = request.task.clone();
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let mut run = Box::pin(runner.run_one(&agents, request, None, None));
                assert!(futures::poll!(&mut run).is_pending());
                let entry = registry()
                    .lock()
                    .unwrap()
                    .roster()
                    .into_iter()
                    .find(|entry| entry.task == assignment)
                    .expect("pending runner has a registered process");
                let pid = entry.pid.expect("test reaches process activation");
                drop(run);
                let (authority, settled) = {
                    let hub = registry().lock().unwrap();
                    (hub.control_pid(&entry.id), hub.get(&entry.id).unwrap().clone())
                };
                assert_eq!(authority, None);
                assert_eq!(settled.pid, Some(pid));
                assert_eq!(settled.status, ChildStatus::Cancelled);
                let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
                assert!(rustix::process::test_kill_process(pid).is_err());
            });
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
