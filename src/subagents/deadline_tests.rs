//! Real subprocess tests for request-wide execution budgets.

#![cfg(unix)]

use super::*;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tempfile::TempDir;

fn fixture(script: &str, timeout: Duration) -> (TempDir, SubagentTool) {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("global");
    std::fs::create_dir_all(global.join("agents")).unwrap();
    std::fs::write(
        global.join("agents/worker.md"),
        "---\nname: worker\ndescription: bounded child\n---\nFinish the assignment.\n",
    )
    .unwrap();
    let binary = root.path().join("child.sh");
    std::fs::write(&binary, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let tool =
        SubagentTool::with_paths(root.path().to_path_buf(), global, binary).with_timeout(timeout);
    (root, tool)
}

fn end(text: &str) -> String {
    let event = json!({"type":"agent_end","messages":[{
        "role":"assistant","stopReason":"stop","content":[{"type":"text","text":text}]
    }]})
    .to_string();
    format!("printf '%s\\n' '{}'\n", event.replace('\'', "'\"'\"'"))
}

fn request() -> Value {
    json!({"agent":"worker","task":"finish"})
}

fn run(
    tool: &SubagentTool,
    input: Value,
    callback: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
) -> Result<ToolOutput> {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
        .block_on(tool.execute("deadline-test", input, callback))
}

fn results(output: &ToolOutput) -> &Vec<Value> {
    output.details.as_ref().unwrap()["results"]
        .as_array()
        .unwrap()
}

fn assert_timeout(value: &Value) {
    assert_eq!(value["status"], "failed", "{value}");
    assert!(
        value["error"]
            .as_str()
            .unwrap()
            .contains("PI_SUBAGENT_TIMEOUT"),
        "{value}"
    );
}

#[test]
fn invalid_request_timeouts_never_launch() {
    let (_root, tool) = fixture("touch launched", Duration::from_secs(5));
    assert_eq!(
        tool.parameters()["properties"]["timeoutSeconds"]["maximum"],
        86_400
    );
    for timeout in [
        json!(0),
        Value::Null,
        json!(-1),
        json!(1.5),
        json!("5"),
        json!(86_401),
    ] {
        let mut input = request();
        input["timeoutSeconds"] = timeout;
        assert!(run(&tool, input, None).is_err());
        assert!(!tool.cwd.join("launched").exists());
    }
}

#[test]
fn an_uncooperative_running_child_is_terminated_by_the_parent() {
    let (_root, tool) = fixture(
        "printf 'started\\n' > launched\nexec sleep 30",
        Duration::from_secs(3),
    );
    let started = Instant::now();
    let output = run(&tool, request(), None).unwrap();
    assert!(output.is_error);
    let value = &results(&output)[0];
    assert_timeout(value);
    assert!(value["pid"].as_u64().is_some(), "exercise a running child");
    assert!(tool.cwd.join("launched").exists());
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "must not wait for sleep 30"
    );
    let pid = value["pid"].as_u64().unwrap();
    let entry = crate::agent_hub::registry()
        .lock()
        .unwrap()
        .roster()
        .into_iter()
        .find(|entry| entry.pid.map(u64::from) == Some(pid))
        .unwrap();
    assert_eq!(entry.status, crate::agent_hub::ChildStatus::Failed);
}

#[test]
fn a_terminal_frame_cannot_hide_a_process_that_never_exits() {
    let (_root, tool) = fixture(
        &format!("{}exec sleep 30", end("looks complete")),
        Duration::from_secs(3),
    );
    let output = run(&tool, request(), None).unwrap();
    assert!(output.is_error);
    assert_timeout(&results(&output)[0]);
    assert_eq!(results(&output)[0]["output"], "looks complete");
}

#[test]
fn queued_parallel_work_consumes_the_original_budget() {
    let (_root, tool) = fixture(
        "printf 'started\\n' >> launches\nexec sleep 30",
        Duration::from_secs(3),
    );
    let output = run(
        &tool,
        json!({"concurrency":1,"tasks":[
            {"agent":"worker","task":"first"}, {"agent":"worker","task":"queued"}
        ]}),
        None,
    )
    .unwrap();
    assert_eq!(results(&output).len(), 2);
    assert_timeout(&results(&output)[0]);
    assert_timeout(&results(&output)[1]);
    assert!(results(&output)[0]["pid"].as_u64().is_some());
    assert!(
        results(&output)[1]["pid"].is_null(),
        "expired queued work must never spawn"
    );
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("launches")).unwrap(),
        "started\n"
    );
}

#[test]
fn starting_callback_can_exhaust_budget_before_spawn() {
    let (_root, tool) = fixture("touch launched", Duration::from_secs(1));
    let observed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&observed);
    let output = run(
        &tool,
        request(),
        Some(Box::new(move |update| {
            if update
                .details
                .as_ref()
                .is_some_and(|value| value["result"]["status"] == "starting")
            {
                flag.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(1100));
            }
        })),
    )
    .unwrap();
    assert!(observed.load(Ordering::SeqCst));
    assert_timeout(&results(&output)[0]);
    assert!(results(&output)[0]["pid"].is_null());
    assert!(!tool.cwd.join("launched").exists());
}

#[test]
fn chained_steps_do_not_get_a_fresh_timeout() {
    let (_root, tool) = fixture(
        &format!("printf 'started\\n' >> launches\n{}", end("first")),
        Duration::from_secs(1),
    );
    let observed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&observed);
    let output = run(
        &tool,
        json!({"chain":[
            {"agent":"worker","task":"first"}, {"agent":"worker","task":"second"}
        ]}),
        Some(Box::new(move |update| {
            if update
                .details
                .as_ref()
                .is_some_and(|value| value["result"]["status"] == "completed")
                && !flag.swap(true, Ordering::SeqCst)
            {
                std::thread::sleep(Duration::from_millis(1100));
            }
        })),
    )
    .unwrap();
    assert!(observed.load(Ordering::SeqCst));
    assert_eq!(results(&output)[0]["status"], "completed");
    assert_timeout(&results(&output)[1]);
    assert!(results(&output)[1]["pid"].is_null());
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("launches")).unwrap(),
        "started\n"
    );
}

#[test]
fn an_expired_budget_does_not_launch_a_corrective_retry() {
    let (_root, tool) = fixture(
        &format!("printf 'started\\n' >> launches\n{}", end("not JSON")),
        Duration::from_secs(1),
    );
    let observed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&observed);
    let mut input = request();
    input["outputSchema"] = json!({"type":"object"});
    let output = run(
        &tool,
        input,
        Some(Box::new(move |update| {
            if update
                .details
                .as_ref()
                .is_some_and(|value| value["result"]["schemaValid"] == false)
                && !flag.swap(true, Ordering::SeqCst)
            {
                std::thread::sleep(Duration::from_millis(1100));
            }
        })),
    )
    .unwrap();
    assert!(observed.load(Ordering::SeqCst));
    assert_timeout(&results(&output)[0]);
    assert_eq!(results(&output)[0]["schemaRetries"], 0);
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("launches")).unwrap(),
        "started\n"
    );
}

#[test]
fn corrective_processes_receive_the_same_absolute_deadline() {
    let script = format!(
        "test \"$1\" = --max-time || exit 11\n\
         test \"$2\" -gt 0 || exit 12\n\
         printf '%s\\n' \"$PI_SUBAGENT_DEADLINE_UNIX_MS\" >> deadlines\n\
         if [ -f first ]; then\n{}else\n: > first\n{}fi\n",
        end(r#"{"ok":true}"#),
        end("not JSON"),
    );
    let (_root, tool) = fixture(&script, Duration::from_secs(10));
    let mut input = request();
    input["outputSchema"] = json!({"type":"object"});
    let output = run(&tool, input, None).unwrap();
    assert!(!output.is_error, "{output:?}");
    assert_eq!(results(&output)[0]["schemaRetries"], 1);
    let recorded = std::fs::read_to_string(tool.cwd.join("deadlines")).unwrap();
    let values: Vec<_> = recorded.lines().collect();
    assert_eq!(values.len(), 2);
    assert_eq!(
        values[0], values[1],
        "a retry must inherit the original ceiling"
    );
    assert!(values[0].parse::<u64>().unwrap() > 0);
}

#[test]
fn expired_isolated_work_is_preserved_and_never_applied() {
    let (_root, tool) = fixture(
        "printf 'unaccepted\\n' > tracked.txt\nexec sleep 30",
        Duration::from_secs(5),
    );
    for args in [
        vec!["init", "--quiet", "-b", "main"],
        vec!["config", "user.name", "Pi Budget Fixture"],
        vec!["config", "user.email", "budget@localhost"],
        vec!["config", "commit.gpgSign", "false"],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&tool.cwd)
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::write(tool.cwd.join("tracked.txt"), "original\n").unwrap();
    for args in [vec!["add", "."], vec!["commit", "--quiet", "-m", "fixture"]] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&tool.cwd)
                .status()
                .unwrap()
                .success()
        );
    }
    let output = run(
        &tool,
        json!({"tasks":[{
            "agent":"worker","task":"change file","isolation":"worktree","isoApply":"apply"
        }]}),
        None,
    )
    .unwrap();
    let value = &results(&output)[0];
    assert_timeout(value);
    assert_eq!(value["iso"]["applyMode"], "keep");
    assert_eq!(value["iso"]["applied"], false);
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("tracked.txt")).unwrap(),
        "original\n"
    );
    let retained = Path::new(value["iso"]["worktreePath"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(retained.join("tracked.txt")).unwrap(),
        "unaccepted\n"
    );
}

#[test]
fn background_tan_uses_the_same_host_budget() {
    let (_root, tool) = fixture("exec sleep 30", Duration::from_secs(3));
    let completion = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
        .block_on(tool.run_background_tan("bounded background work"))
        .unwrap();
    assert!(completion.is_error);
    assert_eq!(completion.status, "failed");
    assert!(completion.error.unwrap().contains("PI_SUBAGENT_TIMEOUT"));
}
