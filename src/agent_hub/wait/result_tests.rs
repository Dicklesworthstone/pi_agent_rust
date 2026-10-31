//! Real retained-child execution through the native tool and owned jobs runner.

use super::*;
use crate::subagents::SubagentTool;
use crate::text_completion::{AuxiliaryPrivacy, AuxiliaryPrivacySnapshot};
use crate::tools::{Tool, ToolOutput};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt as _;

struct Fixture {
    root: tempfile::TempDir,
    tool: Arc<SubagentTool>,
    owner: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        std::fs::create_dir_all(global.join("agents")).unwrap();
        std::fs::write(
            global.join("agents/worker.md"),
            "---\nname: worker\ndescription: wait fixture\ntools: []\n---\nComplete the task.\n",
        )
        .unwrap();
        let binary = root.path().join("child.sh");
        std::fs::write(
            &binary,
            r#"#!/bin/sh
while IFS= read -r line; do
    id=${line#*\"id\":\"}
    id=${id%%\"*}
    case "$line" in
        *'"type":"get_state"'*)
            printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":false}}\n' "$id"
            ;;
        *'"type":"prompt"'*)
            printf '{"type":"response","id":"%s","command":"prompt","success":true}\n' "$id"
            printf '%s\n' '{"type":"agent_start"}'
            printf '%s' "$$" > running
            while [ ! -f release ]; do sleep 0.02; done
            printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"finished"}]}]}'
            ;;
    esac
done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = format!("result-wait-{}", uuid::Uuid::new_v4());
        let mut tool = SubagentTool::with_paths(root.path().to_path_buf(), global, binary)
            .with_timeout(Duration::from_secs(20));
        tool.bind_job_session_scope(crate::jobs::JobSessionScope::fixed(&owner));
        Self {
            root,
            tool: Arc::new(tool),
            owner,
        }
    }

    async fn call(&self, input: Value) -> Value {
        self.tool
            .execute("result-wait", input, None)
            .await
            .unwrap()
            .details
            .unwrap()
    }

    async fn child(&self) -> String {
        until(|| {
            std::fs::read_to_string(self.root.path().join("running"))
                .ok()
                .and_then(|pid| pid.parse::<u32>().ok())
                .is_some()
        })
        .await;
        let pid: u32 = std::fs::read_to_string(self.root.path().join("running"))
            .unwrap()
            .parse()
            .unwrap();
        crate::agent_hub::registry()
            .lock()
            .unwrap()
            .roster()
            .into_iter()
            .find(|entry| entry.pid == Some(pid))
            .unwrap()
            .id
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(runtime) = asupersync::runtime::RuntimeBuilder::current_thread().build() {
            let _ = runtime.block_on(crate::jobs::kill_session(&self.owner));
        }
    }
}

async fn until(mut ready: impl FnMut() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(started.elapsed() < Duration::from_secs(5));
        AgentCx::for_request()
            .time()
            .sleep(Duration::from_millis(10))
            .await;
    }
}

fn parent_status(child: &str) -> InboxWaitStatus {
    crate::agent_hub::registry()
        .lock()
        .unwrap()
        .wait_snapshot(child, Some("parent"), 0)
        .unwrap()
        .status
}

fn worker_wait(worker: &str) -> Value {
    json!({"operation":"worker_wait", "workerId":worker, "turnId":1, "timeoutMs":5000})
}

#[test]
fn real_worker_and_job_waits_release_on_drop_cancel_and_turn_acceptance() {
    let _guard = crate::jobs::process_test_guard();
    let fixture = Fixture::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let started = fixture
            .call(json!({"operation":"worker_start", "agent":"worker", "task":"wait"}))
            .await;
        let worker = started["workerId"].as_str().unwrap();
        let job = started["jobId"].as_str().unwrap();
        let child = fixture.child().await;
        assert_eq!(parent_status(&child), InboxWaitStatus::Pending);

        let mut waiting = Box::pin(fixture.tool.execute("wait", worker_wait(worker), None));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        assert_eq!(parent_status(&child), InboxWaitStatus::PeerBlocked);
        drop(waiting);
        assert_eq!(parent_status(&child), InboxWaitStatus::Pending);

        let owner = AgentCx::for_request();
        let mut waiting = Box::pin(owner.with_current(fixture.tool.execute(
            "cancel-wait",
            worker_wait(worker),
            None,
        )));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        assert_eq!(parent_status(&child), InboxWaitStatus::PeerBlocked);
        owner.set_cancel_requested(true);
        assert_eq!(parent_status(&child), InboxWaitStatus::Pending);
        assert!(waiting.await.is_err());
        assert_eq!(parent_status(&child), InboxWaitStatus::Pending);

        assert!(
            crate::jobs::wait_async("foreign-owner", job, Duration::from_secs(1))
                .await
                .is_err()
        );
        assert_eq!(parent_status(&child), InboxWaitStatus::Pending);
        let owner = AgentCx::for_request();
        let mut waiting = Box::pin(owner.with_current(crate::jobs::wait_async(
            &fixture.owner,
            job,
            Duration::from_secs(5),
        )));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        assert_eq!(parent_status(&child), InboxWaitStatus::PeerBlocked);
        owner.set_cancel_requested(true);
        assert_eq!(parent_status(&child), InboxWaitStatus::Pending);
        assert!(waiting.await.is_err());
        assert_eq!(parent_status(&child), InboxWaitStatus::Pending);

        let mut waiting = Box::pin(fixture.tool.execute("complete", worker_wait(worker), None));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        assert_eq!(parent_status(&child), InboxWaitStatus::PeerBlocked);
        std::fs::write(fixture.root.path().join("release"), b"continue").unwrap();
        // Deliberately do not poll the old wait while the child result commits.
        until(|| parent_status(&child) == InboxWaitStatus::Pending).await;
        assert!(!waiting.await.unwrap().is_error);
        assert!(
            crate::agent_hub::registry()
                .lock()
                .unwrap()
                .control_pid(&child)
                .is_some(),
            "turn completion must release dependency while the worker stays alive"
        );
        fixture
            .call(json!({"operation":"worker_close", "workerId":worker}))
            .await;
        assert!(
            !crate::agent_hub::registry()
                .lock()
                .unwrap()
                .native_jobs
                .contains_key(&child)
        );
    });
}

#[test]
fn internal_background_worker_wait_does_not_block_the_available_parent() {
    let _guard = crate::jobs::process_test_guard();
    let fixture = Fixture::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let started = fixture
            .call(json!({"operation":"worker_start", "agent":"worker", "task":"wait"}))
            .await;
        let worker = started["workerId"].as_str().unwrap().to_string();
        let child = fixture.child().await;
        let admitted = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&admitted);
        let tool = Arc::clone(&fixture.tool);
        let job_owner = fixture.owner.clone();
        let worker_job = started["jobId"].as_str().unwrap().to_string();
        let native = crate::jobs::spawn_owned_task(
            crate::jobs::admit_owned_task(&fixture.owner).unwrap(),
            "internal result observer",
            AgentCx::for_request(),
            Duration::from_secs(10),
            Some(fixture.root.path()),
            AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::from_settings(None)),
            async move {
                let mut waiting = Box::pin(tool.execute("internal", worker_wait(&worker), None));
                assert!(futures::poll!(waiting.as_mut()).is_pending());
                let mut job_wait = Box::pin(crate::jobs::wait_async(
                    &job_owner,
                    &worker_job,
                    Duration::from_secs(5),
                ));
                assert!(futures::poll!(job_wait.as_mut()).is_pending());
                observed.store(true, Ordering::Release);
                let output = waiting.await;
                drop(job_wait);
                output
            },
        )
        .unwrap();
        until(|| admitted.load(Ordering::Acquire)).await;
        assert_eq!(parent_status(&child), InboxWaitStatus::Pending);
        let worker_job = started["jobId"].as_str().unwrap();
        let mut explicit = Box::pin(crate::jobs::wait_async(
            &fixture.owner,
            worker_job,
            Duration::from_secs(5),
        ));
        assert!(futures::poll!(explicit.as_mut()).is_pending());
        assert_eq!(parent_status(&child), InboxWaitStatus::PeerBlocked);
        drop(explicit);
        assert_eq!(parent_status(&child), InboxWaitStatus::Pending);
        std::fs::write(fixture.root.path().join("release"), b"continue").unwrap();
        let completed = crate::jobs::wait_async(&fixture.owner, &native.id, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(completed.status, "exited");
    });
}

#[test]
fn native_pool_item_all_and_drain_waits_hold_only_their_observer_lifetimes() {
    let _guard = crate::jobs::process_test_guard();
    let fixture = Fixture::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let started = fixture
            .call(json!({
                "operation":"pool_start", "agent":"worker", "concurrency":1, "tasks":["wait"]
            }))
            .await;
        let pool = started["poolId"].as_str().unwrap();
        let child = fixture.child().await;
        for request in [
            json!({"operation":"pool_wait", "poolId":pool, "itemId":1, "timeoutMs":5000}),
            json!({"operation":"pool_wait", "poolId":pool, "timeoutMs":5000}),
            json!({"operation":"pool_drain", "poolId":pool, "timeoutMs":5000}),
        ] {
            assert_eq!(parent_status(&child), InboxWaitStatus::Pending);
            let mut waiting = Box::pin(fixture.tool.execute("pool-wait", request, None));
            assert!(futures::poll!(waiting.as_mut()).is_pending());
            assert_eq!(parent_status(&child), InboxWaitStatus::PeerBlocked);
            drop(waiting);
            assert_eq!(parent_status(&child), InboxWaitStatus::Pending);
        }
        fixture
            .call(json!({"operation":"pool_finish", "poolId":pool}))
            .await;
        std::fs::write(fixture.root.path().join("release"), b"continue").unwrap();
        let done = fixture
            .call(json!({"operation":"pool_wait", "poolId":pool, "timeoutMs":5000}))
            .await;
        assert_eq!(done["status"], "completed");
    });
}

struct NativeRegistration(String);

impl NativeRegistration {
    fn new() -> Self {
        let entry = crate::agent_hub::registry()
            .lock()
            .unwrap()
            .register_worker("later-wave", crate::agent_hub::ChildKind::Subagent)
            .unwrap();
        Self(entry.id)
    }
}

impl Drop for NativeRegistration {
    fn drop(&mut self) {
        let mut hub = crate::agent_hub::registry().lock().unwrap();
        hub.settle(&self.0, ChildStatus::Done);
        hub.finish_native_execution(&self.0);
    }
}

#[test]
fn owned_job_identity_survives_yields_and_binds_children_after_wait_admission() {
    let _guard = crate::jobs::process_test_guard();
    let fixture = Fixture::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (begin, admitted) = futures::channel::oneshot::channel();
        let (first_send, first_receive) = futures::channel::oneshot::channel();
        let (advance, next_wave) = futures::channel::oneshot::channel();
        let (second_send, second_receive) = futures::channel::oneshot::channel();
        let (finish, finished) = futures::channel::oneshot::channel();
        let job = crate::jobs::spawn_owned_task(
            crate::jobs::admit_owned_task(&fixture.owner).unwrap(),
            "late child waves",
            AgentCx::for_request(),
            Duration::from_secs(10),
            Some(fixture.root.path()),
            AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::from_settings(None)),
            async move {
                let thread = std::thread::current().id();
                let owner = crate::jobs::current_native_task().unwrap();
                admitted.await.unwrap();
                assert!(crate::jobs::current_native_task().as_ref() == Some(&owner));
                let first = NativeRegistration::new();
                first_send.send(first.0.clone()).unwrap();
                next_wave.await.unwrap();
                drop(first);
                assert_eq!(std::thread::current().id(), thread);
                assert!(crate::jobs::current_native_task().as_ref() == Some(&owner));
                let second = NativeRegistration::new();
                second_send.send(second.0.clone()).unwrap();
                finished.await.unwrap();
                drop(second);
                Ok(ToolOutput {
                    content: Vec::new(),
                    details: None,
                    is_error: false,
                })
            },
        )
        .unwrap();
        let mut waiting = Box::pin(crate::jobs::wait_async(
            &fixture.owner,
            &job.id,
            Duration::from_secs(5),
        ));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        begin.send(()).unwrap();
        let first = first_receive.await.unwrap();
        assert_eq!(parent_status(&first), InboxWaitStatus::PeerBlocked);
        advance.send(()).unwrap();
        let second = second_receive.await.unwrap();
        assert_eq!(parent_status(&second), InboxWaitStatus::PeerBlocked);
        assert!(
            !crate::agent_hub::registry()
                .lock()
                .unwrap()
                .native_jobs
                .contains_key(&first)
        );
        drop(waiting);
        assert_eq!(parent_status(&second), InboxWaitStatus::Pending);
        finish.send(()).unwrap();
        let completed = crate::jobs::wait_async(&fixture.owner, &job.id, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(completed.status, "exited");
        assert!(crate::jobs::current_native_task().is_none());
    });
}
