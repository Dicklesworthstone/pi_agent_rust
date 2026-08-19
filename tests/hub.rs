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
    let tool = pi::tools::HubTool::new(cwd);
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
    std::thread::sleep(Duration::from_millis(500));

    for pid in [pid_a, pid_b] {
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
    let bash = pi::tools::BashTool::new(&root);
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
