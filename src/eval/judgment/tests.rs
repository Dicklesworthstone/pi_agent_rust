//! Native eval/kernel integrations with a controlled provider, not live-model acceptance.
use super::*;
use crate::eval::EvalTool;
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, UserContent,
};
use crate::provider::{Context, Provider};
use crate::tools::Tool;
use futures::Stream;
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

const ANSWER: &str = r#"{"q":0.8}"#;

enum Reply {
    Answer(&'static str),
    Pending,
}

struct RecordedProvider {
    replies: Mutex<VecDeque<Reply>>,
    prompts: Mutex<Vec<String>>,
    dropped: Arc<AtomicBool>,
}

impl RecordedProvider {
    fn new(replies: impl IntoIterator<Item = Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().collect()),
            prompts: Mutex::new(Vec::new()),
            dropped: Arc::new(AtomicBool::new(false)),
        })
    }

    fn judge(self: &Arc<Self>) -> Judge {
        Judge::new(
            self.clone(),
            StreamOptions {
                api_key: Some("host-only-credential".into()),
                headers: std::iter::once(("X-Host-Binding".into(), "retained-header".into()))
                    .collect(),
                max_tokens: Some(99),
                ..StreamOptions::default()
            },
        )
    }
}

struct PendingReply(Arc<AtomicBool>);

impl Stream for PendingReply {
    type Item = Result<StreamEvent>;
    fn poll_next(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl Drop for PendingReply {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[async_trait::async_trait]
impl Provider for RecordedProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }
    fn api(&self) -> &'static str {
        "fixture-stream"
    }
    fn model_id(&self) -> &'static str {
        "eval-judge"
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        assert!(context.tools.is_empty(), "judgments must not execute tools");
        assert_eq!(context.messages.len(), 1, "no conversation replay");
        assert_eq!(options.api_key.as_deref(), Some("host-only-credential"));
        assert_eq!(
            options.headers.get("X-Host-Binding").map(String::as_str),
            Some("retained-header")
        );
        assert_eq!(options.max_tokens, Some(99));
        let Message::User(message) = &context.messages[0] else {
            panic!("state message");
        };
        let UserContent::Text(text) = &message.content else {
            panic!("text state");
        };
        self.prompts.lock().unwrap().push(text.clone());
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Reply::Answer(ANSWER));
        match reply {
            Reply::Pending => Ok(Box::pin(PendingReply(Arc::clone(&self.dropped)))),
            Reply::Answer(answer) => {
                let mut message = AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent::new(answer))],
                    stop_reason: StopReason::Stop,
                    ..Default::default()
                };
                message.usage.input = 11;
                message.usage.output = 2;
                Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message,
                })])))
            }
        }
    }
}

fn request() -> Value {
    json!({"state":{"change":"adds a regression test"},"questions":{
        "q":{"type":"bool","instructions":"Does the change add tests?"}
    }})
}

fn code(kernel: &str) -> String {
    let request = request().to_string();
    if kernel == "js" {
        format!("JSON.parse(await tool.call('__pi_judge', {request})).answers.q.bool")
    } else {
        let request = serde_json::to_string(&request).unwrap();
        format!(
            "import json\nreply = json.loads(await tool.call('__pi_judge', json.loads({request})))\nreply['answers']['q']['bool']"
        )
    }
}

fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
}

fn python_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn model_selection_is_explicit_bounded_and_rejects_guest_credentials() {
    assert!(Selection::parse(&json!({"provider":"fixture","model":"family/model"})).is_ok());
    for role in ModelRole::ALL {
        let selection = json!({"role":role.as_str()});
        assert_eq!(
            serde_json::to_value(Selection::parse(&selection).unwrap()).unwrap(),
            selection
        );
    }
    for selection in [
        Value::Null,
        json!("fixture/model"),
        json!({}),
        json!({"provider":"fixture","model":""}),
        json!({"provider":"fixture","model":"x","api_key":"CANARY"}),
        json!({"provider":"fixture","model":"x","base_url":"http://other"}),
        json!({"provider":" fixture","model":"x"}),
        json!({"provider":"fixture","model":"x\n"}),
        json!({"provider":"fixture","model":"界".repeat(86)}),
        json!({"role":"other"}),
        json!({"role":"Smol"}),
        json!({"role":" smol"}),
        json!({"role":null}),
        json!({"role":"provider/model"}),
        json!({"role":"smol","provider":"fixture"}),
        json!({"role":"smol","provider":"fixture","model":"x"}),
        json!({"role":"smol","api_key":"CANARY"}),
    ] {
        let error = Selection::parse(&selection).unwrap_err().to_string();
        assert!(error.contains("EVAL_INPUT"));
        assert!(!error.contains("CANARY"));
    }
}

fn catalog_entry(provider: &str, id: &str, reasoning: bool) -> ModelEntry {
    ModelEntry {
        model: crate::provider::Model {
            id: id.into(),
            name: id.into(),
            api: "openai-responses".into(),
            provider: provider.into(),
            base_url: "https://example.test/v1".into(),
            reasoning,
            input: vec![crate::provider::InputType::Text],
            cost: crate::provider::ModelCost {
                audio: None,
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 8_192,
            headers: HashMap::default(),
        },
        api_key: Some("catalog-owned-key".into()),
        headers: HashMap::default(),
        auth_header: true,
        compat: None,
        oauth_config: None,
    }
}

#[test]
fn configured_roles_resolve_registered_models_and_clamp_their_thinking() {
    let registry = ModelRegistry::from_entries_for_tests(vec![
        catalog_entry("openai", "fast-model", false),
        catalog_entry("anthropic", "reasoning-model", true),
    ]);
    let config = Config {
        model_roles: Some(crate::config::ModelRoleSettings {
            smol: Some("openai/fast-model:high".into()),
            slow: Some("anthropic/reasoning-model:xhigh".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    for (role, provider, model, thinking) in [
        (
            "smol",
            "openai",
            "fast-model",
            crate::model::ThinkingLevel::Off,
        ),
        (
            "slow",
            "anthropic",
            "reasoning-model",
            crate::model::ThinkingLevel::High,
        ),
    ] {
        let selection = Selection::role(&json!(role)).unwrap();
        let (entry, level) = resolve_entry(
            Path::new("/workspace/project"),
            &selection,
            &config,
            &registry,
        )
        .unwrap();
        assert_eq!(entry.model.provider, provider);
        assert_eq!(entry.model.id, model);
        assert_eq!(entry.api_key.as_deref(), Some("catalog-owned-key"));
        assert_eq!(level, Some(thinking));
    }
}

#[test]
fn role_resolution_never_falls_back_or_escapes_provider_policy() {
    let cwd = Path::new("/workspace/project");
    let registry =
        ModelRegistry::from_entries_for_tests(vec![catalog_entry("openai", "fast-model", false)]);
    let mut config = Config {
        default_provider: Some("openai".into()),
        default_model: Some("fast-model".into()),
        model_roles: Some(crate::config::ModelRoleSettings {
            default: Some("openai/fast-model".into()),
            slow: Some("openai/unknown-model".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    for role in ["smol", "slow"] {
        let error = resolve_entry(
            cwd,
            &Selection::role(&json!(role)).unwrap(),
            &config,
            &registry,
        )
        .unwrap_err();
        assert!(error.to_string().contains("PI_JUDGE_BINDING"));
        assert!(!error.to_string().contains("unknown-model"));
    }
    let role = Selection::role(&json!("default")).unwrap();
    let named = Selection::parse(&json!({"provider":"openai","model":"fast-model"})).unwrap();
    config.disabled_providers = Some(vec!["openai".into()]);
    for selection in [&role, &named] {
        assert!(resolve_entry(cwd, selection, &config, &registry).is_err());
    }
    config.disabled_providers = None;
    config.model_scope_overrides = Some(vec![crate::config::ModelScopeOverride {
        path: "/workspace/project".into(),
        disabled_providers: Some(vec!["openai".into()]),
        ..Default::default()
    }]);
    for selection in [&role, &named] {
        assert!(resolve_entry(cwd, selection, &config, &registry).is_err());
        assert!(
            resolve_entry(Path::new("/another/project"), selection, &config, &registry).is_ok()
        );
    }
}

#[test]
fn each_request_uses_its_selected_role_and_all_roles_share_the_cell_budget() {
    let dir = tempfile::tempdir().unwrap();
    let fast = RecordedProvider::new([Reply::Answer("fast answer")]);
    let slow = RecordedProvider::new([
        Reply::Answer(r#"{"q":0.2}"#),
        Reply::Answer(r#"{"q":0.3}"#),
        Reply::Answer(r#"{"q":0.4}"#),
    ]);
    let mut invocation =
        Invocation::new(None, &json!({"judge_model":{"role":"smol"}}), None).unwrap();
    invocation.resolved = vec![
        (Selection::role(&json!("smol")).unwrap(), fast.judge()),
        (Selection::role(&json!("slow")).unwrap(), slow.judge()),
    ];
    runtime().block_on(async {
        let reply: Value = serde_json::from_str(&invocation.complete(dir.path(), json!({"prompt":"extract"})).await.unwrap()).unwrap();
        assert_eq!(reply["text"], "fast answer");
        let mut input = request();
        input["model"] = json!("slow");
        let reply: Value = serde_json::from_str(&invocation.evaluate(dir.path(), input).await.unwrap()).unwrap();
        assert_eq!(reply["answers"]["q"]["bool"], 0.2);
        let input = json!({"states":["first","second"],"questions":request()["questions"],"model":"slow","concurrency":1});
        let reply: Value = serde_json::from_str(&invocation.evaluate_batch(dir.path(), input).await.unwrap()).unwrap();
        assert_eq!(reply["items"][0]["verdict"]["answers"]["q"]["bool"], 0.3);
        assert_eq!(reply["items"][1]["verdict"]["answers"]["q"]["bool"], 0.4);
        let reply: Value = serde_json::from_str(&invocation.evaluate(dir.path(), request()).await.unwrap()).unwrap();
        assert_eq!(reply["answers"]["q"]["bool"], 0.8, "a previous override must not replace the cell default");
        for receipt in &invocation.receipts[1..4] {
            assert_eq!(receipt["selectedModel"]["role"], "slow");
        }
        assert_eq!(invocation.receipts[0]["selectedModel"]["role"], "smol");
        assert_eq!(invocation.receipts[4]["selectedModel"]["role"], "smol");
        while invocation.receipts.len() < MAX_CALLS {
            invocation.evaluate(dir.path(), request()).await.unwrap();
        }
        let error = invocation.complete(dir.path(), json!({"prompt":"one too many","model":"slow"})).await.unwrap_err();
        assert!(error.contains("PI_COMPLETION_LIMIT"));
    });
    assert_eq!(slow.prompts.lock().unwrap().len(), 3);
    assert_eq!(fast.prompts.lock().unwrap().len(), MAX_CALLS - 3);
    assert!(
        invocation.judge.is_none(),
        "resolved roles must never become a host-bound authority"
    );
    assert_eq!(invocation.resolved.len(), 2);
}

#[test]
fn helper_roles_cannot_admit_an_unbound_cell_or_replace_host_authority() {
    let dir = tempfile::tempdir().unwrap();
    let provider = RecordedProvider::new([]);
    runtime().block_on(async {
        for binding in [None, Some(provider.judge())] {
            let expected = if binding.is_some() {
                "BINDING"
            } else {
                "UNBOUND"
            };
            let mut invocation = Invocation::new(binding, &json!({}), None).unwrap();
            let mut input = request();
            input["model"] = json!("smol");
            assert!(
                invocation
                    .evaluate(dir.path(), input)
                    .await
                    .unwrap_err()
                    .contains(expected)
            );
            assert!(
                invocation
                    .complete(dir.path(), json!({"prompt":"work","model":"smol"}))
                    .await
                    .unwrap_err()
                    .contains(expected)
            );
            let input =
                json!({"states":["state"],"questions":request()["questions"],"model":"smol"});
            assert!(
                invocation
                    .evaluate_batch(dir.path(), input)
                    .await
                    .unwrap_err()
                    .contains(expected)
            );
            assert!(invocation.receipts.is_empty());
            assert!(invocation.resolved.is_empty());
        }
    });
    assert!(provider.prompts.lock().unwrap().is_empty());
    let tool = EvalTool::new(dir.path());
    assert!(
        tool.effects_for_input(&json!({"code":"42","judge_model":{"role":"smol"}}))
            .networks()
    );
    assert!(
        !tool
            .effects_for_input(&json!({"code":"await completion('work',{model:'smol'})"}))
            .networks()
    );
}

#[test]
fn invalid_or_conflicting_selection_never_waits_or_resets_a_kernel() {
    let dir = tempfile::tempdir().unwrap();
    let provider = RecordedProvider::new([]);
    let tool = EvalTool::new(dir.path()).with_judge(provider.judge());
    runtime().block_on(async {
        tool.execute(
            "seed",
            json!({"kernel":"js","code":"globalThis.keep = 7"}),
            None,
        )
        .await
        .unwrap();
        let lease = tool.execution.lock().await;
        for selection in [Value::Null, json!({"provider":"fixture","model":"x"})] {
            let mut invalid = tool.execute(
                "invalid",
                json!({
                    "kernel":"js","code":"globalThis.keep = 99","reset":true,"judge_model":selection
                }),
                None,
            );
            assert!(matches!(
                futures::poll!(invalid.as_mut()),
                Poll::Ready(Err(_))
            ));
        }
        drop(lease);
        let kept = tool
            .execute("kept", json!({"kernel":"js","code":"keep"}), None)
            .await
            .unwrap();
        assert_eq!(text(&kept).trim(), "7");
        assert_eq!(kept.details.unwrap()["cell"], 2);
    });
    assert!(provider.prompts.lock().unwrap().is_empty());
}

#[test]
fn both_native_kernels_return_typed_results_and_host_owned_usage_receipts() {
    for kernel in ["js", "python"] {
        if kernel == "python" && !python_available() {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let provider = RecordedProvider::new([]);
        let tool = EvalTool::new(dir.path()).with_judge(provider.judge());
        runtime().block_on(async {
            for cell in 1..=2 {
                let output = tool
                    .execute("judge", json!({"kernel":kernel,"code":code(kernel)}), None)
                    .await
                    .unwrap();
                assert!(!output.is_error, "{}", text(&output));
                // Only the fresh kernel's first cell carries its start banner.
                let started = match (cell, kernel) {
                    (1, "js") => "(js kernel started)\n",
                    (1, _) => "(kernel started)\n",
                    _ => "",
                };
                assert_eq!(text(&output).trim(), format!("{started}0.8"));
                let details = output.details.unwrap();
                assert_eq!(details["cell"], cell);
                let attempts = details["judgments"]["attempts"].as_array().unwrap();
                assert_eq!(attempts.len(), 1, "new cell, new receipt scope");
                assert_eq!(attempts[0]["status"], "completed");
                assert_eq!(attempts[0]["provider"], "fixture");
                assert_eq!(attempts[0]["modelId"], "eval-judge");
                assert_eq!(attempts[0]["usage"]["input"], 11);
                assert_eq!(attempts[0]["usageKnown"], true);
                assert!(!details.to_string().contains("host-only-credential"));
            }
        });
        assert_eq!(provider.prompts.lock().unwrap().len(), 2);
    }
}

#[test]
fn unbound_judgments_and_nested_result_operations_do_not_expand_tool_authority() {
    let dir = tempfile::tempdir().unwrap();
    let unbound = EvalTool::new(dir.path());
    let output = runtime()
        .block_on(unbound.execute("unbound", json!({"kernel":"js","code":code("js")}), None))
        .unwrap();
    assert!(output.is_error);
    assert!(text(&output).contains("PI_JUDGE_UNBOUND"));
    assert!(output.details.unwrap().get("judgments").is_none());
    let provider = RecordedProvider::new([]);
    let tool = EvalTool::new(dir.path()).with_judge(provider.judge());
    let nested = format!("await tool.result('__pi_judge', {})", request());
    let output = runtime()
        .block_on(tool.execute("nested", json!({"kernel":"js","code":nested}), None))
        .unwrap();
    assert!(output.is_error);
    assert!(text(&output).contains("EVAL_BRIDGE_DENIED"));
    assert!(provider.prompts.lock().unwrap().is_empty());
    assert!(tool.effects_for_input(&json!({"code":"42"})).networks());
    assert!(!unbound.effects_for_input(&json!({"code":"42"})).networks());
}

#[test]
fn live_privacy_is_applied_to_dynamic_state_and_never_retained_between_cells() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("evidence.txt"), "SESSION-CANARY").unwrap();
    let provider = RecordedProvider::new([]);
    let tool = EvalTool::new(dir.path()).with_judge(provider.judge());
    let input = json!({"kernel":"js","code":
        "await tool.call('__pi_judge', {state:await tool.call('read',{path:'evidence.txt'}),questions:{q:{type:'bool',instructions:'Evaluate'}}}); 'discarded verdict'"
    });
    runtime().block_on(async {
        for replacement in ["redacted-one", "redacted-two"] {
            let protect = |parts: &[&str]| -> Result<Vec<String>> {
                Ok(parts
                    .iter()
                    .map(|part| part.replace("SESSION-CANARY", replacement))
                    .collect())
            };
            let output = tool
                .execute_with_auxiliary_privacy("private", input.clone(), None, &protect)
                .await
                .unwrap();
            assert!(!output.is_error, "{}", text(&output));
            assert_eq!(
                output.details.unwrap()["judgments"]["attempts"][0]["status"],
                "completed"
            );
        }
        let blocked = |_parts: &[&str]| -> Result<Vec<String>> {
            Err(Error::validation(
                "PI_SECRET_BLOCK: SESSION-CANARY must not escape",
            ))
        };
        let output = tool
            .execute_with_auxiliary_privacy("blocked", input, None, &blocked)
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(!text(&output).contains("SESSION-CANARY"));
        assert_eq!(
            output.details.unwrap()["judgments"]["attempts"][0]["usageKnown"],
            false
        );
    });
    let prompts = provider.prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2, "blocked before provider admission");
    assert!(prompts[0].contains("redacted-one"));
    assert!(prompts[1].contains("redacted-two"));
    assert!(
        prompts
            .iter()
            .all(|prompt| !prompt.contains("SESSION-CANARY"))
    );
    drop(prompts);
}

#[test]
fn caught_failures_and_discarded_answers_still_have_bounded_per_cell_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let provider = RecordedProvider::new([Reply::Answer("not a verdict")]);
    let tool = EvalTool::new(dir.path()).with_judge(provider.judge());
    runtime().block_on(async {
        let caught_code = format!("try {{ await tool.call('__pi_judge', {}); }} catch(e) {{}} 'caught'", request());
        let output = tool.execute("caught", json!({"kernel":"js","code":caught_code}), None).await.unwrap();
        assert!(!output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["judgments"]["attempts"][0]["status"], "failed");
        assert_eq!(details["judgments"]["attempts"][0]["usage"], Value::Null);
        let limited_code = format!("globalThis.denied = ''; for (let i=0;i<17;i++) {{ try {{await tool.call('__pi_judge', {});}} catch(e) {{denied=String(e);}} }} denied", request());
        let output = tool.execute("limit", json!({"kernel":"js","code":limited_code}), None).await.unwrap();
        assert!(!output.is_error);
        assert!(text(&output).contains("PI_JUDGE_LIMIT"));
        assert_eq!(output.details.unwrap()["judgments"]["attempts"].as_array().unwrap().len(), MAX_CALLS);
        let output = tool.execute("fresh-budget", json!({"kernel":"js","code":code("js")}), None).await.unwrap();
        assert!(!output.is_error);
        assert_eq!(output.details.unwrap()["judgments"]["attempts"].as_array().unwrap().len(), 1);
    });
    assert_eq!(provider.prompts.lock().unwrap().len(), 18);
}

#[test]
fn per_call_kernels_preserve_host_binding_but_not_guest_state_or_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let provider = RecordedProvider::new([]);
    let tool = EvalTool::new(dir.path()).with_judge(provider.judge());
    runtime().block_on(async {
        tool.execute("seed", json!({"kernel":"js","code":"globalThis.keep = 7"}), None).await.unwrap();
        let private = tool.execute("private", json!({
            "kernel":"js","kernel_mode":"per_call","code":format!("globalThis.keep = 99; {}", code("js"))
        }), None).await.unwrap();
        assert!(!private.is_error);
        assert_eq!(private.details.unwrap()["judgments"]["attempts"][0]["status"], "completed");
        let kept = tool.execute("kept", json!({"kernel":"js","code":"keep"}), None).await.unwrap();
        assert_eq!(text(&kept).trim(), "7");
        assert!(kept.details.unwrap().get("judgments").is_none());
    });
}

#[test]
fn cell_deadline_drops_an_admitted_judgment_and_reports_unknown_usage() {
    let dir = tempfile::tempdir().unwrap();
    let provider = RecordedProvider::new([Reply::Pending]);
    let tool = EvalTool::new(dir.path()).with_judge(provider.judge());
    let started = Instant::now();
    let output = runtime()
        .block_on(tool.execute(
            "deadline",
            json!({
                "kernel":"js","code":code("js"),"timeout_secs":2
            }),
            None,
        ))
        .unwrap();
    assert!(output.is_error);
    assert!(text(&output).contains("EVAL_TIMEOUT"));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        provider.prompts.lock().unwrap().len(),
        1,
        "causal provider admission"
    );
    assert!(
        provider.dropped.load(Ordering::Acquire),
        "no detached judgment stream"
    );
    let details = output.details.unwrap();
    assert_eq!(details["stateReset"], true);
    assert_eq!(details["judgments"]["attempts"][0]["status"], "incomplete");
    assert_eq!(details["judgments"]["attempts"][0]["usageKnown"], false);
    assert!(tool.execution.try_lock().is_some());
    let fresh = runtime()
        .block_on(tool.execute("fresh", json!({"kernel":"js","code":"42"}), None))
        .unwrap();
    assert!(!fresh.is_error);
    assert_eq!(fresh.details.unwrap()["cell"], 1);
}
