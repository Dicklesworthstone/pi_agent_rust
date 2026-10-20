//! Exercise actual background admission, jobs and the native DAG runner.
//! Shell fixtures emit the child protocol; they never pretend to be a model.

use super::*;
use crate::jobs::{JobSessionScope, JobSnapshot};
use crate::tools::{Tool, ToolModelScope};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::time::{Duration, Instant};

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8).build().expect("native workflow test runtime")
        .block_on(future)
}

fn event(case: &str, phase: &str) {
    eprintln!("{}", json!({"suite":"background_workflow", "case":case, "phase":phase}));
}

fn fixture(script: &str) -> (tempfile::TempDir, SubagentTool, String) {
    let root = tempfile::tempdir().expect("isolated workflow fixture");
    let global = root.path().join("global");
    std::fs::create_dir_all(global.join("agents")).expect("agent directory");
    std::fs::write(global.join("agents/worker.md"),
        "---\nname: worker\ndescription: native graph fixture\n---\nOriginal graph definition.\n")
        .expect("agent definition");
    let child = root.path().join("child.sh");
    std::fs::write(&child, format!("#!/bin/sh\nset -eu\n{script}\n")).expect("child fixture");
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700))
        .expect("executable fixture");
    let session = format!("workflow-background-{}", uuid::Uuid::new_v4());
    let mut tool = SubagentTool::with_paths(root.path().to_path_buf(), global, child)
        .with_timeout(Duration::from_secs(20));
    tool.bind_job_session_scope(JobSessionScope::fixed(&session));
    (root, tool, session)
}

fn end(text: &str) -> String {
    let output = json!({"type":"agent_end", "messages":[{
        "role":"assistant", "stopReason":"stop", "content":[{"type":"text", "text":text}]
    }]}).to_string();
    format!("printf '%s\\n' '{}'", output.replace('\'', "'\"'\"'"))
}

fn node(id: &str, needs: &[&str]) -> Value {
    json!({"id":id, "needs":needs, "call":{"agent":"worker", "task":id}})
}

fn request(nodes: Vec<Value>) -> Value {
    json!({"async":true, "workflow":nodes, "concurrency":2})
}

async fn marker(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "fixture marker never appeared: {}", path.display());
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
    }
}

fn job_id(output: &ToolOutput) -> &str {
    assert!(!output.is_error, "{:?}", output.content);
    let details = output.details.as_ref().expect("background receipt");
    assert_eq!(details["schema"], "pi.subagent.background.v1");
    assert_eq!(details["mode"], "workflow");
    details["jobId"].as_str().expect("job identifier")
}

async fn settled(session: &str, id: &str) -> JobSnapshot {
    let snapshot = crate::jobs::wait_async(session, id, Duration::from_secs(10))
        .await.expect("owning session can wait for its graph");
    assert_ne!(snapshot.status, "running", "{snapshot:?}");
    assert_eq!(snapshot.schema, "pi.host_job.v1");
    snapshot
}

#[test]
fn graph_returns_early_and_preserves_waves_typed_inputs_and_frozen_launches() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("typed-diamond", "start");
    // The last argv is the literal task plus any framed typed inputs. The two
    // middle children cannot finish until both have started, proving concurrency.
    let script = format!(r#"
for task do :; done
case "$task" in
  'Task: seed'*) touch seed.started; while [ ! -f release ]; do sleep 0.02; done; {} ;;
  'Task: left'*) printf '%s\n' "$@" > left.argv; touch left.started; while [ ! -f right.started ]; do sleep 0.02; done; {} ;;
  'Task: right'*) printf '%s\n' "$@" > right.argv; touch right.started; while [ ! -f left.started ]; do sleep 0.02; done; {} ;;
  'Task: join'*) printf '%s\n' "$task" > join.task; test -f left.started; test -f right.started; {} ;;
  *) exit 19 ;;
esac
"#, end("41"), end("42"), end("43"), end("85"));
    let (root, mut tool, session) = fixture(&script);
    let model = ToolModelScope::default();
    model.set("provider-before", "model-before");
    tool.bind_model_scope(model.clone());
    let mut seed = node("seed", &[]);
    seed["call"]["outputSchema"] = json!({"type":"integer"});
    let mut left = node("left", &["seed"]);
    left["inputs"] = json!({"source":{"from":"seed","pointer":""}});
    left["call"]["outputSchema"] = json!({"type":"integer"});
    let mut right = left.clone();
    right["id"] = json!("right");
    right["call"]["task"] = json!("right");
    let mut join = node("join", &["left", "right"]);
    join["inputs"] = json!({"left":{"from":"left","pointer":""}, "right":{"from":"right","pointer":""}});
    run(async {
        let receipt = tool.execute("diamond", request(vec![join, left, right, seed]), None).await?;
        let id = job_id(&receipt);
        marker(&root.path().join("seed.started")).await;
        let pending = crate::jobs::wait_async(&session, id, Duration::ZERO).await?;
        assert_eq!(pending.status, "running");
        assert!(pending.result.is_none());
        assert!(crate::jobs::take_completion_notices(&session).is_empty());
        assert!(!root.path().join("left.started").exists());
        model.set("provider-after", "model-after");
        std::fs::write(tool.global_dir.join("agents/worker.md"),
            "---\nname: worker\ndescription: changed\nmodel: changed-model\n---\nChanged graph definition.\n")?;
        std::fs::write(root.path().join("release"), "go")?;
        let snapshot = settled(&session, id).await;
        assert_eq!(snapshot.status, "exited", "{snapshot:?}");
        let output = snapshot.result.expect("native workflow result");
        assert!(!output.is_error);
        let details = output.details.expect("workflow report");
        assert_eq!(details["schema"], "pi.subagent.workflow.v1");
        assert_eq!(details["acceptedNodes"], 4);
        assert_eq!(details["wavesExecuted"], 3);
        assert_eq!(details["nodes"][0]["id"], "join");
        assert_eq!(details["nodes"][0]["wave"], 2);
        assert_eq!(details["nodes"][1]["result"]["data"], 42);
        for index in 0..4 {
            assert_eq!(details["nodes"][index]["state"], "accepted");
            assert!(details["nodes"][index]["hubId"].as_str().is_some());
        }
        for branch in ["left", "right"] {
            let argv = std::fs::read_to_string(root.path().join(format!("{branch}.argv")))?;
            assert!(argv.contains("Original graph definition."));
            assert!(argv.contains("provider-before") && argv.contains("model-before"));
            assert!(!argv.contains("Changed graph definition.") && !argv.contains("model-after"));
            assert!(argv.contains("\"source\":41"));
        }
        let input = std::fs::read_to_string(root.path().join("join.task"))?;
        assert!(input.contains("\"left\":42") && input.contains("\"right\":43"));
        assert_eq!(crate::jobs::take_completion_notices(&session).len(), 1);
        assert!(crate::jobs::take_completion_notices(&session).is_empty());
        Ok::<_, Box<dyn std::error::Error>>(())
    })?;
    event("typed-diamond", "passed");
    Ok(())
}

#[test]
fn failed_branch_skips_descendants_but_keeps_independent_results() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("branch-failure", "start");
    let script = format!(r#"
for task do :; done
case "$task" in
  'Task: bad') exit 23 ;;
  'Task: independent') touch independent.ran; {} ;;
  'Task: blocked') touch must-not-run; {} ;;
  *) exit 19 ;;
esac
"#, end("independent output"), end("forbidden dependent output"));
    let (root, tool, session) = fixture(&script);
    run(async {
        let output = tool.execute("fail", request(vec![node("bad", &[]),
            node("blocked", &["bad"]), node("independent", &[])]), None).await?;
        let snapshot = settled(&session, job_id(&output)).await;
        assert_eq!(snapshot.status, "failed");
        let result = snapshot.result.expect("failed native report");
        assert!(result.is_error);
        let report = result.details.expect("graph detail survives failure");
        assert_eq!(report["rejectedNodes"], 1);
        assert_eq!(report["skippedNodes"], 1);
        assert_eq!(report["acceptedNodes"], 1);
        assert_eq!(report["nodes"][1]["state"], "skipped");
        assert_eq!(report["nodes"][1]["blockedBy"], json!(["bad"]));
        assert_eq!(report["nodes"][1]["result"], Value::Null);
        assert!(root.path().join("independent.ran").exists());
        assert!(!root.path().join("must-not-run").exists());
        Ok::<_, Box<dyn std::error::Error>>(())
    })?;
    event("branch-failure", "passed");
    Ok(())
}

#[test]
fn typed_fanout_and_downstream_join_use_the_same_native_result_shape() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("fanout", "start");
    let script = format!(r#"
for task do :; done
case "$task" in
  'Task: source') {} ;;
  'Task: item'*) {} ;;
  'Task: join'*) printf '%s\n' "$task" > join.task; {} ;;
  *) exit 19 ;;
esac
"#, end("[1,2,3]"), end("7"), end("done"));
    let (root, tool, session) = fixture(&script);
    let mut source = node("source", &[]);
    source["call"]["outputSchema"] = json!({"type":"array","items":{"type":"integer"}});
    let mut item = node("item", &["source"]);
    item["call"]["outputSchema"] = json!({"type":"integer"});
    item["forEach"] = json!({"from":"source","pointer":"","as":"value","maxItems":3});
    let mut join = node("join", &["item"]);
    join["inputs"] = json!({"results":{"from":"item","pointer":""}});
    run(async {
        let output = tool.execute("fanout", request(vec![source, item, join]), None).await?;
        let snapshot = settled(&session, job_id(&output)).await;
        assert_eq!(snapshot.status, "exited");
        let detail = snapshot.result.expect("native result").details.expect("graph details");
        assert_eq!(detail["nodes"][1]["fanout"]["itemCount"], 3);
        assert_eq!(detail["nodes"][1]["fanout"]["data"], json!([7,7,7]));
        let items = detail["nodes"][1]["fanout"]["items"].as_array().expect("item results");
        assert_eq!(items.len(), 3);
        for (index, item) in items.iter().enumerate() {
            assert_eq!(item["index"], index);
            assert_eq!(item["state"], "accepted");
            assert!(item["hubId"].as_str().is_some());
        }
        assert!(std::fs::read_to_string(root.path().join("join.task"))?.contains("\"results\":[7,7,7]"));
        Ok::<_, Box<dyn std::error::Error>>(())
    })?;
    event("fanout", "passed");
    Ok(())
}

#[test]
fn whole_graph_preflight_refuses_invalid_tail_without_any_job_or_process() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("whole-graph-admission", "start");
    let (root, tool, session) = fixture("touch must-not-run");
    let mut missing = node("later", &["valid"]);
    missing["call"]["agent"] = json!("not-an-agent");
    let mut bad_schema = node("later", &["valid"]);
    bad_schema["call"]["outputSchema"] = json!({"type":"not-a-schema-type"});
    let mut missing_schema = node("later", &["valid"]);
    missing_schema["inputs"] = json!({"data":{"from":"valid","pointer":""}});
    let mut mixed = request(vec![node("valid", &[])]);
    mixed["tasks"] = json!([]);
    run(async {
        for input in [
            request(vec![node("valid", &[]), missing]),
            request(vec![node("valid", &[]), bad_schema]),
            request(vec![node("valid", &[]), missing_schema]),
            request(vec![node("valid", &[]), node("a", &["b"]), node("b", &["a"])]),
            request(vec![node("valid", &[]), node("later", &["missing"])]),
            mixed,
        ] {
            let error = tool.execute("invalid", input, None).await.expect_err("invalid graph must refuse");
            assert!(error.to_string().contains("PI_WORKFLOW_INVALID"), "{error}");
        }
        assert!(crate::jobs::list(&session)?.is_empty());
        assert!(crate::jobs::take_completion_notices(&session).is_empty());
        assert!(!root.path().join("must-not-run").exists());
        Ok::<_, Box<dyn std::error::Error>>(())
    })?;
    event("whole-graph-admission", "passed");
    Ok(())
}

#[test]
fn cancellation_retires_every_active_root_and_never_starts_a_dependent() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("cancel-roots", "start");
    let (root, tool, session) = fixture(r#"
for task do :; done
task=${task#Task: }
case "$task" in
  left|right) printf '%s\n' "$$" > "$task.pid"; exec sleep 30 ;;
  join) touch must-not-run; exit 0 ;;
  *) exit 19 ;;
esac
"#);
    run(async {
        let receipt = tool.execute("cancel", request(vec![node("left", &[]),
            node("right", &[]), node("join", &["left","right"])]), None).await?;
        let id = job_id(&receipt);
        marker(&root.path().join("left.pid")).await;
        marker(&root.path().join("right.pid")).await;
        let foreign = crate::jobs::cancel_async("foreign-owner", id).await.expect_err("owner isolation");
        assert!(foreign.to_string().contains("PI_JOBS_UNKNOWN_ID"));
        let stopped = crate::jobs::cancel_async(&session, id).await?;
        assert_eq!(stopped.status, "killed");
        assert!(stopped.result.expect("cancel result").is_error);
        for branch in ["left", "right"] {
            let pid = std::fs::read_to_string(root.path().join(format!("{branch}.pid")))?;
            let status = std::process::Command::new("kill").args(["-0", pid.trim()])
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status()?;
            assert!(!status.success(), "cancellation returned with {branch} alive");
        }
        assert!(!root.path().join("must-not-run").exists());
        assert_eq!(crate::jobs::take_completion_notices(&session).len(), 1);
        Ok::<_, Box<dyn std::error::Error>>(())
    })?;
    event("cancel-roots", "passed");
    Ok(())
}

#[test]
fn owner_change_after_graph_preparation_cannot_publish_a_job() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("owner-fence", "start");
    let (root, mut tool, session) = fixture("touch must-not-run");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let original = session.clone();
    let scope = JobSessionScope::fixed(&session);
    scope.bind(Arc::new(move || {
        let id = if observed.fetch_add(1, Ordering::SeqCst) == 0 { original.clone() }
            else { "changed-workflow-owner".to_string() };
        Box::pin(async move { Some(id) })
    }));
    tool.bind_job_session_scope(scope);
    let error = run(tool.execute("changed", request(vec![node("valid", &[])]), None))
        .expect_err("changed owner must reject publication");
    assert!(error.to_string().contains("PI_SUBAGENT_SESSION_CHANGED"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(crate::jobs::list(&session)?.is_empty());
    assert!(crate::jobs::list("changed-workflow-owner")?.is_empty());
    assert!(!root.path().join("must-not-run").exists());
    event("owner-fence", "passed");
    Ok(())
}

#[test]
fn preparation_owns_its_inputs_but_not_the_preparing_callers_context() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("owned-preparation", "start");
    let (root, tool, _) = fixture(&format!("touch must-not-run\n{}", end("done")));
    let owner = AgentCx::for_request();
    let input = json!({"workflow":[node("only", &[])]});
    let prepared = {
        let _scope = owner.cx().clone().set_current_restricted();
        let plan = Plan::parse(&input)?.expect("graph");
        let deadline = plan.deadline(&tool)?;
        plan.prepare(&tool, deadline)?
    };
    // A native job installs the original context itself. The frozen graph must
    // neither retain a second context nor bypass the currently installed owner.
    owner.set_cancel_requested(true);
    let error = run(owner.with_current(prepared.run(None)))
        .expect_err("cancelled job owner must prevent all children");
    assert!(error.to_string().contains("PI_WORKFLOW_CANCELLED"));
    assert!(!root.path().join("must-not-run").exists());
    event("owned-preparation", "passed");
    Ok(())
}

#[test]
fn background_schema_and_foreground_false_keep_explicit_mode_contracts() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("foreground-contract", "start");
    let (_root, tool, session) = fixture(&end("done"));
    let schema = tool.parameters();
    assert!(schema["properties"]["async"]["description"].as_str().expect("description").contains("workflow"));
    assert_eq!(schema["properties"]["async"]["type"], "boolean");
    let mut input = request(vec![node("only", &[])]);
    input["async"] = json!(false);
    let output = run(tool.execute("foreground", input, None))?;
    assert!(!output.is_error);
    assert_eq!(output.details.expect("foreground graph report")["schema"], "pi.subagent.workflow.v1");
    assert!(crate::jobs::list(&session)?.is_empty());
    event("foreground-contract", "passed");
    Ok(())
}
