//! Actual SubagentTool admission, RPC children, retained results and privacy.
//! The deterministic child is a protocol peer, not a model or schema oracle.

use crate::subagents::{SubagentTool, worker};
use crate::tools::{Tool, ToolOutput};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap().block_on(future)
}

fn event(case: &str) {
    eprintln!("{}", json!({"suite":"typed_rpc_worker", "case":case, "phase":"start"}));
}

const CHILD: &str = r#"#!/bin/sh
set -eu
printf '%s\n' "$$" >> launches
count=0
busy=0
while IFS= read -r line; do
    id=${line#*\"id\":\"}
    id=${id%%\"*}
    case "$line" in
      *'"type":"get_state"'*)
        if [ "$busy" -eq 1 ]; then
          busy=0
          printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":true}}\n' "$id"
        else
          printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":false}}\n' "$id"
        fi ;;
      *'"type":"prompt"'*)
        test "$busy" -eq 0
        count=$((count + 1))
        printf '%s\n' "$line" >> prompts
        printf '{"type":"response","id":"%s","command":"prompt","success":true}\n' "$id"
        printf '%s\n' '{"type":"agent_start"}'
        case "$line" in
          *'Task: gated'*) touch active; while [ ! -f release ]; do sleep 0.02; done ;;
        esac
        case "$line" in
          *'Task: bad'*) answer='{\"value\":\"wrong\"}' ;;
          *'Task: duplicate'*) answer='{\"value\":1,\"\\u0076alue\":2}' ;;
          *'Task: fenced'*) answer='```json\n{\"value\":1}\n```' ;;
          *'Task: null'*) answer='null' ;;
          *'Task: secret'*) answer='\"typed-worker-secret-sentinel\"' ;;
          *) answer="{\\\"value\\\":$count}" ;;
        esac
        printf '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"%s"}]}]}\n' "$answer"
        busy=1 ;;
    esac
done
"#;

struct Fixture {
    root: tempfile::TempDir,
    tool: SubagentTool,
    owner: String,
}

impl Fixture {
    fn new(schema: Option<&Value>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        std::fs::create_dir_all(global.join("agents")).unwrap();
        let output = schema.map_or_else(String::new, |schema| format!("output_schema: {schema}\n"));
        std::fs::write(global.join("agents/typed.md"), format!(
            "---\nname: typed\ndescription: retained typed worker fixture\ntools: []\n{output}---\nAnswer the assigned task.\n"
        )).unwrap();
        let binary = root.path().join("child.sh");
        std::fs::write(&binary, CHILD).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = format!("typed-worker-{}", uuid::Uuid::new_v4());
        let mut tool = SubagentTool::with_paths(root.path().to_path_buf(), global, binary)
            .with_timeout(Duration::from_secs(15));
        tool.bind_job_session_scope(crate::jobs::JobSessionScope::fixed(&owner));
        Self { root, tool, owner }
    }

    async fn call(&self, input: Value) -> crate::error::Result<Value> {
        self.tool.execute("typed-worker", input, None).await.map(|output| {
            assert!(!output.is_error, "native control operation failed");
            output.details.unwrap()
        })
    }

    async fn wait(&self, id: &str, turn: u64) -> Value {
        self.call(json!({"operation":"worker_wait", "workerId":id, "turnId":turn, "timeoutMs":5000}))
            .await.unwrap()
    }

    async fn close(&self, id: &str) {
        self.call(json!({"operation":"worker_close", "workerId":id})).await.unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = run(crate::jobs::kill_session(&self.owner));
    }
}

fn schema() -> Value {
    json!({"type":"object", "required":["value"], "additionalProperties":false,
        "properties":{"value":{"type":"integer"}}})
}

fn result(snapshot: &Value) -> &Value { &snapshot["turn"]["result"]["details"]["results"][0] }

#[test]
fn definition_contract_is_frozen_and_repeated_on_each_reused_turn() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("frozen-definition");
    let fixture = Fixture::new(Some(&schema()));
    run(async {
        let started = fixture.call(json!({"operation":"worker_start","agent":"typed","task":"valid"})).await?;
        let id = started["workerId"].as_str().unwrap();
        let first = fixture.wait(id, 1).await;
        assert_eq!(result(&first)["schemaValid"], true);
        assert_eq!(result(&first)["data"], json!({"value":1}));
        assert_eq!(result(&first)["schemaRetries"], 0);
        std::fs::write(fixture.root.path().join("global/agents/typed.md"),
            "---\nname: typed\ndescription: changed\noutput_schema: {\"type\":\"null\"}\n---\nChanged instructions.\n")?;
        fixture.call(json!({"operation":"worker_send","workerId":id,"task":"valid again"})).await?;
        let second = fixture.wait(id, 2).await;
        assert_eq!(result(&second)["data"], json!({"value":2}));
        assert_eq!(result(&first)["pid"], result(&second)["pid"]);
        assert_eq!(std::fs::read_to_string(fixture.root.path().join("launches"))?.lines().count(), 1);
        let prompts = std::fs::read_to_string(fixture.root.path().join("prompts"))?;
        assert_eq!(prompts.lines().count(), 2);
        for line in prompts.lines() {
            let request: Value = serde_json::from_str(line)?;
            let message = request["message"].as_str().unwrap();
            assert!(message.contains("Output contract for this turn:"));
            assert!(message.contains("\"type\":\"integer\""));
            assert!(!message.contains("Changed instructions."));
        }
        fixture.close(id).await;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}

#[test]
fn explicit_launch_schema_supports_null_and_overrides_definition_without_new_turn_policy() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("explicit-null");
    let fixture = Fixture::new(Some(&schema()));
    run(async {
        let started = fixture.call(json!({"operation":"worker_start","agent":"typed","task":"null",
            "outputSchema":{"type":"null"},"schemaMode":"strict"})).await?;
        let id = started["workerId"].as_str().unwrap();
        let settled = fixture.wait(id, 1).await;
        assert_eq!(settled["turn"]["status"], "completed");
        assert_eq!(result(&settled)["schemaValid"], true);
        assert!(result(&settled).as_object().unwrap().contains_key("data"));
        assert_eq!(result(&settled)["data"], Value::Null);
        assert!(fixture.call(json!({"operation":"worker_send","workerId":id,"task":"valid",
            "outputSchema":true})).await.is_err());
        fixture.close(id).await;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}

#[test]
fn invalid_answers_fail_only_their_turn_and_never_trigger_automatic_retries() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("strict-failure-no-retry");
    let fixture = Fixture::new(Some(&schema()));
    run(async {
        let started = fixture.call(json!({"operation":"worker_start","agent":"typed","task":"bad"})).await?;
        let id = started["workerId"].as_str().unwrap();
        for (turn, prompt) in [(1, "bad"), (2, "duplicate"), (3, "fenced")] {
            if turn > 1 {
                fixture.call(json!({"operation":"worker_send","workerId":id,"task":prompt})).await?;
            }
            let failed = fixture.wait(id, turn).await;
            assert_eq!(failed["turn"]["status"], "failed");
            assert_eq!(result(&failed)["schemaValid"], false);
            assert_eq!(result(&failed)["schemaRetries"], 0);
            assert!(result(&failed).get("data").is_none());
            assert_eq!(failed["turn"]["result"]["isError"], true);
        }
        fixture.call(json!({"operation":"worker_send","workerId":id,"task":"valid"})).await?;
        let good = fixture.wait(id, 4).await;
        assert_eq!(result(&good)["data"], json!({"value":4}));
        assert_eq!(std::fs::read_to_string(fixture.root.path().join("prompts"))?.lines().count(), 4);
        assert_eq!(std::fs::read_to_string(fixture.root.path().join("launches"))?.lines().count(), 1);
        fixture.close(id).await;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}

#[test]
fn invalid_contracts_are_refused_before_carrier_publication_or_child_launch() {
    let _guard = crate::jobs::process_test_guard();
    event("preflight");
    let fixture = Fixture::new(None);
    run(async {
        for invalid in [Value::Null, json!([]), json!({"type":"unknown"}),
            json!({"$ref":"file:///not-a-resource"}), json!({"description":"x".repeat(17000)})] {
            assert!(fixture.call(json!({"operation":"worker_start","agent":"typed","task":"valid",
                "outputSchema":invalid})).await.is_err());
        }
        for extra in [json!({"schemaMode":"strict"}), json!({"outputSchema":true,"schemaMode":"permissive"})] {
            let mut input = json!({"operation":"worker_start","agent":"typed","task":"valid"});
            input.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            assert!(fixture.call(input).await.is_err());
        }
        assert!(!fixture.root.path().join("launches").exists());
    });
}

#[test]
fn schema_validation_does_not_complete_a_running_or_cancelled_turn() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("cancellation");
    let fixture = Fixture::new(Some(&schema()));
    run(async {
        let started = fixture.call(json!({"operation":"worker_start","agent":"typed","task":"gated"})).await?;
        let id = started["workerId"].as_str().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !fixture.root.path().join("active").exists() {
            assert!(std::time::Instant::now() < deadline);
            crate::agent_cx::AgentCx::for_request().time().sleep(Duration::from_millis(10)).await;
        }
        let active = fixture.call(json!({"operation":"worker_status","workerId":id})).await?;
        assert_eq!(active["turn"]["result"], Value::Null);
        fixture.close(id).await;
        let stopped = fixture.call(json!({"operation":"worker_status","workerId":id})).await?;
        assert_eq!(stopped["turn"]["status"], "cancelled");
        assert!(result(&stopped).get("schemaValid").is_none());
        let pid: i32 = std::fs::read_to_string(fixture.root.path().join("launches"))?.trim().parse()?;
        assert!(rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()).is_err());
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}

#[test]
fn privacy_withholds_changed_typed_storage_and_later_observations() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("typed-privacy");
    let fixture = Fixture::new(Some(&json!({"type":"string"})));
    run(async {
        let started = fixture.call(json!({"operation":"worker_start","agent":"typed","task":"secret"})).await?;
        let id = started["workerId"].as_str().unwrap();
        let settled = fixture.wait(id, 1).await;
        assert_eq!(result(&settled)["schemaValid"], true);
        let secret = "typed-worker-secret-sentinel";
        let mut vault = crate::secrets::SecretVault::default();
        crate::secrets::obfuscate(secret, &mut vault, &[regex::Regex::new(secret)?]);
        let privacy = crate::text_completion::AuxiliaryPrivacySnapshot::with_vault(
            crate::text_completion::AuxiliaryPrivacy::from_settings(None), &vault,
        );
        let raw: ToolOutput = serde_json::from_value(settled["turn"]["result"].clone())?;
        let stored = worker::protected_result(Ok(raw), &privacy);
        assert!(stored.is_error);
        assert!(!serde_json::to_string(&stored)?.contains(secret));
        let error = worker::projected_snapshot(settled, &privacy).unwrap_err();
        assert!(error.to_string().contains("PI_SUBAGENT_WORKER_PRIVACY"));
        assert!(!error.to_string().contains(secret));
        fixture.close(id).await;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}

fn pool_result(snapshot: &Value) -> &Value {
    &snapshot["item"]["result"]["details"]["results"][0]
}

async fn wait_item(fixture: &Fixture, pool: &str, item: u64) -> Value {
    fixture.call(json!({"operation":"pool_wait","poolId":pool,"itemId":item,"timeoutMs":5000}))
        .await.unwrap()
}

async fn finish_pool(fixture: &Fixture, pool: &str, job: &str) -> crate::jobs::JobSnapshot {
    fixture.call(json!({"operation":"pool_finish","poolId":pool})).await.unwrap();
    crate::jobs::wait_async(&fixture.owner, job, Duration::from_secs(5)).await.unwrap()
}

#[test]
fn typed_pool_reuses_children_with_one_frozen_contract_and_incremental_results() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("pool-frozen-contract");
    let fixture = Fixture::new(Some(&schema()));
    run(async {
        let started = fixture.call(json!({"operation":"pool_start","agent":"typed",
            "concurrency":2,"tasks":["valid","gated"]})).await?;
        let pool = started["poolId"].as_str().unwrap();
        let job = started["jobId"].as_str().unwrap();
        let first = wait_item(&fixture, pool, 1).await;
        assert_eq!(pool_result(&first)["data"], json!({"value":1}));
        assert_eq!(pool_result(&first)["schemaValid"], true);
        let page = fixture.call(json!({"operation":"pool_drain","poolId":pool})).await?;
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["items"][0]["id"], 1);
        assert_eq!(page["items"][0]["result"]["details"]["results"][0]["data"], json!({"value":1}));
        assert_eq!(page["pendingItems"], 1);
        assert_eq!(page["sealed"], false);

        // Admission after discovery must retain the original contract, even
        // after the definition changes to an incompatible schema on disk.
        std::fs::write(fixture.root.path().join("global/agents/typed.md"),
            "---\nname: typed\ndescription: changed\noutput_schema: {\"type\":\"null\"}\n---\nChanged instructions.\n")?;
        fixture.call(json!({"operation":"pool_push","poolId":pool,"tasks":["valid again"]})).await?;
        let third = wait_item(&fixture, pool, 3).await;
        assert_eq!(pool_result(&third)["data"], json!({"value":2}));
        assert_eq!(pool_result(&third)["pid"], pool_result(&first)["pid"]);
        let next = fixture.call(json!({"operation":"pool_drain","poolId":pool,
            "cursor":page["nextCursor"]})).await?;
        assert_eq!(next["items"][0]["id"], 3);
        assert_eq!(next["pendingItems"], 1);
        std::fs::write(fixture.root.path().join("release"), "continue")?;
        let settled = finish_pool(&fixture, pool, job).await;
        assert_eq!(settled.status, "exited");
        let last = fixture.call(json!({"operation":"pool_drain","poolId":pool,
            "cursor":next["nextCursor"]})).await?;
        assert_eq!(last["items"].as_array().unwrap().len(), 1);
        assert_eq!(last["items"][0]["id"], 2);
        assert_eq!(last["items"][0]["result"]["details"]["results"][0]["schemaValid"], true);
        assert_eq!(last["done"], true);
        assert_eq!(std::fs::read_to_string(fixture.root.path().join("launches"))?.lines().count(), 2);
        let prompts = std::fs::read_to_string(fixture.root.path().join("prompts"))?;
        assert_eq!(prompts.lines().count(), 3);
        for line in prompts.lines() {
            let request: Value = serde_json::from_str(line)?;
            let message = request["message"].as_str().unwrap();
            assert!(message.contains("Output contract for this turn:"));
            assert!(message.contains("\"type\":\"integer\""));
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}

#[test]
fn typed_pool_keeps_successful_peers_and_reports_failed_aggregate_without_retries() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("pool-mixed-outcomes");
    let fixture = Fixture::new(Some(&schema()));
    run(async {
        let started = fixture.call(json!({"operation":"pool_start","agent":"typed",
            "concurrency":1,"tasks":["bad","duplicate","fenced","valid"]})).await?;
        let pool = started["poolId"].as_str().unwrap();
        let job = started["jobId"].as_str().unwrap();
        let settled = finish_pool(&fixture, pool, job).await;
        assert_eq!(settled.status, "failed");
        let aggregate = settled.result.as_ref().unwrap();
        assert!(aggregate.is_error);
        let manifest = aggregate.details.as_ref().unwrap();
        assert_eq!(manifest["completedItems"], 1);
        assert_eq!(manifest["failedItems"], 3);
        let page = fixture.call(json!({"operation":"pool_drain","poolId":pool})).await?;
        assert_eq!(page["items"].as_array().unwrap().len(), 4);
        assert_eq!(page["done"], true);
        for index in 0..3 {
            let item = &page["items"][index];
            let result = &item["result"]["details"]["results"][0];
            assert_eq!(item["status"], "failed");
            assert_eq!(item["result"]["isError"], true);
            assert_eq!(result["schemaValid"], false);
            assert_eq!(result["schemaRetries"], 0);
            assert!(result.get("data").is_none());
        }
        let good = &page["items"][3]["result"]["details"]["results"][0];
        assert_eq!(good["data"], json!({"value":4}));
        assert_eq!(good["schemaValid"], true);
        let again = fixture.call(json!({"operation":"pool_drain","poolId":pool,
            "cursor":page["cursor"]})).await?;
        assert_eq!(again, page);
        assert_eq!(std::fs::read_to_string(fixture.root.path().join("launches"))?.lines().count(), 1);
        assert_eq!(std::fs::read_to_string(fixture.root.path().join("prompts"))?.lines().count(), 4);
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}

#[test]
fn pool_schema_override_preserves_null_and_rejects_changes_on_push() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("pool-override-null");
    let fixture = Fixture::new(Some(&schema()));
    run(async {
        let started = fixture.call(json!({"operation":"pool_start","agent":"typed",
            "concurrency":1,"tasks":["null"],"outputSchema":{"type":"null"},"schemaMode":"strict"})).await?;
        let pool = started["poolId"].as_str().unwrap();
        let item = wait_item(&fixture, pool, 1).await;
        assert_eq!(pool_result(&item)["schemaValid"], true);
        assert!(pool_result(&item).as_object().unwrap().contains_key("data"));
        assert_eq!(pool_result(&item)["data"], Value::Null);
        for extra in [json!({"outputSchema":true}), json!({"schemaMode":"strict"})] {
            let mut input = json!({"operation":"pool_push","poolId":pool,"tasks":["valid"]});
            input.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            assert!(fixture.call(input).await.is_err());
        }
        let observed = fixture.call(json!({"operation":"pool_status","poolId":pool})).await?;
        assert_eq!(observed["admittedItems"], 1);
        let settled = finish_pool(&fixture, pool, started["jobId"].as_str().unwrap()).await;
        assert_eq!(settled.status, "exited");
        assert_eq!(std::fs::read_to_string(fixture.root.path().join("prompts"))?.lines().count(), 1);
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}

#[test]
fn invalid_pool_contracts_and_schema_privacy_fail_before_any_child_or_carrier() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("pool-preflight");
    let fixture = Fixture::new(None);
    run(async {
        for extra in [json!({"outputSchema":null}), json!({"outputSchema":[]}),
            json!({"outputSchema":{"type":"unknown"}}),
            json!({"outputSchema":{"$ref":"file:///not-a-resource"}}),
            json!({"outputSchema":{"description":"x".repeat(17000)}}),
            json!({"schemaMode":"strict"}), json!({"outputSchema":true,"schemaMode":"permissive"})] {
            let mut input = json!({"operation":"pool_start","agent":"typed","tasks":["valid","valid"]});
            input.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            assert!(fixture.call(input).await.is_err());
        }
        let secret = "pool-schema-annotation-sentinel";
        let mut vault = crate::secrets::SecretVault::default();
        crate::secrets::obfuscate(secret, &mut vault, &[regex::Regex::new(secret)?]);
        let privacy = crate::text_completion::AuxiliaryPrivacySnapshot::with_vault(
            crate::text_completion::AuxiliaryPrivacy::from_settings(None), &vault,
        );
        let error = worker::pool::execute(&fixture.tool,
            json!({"operation":"pool_start","agent":"typed","tasks":["valid"],
                "outputSchema":{"type":"string","description":secret}}), privacy).await.unwrap_err();
        assert!(error.to_string().contains("PI_SUBAGENT_WORKER_SCHEMA_PRIVACY"));
        assert!(!error.to_string().contains(secret));
        assert!(!fixture.root.path().join("launches").exists());
        assert!(crate::jobs::list(&fixture.owner)?.is_empty());
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}

#[test]
fn pool_cancellation_retains_validated_peers_without_fabricating_pending_data() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("pool-cancellation");
    let fixture = Fixture::new(Some(&schema()));
    run(async {
        let started = fixture.call(json!({"operation":"pool_start","agent":"typed",
            "concurrency":2,"tasks":["valid","gated"]})).await?;
        let pool = started["poolId"].as_str().unwrap();
        wait_item(&fixture, pool, 1).await;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !fixture.root.path().join("active").exists() {
            assert!(std::time::Instant::now() < deadline);
            crate::agent_cx::AgentCx::for_request().time().sleep(Duration::from_millis(10)).await;
        }
        fixture.call(json!({"operation":"pool_close","poolId":pool})).await?;
        let page = fixture.call(json!({"operation":"pool_drain","poolId":pool})).await?;
        assert_eq!(page["status"], "cancelled");
        assert_eq!(page["done"], true);
        assert_eq!(page["items"][0]["result"]["details"]["results"][0]["schemaValid"], true);
        assert_eq!(page["items"][1]["status"], "cancelled");
        assert_eq!(page["items"][1]["result"]["isError"], true);
        assert!(page["items"][1]["result"].pointer("/details/results/0/data").is_none());
        for pid in std::fs::read_to_string(fixture.root.path().join("launches"))?.lines() {
            let pid = rustix::process::Pid::from_raw(pid.parse()?).unwrap();
            assert!(rustix::process::test_kill_process(pid).is_err());
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}

#[test]
fn typed_pool_reads_refuse_privacy_changes_and_later_batches_keep_that_floor() -> TestResult {
    let _guard = crate::jobs::process_test_guard();
    event("pool-typed-privacy");
    let fixture = Fixture::new(Some(&json!({"type":"string"})));
    run(async {
        let started = fixture.call(json!({"operation":"pool_start","agent":"typed",
            "concurrency":1,"tasks":["secret"]})).await?;
        let pool = started["poolId"].as_str().unwrap();
        let first = wait_item(&fixture, pool, 1).await;
        assert_eq!(pool_result(&first)["schemaValid"], true);
        let secret = "typed-worker-secret-sentinel";
        let mut vault = crate::secrets::SecretVault::default();
        crate::secrets::obfuscate(secret, &mut vault, &[regex::Regex::new(secret)?]);
        let privacy = crate::text_completion::AuxiliaryPrivacySnapshot::with_vault(
            crate::text_completion::AuxiliaryPrivacy::from_settings(None), &vault,
        );
        for input in [json!({"operation":"pool_result","poolId":pool,"itemId":1}),
            json!({"operation":"pool_wait","poolId":pool,"itemId":1,"timeoutMs":0}),
            json!({"operation":"pool_drain","poolId":pool})] {
            let error = worker::pool::execute(&fixture.tool, input, privacy.clone()).await.unwrap_err();
            assert!(error.to_string().contains("PI_SUBAGENT_POOL_PRIVACY"));
            assert!(!error.to_string().contains(secret));
        }
        worker::pool::execute(&fixture.tool,
            json!({"operation":"pool_push","poolId":pool,"tasks":["secret"]}), privacy).await?;
        let second = wait_item(&fixture, pool, 2).await;
        assert_eq!(second["item"]["status"], "failed");
        assert_eq!(second["item"]["result"]["isError"], true);
        assert!(!serde_json::to_string(&second)?.contains(secret));
        // The later accepted batch's policy protects earlier typed answers
        // even when a subsequent caller brings a weaker privacy snapshot.
        assert!(fixture.call(json!({"operation":"pool_result","poolId":pool,"itemId":1})).await.is_err());
        assert!(fixture.call(json!({"operation":"pool_drain","poolId":pool})).await.is_err());
        let settled = finish_pool(&fixture, pool, started["jobId"].as_str().unwrap()).await;
        assert_eq!(settled.status, "failed");
        assert!(!serde_json::to_string(&settled.result)?.contains(secret));
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}
