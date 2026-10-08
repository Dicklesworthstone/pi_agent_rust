//! Integration tests for hub process supervision (bd-cv653.5.4).
//!
//! Acceptance coverage:
//! 1. Fixture HTTP server with ready.log+ready.port: start returns only
//!    after both gates pass; ps shows running; logs cursor advances;
//!    stop leaves no processes.
//! 2. PTY send drives a `python3 -i` REPL fixture through the tool surface.
//! 3. Duplicate live name → `PI_HUB_NAME_TAKEN`; restart after completion.
//! 4. `kill_session_services` (session exit) leaves zero survivors.
//!
//! Logging: structured JSONL per tests/common/logging.rs, v2-validated,
//! recorded as artifacts.

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::tools::{Tool, ToolOutput};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::time::Duration;

/// The hub registry is process-global by design; tests serialize. Poison
/// from a failed peer is tolerated (the lock only serializes, it guards no
/// local state).
static HUB_TEST_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

fn hub_test_guard() -> std::sync::MutexGuard<'static, ()> {
    HUB_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn first_text(output: &ToolOutput) -> &str {
    output
        .content
        .iter()
        .find_map(|block| match block {
            pi::model::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(errors.is_empty(), "JSONL v2 validation errors: {errors:?}");
}

fn block_on_local<F: std::future::Future>(future: F) -> F::Output {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future)
}

fn hub_exec(cwd: &std::path::Path, input: Value) -> ToolOutput {
    hub_exec_for_session(cwd, "hub-integration-session", input)
}

fn hub_exec_for_session(cwd: &std::path::Path, session_id: &str, input: Value) -> ToolOutput {
    let mut tool = pi::tools::HubTool::new(cwd);
    tool.bind_job_session_scope(pi::jobs::JobSessionScope::fixed(session_id));
    block_on_local(tool.execute("call-1", input, None)).expect("hub execute")
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local addr").port()
}

fn proc_state(pid: u32) -> Option<char> {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| stat.rsplit(')').next()?.trim().chars().next())
}

#[cfg(unix)]
fn assert_reaped(pid: u32) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).expect("pid fits i32"))
        .expect("positive child pid");
    assert!(
        // ubs:ignore[rust.panic.assert-macros] -- The fixture requires actual process absence after successful stop.
        rustix::process::test_kill_process(pid).is_err(),
        "service must be reaped before successful settlement, including on platforms without /proc"
    );
}

#[cfg(unix)]
struct ServiceCleanup(&'static str);

#[cfg(unix)]
impl Drop for ServiceCleanup {
    fn drop(&mut self) {
        let _ = pi::hub::send_signal(self.0, sysinfo::Signal::Kill);
        let _ = pi::hub::stop(self.0);
    }
}

#[cfg(unix)]
async fn with_hub_owner<F: std::future::Future>(
    owner: &pi::agent_cx::AgentCx,
    future: F,
) -> F::Output {
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|task_cx| {
        let _guard = owner.cx().clone().set_current_restricted();
        std::future::Future::poll(future.as_mut(), task_cx)
    })
    .await
}

#[cfg(unix)]
async fn wait_for_service_pid(name: &str) -> u32 {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(snapshot) = pi::hub::describe(name)
            && let Some(pid) = snapshot.pid
        {
            return pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "service must publish its actual process before cancellation"
        );
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
#[test]
fn cancelling_readiness_yields_the_executor_and_reaps_before_returning() {
    let _guard = hub_test_guard();
    let case = "cancelling_readiness_yields_the_executor_and_reaps_before_returning";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let name = "hub-cancel-readiness";
    let _cleanup = ServiceCleanup(name);
    let tool = pi::tools::HubTool::new(&root);
    let owner = pi::agent_cx::AgentCx::for_request();
    let (output, pid) = block_on_local(async {
        let mut call = Box::pin(with_hub_owner(
            &owner,
            tool.execute(
                "cancel-readiness",
                json!({
                    "op": "start", "name": name, "application": "python3",
                    "args": ["-u", "-c", "import time; print('waiting-for-ready', flush=True); time.sleep(60)"],
                    "ready": {"log": "never-ready", "timeoutSecs": 4}
                }),
                None,
            ),
        ));
        std::future::poll_fn(|task_cx| {
            assert!(
                std::future::Future::poll(call.as_mut(), task_cx).is_pending(),
                "readiness must yield before its timeout, including without a context blocking pool"
            );
            std::task::Poll::Ready(())
        })
        .await;
        let cancel = async {
            let pid = wait_for_service_pid(name).await;
            owner.cancel_with(
                asupersync::types::CancelKind::User,
                Some("cancel readiness fixture"),
            );
            pid
        };
        futures::join!(call, cancel)
    });
    let output = output.expect("hub cancellation is a domain result");
    assert!(output.is_error);
    assert!(first_text(&output).contains("PI_HUB_CANCELLED"));
    let settled = pi::hub::describe(name).expect("settled startup");
    assert_eq!(settled.status, "killed");
    assert!(settled.pid.is_none());
    assert_reaped(pid);
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn dropping_a_readiness_future_stops_its_unaccepted_service() {
    let _guard = hub_test_guard();
    let case = "dropping_a_readiness_future_stops_its_unaccepted_service";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let name = "hub-dropped-readiness";
    let _cleanup = ServiceCleanup(name);
    let tool = pi::tools::HubTool::new(&root);
    let pid = block_on_local(async {
        let mut call = Box::pin(tool.execute(
            "drop-readiness",
            json!({
                "op": "start", "name": name, "application": "python3",
                "args": ["-u", "-c", "import time; print('waiting-for-ready', flush=True); time.sleep(60)"],
                "ready": {"log": "never-ready", "timeoutSecs": 60}
            }),
            None,
        ));
        let pid = match futures::future::select(call.as_mut(), Box::pin(wait_for_service_pid(name)))
            .await
        {
            futures::future::Either::Left((result, _)) => {
                panic!("startup returned before the caller could abandon it: {result:?}")
            }
            futures::future::Either::Right((pid, _)) => pid,
        };
        drop(call);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = pi::hub::describe(name).expect("abandoned service stays observable");
            if snapshot.pid.is_none() && snapshot.status == "killed" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "dropping startup must request cleanup without waiting for readiness timeout"
            );
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
        }
        pid
    });
    assert_reaped(pid);
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn abandoning_a_completed_start_never_stops_a_replacement_incarnation() {
    let _guard = hub_test_guard();
    let case = "abandoning_a_completed_start_never_stops_a_replacement_incarnation";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let name = "hub-unaccepted-result";
    let _cleanup = ServiceCleanup(name);
    let tool = pi::tools::HubTool::new(&root);
    let script = r"import pathlib, time
while not pathlib.Path('release-ready').exists():
    time.sleep(0.01)
print('old-fixture-ready', flush=True)
time.sleep(60)
";
    block_on_local(async {
        let mut first = Box::pin(tool.execute(
            "unaccepted-start",
            json!({
                "op": "start", "name": name, "application": "python3",
                "args": ["-u", "-c", script],
                "ready": {"log": "old-fixture-ready", "timeoutSecs": 30}
            }),
            None,
        ));
        std::future::poll_fn(|task_cx| {
            assert!(
                std::future::Future::poll(first.as_mut(), task_cx).is_pending(),
                "startup must yield while its output gate is held"
            );
            std::task::Poll::Ready(())
        })
        .await;
        let old_pid = wait_for_service_pid(name).await;
        std::fs::write(root.join("release-ready"), "ready").expect("release readiness");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while pi::hub::describe(name).expect("first service").status != "running" {
            assert!(
                std::time::Instant::now() < deadline,
                "worker must complete readiness before the name is reused"
            );
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
        }
        // The original tool future is deliberately not polled again. Its
        // startup has completed, but that result has not reached its caller.
        let stopped = tool
            .execute("stop-old", json!({"op": "stop", "name": name}), None)
            .await
            .expect("stop old service");
        assert!(!stopped.is_error, "{}", first_text(&stopped));
        assert_reaped(old_pid);
        let replacement = tool
            .execute(
                "replacement",
                json!({
                    "op": "start", "name": name, "application": "python3",
                    "args": ["-u", "-c", "import time; print('replacement-ready', flush=True); time.sleep(60)"],
                    "ready": {"log": "replacement-ready", "timeoutSecs": 10}
                }),
                None,
            )
            .await
            .expect("start replacement");
        assert!(!replacement.is_error, "{}", first_text(&replacement));
        let before = pi::hub::describe(name).expect("replacement is running");
        assert_eq!(before.status, "running");
        assert!(before.pid.is_some());
        drop(first);
        let after = pi::hub::describe(name).expect("replacement survives old result abandonment");
        assert_eq!(after.status, "running");
        assert_eq!(after.pid, before.pid);
    });
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn cancelling_a_log_wait_preserves_the_running_service() {
    let _guard = hub_test_guard();
    let case = "cancelling_a_log_wait_preserves_the_running_service";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let name = "hub-cancel-log-wait";
    let _cleanup = ServiceCleanup(name);
    let started = hub_exec(
        &root,
        json!({
            "op": "start", "name": name, "application": "python3",
            "args": ["-u", "-c", "import time; print('log-fixture-ready', flush=True); time.sleep(60)"],
            "ready": {"log": "log-fixture-ready", "timeoutSecs": 10}
        }),
    );
    assert!(!started.is_error, "{}", first_text(&started));
    let before = pi::hub::describe(name).expect("running service");
    let tool = pi::tools::HubTool::new(&root);
    let owner = pi::agent_cx::AgentCx::for_request();
    let output = block_on_local(async {
        let mut call = Box::pin(with_hub_owner(
            &owner,
            tool.execute(
                "cancel-log-wait",
                json!({"op": "logs", "name": name, "grep": "missing-line", "waitMs": 1000}),
                None,
            ),
        ));
        std::future::poll_fn(|task_cx| {
            assert!(
                std::future::Future::poll(call.as_mut(), task_cx).is_pending(),
                "waiting for logs must yield the executor"
            );
            std::task::Poll::Ready(())
        })
        .await;
        owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("cancel log wait fixture"),
        );
        call.await
    })
    .expect("hub cancellation is a domain result");
    assert!(output.is_error);
    assert!(first_text(&output).contains("PI_HUB_CANCELLED"));
    let after = pi::hub::describe(name).expect("service is not owned by the log request");
    assert_eq!(after.status, "running");
    assert_eq!(after.pid, before.pid);
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn noisy_service_caps_raw_artifact_without_losing_readiness_or_later_input() {
    let _guard = hub_test_guard();
    let case = "noisy_service_caps_raw_artifact_without_losing_readiness_or_later_input";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let name = "hub-artifact-cap";
    let _cleanup = ServiceCleanup(name);
    let script = r"import os, sys
block = b'x' * 8191 + b'\n'
for _ in range(2176):
    remaining = memoryview(block)
    while remaining:
        remaining = remaining[os.write(1, remaining):]
print('hub-ready-after-cap', flush=True)
line = sys.stdin.readline().strip()
print('hub-ack:' + line, flush=True)
";
    let started = hub_exec(
        &root,
        json!({
            "op": "start", "name": name, "application": "python3",
            "args": ["-u", "-c", script],
            "ready": {"log": "(?m)^hub-ready-after-cap$", "timeoutSecs": 30}
        }),
    );
    assert!(!started.is_error, "{}", first_text(&started));
    assert!(first_text(&started).contains("Raw service log is incomplete"));
    let start_details = started.details.as_ref().expect("start details");
    let pid = u32::try_from(start_details["pid"].as_u64().expect("live child pid"))
        .expect("pid fits u32");
    let capture = &start_details["logCapture"];
    assert_eq!(capture["byteLimit"], 16 * 1024 * 1024);
    assert_eq!(capture["retainedBytes"], 16 * 1024 * 1024);
    assert!(capture["observedBytes"].as_u64().expect("observed") > 16 * 1024 * 1024);
    assert!(capture["droppedBytes"].as_u64().expect("dropped") > 0);
    assert_eq!(capture["truncated"], true);
    assert_eq!(capture["drainState"], "streaming");
    assert!(capture.get("writeError").is_none());
    assert!(capture.get("readError").is_none());
    let log_path = start_details["logPath"].as_str().expect("raw log path");
    let artifact = std::fs::read(log_path).expect("raw artifact");
    assert_eq!(artifact.len(), 16 * 1024 * 1024);
    assert!(artifact.starts_with(b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"));
    assert!(
        !std::str::from_utf8(&artifact)
            .expect("ASCII output")
            .contains("hub-ready-after-cap"),
        "readiness must observe bytes that are not retained in the raw artifact"
    );
    let logs = hub_exec(
        &root,
        json!({"op": "logs", "name": name, "cursor": 0, "grep": "hub-ready-after-cap"}),
    );
    assert!(!logs.is_error, "{}", first_text(&logs));
    assert!(first_text(&logs).contains("hub-ready-after-cap"));
    assert!(first_text(&logs).contains("requested log lines were evicted"));
    assert!(first_text(&logs).contains("Raw service log is incomplete"));
    let page = logs.details.as_ref().expect("log details");
    assert_eq!(page["logPath"], log_path);
    assert!(page["lostLines"].as_u64().expect("lost lines") > 0);
    assert_eq!(page["lostLines"], page["oldestCursor"]);
    let cursor = page["cursor"].as_u64().expect("cursor");

    let described = hub_exec(&root, json!({"op": "describe", "name": name}));
    assert!(!described.is_error, "{}", first_text(&described));
    assert_eq!(
        described.details.as_ref().expect("descriptor")["logCapture"],
        *capture
    );
    let roster = hub_exec(&root, json!({"op": "ps"}));
    assert!(!roster.is_error, "{}", first_text(&roster));
    let service = roster.details.as_ref().expect("roster")["services"]
        .as_array()
        .expect("services")
        .iter()
        .find(|service| service["name"] == name)
        .expect("service in roster");
    assert_eq!(service["logCapture"], *capture);

    let sent = hub_exec(
        &root,
        json!({"op": "send", "name": name, "text": "input-after-cap"}),
    );
    assert!(!sent.is_error, "{}", first_text(&sent));
    let acknowledged = hub_exec(
        &root,
        json!({
            "op": "logs", "name": name, "cursor": cursor,
            "grep": "hub-ack:input-after-cap", "waitMs": 5000
        }),
    );
    assert!(!acknowledged.is_error, "{}", first_text(&acknowledged));
    assert!(first_text(&acknowledged).contains("hub-ack:input-after-cap"));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let finished = loop {
        let snapshot = pi::hub::describe(name).expect("finished service");
        if snapshot.status == "exited"
            && snapshot.log_capture.drain_state == pi::hub::LogDrainState::Complete
        {
            break snapshot;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "service must drain and exit"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_reaped(pid);
    assert_eq!(finished.log_capture.retained_bytes, 16 * 1024 * 1024);
    assert_eq!(
        finished.log_capture.dropped_bytes,
        finished.log_capture.observed_bytes - finished.log_capture.retained_bytes
    );
    assert!(finished.log_capture.read_error.is_none());
    assert!(finished.log_capture.write_error.is_none());
    assert_eq!(
        std::fs::metadata(log_path).expect("capped log").len(),
        16 * 1024 * 1024
    );
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn short_service_output_is_complete_and_unchanged() {
    let _guard = hub_test_guard();
    let case = "short_service_output_is_complete_and_unchanged";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let name = "hub-short-complete-log";
    let _cleanup = ServiceCleanup(name);
    let started = hub_exec(
        &root,
        json!({
            "op": "start", "name": name, "application": "python3",
            "args": ["-u", "-c", "print('ordinary service output')"]
        }),
    );
    assert!(!started.is_error, "{}", first_text(&started));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = pi::hub::describe(name).expect("service");
        if snapshot.status == "exited"
            && snapshot.log_capture.drain_state == pi::hub::LogDrainState::Complete
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "short service must drain and exit"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let logs = hub_exec(&root, json!({"op": "logs", "name": name}));
    assert!(!logs.is_error, "{}", first_text(&logs));
    assert_eq!(first_text(&logs), "ordinary service output");
    let page = logs.details.as_ref().expect("log details");
    let capture = &page["logCapture"];
    assert_eq!(capture["drainState"], "complete");
    assert_eq!(capture["truncated"], false);
    assert_eq!(capture["droppedBytes"], 0);
    assert_eq!(page["lostLines"], 0);
    assert!(capture.get("readError").is_none());
    assert!(capture.get("writeError").is_none());
    let artifact =
        std::fs::read(page["logPath"].as_str().expect("log path")).expect("complete raw artifact");
    assert_eq!(capture["observedBytes"], artifact.len() as u64);
    assert_eq!(capture["retainedBytes"], artifact.len() as u64);
    assert_eq!(
        std::str::from_utf8(&artifact).expect("raw text").trim_end(),
        "ordinary service output"
    );
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn cancelling_restart_finishes_the_old_process_without_launching_a_replacement() {
    let _guard = hub_test_guard();
    let case = "cancelling_restart_finishes_the_old_process_without_launching_a_replacement";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let name = "hub-cancel-restart";
    let _cleanup = ServiceCleanup(name);
    let script = r"import pathlib, signal, sys, time
counter = pathlib.Path('launch-count')
counter.write_text(str(int(counter.read_text()) + 1) if counter.exists() else '1')
def terminate(*_):
    pathlib.Path('stop-begun').write_text('stopping')
    while not pathlib.Path('release-stop').exists():
        time.sleep(0.01)
    sys.exit(0)
signal.signal(signal.SIGTERM, terminate)
print('restart-fixture-ready', flush=True)
while True:
    time.sleep(60)
";
    let started = hub_exec(
        &root,
        json!({
            "op": "start", "name": name, "application": "python3",
            "args": ["-u", "-c", script],
            "ready": {"log": "restart-fixture-ready", "timeoutSecs": 10}
        }),
    );
    assert!(!started.is_error, "{}", first_text(&started));
    let before = pi::hub::describe(name).expect("initial service");
    let tool = pi::tools::HubTool::new(&root);
    let owner = pi::agent_cx::AgentCx::for_request();
    let output = block_on_local(async {
        let call = with_hub_owner(
            &owner,
            tool.execute("cancel-restart", json!({"op": "restart", "name": name}), None),
        );
        let cancel = async {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !root.join("stop-begun").exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "restart must reach the old service's graceful shutdown"
                );
                asupersync::time::sleep(
                    asupersync::time::wall_now(),
                    Duration::from_millis(10),
                )
                .await;
            }
            owner.cancel_with(
                asupersync::types::CancelKind::User,
                Some("cancel during restart cleanup"),
            );
            std::fs::write(root.join("release-stop"), "finish").expect("release graceful stop");
        };
        let (output, ()) = futures::join!(call, cancel);
        output
    })
    .expect("hub cancellation is a domain result");
    assert!(output.is_error);
    assert!(first_text(&output).contains("PI_HUB_CANCELLED"));
    assert_eq!(std::fs::read_to_string(root.join("launch-count")).unwrap(), "1");
    let settled = pi::hub::describe(name).expect("old service settles");
    assert_eq!(settled.status, "killed");
    assert_eq!(settled.exit_code, Some(0));
    assert!(settled.pid.is_none());
    assert_reaped(before.pid.expect("old process id"));
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn graceful_stop_preserves_cleanup_and_returns_reaped_exit_metadata() {
    let _guard = hub_test_guard();
    let case = "graceful_stop_preserves_cleanup_and_returns_reaped_exit_metadata";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let name = "hub-graceful-cleanup";
    let _cleanup = ServiceCleanup(name);
    let script = r"import pathlib, signal, sys, time
def terminate(*_):
    time.sleep(1)
    pathlib.Path('grace-completed').write_text('clean shutdown')
    sys.exit(0)
signal.signal(signal.SIGTERM, terminate)
print('graceful-ready', flush=True)
while True:
    time.sleep(60)
";
    let started = hub_exec(
        &root,
        json!({
            "op": "start", "name": name, "application": "python3",
            "args": ["-u", "-c", script],
            "ready": {"log": "graceful-ready", "timeoutSecs": 20}
        }),
    );
    assert!(!started.is_error, "{}", first_text(&started)); // ubs:ignore[rust.panic.assert-macros] -- Real service startup must succeed before exercising cleanup.
    let pid = u32::try_from(
        started.details.as_ref().expect("start details")["pid"] // ubs:ignore[rust.panic.direct-indexing] -- Successful service descriptor must carry its child PID.
            .as_u64()
            .expect("child pid"),
    )
    .expect("pid fits u32");
    let stopped = hub_exec(&root, json!({"op": "stop", "name": name}));
    assert!(!stopped.is_error, "{}", first_text(&stopped)); // ubs:ignore[rust.panic.assert-macros] -- Stop must finish the real child lifecycle.
    let details = stopped.details.as_ref().expect("stop details");
    assert_eq!(details["status"], "killed"); // ubs:ignore[rust.panic.assert-macros] -- Requested termination must have settled.
    assert_eq!(details["exitCode"], 0); // ubs:ignore[rust.panic.assert-macros] -- Graceful handler exited successfully.
    assert!(details["pid"].is_null()); // ubs:ignore[rust.panic.assert-macros] -- Reaped children no longer publish a PID.
    assert_eq!(
        // ubs:ignore[rust.panic.assert-macros] -- The grace window must preserve the child's cleanup work.
        std::fs::read_to_string(root.join("grace-completed")).expect("graceful cleanup ran"),
        "clean shutdown"
    );
    assert_reaped(pid);
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn restart_escalates_term_ignoring_service_before_rebinding_its_port() {
    let _guard = hub_test_guard();
    let case = "restart_escalates_term_ignoring_service_before_rebinding_its_port";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let name = "hub-stubborn-restart";
    let _cleanup = ServiceCleanup(name);
    let port = free_port();
    let script = r"import signal, socket, sys, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
server = socket.socket()
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(('127.0.0.1', int(sys.argv[1])))
server.listen()
print('stubborn-ready', flush=True)
while True:
    time.sleep(60)
";
    let first = hub_exec(
        &root,
        json!({
            "op": "start", "name": name, "application": "python3",
            "args": ["-u", "-c", script, port.to_string()],
            "ready": {"log": "stubborn-ready", "port": port, "timeoutSecs": 20}
        }),
    );
    assert!(!first.is_error, "{}", first_text(&first)); // ubs:ignore[rust.panic.assert-macros] -- Real listening service is the restart precondition.
    let first_details = first.details.as_ref().expect("first details");
    let old_pid = u32::try_from(first_details["pid"].as_u64().expect("first pid")) // ubs:ignore[rust.panic.direct-indexing] -- Capture the actual first incarnation before restart.
        .expect("pid fits u32");
    let restart_started = std::time::Instant::now();
    let restarted = hub_exec(&root, json!({"op": "restart", "name": name}));
    assert!(!restarted.is_error, "{}", first_text(&restarted)); // ubs:ignore[rust.panic.assert-macros] -- Replacement must bind after the previous process is gone.
    assert!(restart_started.elapsed() >= Duration::from_secs(3)); // ubs:ignore[rust.panic.assert-macros] -- TERM-ignoring child must receive the full grace window.
    let details = restarted.details.as_ref().expect("restart details");
    assert_eq!(details["status"], "running"); // ubs:ignore[rust.panic.assert-macros] -- New service reached readiness.
    assert_eq!(details["ready"], true); // ubs:ignore[rust.panic.assert-macros] -- Both readiness gates passed.
    assert_ne!(first_details["pid"], details["pid"]); // ubs:ignore[rust.panic.assert-macros] -- Restart created a distinct process.
    assert_ne!(first_details["logPath"], details["logPath"]); // ubs:ignore[rust.panic.assert-macros] -- Incarnations retain separate logs.
    assert_reaped(old_pid);
    finish_case(&harness, case);
}

#[test]
fn fixture_server_readiness_conjunction_and_lifecycle() {
    let _guard = hub_test_guard();
    let case = "fixture_server_readiness_conjunction_and_lifecycle";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let port = free_port();

    let started = std::time::Instant::now();
    let out = hub_exec(
        &root,
        json!({
            "op": "start",
            "name": "fixture-http",
            "application": "python3",
            "args": ["-m", "http.server", &port.to_string(), "--bind", "127.0.0.1"],
            "ready": {
                "log": "Serving HTTP",
                "port": port,
                "timeoutSecs": 20
            }
        }),
    );
    let text = first_text(&out);
    harness.log().info(
        "verify",
        format!("start took {:?}: {}", started.elapsed(), text),
    );
    assert!(
        text.contains("is running"),
        "start must return only after readiness: {text}"
    );
    assert!(!out.is_error, "{text}");
    let details = out.details.as_ref().expect("details");
    assert_eq!(details["schema"], "pi.hub.service.v1");
    assert_eq!(details["status"], "running");
    assert_eq!(details["ready"], true);
    let pid = u32::try_from(details["pid"].as_u64().expect("pid")).expect("pid fits u32"); // ubs:ignore test fixture

    // ps shows the service running.
    let ps = hub_exec(&root, json!({"op": "ps"}));
    let ps_text = first_text(&ps);
    harness.log().info("verify", format!("ps: {ps_text}"));
    assert!(ps_text.contains("fixture-http"), "{ps_text}");
    assert!(ps_text.contains("running"), "{ps_text}");

    // logs snapshot contains the serving banner.
    let page1 = hub_exec(&root, json!({"op": "logs", "name": "fixture-http"}));
    let page1_text = first_text(&page1);
    harness
        .log()
        .info("verify", format!("logs page 1: {page1_text}"));
    assert!(page1_text.contains("Serving HTTP"), "{page1_text}");
    let cursor = page1.details.as_ref().expect("page1 details")["cursor"] // ubs:ignore test fixture
        .as_u64()
        .expect("cursor");

    // A real HTTP request produces a NEW line; the cursor read sees it.
    let body = ureq_get(&format!("http://127.0.0.1:{port}/"));
    harness
        .log()
        .info("verify", format!("http GET status: {body}"));
    let page2 = hub_exec(
        &root,
        json!({"op": "logs", "name": "fixture-http", "cursor": cursor, "grep": "GET", "waitMs": 5000}),
    );
    let page2_text = first_text(&page2);
    harness
        .log()
        .info("verify", format!("logs page 2: {page2_text}"));
    assert!(
        page2_text.contains("GET"),
        "incremental cursor read must see the request log line: {page2_text}"
    );
    let cursor2 = page2.details.as_ref().expect("page2 details")["cursor"] // ubs:ignore test fixture
        .as_u64()
        .expect("cursor2");
    assert!(
        cursor2 > cursor,
        "cursor must advance: {cursor2} > {cursor}"
    );

    // stop leaves no processes.
    let stopped = hub_exec(&root, json!({"op": "stop", "name": "fixture-http"}));
    let stopped_text = first_text(&stopped);
    harness
        .log()
        .info("verify", format!("stop: {stopped_text}"));
    assert!(stopped_text.contains("stopped"), "{stopped_text}");
    std::thread::sleep(Duration::from_millis(500));
    let state = proc_state(pid);
    harness
        .log()
        .info("verify", format!("pid {pid} state after stop: {state:?}"));
    assert!(
        state.is_none() || state == Some('Z'),
        "server process {pid} survived stop (state {state:?})"
    );
    finish_case(&harness, case);
}

/// Minimal HTTP GET (status line only) without pulling a client dependency.
fn ureq_get(url: &str) -> String {
    let addr = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("127.0.0.1:80")
        .to_string();
    let mut stream = std::net::TcpStream::connect(&addr).expect("connect");
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .expect("write");
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);
    buf.lines().next().unwrap_or("").to_string()
}

#[test]
fn send_drives_python_repl_through_tool() {
    let _guard = hub_test_guard();
    let case = "send_drives_python_repl_through_tool";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");

    let out = hub_exec(
        &root,
        json!({
            "op": "start",
            "name": "repl",
            "application": "python3",
            "args": ["-i", "-q"],
            "ready": { "log": ">>>", "timeoutSecs": 15 }
        }),
    );
    let text = first_text(&out);
    harness.log().info("verify", format!("repl start: {text}"));
    assert!(text.contains("is running"), "{text}");

    let sent = hub_exec(
        &root,
        json!({"op": "send", "name": "repl", "text": "print(6 * 7)"}),
    );
    let sent_text = first_text(&sent);
    harness.log().info("verify", format!("send: {sent_text}"));
    assert!(sent_text.contains("text"), "{sent_text}");

    let page = hub_exec(
        &root,
        json!({"op": "logs", "name": "repl", "grep": "42", "waitMs": 5000}),
    );
    let page_text = first_text(&page);
    harness
        .log()
        .info("verify", format!("repl output page: {page_text}"));
    assert!(
        page_text.contains("42"),
        "REPL must evaluate the sent expression: {page_text}"
    );

    // Named keys: CTRL_C at the prompt is safe and exercises the key path.
    let keyed = hub_exec(
        &root,
        json!({"op": "send", "name": "repl", "keys": ["CTRL_C"]}),
    );
    assert!(first_text(&keyed).contains("CTRL_C"));

    let _ = hub_exec(&root, json!({"op": "stop", "name": "repl"}));
    finish_case(&harness, case);
}

#[test]
fn duplicate_name_and_restart_flow() {
    let _guard = hub_test_guard();
    let case = "duplicate_name_and_restart_flow";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");

    let first = hub_exec(
        &root,
        json!({"op": "start", "name": "dupe", "application": "sleep", "args": ["120"]}),
    );
    assert!(first_text(&first).contains("is running"));

    let second = hub_exec(
        &root,
        json!({"op": "start", "name": "dupe", "application": "sleep", "args": ["120"]}),
    );
    let second_text = first_text(&second);
    harness
        .log()
        .info("verify", format!("duplicate start: {second_text}"));
    assert!(
        second_text.contains("PI_HUB_NAME_TAKEN"),
        "duplicate live name must be a named error: {second_text}"
    );

    let _ = hub_exec(&root, json!({"op": "stop", "name": "dupe"}));

    // Completed service restarts from the retained spec.
    let quick = hub_exec(
        &root,
        json!({"op": "start", "name": "quick", "application": "echo", "args": ["hello-restart"]}),
    );
    assert!(first_text(&quick).contains("running"));
    std::thread::sleep(Duration::from_millis(400));
    let restarted = hub_exec(&root, json!({"op": "restart", "name": "quick"}));
    let restarted_text = first_text(&restarted);
    harness
        .log()
        .info("verify", format!("restart: {restarted_text}"));
    assert!(
        restarted_text.contains("restarted"),
        "restart after completion must work: {restarted_text}"
    );
    let page = hub_exec(
        &root,
        json!({"op": "logs", "name": "quick", "grep": "hello-restart", "waitMs": 5000}),
    );
    assert!(first_text(&page).contains("hello-restart"));
    let _ = hub_exec(&root, json!({"op": "stop", "name": "quick"}));
    finish_case(&harness, case);
}

#[test]
fn session_exit_kills_non_detached_services() {
    let _guard = hub_test_guard();
    let case = "session_exit_kills_non_detached_services";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");

    let first = hub_exec(
        &root,
        json!({"op": "start", "name": "svc-a", "application": "sleep", "args": ["300"]}),
    );
    let second = hub_exec(
        &root,
        json!({"op": "start", "name": "svc-b", "application": "sleep", "args": ["300"]}),
    );
    let pid_a = u32::try_from(
        first.details.as_ref().expect("a details")["pid"] // ubs:ignore test fixture
            .as_u64()
            .expect("a pid"),
    )
    .expect("pid fits u32");
    let pid_b = u32::try_from(
        second.details.as_ref().expect("b details")["pid"] // ubs:ignore test fixture
            .as_u64()
            .expect("b pid"),
    )
    .expect("pid fits u32");
    harness
        .log()
        .info("verify", format!("service pids: {pid_a}, {pid_b}"));

    pi::hub::kill_session_services();

    for name in ["svc-a", "svc-b"] {
        let settled = pi::hub::describe(name).expect("settled service");
        assert_eq!(settled.status, "killed"); // ubs:ignore[rust.panic.assert-macros] -- Session cleanup must settle every service.
        assert!(settled.pid.is_none()); // ubs:ignore[rust.panic.assert-macros] -- Cleanup returns only after reap.
        assert!(settled.exit_code.is_some()); // ubs:ignore[rust.panic.assert-macros] -- Terminal exit metadata is already available.
    }

    for pid in [pid_a, pid_b] {
        #[cfg(unix)]
        assert_reaped(pid);
        let state = proc_state(pid);
        harness.log().info(
            "verify",
            format!("pid {pid} state after session kill: {state:?}"), // ubs:ignore two-iteration test loop
        );
        assert!(
            state.is_none() || state == Some('Z'),
            "service pid {pid} survived session exit (state {state:?})"
        );
    }
    finish_case(&harness, case);
}

#[test]
fn hub_jobs_group_wraps_background_jobs() {
    let _guard = hub_test_guard();
    let case = "hub_jobs_group_wraps_background_jobs";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");

    // Spawn a background job through the bash tool, then manage it via hub.
    let mut bash = pi::tools::BashTool::new(&root);
    bash.bind_job_session_scope(pi::jobs::JobSessionScope::fixed("hub-integration-session"));
    let out = block_on_local(bash.execute(
        "call-1",
        json!({"command": "echo hub-jobs-marker", "background": true, "timeout": 30}),
        None,
    ))
    .expect("bash background");
    let job_id = out.details.as_ref().expect("job details")["id"] // ubs:ignore test fixture
        .as_str()
        .expect("job id")
        .to_string();
    harness.log().info("verify", format!("spawned {job_id}"));

    let listed = hub_exec(&root, json!({"op": "jobs", "action": "list"}));
    let listed_text = first_text(&listed);
    harness
        .log()
        .info("verify", format!("hub jobs list: {listed_text}"));
    assert!(
        listed_text.contains(&job_id),
        "hub jobs list must show the job: {listed_text}"
    );

    let waited = hub_exec(
        &root,
        json!({"op": "jobs", "action": "wait", "jobId": job_id, "timeoutMs": 10000}),
    );
    let waited_text = first_text(&waited);
    harness
        .log()
        .info("verify", format!("hub jobs wait: {waited_text}"));
    assert!(waited_text.contains("exited"), "{waited_text}");
    finish_case(&harness, case);
}

#[test]
fn hub_jobs_group_hides_foreign_session_jobs() {
    let _guard = hub_test_guard();
    let case = "hub_jobs_group_hides_foreign_session_jobs";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let owner = format!("hub-owner-{}", uuid::Uuid::new_v4().simple());
    let foreign = format!("hub-foreign-{}", uuid::Uuid::new_v4().simple());
    let mut bash = pi::tools::BashTool::new(&root);
    bash.bind_job_session_scope(pi::jobs::JobSessionScope::fixed(owner.clone()));
    let output = block_on_local(bash.execute(
        "owner-job",
        json!({
            "command": "printf private-hub-marker",
            "background": true,
            "timeout": 30
        }),
        None,
    ))
    .expect("bash background");
    let job_id = output.details.as_ref().expect("job details")["id"]
        .as_str()
        .expect("job id")
        .to_string();

    let foreign_list =
        hub_exec_for_session(&root, &foreign, json!({"op": "jobs", "action": "list"}));
    assert!(!first_text(&foreign_list).contains(&job_id));
    let foreign_wait = hub_exec_for_session(
        &root,
        &foreign,
        json!({
            "op": "jobs",
            "action": "wait",
            "jobId": job_id,
            "timeoutMs": 10
        }),
    );
    assert!(foreign_wait.is_error);
    let foreign_text = first_text(&foreign_wait);
    assert!(foreign_text.contains("PI_JOBS_UNKNOWN_ID"));
    assert!(!foreign_text.contains("private-hub-marker"));
    let foreign_cancel = hub_exec_for_session(
        &root,
        &foreign,
        json!({"op": "jobs", "action": "cancel", "jobId": job_id}),
    );
    assert!(foreign_cancel.is_error);
    assert!(first_text(&foreign_cancel).contains("PI_JOBS_UNKNOWN_ID"));

    let owner_wait = hub_exec_for_session(
        &root,
        &owner,
        json!({
            "op": "jobs",
            "action": "wait",
            "jobId": job_id,
            "timeoutMs": 10_000
        }),
    );
    assert!(!owner_wait.is_error);
    assert!(first_text(&owner_wait).contains("exited"));
    let _ = pi::jobs::take_completion_notices(&owner);
    finish_case(&harness, case);
}
