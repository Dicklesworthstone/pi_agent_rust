//! Integration tests for background bash jobs (bd-cv653.3.10).
//!
//! Acceptance coverage:
//! 1. background sleep+echo: tool returns id instantly; completion notice
//!    arrives in the follow-up drain with the output tail.
//! 2. cancel mid-run kills the whole process tree (child-spawning script).
//! 3. `kill_all` (session exit) with 2 running jobs leaves zero survivors.
//! 4. The concurrency cap rejects the 9th job with `PI_JOBS_AT_CAPACITY`.
//!
//! Logging: structured JSONL per tests/common/logging.rs, v2-validated,
//! recorded as artifacts.

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::tools::{Tool, ToolOutput, ToolRegistry};
use serde_json::json;
use std::time::Duration;

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

/// The jobs registry is process-global by design; tests that spawn jobs
/// serialize on this lock so capacity/kill assertions don't race.
static JOBS_TEST_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

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

fn execute(tool: &pi::tools::BashTool, input: serde_json::Value) -> ToolOutput {
    block_on_local(tool.execute("call-1", input, None)).expect("execute")
}

const fn jobs_tool() -> pi::tools::JobsTool {
    pi::tools::JobsTool
}

fn execute_jobs(action: &str, job_id: Option<&str>, timeout_ms: Option<u64>) -> ToolOutput {
    let mut input = json!({"action": action});
    if let Some(id) = job_id {
        input["jobId"] = json!(id); // ubs:ignore Value index assignment never panics
    }
    if let Some(ms) = timeout_ms {
        input["timeoutMs"] = json!(ms); // ubs:ignore Value index assignment never panics
    }
    block_on_local(jobs_tool().execute("call-1", input, None)).expect("jobs execute") // ubs:ignore test helper
}

fn job_id(output: &ToolOutput) -> String {
    output.details.as_ref().expect("job details")["id"] // ubs:ignore test helper
        .as_str()
        .expect("job id") // ubs:ignore test helper
        .to_string()
}

#[test]
fn background_returns_instantly_and_notices_with_tail() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock"); // ubs:ignore test guard
    let case = "background_returns_instantly_and_notices_with_tail";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");

    let tool = pi::tools::BashTool::new(&root);
    let started = std::time::Instant::now();
    let out = execute(
        &tool,
        json!({"command": "sleep 1; echo bg-marker-$$", "background": true, "timeout": 30}),
    );
    let elapsed = started.elapsed();
    let text = first_text(&out);
    harness.log().info(
        "verify",
        format!("background start took {elapsed:?}: {text}"),
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "background spawn must return instantly, took {elapsed:?}"
    );
    assert!(text.contains("Background job"), "{text}");
    assert!(!out.is_error);

    let id = job_id(&out);
    let details = out.details.as_ref().expect("details");
    assert_eq!(details["schema"], "pi.bash_job.v1");
    assert_eq!(details["status"], "running");

    // Wait for settle via the jobs tool, then verify the completion notice
    // drained for the follow-up queue carries the output tail.
    let waited = execute_jobs("wait", Some(&id), Some(10_000));
    let waited_text = first_text(&waited);
    harness
        .log()
        .info("verify", format!("wait result: {waited_text}"));
    assert!(waited_text.contains("exited"), "{waited_text}");
    assert!(waited_text.contains("bg-marker-"), "{waited_text}");

    let notices = pi::jobs::take_completion_notices();
    let rendered: Vec<String> = notices
        .iter()
        .map(|message| match &message {
            pi::model::Message::User(user) => match &user.content {
                pi::model::UserContent::Text(text) => text.clone(),
                pi::model::UserContent::Blocks(_) => String::new(),
            },
            _ => String::new(),
        })
        .collect();
    harness.log().info(
        "verify",
        format!("drained {} notice(s): {:?}", rendered.len(), rendered),
    );
    assert!(
        rendered
            .iter()
            .any(|notice| notice.contains(&id) && notice.contains("bg-marker-")),
        "a completion notice naming the job and output tail must drain: {rendered:?}"
    );
    finish_case(&harness, case);
}

#[test]
fn cancel_kills_whole_tree() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock"); // ubs:ignore test guard
    let case = "cancel_kills_whole_tree";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");

    // The child-spawning script records its background child's pid so the
    // test can prove the tree kill caught the grandchild.
    let script = root.join("spawner.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nsleep 300 &\necho $! > child.pid\nwait\n",
    )
    .expect("write spawner");

    let tool = pi::tools::BashTool::new(&root);
    let out = execute(
        &tool,
        json!({"command": "sh spawner.sh", "background": true, "timeout": 300}),
    );
    let id = job_id(&out);
    // Give the script a beat to spawn its child and record the pid.
    std::thread::sleep(Duration::from_millis(500));
    let child_pid: u32 = std::fs::read_to_string(root.join("child.pid")) // ubs:ignore test fixture
        .expect("child pid file") // ubs:ignore test fixture
        .trim()
        .parse() // ubs:ignore test fixture
        .expect("parse child pid"); // ubs:ignore test fixture
    harness
        .log()
        .info("verify", format!("grandchild pid: {child_pid}"));

    let cancelled = execute_jobs("cancel", Some(&id), None);
    let text = first_text(&cancelled);
    harness
        .log()
        .info("verify", format!("cancel result: {text}"));
    assert!(text.contains("killed"), "{text}");

    // No survivors: the grandchild sleep must be gone.
    std::thread::sleep(Duration::from_millis(300));
    let alive =
        std::path::Path::new(&format!("/proc/{child_pid}")).exists() || kill_zero(child_pid);
    harness.log().info(
        "verify",
        format!("grandchild {child_pid} alive after tree kill: {alive}"),
    );
    assert!(
        !alive,
        "grandchild process {child_pid} survived the tree kill"
    );
    finish_case(&harness, case);
}

fn kill_zero(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn session_exit_kills_all_survivors() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock"); // ubs:ignore test guard
    let case = "session_exit_kills_all_survivors";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");

    let tool = pi::tools::BashTool::new(&root);
    let first = execute(
        &tool,
        json!({"command": "sleep 300", "background": true, "timeout": 300}),
    );
    let second = execute(
        &tool,
        json!({"command": "sleep 300", "background": true, "timeout": 300}),
    );
    let first_pid = u32::try_from(
        first.details.as_ref().expect("first details")["pid"] // ubs:ignore test fixture
            .as_u64()
            .expect("first pid"), // ubs:ignore test fixture
    )
    .expect("pid fits u32");
    let second_pid = u32::try_from(
        second.details.as_ref().expect("second details")["pid"] // ubs:ignore test fixture
            .as_u64()
            .expect("second pid"), // ubs:ignore test fixture
    )
    .expect("pid fits u32");
    harness.log().info(
        "verify",
        format!("running jobs pids: {first_pid}, {second_pid}"),
    );

    pi::jobs::kill_all();
    std::thread::sleep(Duration::from_millis(500));

    for pid in [first_pid, second_pid] {
        let proc_path = format!("/proc/{pid}"); // ubs:ignore two-iteration test loop
        let alive = std::path::Path::new(&proc_path).exists() || kill_zero(pid);
        let message = format!("pid {pid} alive after kill_all: {alive}"); // ubs:ignore test loop
        harness.log().info("verify", message);
        assert!(!alive, "job process {pid} survived session-exit kill_all");
    }
    finish_case(&harness, case);
}

#[test]
fn capacity_rejects_ninth_job() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock"); // ubs:ignore test guard
    let case = "capacity_rejects_ninth_job";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");

    let tool = pi::tools::BashTool::new(&root);
    let mut started = Vec::new();
    for index in 0..8 {
        let out = execute(
            &tool,
            json!({"command": "sleep 120", "background": true, "timeout": 300}),
        );
        assert!(
            !out.is_error,
            "job {} should start: {}",
            index + 1,
            first_text(&out)
        );
        started.push(job_id(&out));
    }
    harness
        .log()
        .info("verify", format!("started 8 jobs: {started:?}"));

    let ninth = execute(
        &tool,
        json!({"command": "sleep 120", "background": true, "timeout": 300}),
    );
    let text = first_text(&ninth);
    harness
        .log()
        .info("verify", format!("ninth job result: {text}"));
    assert!(
        text.contains("PI_JOBS_AT_CAPACITY"),
        "the 9th job must be rejected with the named capacity error: {text}"
    );

    // Clean up the 8 sleepers so they do not linger past the test.
    pi::jobs::kill_all();
    finish_case(&harness, case);
}

#[test]
fn registry_exposes_jobs_tool_by_default() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock"); // ubs:ignore test guard
    let case = "registry_exposes_jobs_tool_by_default";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let registry = ToolRegistry::new(
        &[
            "read",
            "bash",
            "edit",
            "write",
            "grep",
            "find",
            "ls",
            "hashline_edit",
            "web_search",
            "ast_grep",
            "ast_edit",
            "lsp",
            "debug",
            "ask",
            "todo",
            "submit_plan",
            "jobs",
        ],
        &root,
        None::<&pi::config::Config>,
    );
    let names: Vec<&str> = registry.tools().iter().map(|tool| tool.name()).collect();
    harness
        .log()
        .info("verify", format!("registry tools: {names:?}"));
    assert!(
        names.contains(&"jobs"),
        "the default tool set must expose the jobs tool: {names:?}"
    );
    finish_case(&harness, case);
}

#[test]
fn bash_background_through_registry() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock"); // ubs:ignore test guard
    let case = "bash_background_through_registry";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let registry = ToolRegistry::new(&["bash", "jobs"], &root, None::<&pi::config::Config>);
    let bash = registry
        .tools()
        .iter()
        .find(|tool| tool.name() == "bash")
        .expect("bash tool");
    let out = block_on_local(bash.execute(
        "call-1",
        json!({"command": "echo registry-bg", "background": true, "timeout": 30}),
        None,
    ))
    .expect("execute");
    let text = first_text(&out);
    harness.log().info("verify", format!("registry bg: {text}"));
    assert!(text.contains("Background job"), "{text}");
    let id = job_id(&out);
    let waited = execute_jobs("wait", Some(&id), Some(10_000));
    assert!(first_text(&waited).contains("exited"));
    pi::jobs::kill_all();
    finish_case(&harness, case);
}
