//! Native eval/kernel integrations with a controlled provider, not live-model acceptance.
use super::*;
use crate::eval::EvalTool;
use crate::model::{AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, UserContent};
use crate::provider::{Context, Provider};
use crate::tools::Tool;
use futures::Stream;
use std::collections::VecDeque;
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
        Judge::new(self.clone(), StreamOptions {
            api_key: Some("host-only-credential".into()),
            headers: [("X-Host-Binding".into(), "retained-header".into())].into_iter().collect(),
            max_tokens: Some(99),
            ..StreamOptions::default()
        })
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
    fn drop(&mut self) { self.0.store(true, Ordering::Release); }
}

#[async_trait::async_trait]
impl Provider for RecordedProvider {
    fn name(&self) -> &str { "fixture" }
    fn api(&self) -> &str { "fixture-stream" }
    fn model_id(&self) -> &str { "eval-judge" }

    async fn stream(&self, context: &Context<'_>, options: &StreamOptions)
        -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        assert!(context.tools.is_empty(), "judgments must not execute tools");
        assert_eq!(context.messages.len(), 1, "no conversation replay");
        assert_eq!(options.api_key.as_deref(), Some("host-only-credential"));
        assert_eq!(options.headers.get("X-Host-Binding").map(String::as_str), Some("retained-header"));
        assert_eq!(options.max_tokens, Some(99));
        let Message::User(message) = &context.messages[0] else { panic!("state message"); };
        let UserContent::Text(text) = &message.content else { panic!("text state"); };
        self.prompts.lock().unwrap().push(text.clone());
        let reply = self.replies.lock().unwrap().pop_front().unwrap_or(Reply::Answer(ANSWER));
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
                    reason: StopReason::Stop, message,
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
        format!("import json\nreply = json.loads(await tool.call('__pi_judge', json.loads({request})))\nreply['answers']['q']['bool']")
    }
}

fn text(output: &ToolOutput) -> String {
    output.content.iter().filter_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    }).collect()
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread().blocking_threads(1, 8)
        .build().unwrap()
}

fn python_available() -> bool {
    std::process::Command::new("python3").arg("--version")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .status().is_ok_and(|status| status.success())
}

#[test]
fn model_selection_is_explicit_bounded_and_rejects_guest_credentials() {
    assert!(Selection::parse(&json!({"provider":"fixture","model":"family/model"})).is_ok());
    for selection in [Value::Null, json!("fixture/model"), json!({}),
        json!({"provider":"fixture","model":""}),
        json!({"provider":"fixture","model":"x","api_key":"CANARY"}),
        json!({"provider":"fixture","model":"x","base_url":"http://other"}),
        json!({"provider":" fixture","model":"x"}),
        json!({"provider":"fixture","model":"x\n"}),
        json!({"provider":"fixture","model":"界".repeat(86)}),
    ] {
        let error = Selection::parse(&selection).unwrap_err().to_string();
        assert!(error.contains("EVAL_INPUT"));
        assert!(!error.contains("CANARY"));
    }
}

#[test]
fn invalid_or_conflicting_selection_never_waits_or_resets_a_kernel() {
    let dir = tempfile::tempdir().unwrap();
    let provider = RecordedProvider::new([]);
    let tool = EvalTool::new(dir.path()).with_judge(provider.judge());
    runtime().block_on(async {
        tool.execute("seed", json!({"kernel":"js","code":"globalThis.keep = 7"}), None).await.unwrap();
        let lease = tool.execution.lock().await;
        for selection in [Value::Null, json!({"provider":"fixture","model":"x"})] {
            let mut invalid = tool.execute("invalid", json!({
                "kernel":"js","code":"globalThis.keep = 99","reset":true,"judge_model":selection
            }), None);
            assert!(matches!(futures::poll!(invalid.as_mut()), Poll::Ready(Err(_))));
        }
        drop(lease);
        let kept = tool.execute("kept", json!({"kernel":"js","code":"keep"}), None).await.unwrap();
        assert_eq!(text(&kept).trim(), "7");
        assert_eq!(kept.details.unwrap()["cell"], 2);
    });
    assert!(provider.prompts.lock().unwrap().is_empty());
}

#[test]
fn both_native_kernels_return_typed_results_and_host_owned_usage_receipts() {
    for kernel in ["js", "python"] {
        if kernel == "python" && !python_available() { continue; }
        let dir = tempfile::tempdir().unwrap();
        let provider = RecordedProvider::new([]);
        let tool = EvalTool::new(dir.path()).with_judge(provider.judge());
        runtime().block_on(async {
            for cell in 1..=2 {
                let output = tool.execute("judge", json!({"kernel":kernel,"code":code(kernel)}), None).await.unwrap();
                assert!(!output.is_error, "{}", text(&output));
                assert_eq!(text(&output).trim(), "0.8");
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
    let output = runtime().block_on(unbound.execute("unbound", json!({"kernel":"js","code":code("js")}), None)).unwrap();
    assert!(output.is_error);
    assert!(text(&output).contains("PI_JUDGE_UNBOUND"));
    assert!(output.details.unwrap().get("judgments").is_none());
    let provider = RecordedProvider::new([]);
    let tool = EvalTool::new(dir.path()).with_judge(provider.judge());
    let nested = format!("await tool.result('__pi_judge', {})", request());
    let output = runtime().block_on(tool.execute("nested", json!({"kernel":"js","code":nested}), None)).unwrap();
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
                Ok(parts.iter().map(|part| part.replace("SESSION-CANARY", replacement)).collect())
            };
            let output = tool.execute_with_auxiliary_privacy("private", input.clone(), None, &protect).await.unwrap();
            assert!(!output.is_error, "{}", text(&output));
            assert_eq!(output.details.unwrap()["judgments"]["attempts"][0]["status"], "completed");
        }
        let blocked = |_parts: &[&str]| -> Result<Vec<String>> {
            Err(Error::validation("PI_SECRET_BLOCK: SESSION-CANARY must not escape"))
        };
        let output = tool.execute_with_auxiliary_privacy("blocked", input, None, &blocked).await.unwrap();
        assert!(output.is_error);
        assert!(!text(&output).contains("SESSION-CANARY"));
        assert_eq!(output.details.unwrap()["judgments"]["attempts"][0]["usageKnown"], false);
    });
    let prompts = provider.prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2, "blocked before provider admission");
    assert!(prompts[0].contains("redacted-one"));
    assert!(prompts[1].contains("redacted-two"));
    assert!(prompts.iter().all(|prompt| !prompt.contains("SESSION-CANARY")));
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
    let output = runtime().block_on(tool.execute("deadline", json!({
        "kernel":"js","code":code("js"),"timeout_secs":2
    }), None)).unwrap();
    assert!(output.is_error);
    assert!(text(&output).contains("EVAL_TIMEOUT"));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(provider.prompts.lock().unwrap().len(), 1, "causal provider admission");
    assert!(provider.dropped.load(Ordering::Acquire), "no detached judgment stream");
    let details = output.details.unwrap();
    assert_eq!(details["stateReset"], true);
    assert_eq!(details["judgments"]["attempts"][0]["status"], "incomplete");
    assert_eq!(details["judgments"]["attempts"][0]["usageKnown"], false);
    assert!(tool.execution.try_lock().is_some());
    let fresh = runtime().block_on(tool.execute("fresh", json!({"kernel":"js","code":"42"}), None)).unwrap();
    assert!(!fresh.is_error);
    assert_eq!(fresh.details.unwrap()["cell"], 1);
}
