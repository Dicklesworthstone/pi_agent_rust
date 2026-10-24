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

fn auto_bash_tool(
    root: &std::path::Path,
    session_id: &str,
    threshold_ms: u64,
) -> pi::tools::BashTool {
    bash_tool_for_session(root, session_id).with_mediation(Some(pi::config::BashSettings {
        pty: Some("off".to_string()),
        auto_background: Some(pi::config::BashAutoBackgroundSettings {
            enabled: Some(true),
            threshold_ms: Some(threshold_ms),
        }),
        ..Default::default()
    }))
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
fn automatic_foreground_completion_preserves_output_and_exit_status() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock");
    let case = "automatic_foreground_completion_preserves_output_and_exit_status";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let owner = format!("jobs-inline-{}", uuid::Uuid::new_v4().simple());
    let tool = auto_bash_tool(&root, &owner, 60_000);

    let output = execute(&tool, json!({"command": "printf inline-marker"}));
    assert!(!output.is_error, "{}", first_text(&output));
    assert_eq!(first_text(&output), "inline-marker");
    assert!(output.details.is_none());

    // Managed foreground output must keep the ordinary Bash display tail,
    // rather than silently adopting the background registry's 64 KiB tail.
    let large = execute(&tool, json!({"command": "printf '%100000s' '' | tr ' ' x"}));
    assert!(!large.is_error, "{}", first_text(&large));
    assert_eq!(first_text(&large).len(), 100_000);
    assert!(first_text(&large).bytes().all(|byte| byte == b'x'));
    assert!(large.details.is_none());

    let failed = execute(&tool, json!({"command": "printf failed-marker; exit 7"}));
    assert!(failed.is_error, "nonzero exit must remain a tool error");
    assert!(first_text(&failed).contains("failed-marker"));
    assert!(first_text(&failed).contains("Command exited with code 7"));
    assert!(pi::jobs::list(&owner).expect("list").is_empty());
    assert!(pi::jobs::take_completion_notices(&owner).is_empty());
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)] // inline bash scripts that set, restore and clear shell state
fn session_shell_restores_native_state_across_tool_rebuilds_without_replay() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock");
    let case = "session_shell_restores_native_state_across_tool_rebuilds_without_replay";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    std::fs::create_dir(root.join("nested")).expect("working directory");
    let owner = format!("shell-state-{}", uuid::Uuid::new_v4().simple());
    let output = execute(
        &bash_tool_for_session(&root, &owner),
        json!({"background": false, "command": r#"
PI_STATE_SCALAR=$'one\nquoted\047 $(touch replay-marker)'
PI_STATE_ARRAY=([2]='two words' [9]=$'nine\nlines')
if (( BASH_VERSINFO[0] >= 4 )); then
    declare -A PI_STATE_MAP=(["key with spaces"]="mapped value")
fi
shopt -s expand_aliases extglob
pi_state_fn() { printf '<%s>' "$PI_STATE_SCALAR"; }
pi_state_pattern() { case "$1" in +(x)) printf extglob-ok ;; esac; }
alias pi_state_alias=$'printf alias-first\nprintf alias-second'
alias declare='printf poisoned-alias'
set -u -o pipefail
set +B
export -n PATH
umask 077
printf once >> starts
cd nested
"#}),
    );
    assert!(!output.is_error, "{}", first_text(&output));
    assert_eq!(first_text(&output), "(no output)");

    // Real surfaces rebuild their registry. A new BashTool with the same live
    // session id must see the state, not an instance-local cache.
    let restored = execute(
        &bash_tool_for_session(&root, &owner),
        json!({"background": false, "command": r#"
pi_state_fn
printf '|%s|%s|' "${PI_STATE_ARRAY[2]}" "${PI_STATE_ARRAY[9]}"
if (( BASH_VERSINFO[0] >= 4 )); then printf '%s|' "${PI_STATE_MAP["key with spaces"]}"; fi
pi_state_pattern xxx
pi_state_alias
printf '|%s|%s|' "${SHELLOPTS}" "{a,b}"
printf '%s|' {a,b}
builtin declare -p PATH
pwd
umask
unset PI_STATE_SCALAR
unset -f pi_state_fn
unalias pi_state_alias
unset HOME
set +u
"#}),
    );
    assert!(!restored.is_error, "{}", first_text(&restored));
    let text = first_text(&restored);
    assert!(
        text.contains("<one\nquoted' $(touch replay-marker)>"),
        "{text}"
    );
    assert!(text.contains("|two words|nine\nlines|"), "{text}");
    assert!(text.contains("extglob-okalias-firstalias-second"), "{text}");
    assert!(
        text.contains("nounset") && text.contains("pipefail"),
        "{text}"
    );
    assert!(
        text.contains("|{a,b}|{a,b}|"),
        "brace expansion must stay disabled: {text}"
    );
    assert!(
        text.contains("declare -- PATH="),
        "export -n must persist: {text}"
    );
    assert!(
        !text.contains("poisoned-alias"),
        "aliases must not rewrite state restoration"
    );
    assert!(
        text.contains("/nested\n0077"),
        "cwd and umask must persist: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("starts")).expect("one side effect"),
        "once"
    );
    assert!(!root.join("replay-marker").exists());
    assert!(!root.join("nested/replay-marker").exists());

    let removed = execute(
        &bash_tool_for_session(&root, &owner),
        json!({"background": false, "command": r#"
printf '%s|%s|' "${PI_STATE_SCALAR-unset}" "${HOME-unset}"
if ! type pi_state_fn >/dev/null 2>&1; then printf function-removed; fi
if ! alias pi_state_alias >/dev/null 2>&1; then printf alias-removed; fi
"#}),
    );
    assert_eq!(
        first_text(&removed),
        "unset|unset|function-removedalias-removed"
    );
    let isolated = execute(
        &bash_tool_for_session(&root, &format!("{owner}-other")),
        json!({"background": false, "command": "printf '%s' \"${PI_STATE_ARRAY[2]-isolated}\""}),
    );
    assert_eq!(first_text(&isolated), "isolated");
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn promoted_session_shell_keeps_its_lease_until_state_is_committed() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock");
    let case = "promoted_session_shell_keeps_its_lease_until_state_is_committed";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let owner = format!("shell-promoted-{}", uuid::Uuid::new_v4().simple());
    let seeded = execute(
        &bash_tool_for_session(&root, &owner),
        json!({"command": "PI_PROMOTED_VALUE=before", "background": false}),
    );
    assert!(!seeded.is_error);
    let promoted = execute(
        &auto_bash_tool(&root, &owner, 0),
        json!({"timeout": 30, "command": "printf once >> starts; while [ ! -f release ]; do sleep 0.025; done; PI_PROMOTED_VALUE=after; pi_promoted_fn() { printf kept-function; }; printf completed"}),
    );
    let id = job_id(&promoted);
    assert_eq!(
        promoted.details.as_ref().expect("job details")["autoBackgrounded"],
        true
    );

    let overlap = execute(
        &bash_tool_for_session(&root, &owner),
        json!({"background": false, "command": "printf '%s' \"${PI_PROMOTED_VALUE-unavailable}\"; PI_PROMOTED_VALUE=must-not-persist"}),
    );
    assert!(!overlap.is_error);
    assert!(first_text(&overlap).starts_with("unavailable"));
    assert!(first_text(&overlap).contains("session shell is busy"));
    assert_eq!(
        overlap.details.as_ref().expect("temporary shell details")["shellState"]["mode"],
        "temporary"
    );

    std::fs::write(root.join("release"), "continue").expect("release command");
    let settled = block_on_local(pi::jobs::wait_async(&owner, &id, Duration::from_secs(10)))
        .expect("promoted shell settles");
    assert_eq!(settled.status, "exited");
    let after = execute(
        &bash_tool_for_session(&root, &owner),
        json!({"background": false, "command": "printf '%s|' \"$PI_PROMOTED_VALUE\"; pi_promoted_fn"}),
    );
    assert_eq!(first_text(&after), "after|kept-function");
    assert_eq!(
        std::fs::read_to_string(root.join("starts")).expect("one spawn"),
        "once"
    );
    assert_eq!(pi::jobs::take_completion_notices(&owner).len(), 1);
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn explicit_background_shell_is_isolated_without_holding_session_state() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock");
    let case = "explicit_background_shell_is_isolated_without_holding_session_state";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let owner = format!("shell-explicit-{}", uuid::Uuid::new_v4().simple());
    let tool = bash_tool_for_session(&root, &owner);
    execute(
        &tool,
        json!({"command": "PI_EXPLICIT_VALUE=seed", "background": false}),
    );
    let background = execute(
        &tool,
        json!({"background": true, "timeout": 30, "command": "printf '%s' \"${PI_EXPLICIT_VALUE-isolated}\"; PI_EXPLICIT_VALUE=background; while [ ! -f release ]; do sleep 0.025; done"}),
    );
    let id = job_id(&background);
    let foreground = execute(
        &tool,
        json!({"background": false, "command": "printf '%s' \"$PI_EXPLICIT_VALUE\"; PI_EXPLICIT_VALUE=foreground"}),
    );
    assert_eq!(first_text(&foreground), "seed");
    assert!(
        foreground.details.is_none(),
        "an explicit job must not occupy the session lease"
    );
    std::fs::write(root.join("release"), "continue").expect("release command");
    let settled = block_on_local(pi::jobs::wait_async(&owner, &id, Duration::from_secs(10)))
        .expect("explicit job settles");
    assert_eq!(settled.output_tail, "isolated");
    let after = execute(
        &tool,
        json!({"background": false, "command": "printf '%s' \"$PI_EXPLICIT_VALUE\""}),
    );
    assert_eq!(first_text(&after), "foreground");
    assert_eq!(pi::jobs::take_completion_notices(&owner).len(), 1);
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn session_shell_commits_normal_nonzero_and_rolls_back_terminated_calls() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock");
    let case = "session_shell_commits_normal_nonzero_and_rolls_back_terminated_calls";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let owner = format!("shell-rollback-{}", uuid::Uuid::new_v4().simple());
    let tool = bash_tool_for_session(&root, &owner);
    let nonzero = execute(
        &tool,
        json!({"background": false, "command": "PI_ROLLBACK_VALUE=committed; false"}),
    );
    assert!(nonzero.is_error);
    let early_exit = execute(
        &tool,
        json!({"background": false, "command": "PI_ROLLBACK_VALUE=discarded; exit 7"}),
    );
    assert!(early_exit.is_error);
    let errexit = execute(
        &tool,
        json!({"background": false, "command": "set -e; PI_ROLLBACK_VALUE=discarded; false; printf unreachable"}),
    );
    assert!(errexit.is_error);
    assert!(!first_text(&errexit).contains("unreachable"));
    let after = execute(
        &tool,
        json!({"background": false, "command": "printf '%s' \"$PI_ROLLBACK_VALUE\""}),
    );
    assert_eq!(first_text(&after), "committed");

    let timed_out = execute(
        &auto_bash_tool(&root, &owner, 0),
        json!({"timeout": 1, "command": "PI_ROLLBACK_VALUE=timed-out; sleep 30"}),
    );
    let id = job_id(&timed_out);
    let settled = block_on_local(pi::jobs::wait_async(&owner, &id, Duration::from_secs(10)))
        .expect("timed-out command settles");
    assert_eq!(settled.status, "timedOut");
    let after_timeout = execute(
        &tool,
        json!({"background": false, "command": "printf '%s' \"$PI_ROLLBACK_VALUE\""}),
    );
    assert_eq!(first_text(&after_timeout), "committed");
    assert_eq!(pi::jobs::take_completion_notices(&owner).len(), 1);

    let cancelled = execute(
        &auto_bash_tool(&root, &owner, 0),
        json!({"timeout": 30, "command": "PI_ROLLBACK_VALUE=cancelled; printf ready > cancel-ready; sleep 30"}),
    );
    let cancelled_id = job_id(&cancelled);
    let wait_started = std::time::Instant::now();
    while !root.join("cancel-ready").exists() && wait_started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        root.join("cancel-ready").exists(),
        "cancellation must follow the shell-state mutation"
    );
    let cancelled = execute_jobs_for_session(&owner, "cancel", Some(&cancelled_id), None);
    assert_eq!(
        cancelled.details.as_ref().expect("cancel details")["status"],
        "killed"
    );
    let after_cancel = execute(
        &tool,
        json!({"background": false, "command": "printf '%s' \"$PI_ROLLBACK_VALUE\""}),
    );
    assert_eq!(first_text(&after_cancel), "committed");
    assert_eq!(pi::jobs::take_completion_notices(&owner).len(), 1);
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn session_shell_state_can_be_disabled_and_recovers_a_removed_working_directory() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock");
    let case = "session_shell_state_can_be_disabled_and_recovers_a_removed_working_directory";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    std::fs::create_dir(root.join("old-directory")).expect("working directory");
    let owner = format!("shell-disabled-{}", uuid::Uuid::new_v4().simple());
    let tool = bash_tool_for_session(&root, &owner);
    execute(
        &tool,
        json!({"background": false, "command": "PI_DIRECTORY_VALUE=kept; cd old-directory"}),
    );
    // A rename removes the saved path without discarding fixture data.
    std::fs::rename(root.join("old-directory"), root.join("moved-directory"))
        .expect("move directory");
    let restored = execute(
        &tool,
        json!({"background": false, "command": "printf '%s|' \"$PI_DIRECTORY_VALUE\"; pwd"}),
    );
    assert!(first_text(&restored).starts_with("kept|"));
    assert!(!first_text(&restored).contains("old-directory"));
    assert!(!restored.is_error);

    let disabled =
        bash_tool_for_session(&root, &owner).with_mediation(Some(pi::config::BashSettings {
            persistent: Some(false),
            ..Default::default()
        }));
    let isolated = execute(
        &disabled,
        json!({"background": false, "command": "printf '%s' \"${PI_DIRECTORY_VALUE-disabled}\"; PI_DIRECTORY_VALUE=discarded"}),
    );
    assert_eq!(first_text(&isolated), "disabled");
    let preserved = execute(
        &tool,
        json!({"background": false, "command": "printf '%s' \"$PI_DIRECTORY_VALUE\""}),
    );
    assert_eq!(first_text(&preserved), "kept");
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn automatic_foreground_reports_the_partial_job_artifact_honestly() {
    if !std::path::Path::new("/dev/zero").exists() {
        return;
    }
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock");
    let case = "automatic_foreground_reports_the_partial_job_artifact_honestly";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let owner = format!("jobs-auto-partial-{}", uuid::Uuid::new_v4().simple());
    let output = execute(
        &auto_bash_tool(&root, &owner, 60_000),
        json!({"command": "head -c 16777217 /dev/zero | tr '\\0' x"}),
    );
    assert!(
        !output.is_error,
        "bounded retention is reported, not a failed exit"
    );
    let text = first_text(&output);
    assert!(
        text.contains("Partial output artifact:"),
        "partial footer missing"
    );
    assert!(
        !text.contains("Full output:"),
        "partial capture cannot be labeled full"
    );
    assert!(!text.contains("undefined"), "partial path must be real");
    let details = output.details.as_ref().expect("capture details");
    assert!(details.get("fullOutputPath").is_none());
    assert_eq!(details["capture"]["outputComplete"], true);
    assert_eq!(details["capture"]["artifactTruncated"], true);
    let path = details["capture"]["artifactPath"]
        .as_str()
        .expect("retained artifact path");
    assert!(text.contains(path));
    assert_eq!(
        std::fs::metadata(path).expect("retained artifact").len(),
        16 * 1024 * 1024
    );
    assert!(pi::jobs::take_completion_notices(&owner).is_empty());
    finish_case(&harness, case);
}

#[test]
fn automatic_promotion_keeps_the_same_process_and_executes_once() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock");
    let case = "automatic_promotion_keeps_the_same_process_and_executes_once";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let owner = format!("jobs-promote-{}", uuid::Uuid::new_v4().simple());
    let tool = auto_bash_tool(&root, &owner, 0);

    // The release file prevents natural completion from racing the zero
    // threshold. Replaying the command would append a second start PID.
    let output = execute(
        &tool,
        json!({
            "command": "printf '%s\\n' \"$$\" >> starts; printf started; while [ ! -f release ]; do sleep 0.025; done; printf '%s\\n' \"$$\" > completed; printf finished",
            "timeout": 30
        }),
    );
    assert!(!output.is_error, "{}", first_text(&output));
    let id = job_id(&output);
    let details = output.details.as_ref().expect("job details");
    assert_eq!(details["autoBackgrounded"], true);
    assert_eq!(details["status"], "running");
    let pid = details["pid"].as_u64().expect("running PID");
    std::fs::write(root.join("release"), "continue").expect("release command");

    let settled = block_on_local(pi::jobs::wait_async(&owner, &id, Duration::from_secs(10)))
        .expect("wait for promoted command");
    assert_eq!(settled.status, "exited");
    assert_eq!(settled.exit_code, Some(0));
    assert_eq!(settled.output_tail, "startedfinished");
    assert_eq!(settled.output_bytes, "startedfinished".len());
    let starts = std::fs::read_to_string(root.join("starts")).expect("start PIDs");
    let completed = std::fs::read_to_string(root.join("completed")).expect("completion PID");
    assert_eq!(starts.lines().count(), 1, "command must execute once");
    assert_eq!(starts.trim(), pid.to_string());
    assert_eq!(
        completed.trim(),
        pid.to_string(),
        "promotion must retain its PID"
    );
    assert_eq!(pi::jobs::take_completion_notices(&owner).len(), 1);
    assert!(pi::jobs::take_completion_notices(&owner).is_empty());
    finish_case(&harness, case);
}

#[test]
fn automatic_promotion_keeps_the_original_execution_deadline() {
    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock");
    let case = "automatic_promotion_keeps_the_original_execution_deadline";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let owner = format!("jobs-auto-timeout-{}", uuid::Uuid::new_v4().simple());
    let output = execute(
        &auto_bash_tool(&root, &owner, 0),
        json!({"command": "printf timeout-marker; sleep 300", "timeout": 1}),
    );
    let id = job_id(&output);
    assert_eq!(
        output.details.as_ref().expect("job details")["autoBackgrounded"],
        true
    );
    let settled = block_on_local(pi::jobs::wait_async(&owner, &id, Duration::from_secs(10)))
        .expect("wait for original deadline");
    let status = settled.status.clone();
    // Clean up even if a regression reset the timeout to the 30-minute
    // explicit-background default.
    if status == "running" {
        block_on_local(pi::jobs::kill_session(&owner)).expect("timeout test cleanup");
    }
    assert_eq!(status, "timedOut", "promotion must not reset the timeout");
    assert!(settled.output_tail.contains("timeout-marker"));
    assert_eq!(pi::jobs::take_completion_notices(&owner).len(), 1);
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn dropping_an_automatic_foreground_call_kills_its_process_tree() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock");
    let case = "dropping_an_automatic_foreground_call_kills_its_process_tree";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let owner = format!("jobs-auto-drop-{}", uuid::Uuid::new_v4().simple());
    let tool = auto_bash_tool(&root, &owner, 60_000);
    let ready = Arc::new(AtomicBool::new(false));
    let update_ready = Arc::clone(&ready);
    block_on_local(async {
        let pending = Box::pin(tool.execute(
            "cancelled-call",
            json!({
                "command": "trap '' TERM; sleep 300 & echo $! > child.pid; echo $$ > parent.pid; printf cancel-ready; wait",
                "timeout": 120
            }),
            Some(Box::new(move |update| {
                if update.content.iter().any(|block| {
                    matches!(block, pi::model::ContentBlock::Text(text)
                        if text.text.contains("cancel-ready"))
                }) {
                    update_ready.store(true, Ordering::Release);
                }
            })),
        ));
        let observed = Box::pin(async {
            let started = std::time::Instant::now();
            while !ready.load(Ordering::Acquire) {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "command did not stream its readiness marker"
                );
                asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(25))
                    .await;
            }
        });
        match futures::future::select(pending, observed).await {
            futures::future::Either::Left((result, _)) => {
                panic!("foreground command finished before it was dropped: {result:?}");
            }
            futures::future::Either::Right(((), pending)) => {
                // Drop the owned future, not a Pin<&mut _> that leaves the
                // actual execution future alive in the test stack.
                drop(pending);
            }
        }
    });

    let read_pid = |name| {
        std::fs::read_to_string(root.join(name))
            .expect("PID file")
            .trim()
            .parse::<u32>()
            .expect("PID")
    };
    let parent_pid = read_pid("parent.pid");
    let child_pid = read_pid("child.pid");
    let child_is_running = |pid| {
        #[cfg(target_os = "linux")]
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && stat
                .rsplit_once(')')
                .is_some_and(|(_, fields)| fields.split_whitespace().next() == Some("Z"))
        {
            // KILL can orphan a dead grandchild before a container's PID 1
            // reaps it. That zombie cannot run; the monitor still must reap
            // its own parent, which keeps the strict kill_zero check below.
            return false;
        }
        kill_zero(pid)
    };
    let started = std::time::Instant::now();
    while (kill_zero(parent_pid) || child_is_running(child_pid))
        && started.elapsed() < Duration::from_secs(10)
    {
        std::thread::sleep(Duration::from_millis(25));
    }
    let survived = kill_zero(parent_pid) || child_is_running(child_pid);
    // Observe the drop contract before cleanup, so explicit session shutdown
    // cannot hide a missing foreground cancellation guard.
    block_on_local(pi::jobs::kill_session(&owner)).expect("settle remaining cleanup");
    assert!(!survived, "dropped foreground command left a live process");
    assert!(pi::jobs::list(&owner).expect("list").is_empty());
    assert!(pi::jobs::take_completion_notices(&owner).is_empty());
    finish_case(&harness, case);
}

#[test]
fn managed_commands_inherit_configured_child_proxy_environment() {
    struct RestoreProxy(pi::http::proxy::ProxyConfig);
    impl Drop for RestoreProxy {
        fn drop(&mut self) {
            pi::http::proxy::install(self.0.clone());
        }
    }

    let _guard = JOBS_TEST_LOCK.lock().expect("jobs test lock");
    let case = "managed_commands_inherit_configured_child_proxy_environment";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let owner = format!("jobs-proxy-{}", uuid::Uuid::new_v4().simple());
    let _restore = RestoreProxy(pi::http::proxy::active());
    let settings = pi::http::proxy::HttpSettings {
        http_proxy: Some("http://127.0.0.1:49199".to_string()),
        ignore_env_proxy: Some(true),
        ..Default::default()
    };
    let (proxy, warnings) = pi::http::proxy::ProxyConfig::resolve(Some(&settings), &|_| None);
    assert!(warnings.is_empty(), "{warnings:?}");
    pi::http::proxy::install(proxy);

    let tool = auto_bash_tool(&root, &owner, 60_000);
    let inline = execute(&tool, json!({"command": "printf '%s' \"$HTTP_PROXY\""}));
    assert!(!inline.is_error, "{}", first_text(&inline));
    assert_eq!(first_text(&inline), "http://127.0.0.1:49199");
    let background = execute(
        &tool,
        json!({"command": "printf '%s' \"$HTTP_PROXY\"", "background": true}),
    );
    let id = job_id(&background);
    let settled = block_on_local(pi::jobs::wait_async(&owner, &id, Duration::from_secs(10)))
        .expect("proxy command completion");
    assert_eq!(settled.status, "exited");
    assert_eq!(settled.output_tail, "http://127.0.0.1:49199");
    let _ = pi::jobs::take_completion_notices(&owner);
    finish_case(&harness, case);
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

    // Automatic mode may safely use the ordinary foreground runner when
    // the registry rejects admission before spawning any process.
    let owner = format!("jobs-capacity-inline-{}", uuid::Uuid::new_v4().simple());
    let inline = execute(
        &auto_bash_tool(&root, &owner, 0),
        json!({"command": "printf once >> inline-starts; printf capacity-inline"}),
    );
    assert!(!inline.is_error, "{}", first_text(&inline));
    assert_eq!(first_text(&inline), "capacity-inline");
    assert!(inline.details.is_none());
    assert_eq!(
        std::fs::read_to_string(root.join("inline-starts")).expect("inline counter"),
        "once"
    );
    assert!(pi::jobs::take_completion_notices(&owner).is_empty());

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
