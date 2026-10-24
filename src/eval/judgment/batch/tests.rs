//! Native contracts and controlled-provider workflows; no live model requests.

use super::*;
use crate::error::Result;
use crate::eval::EvalTool;
use crate::judgment::Judge;
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, UserContent,
};
use crate::provider::{Context, Provider, StreamOptions};
use crate::tools::{Tool, ToolOutput};
use futures::Stream;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

fn event(case: &str) {
    eprintln!(
        "{}",
        json!({"suite":"eval_judgment_batch","case":case,"phase":"start"})
    );
}

fn questions() -> Value {
    json!({"q":{"type":"bool","instructions":"Does the state support the claim?"}})
}

fn input(count: usize) -> Value {
    let states: Vec<_> = (0..count).map(|index| json!({"index":index})).collect();
    json!({"states":states,"questions":questions(),"concurrency":2})
}

fn single() -> Value {
    json!({"state":{"index":0},"questions":questions()})
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
}

#[derive(Clone, Copy)]
enum Mode {
    Immediate,
    OutOfOrder,
    Pending,
}

#[derive(Default)]
struct Observed {
    calls: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
    completed: Mutex<Vec<usize>>,
    prompts: Mutex<Vec<String>>,
}

struct Fixture {
    mode: Mode,
    fail_at: Option<usize>,
    seen: Arc<Observed>,
}

impl Fixture {
    fn new(mode: Mode, fail_at: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            mode,
            fail_at,
            seen: Arc::new(Observed::default()),
        })
    }

    fn judge(self: &Arc<Self>) -> Judge {
        Judge::new(
            self.clone(),
            StreamOptions {
                api_key: Some("HOST-CREDENTIAL-CANARY".into()),
                headers: std::iter::once(("X-Host".into(), "fixed-binding".into())).collect(),
                max_tokens: Some(99),
                ..StreamOptions::default()
            },
        )
    }
}

struct Reply {
    mode: Mode,
    seen: Arc<Observed>,
    index: usize,
    failed: bool,
    done: bool,
}

impl Drop for Reply {
    fn drop(&mut self) {
        self.seen.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Stream for Reply {
    type Item = Result<StreamEvent>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        if matches!(this.mode, Mode::Pending) {
            return Poll::Pending;
        }
        if matches!(this.mode, Mode::OutOfOrder) {
            if this.seen.calls.load(Ordering::SeqCst) < 2
                || (this.index == 0 && !this.seen.completed.lock().unwrap().contains(&1))
            {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        }
        this.done = true;
        this.seen.completed.lock().unwrap().push(this.index);
        let answer = if this.failed {
            "PRIVATE-RESPONSE-CANARY is not a verdict".to_string()
        } else {
            let probability = f64::from(u32::try_from(this.index % 8 + 1).unwrap()) / 10.0;
            json!({"q":probability}).to_string()
        };
        let mut message = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(answer))],
            stop_reason: StopReason::Stop,
            ..Default::default()
        };
        message.usage.input = 11;
        message.usage.output = 2;
        Poll::Ready(Some(Ok(StreamEvent::Done {
            reason: StopReason::Stop,
            message,
        })))
    }
}

#[async_trait::async_trait]
impl Provider for Fixture {
    fn name(&self) -> &'static str {
        "fixture"
    }
    fn api(&self) -> &'static str {
        "fixture-stream"
    }
    fn model_id(&self) -> &'static str {
        "batch-judge"
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        assert!(context.tools.is_empty());
        assert_eq!(context.messages.len(), 1);
        assert_eq!(options.api_key.as_deref(), Some("HOST-CREDENTIAL-CANARY"));
        assert_eq!(
            options.headers.get("X-Host").map(String::as_str),
            Some("fixed-binding")
        );
        assert_eq!(options.max_tokens, Some(99));
        let Message::User(message) = &context.messages[0] else {
            panic!("state message");
        };
        let UserContent::Text(text) = &message.content else {
            panic!("text state");
        };
        let wire: Value = serde_json::from_str(text).unwrap();
        let index = usize::try_from(wire["state"]["index"].as_u64().unwrap()).unwrap();
        self.seen.prompts.lock().unwrap().push(text.clone());
        self.seen.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.seen.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.seen.peak.fetch_max(active, Ordering::SeqCst);
        Ok(Box::pin(Reply {
            mode: self.mode,
            seen: Arc::clone(&self.seen),
            index,
            failed: self.fail_at == Some(index),
            done: false,
        }))
    }
}

fn invocation(provider: &Arc<Fixture>) -> Invocation<'static> {
    Invocation::new(Some(provider.judge()), &json!({}), None).unwrap()
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

#[test]
fn batch_shapes_are_strict_and_do_not_infer_scalar_or_empty_work() {
    event("admission");
    for value in [
        Value::Null,
        json!({}),
        json!({"states":[],"questions":questions()}),
        json!({"states":{},"questions":questions()}),
        json!({"states":"one","questions":questions()}),
        input(17),
        json!({"states":[{}],"questions":questions(),"model":"untrusted"}),
        json!({"states":[{}],"questions":questions(),"retries":1}),
    ] {
        assert!(Plan::parse(&value).is_err());
    }
    for concurrency in [
        Value::Null,
        json!(false),
        json!(0),
        json!(9),
        json!(1.5),
        json!("2"),
    ] {
        let mut value = input(1);
        value["concurrency"] = concurrency;
        assert!(Plan::parse(&value).is_err());
    }
    for key in [String::new(), "a\n".into(), "界".repeat(43)] {
        assert!(Plan::parse(&json!({"states":{(key):{}},"questions":questions()})).is_err());
    }
    let mut default = input(1);
    default.as_object_mut().unwrap().remove("concurrency");
    assert_eq!(
        Plan::parse(&default).unwrap().concurrency,
        DEFAULT_CONCURRENCY
    );
}

#[test]
fn every_item_uses_the_native_state_and_rubric_contract_before_admission() {
    event("whole-batch-preflight");
    let provider = Fixture::new(Mode::Immediate, None);
    let mut invocation = invocation(&provider);
    runtime().block_on(async {
        for bad in [Value::Null, json!(false), json!(42), json!("  ")] {
            let mut request = input(2);
            request["states"][1] = bad;
            assert!(
                invocation
                    .evaluate_batch(Path::new("."), request)
                    .await
                    .is_err()
            );
        }
        let mut request = input(2);
        request["questions"]["q"]["type"] = json!("unknown");
        assert!(
            invocation
                .evaluate_batch(Path::new("."), request)
                .await
                .is_err()
        );
    });
    assert!(invocation.receipts.is_empty());
    assert_eq!(provider.seen.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn input_budgets_count_escapes_and_repeated_shared_rubrics_before_copying() {
    event("byte-budgets");
    let mut large = input(1);
    large["states"][0] = json!("\n".repeat(MAX_INPUT_BYTES / 2));
    assert!(
        Plan::parse(&large)
            .err()
            .unwrap()
            .contains("PI_JUDGE_BATCH_LIMIT")
    );
    let rubric: serde_json::Map<String, Value> = (0..16)
        .map(|i| {
            (
                format!("q{i}"),
                json!({"type":"bool","instructions":"r".repeat(4096)}),
            )
        })
        .collect();
    // Small wire input but expanded shared questions exceed the batch bound.
    let repeated = json!({"states":[{},{},{},{}],"questions":rubric});
    assert!(encoded_size(&repeated, MAX_INPUT_BYTES).is_ok());
    assert!(
        Plan::parse(&repeated)
            .err()
            .unwrap()
            .contains("PI_JUDGE_BATCH_LIMIT")
    );
}

#[test]
fn keyed_inputs_preserve_literal_identity_without_turning_keys_into_requests() {
    event("keyed-states");
    let value =
        json!({"states":{"a/b~0": {"index":0},"雪 file": {"index":1}},"questions":questions()});
    let provider = Fixture::new(Mode::Immediate, None);
    let mut invocation = invocation(&provider);
    let result = runtime()
        .block_on(invocation.evaluate_batch(Path::new("."), value))
        .unwrap();
    let result: Value = serde_json::from_str(&result).unwrap();
    let items = result["items"].as_array().unwrap();
    assert!(items.iter().any(|item| item["key"] == "a/b~0"));
    assert!(items.iter().any(|item| item["key"] == "雪 file"));
    let receipts = serde_json::to_string(&invocation.receipts).unwrap();
    assert!(!receipts.contains("a/b~0") && !receipts.contains("雪 file"));
    assert!(!receipts.contains("HOST-CREDENTIAL-CANARY"));
}

#[test]
fn concurrent_completions_remain_in_input_order_and_never_exceed_the_bound() {
    event("concurrent-order");
    let provider = Fixture::new(Mode::OutOfOrder, None);
    let mut invocation = invocation(&provider);
    let result = runtime()
        .block_on(invocation.evaluate_batch(Path::new("."), input(5)))
        .unwrap();
    let result: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(provider.seen.peak.load(Ordering::SeqCst), 2);
    assert_eq!(provider.seen.active.load(Ordering::SeqCst), 0);
    assert_eq!(provider.seen.completed.lock().unwrap()[0], 1);
    assert_eq!(result["allOk"], true);
    for (index, item) in result["items"].as_array().unwrap().iter().enumerate() {
        assert_eq!(item["key"], json!(index));
        let probability = f64::from(u32::try_from(index + 1).unwrap()) / 10.0;
        assert_eq!(item["verdict"]["answers"]["q"]["bool"], json!(probability));
        assert_eq!(invocation.receipts[index]["item"], json!(index));
        assert_eq!(invocation.receipts[index]["usage"]["input"], 11);
        assert_eq!(invocation.receipts[index]["status"], "completed");
    }
}

#[test]
fn a_failed_item_retains_successful_peers_without_retry_or_private_diagnostics() {
    event("partial-failure");
    let provider = Fixture::new(Mode::Immediate, Some(1));
    let mut invocation = invocation(&provider);
    let result = runtime()
        .block_on(invocation.evaluate_batch(Path::new("."), input(3)))
        .unwrap();
    assert!(!result.contains("PRIVATE-RESPONSE-CANARY"));
    let result: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["allOk"], false);
    assert_eq!(result["succeeded"], 2);
    assert_eq!(result["failed"], 1);
    assert_eq!(result["items"][1]["ok"], false);
    assert!(result["items"][1].get("verdict").is_none());
    assert!(
        result["items"][1]["error"]
            .as_str()
            .unwrap()
            .contains("PI_JUDGE_RESPONSE")
    );
    assert_eq!(provider.seen.calls.load(Ordering::SeqCst), 3);
    assert_eq!(invocation.receipts[1]["status"], "failed");
    assert_eq!(invocation.receipts[1]["usageKnown"], false);
}

#[test]
fn single_and_batch_requests_share_one_nonresetting_attempt_budget() {
    event("shared-attempt-budget");
    let provider = Fixture::new(Mode::Immediate, None);
    let mut invocation = invocation(&provider);
    runtime().block_on(async {
        invocation.evaluate(Path::new("."), single()).await.unwrap();
        assert!(
            invocation
                .evaluate_batch(Path::new("."), input(16))
                .await
                .unwrap_err()
                .contains("PI_JUDGE_LIMIT")
        );
        assert_eq!(invocation.receipts.len(), 1);
        invocation
            .evaluate_batch(Path::new("."), input(15))
            .await
            .unwrap();
        assert!(
            invocation
                .evaluate(Path::new("."), single())
                .await
                .unwrap_err()
                .contains("PI_JUDGE_LIMIT")
        );
        assert!(
            invocation
                .evaluate_batch(Path::new("."), input(1))
                .await
                .unwrap_err()
                .contains("PI_JUDGE_LIMIT")
        );
    });
    assert_eq!(provider.seen.calls.load(Ordering::SeqCst), MAX_CALLS);
    assert_eq!(invocation.receipts.len(), MAX_CALLS);
}

#[test]
fn dropping_a_pending_batch_drops_active_streams_and_preserves_unstarted_receipts() {
    event("drop-cancels-owned-futures");
    let provider = Fixture::new(Mode::Pending, None);
    let mut invocation = invocation(&provider);
    runtime().block_on(async {
        let mut pending = Box::pin(invocation.evaluate_batch(Path::new("."), input(5)));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        assert_eq!(provider.seen.calls.load(Ordering::SeqCst), 2);
        assert_eq!(provider.seen.active.load(Ordering::SeqCst), 2);
        drop(pending);
        assert_eq!(provider.seen.active.load(Ordering::SeqCst), 0);
    });
    assert_eq!(invocation.receipts.len(), 5);
    for (index, receipt) in invocation.receipts.iter().enumerate() {
        assert_eq!(
            receipt["status"],
            if index < 2 {
                "incomplete"
            } else {
                "not_started"
            }
        );
        assert_eq!(receipt["usageKnown"], false);
        assert!(receipt["usage"].is_null());
    }
}

#[test]
fn cancelled_and_unbound_invocations_do_not_admit_provider_calls() {
    event("admission-authority");
    let provider = Fixture::new(Mode::Immediate, None);
    let mut invocation = invocation(&provider);
    let owner = AgentCx::for_request();
    owner.set_cancel_requested(true);
    let error = runtime()
        .block_on(owner.with_current(invocation.evaluate_batch(Path::new("."), input(1))))
        .unwrap_err();
    assert!(error.contains("PI_JUDGE_CANCELLED"));
    assert!(invocation.receipts.is_empty());
    let mut unbound = Invocation::new(None, &json!({}), None).unwrap();
    assert!(
        runtime()
            .block_on(unbound.evaluate_batch(Path::new("."), input(1)))
            .unwrap_err()
            .contains("PI_JUDGE_UNBOUND")
    );
    assert!(unbound.receipts.is_empty());
    assert_eq!(provider.seen.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn per_item_privacy_blocks_only_that_item_and_never_exposes_callback_payloads() {
    event("borrowed-live-privacy");
    let provider = Fixture::new(Mode::Immediate, None);
    let protect = |parts: &[&str]| -> Result<Vec<String>> {
        if parts.iter().any(|part| part.contains("BLOCK-CANARY")) {
            return Err(crate::error::Error::validation(
                "PI_SECRET_BLOCK: BLOCK-CANARY",
            ));
        }
        Ok(parts
            .iter()
            .map(|part| part.replace("MASK-CANARY", "redacted"))
            .collect())
    };
    let mut invocation =
        Invocation::new(Some(provider.judge()), &json!({}), Some(&protect)).unwrap();
    let mut request = input(3);
    request["states"][0]["evidence"] = json!("MASK-CANARY");
    request["states"][1]["evidence"] = json!("BLOCK-CANARY");
    let result = runtime()
        .block_on(invocation.evaluate_batch(Path::new("."), request))
        .unwrap();
    assert!(!result.contains("CANARY"));
    let result: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["succeeded"], 2);
    assert!(
        result["items"][1]["error"]
            .as_str()
            .unwrap()
            .contains("PI_SECRET_BLOCK")
    );
    let prompts = provider.seen.prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[0].contains("redacted"));
    assert!(prompts.iter().all(|prompt| !prompt.contains("CANARY")));
    drop(prompts);
}

fn code(kernel: &str, request: &Value) -> String {
    if kernel == "js" {
        format!(
            "globalThis.batch = JSON.parse(await tool.call('__pi_judge_batch', {request})); batch.succeeded"
        )
    } else {
        let request = serde_json::to_string(&request.to_string()).unwrap();
        format!(
            "import json\nbatch = json.loads(await tool.call('__pi_judge_batch', json.loads({request})))\nbatch['succeeded']"
        )
    }
}

#[test]
fn public_python_and_javascript_cells_retain_results_and_report_native_receipts() {
    event("public-native-kernels");
    for kernel in ["js", "python"] {
        if kernel == "python"
            && !std::process::Command::new("python3")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        {
            eprintln!(
                "{}",
                json!({"suite":"eval_judgment_batch","case":"python","phase":"skip","reason":"python3 unavailable"})
            );
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let provider = Fixture::new(Mode::Immediate, Some(1));
        let tool = EvalTool::new(dir.path()).with_judge(provider.judge());
        runtime().block_on(async {
            let output = tool
                .execute(
                    "batch",
                    json!({"kernel":kernel,"code":code(kernel,&input(3))}),
                    None,
                )
                .await
                .unwrap();
            assert!(!output.is_error, "{}", text(&output));
            assert_eq!(text(&output).trim(), "2");
            let details = output.details.unwrap();
            let receipts = details["judgments"]["attempts"].as_array().unwrap();
            assert_eq!(receipts.len(), 3);
            assert_eq!(receipts[1]["status"], "failed");
            let retained = if kernel == "js" {
                "batch.failed"
            } else {
                "batch['failed']"
            };
            let next = tool
                .execute("retained", json!({"kernel":kernel,"code":retained}), None)
                .await
                .unwrap();
            assert_eq!(text(&next).trim(), "1");
            assert!(next.details.unwrap().get("judgments").is_none());
        });
        assert_eq!(provider.seen.calls.load(Ordering::SeqCst), 3);
    }
}

#[test]
fn public_watchdog_drops_running_items_without_launching_or_counting_queued_items_as_calls() {
    event("original-cell-deadline");
    let dir = tempfile::tempdir().unwrap();
    let provider = Fixture::new(Mode::Pending, None);
    let tool = EvalTool::new(dir.path()).with_judge(provider.judge());
    let started = Instant::now();
    let output = runtime()
        .block_on(tool.execute(
            "deadline",
            json!({
                "kernel":"js","code":code("js",&input(5)),"timeout_secs":2
            }),
            None,
        ))
        .unwrap();
    assert!(output.is_error);
    assert!(text(&output).contains("EVAL_TIMEOUT"));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(provider.seen.calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider.seen.active.load(Ordering::SeqCst), 0);
    let details = output.details.unwrap();
    let attempts = details["judgments"]["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 5);
    assert_eq!(attempts[0]["status"], "incomplete");
    assert_eq!(attempts[4]["status"], "not_started");
}

#[test]
fn public_nested_bridge_calls_cannot_reclassify_batch_operations_as_read_only() {
    event("no-whitelist-expansion");
    let dir = tempfile::tempdir().unwrap();
    let provider = Fixture::new(Mode::Immediate, None);
    let tool = EvalTool::new(dir.path()).with_judge(provider.judge());
    let output = runtime()
        .block_on(tool.execute(
            "nested",
            json!({
                "kernel":"js","code":format!("await tool.result('__pi_judge_batch', {})",input(1))
            }),
            None,
        ))
        .unwrap();
    assert!(output.is_error);
    assert!(text(&output).contains("EVAL_BRIDGE_DENIED"));
    assert_eq!(provider.seen.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn model_facing_discovery_describes_batch_results_and_shared_authority() {
    event("model-facing-contract");
    let tool = EvalTool::new(Path::new("."));
    let description = tool.description();
    assert!(description.contains(BRIDGE_NAME));
    assert!(description.contains("allOk"));
    assert!(description.contains("16 reserved attempts"));
    let schema = tool.parameters();
    assert!(
        schema["properties"]["judge_model"]["description"]
            .as_str()
            .unwrap()
            .contains(BRIDGE_NAME)
    );
    assert!(!tool.effects_for_input(&json!({"code":"42"})).networks());
    assert!(
        tool.effects_for_input(
            &json!({"code":"42","judge_model":{"provider":"fixture","model":"batch"}})
        )
        .networks()
    );
}
