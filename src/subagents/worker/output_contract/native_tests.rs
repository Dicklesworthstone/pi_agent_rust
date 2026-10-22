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
