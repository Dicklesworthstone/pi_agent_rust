//! Integration tests for background bash jobs (bd-cv653.3.10).
//!
//! Acceptance coverage:
//! 1. background sleep+echo: tool returns id instantly; completion notice
//!    arrives in the follow-up drain with the output tail.
//! 2. cancel mid-run kills the whole process tree (child-spawning script).
//! 3. Owner-scoped session shutdown kills only that session's jobs.
//! 4. `kill_all` (process exit) with 2 running jobs leaves zero survivors.
//! 5. The concurrency cap rejects the 9th job with `PI_JOBS_AT_CAPACITY`.
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
const TEST_SESSION_ID: &str = "jobs-integration-session";

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

fn bash_tool(root: &std::path::Path) -> pi::tools::BashTool {
    bash_tool_for_session(root, TEST_SESSION_ID)
}

fn bash_tool_for_session(root: &std::path::Path, session_id: &str) -> pi::tools::BashTool {
    let mut tool = pi::tools::BashTool::new(root);
    tool.bind_job_session_scope(pi::jobs::JobSessionScope::fixed(session_id));
    tool
}

fn jobs_tool_for_session(session_id: &str) -> pi::tools::JobsTool {
    let mut tool = pi::tools::JobsTool::new();
    tool.bind_job_session_scope(pi::jobs::JobSessionScope::fixed(session_id));
    tool
}

fn execute_jobs(action: &str, job_id: Option<&str>, timeout_ms: Option<u64>) -> ToolOutput {
    execute_jobs_for_session(TEST_SESSION_ID, action, job_id, timeout_ms)
}

fn execute_jobs_for_session(
    session_id: &str,
    action: &str,
    job_id: Option<&str>,
    timeout_ms: Option<u64>,
) -> ToolOutput {
    let mut input = json!({"action": action});
    if let Some(id) = job_id {
        input["jobId"] = json!(id); // ubs:ignore Value index assignment never panics
    }
    if let Some(ms) = timeout_ms {
        input["timeoutMs"] = json!(ms); // ubs:ignore Value index assignment never panics
    }
    block_on_local(jobs_tool_for_session(session_id).execute("call-1", input, None))
        .expect("jobs execute") // ubs:ignore test helper
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

    let tool = bash_tool(&root);
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

    let notices = pi::jobs::take_completion_notices(TEST_SESSION_ID);
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
fn jobs_tool_rejects_a_foreign_session_job_id_without_metadata() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock"); // ubs:ignore test guard
    let case = "jobs_tool_rejects_a_foreign_session_job_id_without_metadata";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let owner = format!("jobs-owner-{}", uuid::Uuid::new_v4().simple());
    let foreign = format!("jobs-foreign-{}", uuid::Uuid::new_v4().simple());
    let tool = bash_tool_for_session(&root, &owner);
    let output = execute(
        &tool,
        json!({
            "command": "printf private-jobs-marker",
            "background": true,
            "timeout": 30
        }),
    );
    let id = job_id(&output);
    let artifact_path = output.details.as_ref().expect("details")["artifactPath"]
        .as_str()
        .expect("artifact path")
        .to_string();

    let foreign_list = execute_jobs_for_session(&foreign, "list", None, None);
    assert!(!first_text(&foreign_list).contains(&id));
    let foreign_wait = block_on_local(jobs_tool_for_session(&foreign).execute(
        "foreign-wait",
        json!({"action": "wait", "jobId": id, "timeoutMs": 10}),
        None,
    ))
    .expect_err("foreign wait must fail closed");
    let rendered = foreign_wait.to_string();
    assert!(rendered.contains("PI_JOBS_UNKNOWN_ID"));
    assert!(!rendered.contains("private-jobs-marker"));
    assert!(!rendered.contains(&artifact_path));
    let foreign_cancel = block_on_local(jobs_tool_for_session(&foreign).execute(
        "foreign-cancel",
        json!({"action": "cancel", "jobId": id}),
        None,
    ))
    .expect_err("foreign cancel must fail closed");
    assert!(foreign_cancel.to_string().contains("PI_JOBS_UNKNOWN_ID"));

    let owner_wait = execute_jobs_for_session(&owner, "wait", Some(&id), Some(10_000));
    assert!(first_text(&owner_wait).contains("exited"));
    let _ = pi::jobs::take_completion_notices(&owner);
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

    let tool = bash_tool(&root);
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
// The id/pid and owner_a/owner_b pairings are the point of this test; the
// deliberately parallel names beat clippy's similarity heuristic.
#[allow(clippy::similar_names)]
fn owner_scoped_session_shutdown_preserves_foreign_jobs() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock"); // ubs:ignore test guard
    let case = "owner_scoped_session_shutdown_preserves_foreign_jobs";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let owner_a = format!("jobs-shutdown-a-{}", uuid::Uuid::new_v4().simple());
    let owner_b = format!("jobs-shutdown-b-{}", uuid::Uuid::new_v4().simple());

    let first = execute(
        &bash_tool_for_session(&root, &owner_a),
        json!({"command": "sleep 300", "background": true, "timeout": 300}),
    );
    let second = execute(
        &bash_tool_for_session(&root, &owner_b),
        json!({"command": "sleep 300", "background": true, "timeout": 300}),
    );
    let first_id = job_id(&first);
    let second_id = job_id(&second);
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

    block_on_local(pi::jobs::kill_session(&owner_a)).expect("owner A shutdown");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !kill_zero(first_pid),
        "owner A job {first_id} survived owner-scoped shutdown"
    );
    assert!(
        kill_zero(second_pid),
        "foreign owner B job {second_id} was terminated by owner A shutdown"
    );
    let owner_a_jobs = pi::jobs::list(&owner_a).expect("owner A list");
    assert!(
        owner_a_jobs.iter().any(|job| {
            job.id == first_id && job.status == pi::jobs::JobStatus::Killed.as_str()
        })
    );
    let owner_b_jobs = pi::jobs::list(&owner_b).expect("owner B list");
    assert!(
        owner_b_jobs.iter().any(|job| {
            job.id == second_id && job.status == pi::jobs::JobStatus::Running.as_str()
        })
    );

    block_on_local(pi::jobs::kill_session(&owner_b)).expect("owner B cleanup");
    let _ = pi::jobs::take_completion_notices(&owner_a);
    let _ = pi::jobs::take_completion_notices(&owner_b);
    finish_case(&harness, case);
}

#[test]
fn process_exit_kills_all_survivors() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock"); // ubs:ignore test guard
    let case = "process_exit_kills_all_survivors";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");

    let tool = bash_tool(&root);
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

    let tool = bash_tool(&root);
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
    let session_id = TEST_SESSION_ID.to_string();
    let resolver: pi::jobs::JobSessionIdResolver = std::sync::Arc::new(move || {
        let session_id = session_id.clone();
        Box::pin(async move { Some(session_id) })
    });
    registry.bind_job_session_resolver(resolver);
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
    let jobs = registry.get("jobs").expect("jobs tool");
    let waited = block_on_local(jobs.execute(
        "call-2",
        json!({"action": "wait", "jobId": id, "timeoutMs": 10_000}),
        None,
    ))
    .expect("wait through registry jobs tool");
    assert!(first_text(&waited).contains("exited"));
    let _ = pi::jobs::take_completion_notices(TEST_SESSION_ID);
    pi::jobs::kill_all();
    finish_case(&harness, case);
}
