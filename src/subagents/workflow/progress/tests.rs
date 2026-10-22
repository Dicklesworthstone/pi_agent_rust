//! Actual native DAG/job paths with controlled child executables, not model tests.

use crate::jobs::{JobSessionScope, JobSnapshot};
use crate::subagents::SubagentTool;
use crate::tools::Tool;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread().blocking_threads(1, 8)
        .build().expect("workflow checkpoint test runtime").block_on(future)
}

fn end(text: &str) -> String {
    let event = json!({"type":"agent_end", "messages":[{
        "role":"assistant", "stopReason":"stop", "content":[{"type":"text","text":text}]
    }]}).to_string();
    format!("printf '%s\\n' '{}'", event.replace('\'', "'\"'\"'"))
}

fn fixture(script: &str) -> (tempfile::TempDir, SubagentTool, String) {
    let root = tempfile::tempdir().expect("isolated checkpoint workspace");
    let global = root.path().join("global");
    std::fs::create_dir_all(global.join("agents")).expect("agent directory");
    std::fs::write(global.join("agents/worker.md"),
        "---\nname: worker\ndescription: checkpoint fixture\n---\nDo the assigned work.\n")
        .expect("agent definition");
    let child = root.path().join("child.sh");
    std::fs::write(&child, format!("#!/bin/sh\nset -eu\n{script}\n")).expect("child script");
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).expect("executable child");
    let session = format!("workflow-checkpoint-{}", uuid::Uuid::new_v4());
    let mut tool = SubagentTool::with_paths(root.path().to_path_buf(), global, child)
        .with_timeout(Duration::from_secs(20));
    tool.bind_job_session_scope(JobSessionScope::fixed(&session));
    (root, tool, session)
}

fn node(id: &str, needs: &[&str]) -> Value {
    json!({"id":id,"needs":needs,"call":{"agent":"worker","task":id}})
}

fn request(nodes: Vec<Value>) -> Value {
    json!({"async":true,"workflow":nodes,"concurrency":2})
}

async fn launch(tool: &SubagentTool, input: Value) -> String {
    let output = tool.execute("checkpoint", input, None).await.expect("graph admission");
    assert!(!output.is_error);
    output.details.expect("job receipt")["jobId"].as_str().expect("job ID").to_owned()
}

async fn marker(path: &Path) {
    let until = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < until, "child marker absent: {}", path.display());
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
    }
}

async fn settled(session: &str, id: &str) -> JobSnapshot {
    let snapshot = crate::jobs::wait_async(session, id, Duration::from_secs(10)).await.expect("job wait");
    assert_ne!(snapshot.status, "running");
    snapshot
}

fn partial(snapshot: &JobSnapshot, wrapper: &str) -> Value {
    let output = snapshot.result.as_ref().expect("checkpoint result");
    let details = output.details.as_ref().expect("jobs progress envelope");
    assert_eq!(details["schema"], wrapper);
    let partial = &details["partialResult"]["details"];
    assert_eq!(partial["schema"], "pi.subagent.workflow.progress.v1");
    assert_eq!(partial["complete"], false);
    partial.clone()
}

fn staged_script() -> String {
    format!(r#"
for task do :; done
case "$task" in
  'Task: seed') printf 'accepted\n' > accepted.txt; {} ;;
  'Task: hold') test -f accepted.txt; echo "$$" > hold.pid; while [ ! -f release ]; do sleep 0.02; done; {} ;;
  'Task: tail') touch tail.ran; {} ;;
  *) exit 19 ;;
esac
"#, end("41"), end("42"), end("done"))
}

fn staged_request() -> Value {
    let mut seed = node("seed", &[]);
    seed["call"]["outputSchema"] = json!({"type":"integer"});
    request(vec![seed, node("hold", &["seed"]), node("tail", &["hold"])])
}

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git").current_dir(root).args(args)
        .output().expect("fixture git command");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn accepted_worktree_wave_is_inspectable_before_dependants_finish() {
    let _guard = crate::jobs::process_test_guard();
    let (root, mut tool, session) = fixture(&staged_script());
    // Keep transcripts and job artifacts outside the Git snapshot under test.
    let storage = tempfile::tempdir().expect("independent agent storage");
    let global = storage.path().join("global");
    std::fs::create_dir_all(global.join("agents")).expect("external definitions");
    std::fs::copy(tool.global_dir.join("agents/worker.md"), global.join("agents/worker.md"))
        .expect("copy controlled definition");
    tool.global_dir = global;
    git(root.path(), &["init", "-b", "main"]);
    git(root.path(), &["config", "user.name", "Checkpoint fixture"]);
    git(root.path(), &["config", "user.email", "checkpoint@example.invalid"]);
    std::fs::write(root.path().join("tracked.txt"), "baseline\n").expect("baseline");
    git(root.path(), &["add", "tracked.txt"]);
    git(root.path(), &["commit", "-m", "fixture baseline"]);
    let mut input = staged_request();
    input["workflow"][0]["call"]["isolation"] = json!("worktree");
    input["workflow"][0]["call"]["isoApply"] = json!("apply");
    run(async {
        let id = launch(&tool, input).await;
        marker(&root.path().join("hold.pid")).await;
        let observed = crate::jobs::wait_async(&session, &id, Duration::ZERO).await.expect("live snapshot");
        let notices = crate::jobs::take_completion_notices(&session);
        std::fs::write(root.path().join("release"), "go").expect("release native child");
        let final_job = settled(&session, &id).await;
        assert_eq!(observed.status, "running");
        let evidence = partial(&observed, "pi.host_job.progress.v1");
        assert_eq!(evidence["checkpoint"], 1);
        assert_eq!(evidence["settledThroughWave"], 0);
        assert_eq!(evidence["wavesExecuted"], 1);
        assert_eq!(evidence["acceptedNodes"], 1);
        assert_eq!(evidence["pendingNodes"], 2);
        assert_eq!(evidence["nodes"][0]["result"]["data"], 41);
        assert_eq!(evidence["nodes"][0]["result"]["schemaValid"], true);
        assert!(evidence["nodes"][0]["hubId"].as_str().is_some());
        assert_eq!(evidence["nodes"][1]["result"], Value::Null);
        assert!(notices.is_empty(), "checkpoints are not completion notices");
        assert_eq!(std::fs::read_to_string(root.path().join("accepted.txt")).expect("accepted edit"), "accepted\n");
        assert_eq!(final_job.status, "exited");
        let report = final_job.result.expect("terminal native report").details.expect("graph report");
        assert_eq!(report["schema"], "pi.subagent.workflow.v1");
        assert_eq!(report["acceptedNodes"], 3);
        assert!(report.get("partialResult").is_none());
        assert_eq!(crate::jobs::take_completion_notices(&session).len(), 1);
    });
}

#[test]
fn cancellation_preserves_settled_evidence_without_inventing_unfinished_results() {
    let _guard = crate::jobs::process_test_guard();
    let (root, tool, session) = fixture(&staged_script());
    run(async {
        let id = launch(&tool, staged_request()).await;
        marker(&root.path().join("hold.pid")).await;
        let before = crate::jobs::wait_async(&session, &id, Duration::ZERO).await.expect("before cancellation");
        let stopped = crate::jobs::cancel_async(&session, &id).await.expect("owned cancellation");
        assert_eq!(stopped.status, "killed");
        assert!(stopped.result.as_ref().expect("cancelled result").is_error);
        assert_eq!(partial(&stopped, "pi.host_job.interrupted.v1"), partial(&before, "pi.host_job.progress.v1"));
        let evidence = partial(&stopped, "pi.host_job.interrupted.v1");
        for index in [1, 2] {
            assert_eq!(evidence["nodes"][index]["state"], "pending");
            assert_eq!(evidence["nodes"][index]["hubId"], Value::Null);
            assert_eq!(evidence["nodes"][index]["result"], Value::Null);
        }
        assert!(!root.path().join("tail.ran").exists());
        assert!(root.path().join("accepted.txt").exists(), "cancellation is not rollback");
        let pid = std::fs::read_to_string(root.path().join("hold.pid")).expect("real child PID");
        let live = std::process::Command::new("kill").args(["-0", pid.trim()])
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().expect("PID liveness probe");
        assert!(!live.success(), "native child must be reaped before terminal cancellation");
        let again = settled(&session, &id).await;
        assert_eq!(partial(&again, "pi.host_job.interrupted.v1"), evidence);
    });
}

#[test]
fn original_deadline_retains_checkpoint_and_never_launches_later_waves() {
    let _guard = crate::jobs::process_test_guard();
    let (root, tool, session) = fixture(&staged_script());
    let tool = tool.with_timeout(Duration::from_secs(2));
    run(async {
        let started = Instant::now();
        let id = launch(&tool, staged_request()).await;
        marker(&root.path().join("hold.pid")).await;
        let stopped = settled(&session, &id).await;
        assert_eq!(stopped.status, "timedOut");
        assert!(stopped.result.as_ref().expect("timeout result").is_error);
        let evidence = partial(&stopped, "pi.host_job.interrupted.v1");
        assert_eq!(evidence["nodes"][0]["result"]["data"], 41);
        assert_eq!(evidence["acceptedNodes"], 1);
        assert!(!root.path().join("tail.ran").exists());
        assert!(started.elapsed() < Duration::from_secs(10));
    });
}

#[test]
fn an_independent_failure_is_visible_in_progress_without_ending_the_whole_job() {
    let _guard = crate::jobs::process_test_guard();
    let script = format!(r#"
for task do :; done
case "$task" in
  'Task: bad') exit 23 ;;
  'Task: good') {} ;;
  'Task: blocked') touch forbidden; exit 29 ;;
  'Task: hold') touch holding; while [ ! -f release ]; do sleep 0.02; done; {} ;;
  *) exit 19 ;;
esac
"#, end("accepted peer"), end("done"));
    let (root, tool, session) = fixture(&script);
    run(async {
        let id = launch(&tool, request(vec![node("bad", &[]), node("good", &[]),
            node("blocked", &["bad"]), node("hold", &["good"])] )).await;
        marker(&root.path().join("holding")).await;
        let observed = crate::jobs::wait_async(&session, &id, Duration::ZERO).await.expect("mixed wave checkpoint");
        std::fs::write(root.path().join("release"), "go").expect("release independent work");
        let final_job = settled(&session, &id).await;
        assert_eq!(observed.status, "running");
        assert!(!observed.result.as_ref().expect("progress").is_error);
        let evidence = partial(&observed, "pi.host_job.progress.v1");
        assert_eq!(evidence["acceptedNodes"], 1);
        assert_eq!(evidence["rejectedNodes"], 1);
        assert_eq!(evidence["nodes"][0]["state"], "rejected");
        assert_eq!(evidence["nodes"][0]["result"]["isError"], true);
        assert_eq!(final_job.status, "failed");
        let report = final_job.result.expect("final report").details.expect("workflow report");
        assert_eq!(report["skippedNodes"], 1);
        assert_eq!(report["nodes"][2]["state"], "skipped");
        assert!(!root.path().join("forbidden").exists());
    });
}

#[test]
fn settled_fanout_and_deliberate_exclusions_are_not_flattened_or_lost() {
    let _guard = crate::jobs::process_test_guard();
    let script = format!(r#"
for task do :; done
case "$task" in
  'Task: source') {} ;;
  'Task: item'*) {} ;;
  'Task: excluded') touch forbidden; exit 27 ;;
  'Task: join') touch holding; while [ ! -f release ]; do sleep 0.02; done; {} ;;
  *) exit 19 ;;
esac
"#, end("[1,2]"), end("7"), end("done"));
    let (root, tool, session) = fixture(&script);
    let mut source = node("source", &[]);
    source["call"]["outputSchema"] = json!({"type":"array","items":{"type":"integer"}});
    let mut item = node("item", &["source"]);
    item["call"]["outputSchema"] = json!({"type":"integer"});
    item["forEach"] = json!({"from":"source","pointer":"","as":"item","maxItems":2});
    let mut excluded = node("excluded", &["source"]);
    excluded["when"] = json!({"from":"source","pointer":"/0","equals":99});
    let mut join = node("join", &["item", "excluded"]);
    join["join"] = json!("selected");
    run(async {
        let id = launch(&tool, request(vec![source, item, excluded, join])).await;
        marker(&root.path().join("holding")).await;
        let observed = crate::jobs::wait_async(&session, &id, Duration::ZERO).await.expect("fanout checkpoint");
        std::fs::write(root.path().join("release"), "go").expect("release join");
        assert_eq!(settled(&session, &id).await.status, "exited");
        let evidence = partial(&observed, "pi.host_job.progress.v1");
        assert_eq!(evidence["checkpoint"], 2);
        assert_eq!(evidence["notSelectedNodes"], 1);
        assert_eq!(evidence["nodes"][1]["fanout"]["data"], json!([7,7]));
        assert_eq!(evidence["nodes"][1]["hubId"], Value::Null);
        for item in evidence["nodes"][1]["fanout"]["items"].as_array().expect("native item records") {
            assert_eq!(item["state"], "accepted");
            assert!(item["hubId"].as_str().is_some());
        }
        assert_eq!(evidence["nodes"][2]["state"], "not_selected");
        assert_eq!(evidence["nodes"][2]["result"], Value::Null);
        assert!(!root.path().join("forbidden").exists());
    });
}

#[test]
fn empty_fanout_checkpoints_do_not_invent_executed_waves_or_children() {
    let _guard = crate::jobs::process_test_guard();
    let script = format!(r#"
for task do :; done
case "$task" in
  'Task: source') {} ;;
  'Task: item'*) touch forbidden; exit 29 ;;
  'Task: join') touch holding; while [ ! -f release ]; do sleep 0.02; done; {} ;;
  *) exit 19 ;;
esac
"#, end("[]"), end("done"));
    let (root, tool, session) = fixture(&script);
    let mut source = node("source", &[]);
    source["call"]["outputSchema"] = json!({"type":"array"});
    let mut item = node("item", &["source"]);
    item["forEach"] = json!({"from":"source","pointer":"","as":"item","maxItems":2});
    run(async {
        let id = launch(&tool, request(vec![source, item, node("join", &["item"])] )).await;
        marker(&root.path().join("holding")).await;
        let observed = crate::jobs::wait_async(&session, &id, Duration::ZERO).await.expect("empty expansion checkpoint");
        std::fs::write(root.path().join("release"), "go").expect("release join");
        assert_eq!(settled(&session, &id).await.status, "exited");
        let evidence = partial(&observed, "pi.host_job.progress.v1");
        assert_eq!(evidence["checkpoint"], 2);
        assert_eq!(evidence["settledThroughWave"], 1);
        assert_eq!(evidence["wavesExecuted"], 1);
        assert_eq!(evidence["nodes"][1]["fanout"]["itemCount"], 0);
        assert_eq!(evidence["nodes"][1]["hubId"], Value::Null);
        assert_eq!(evidence["nodes"][1]["state"], "accepted");
        assert!(!root.path().join("forbidden").exists());
    });
}

#[test]
fn changed_privacy_evidence_withdraws_previous_checkpoint_and_stops_dependants() {
    let _guard = crate::jobs::process_test_guard();
    let script = format!(r#"
for task do :; done
case "$task" in
  'Task: seed') {} ;;
  'Task: private') {} ;;
  'Task: tail') touch forbidden; {} ;;
  *) exit 19 ;;
esac
"#, end("41"), end(r#"{"value":"WORKFLOW-PRIVATE-CANARY"}"#), end("done"));
    let (root, tool, session) = fixture(&script);
    let settings = crate::secrets::SecretsSettings {
        extra_patterns: Some(vec!["WORKFLOW-PRIVATE-CANARY".into()]), ..Default::default()
    };
    let tool = tool.with_secrets_settings(Some(&settings));
    let mut private = node("private", &["seed"]);
    private["call"]["outputSchema"] = json!({"type":"object","properties":{"value":{"const":"WORKFLOW-PRIVATE-CANARY"}}});
    run(async {
        let id = launch(&tool, request(vec![node("seed", &[]), private, node("tail", &["private"])] )).await;
        let stopped = settled(&session, &id).await;
        assert_eq!(stopped.status, "failed");
        let output = stopped.result.expect("explicit publication failure");
        assert!(output.is_error);
        let wire = serde_json::to_string(&output).expect("failed result JSON");
        assert!(wire.contains("PI_WORKFLOW_PROGRESS_UNAVAILABLE"));
        assert!(!wire.contains("WORKFLOW-PRIVATE-CANARY"));
        assert!(!wire.contains("partialResult"));
        assert!(!root.path().join("forbidden").exists());
        for entry in std::fs::read_dir(root.path().join("global/tool-output-artifacts/jobs")).expect("job artifacts") {
            let path = entry.expect("artifact entry").path();
            if path.is_file() {
                let bytes = std::fs::read(path).expect("artifact bytes");
                assert!(!String::from_utf8_lossy(&bytes).contains("WORKFLOW-PRIVATE-CANARY"));
            }
        }
    });
}

#[test]
fn oversized_complete_evidence_is_not_truncated_into_a_successful_checkpoint() {
    let _guard = crate::jobs::process_test_guard();
    let script = format!(r#"
for task do :; done
case "$task" in
  'Task: left'|'Task: right') cat large.jsonl ;;
  'Task: tail') touch forbidden; {} ;;
  *) exit 19 ;;
esac
"#, end("done"));
    let (root, tool, session) = fixture(&script);
    let text = serde_json::to_string(&"z".repeat(200_000)).expect("large schema-valid string");
    let event = json!({"type":"agent_end","messages":[{
        "role":"assistant","stopReason":"stop","content":[{"type":"text","text":text}]
    }]}).to_string();
    std::fs::write(root.path().join("large.jsonl"), format!("{event}\n")).expect("bounded native output fixture");
    let mut left = node("left", &[]);
    left["call"]["outputSchema"] = json!({"type":"string"});
    let mut right = node("right", &[]);
    right["call"]["outputSchema"] = json!({"type":"string"});
    run(async {
        let id = launch(&tool, request(vec![left, right, node("tail", &["left","right"])] )).await;
        let stopped = settled(&session, &id).await;
        assert_eq!(stopped.status, "failed");
        let output = stopped.result.expect("bounded failure result");
        let wire = serde_json::to_string(&output).expect("failure JSON");
        assert!(wire.contains("PI_WORKFLOW_PROGRESS_LIMIT"), "{wire}");
        assert!(!wire.contains("partialResult"));
        assert!(!root.path().join("forbidden").exists());
    });
}
