use std::future::{Future, pending, poll_fn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::*;
use crate::error::Error;
use crate::model::{ContentBlock, TextContent};
use crate::tools::ToolEffects;

#[derive(Default)]
struct HostState {
    started: usize,
    active: usize,
    peak: usize,
    value: usize,
    parallel_calls: usize,
    events: Vec<String>,
    waiters: Vec<Waker>,
}

/// Native host fixture with explicit scheduling opt-in. Latches use polls and
/// wakeups rather than elapsed time, so a serial implementation cannot pass the
/// concurrency assertions just because the machine happened to be fast.
struct Host {
    state: Mutex<HostState>,
    parallelism: usize,
}

impl Host {
    fn new(parallelism: usize) -> Self {
        Self {
            state: Mutex::new(HostState::default()),
            parallelism,
        }
    }

    fn state(&self) -> MutexGuard<'_, HostState> {
        self.state.lock().unwrap()
    }

    async fn run(&self, call: ToolCall) -> crate::error::Result<ToolOutput> {
        let label = call.arguments["label"].as_str().unwrap_or(&call.name);
        let wake = {
            let mut state = self.state();
            state.started += 1;
            state.active += 1;
            state.peak = state.peak.max(state.active);
            state.events.push(format!("start:{label}"));
            std::mem::take(&mut state.waiters)
        };
        let _active = ActiveCall(self);
        for waker in wake {
            waker.wake();
        }
        if call.arguments["hold"].as_bool() == Some(true) {
            pending::<()>().await;
        }
        let wait_for = call.arguments["waitFor"].as_u64().unwrap_or(0);
        poll_fn(|cx| {
            let mut state = self.state();
            if u64::try_from(state.started).unwrap() >= wait_for {
                Poll::Ready(())
            } else {
                state.waiters.push(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;

        let value = {
            let mut state = self.state();
            if call.name == "write" {
                assert_eq!(state.active, 1, "mutation overlapped a prior or later call");
                state.value += 1;
            }
            state.events.push(format!("done:{label}"));
            state.value
        };
        if call.name == "fail" {
            return Err(Error::tool("fixture", "controlled host failure"));
        }
        let text = if call.name == "large" {
            "x".repeat(3 * 1024 * 1024)
        } else {
            label.to_owned()
        };
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(json!({"value":value,"label":label})),
            is_error: call.name == "error_output",
        })
    }
}

struct ActiveCall<'a>(&'a Host);

impl Drop for ActiveCall<'_> {
    fn drop(&mut self) {
        self.0.state().active -= 1;
    }
}

#[async_trait]
impl ToolInvocationHost for Host {
    async fn call_tool(
        &self,
        call: ToolCall,
        _expected_session: &str,
    ) -> crate::error::Result<ToolOutput> {
        self.run(call).await
    }

    fn tool_effects(&self, call: &ToolCall) -> ToolEffects {
        if call.name == "write" {
            ToolEffects::write()
        } else {
            ToolEffects::read()
        }
    }

    fn tool_parallelism(&self) -> usize {
        self.parallelism
    }

    async fn call_tool_parallel(
        &self,
        call: ToolCall,
        _expected_session: &str,
    ) -> crate::error::Result<ToolOutput> {
        assert!(self.tool_effects(&call).parallel_safe());
        self.state().parallel_calls += 1;
        self.run(call).await
    }
}

fn run_test(future: impl Future<Output = ()>) {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    runtime.block_on(Box::pin(future));
}

/// These fixture futures have no external I/O: only native in-memory latches.
/// A scheduling regression must fail here instead of hanging the quality lane.
fn settle_fixture_inner<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    for _ in 0..256 {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
    }
    panic!("native fixture did not settle within its bounded polling allowance");
}

async fn invocation<'a>(
    tool: &EvalTool,
    host: Option<&'a dyn ToolInvocationHost>,
) -> Invocation<'a> {
    let mut invocation = Invocation::new(None, &json!({}), None)
        .unwrap()
        .with_host(host);
    invocation.bind_host_session(&tool.eval_session_id().await.unwrap());
    invocation
}

#[test]
fn invalid_batches_are_rejected_atomically_before_native_dispatch() {
    run_test(async {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        let host = Host::new(2);
        let mut invocation = invocation(&tool, Some(&host)).await;
        for invalid in [
            Value::Null,
            json!({"calls":[]}),
            json!({"calls":vec![json!({"name":"read","args":{}});17]}),
            json!({"calls":[{"name":"read","args":{}}],"concurrency":8}),
            json!({"calls":[
                {"name":"read","args":{}},{"name":"write","args":[]}
            ]}),
            json!({"calls":[
                {"name":"read","args":{}},{"name":"write","args":{},"extra":1}
            ]}),
            json!({"calls":[
                {"name":"read","args":{}},{"name":"bad\nname","args":{}}
            ]}),
            json!({"calls":[
                {"name":"read","args":{}},{"name":"x".repeat(129),"args":{}}
            ]}),
            json!({"calls":[
                {"name":"read","args":{}},{"name":"eval","args":{}}
            ]}),
            json!({"calls":[
                {"name":"read","args":{}},{"name":"xdev","args":{}}
            ]}),
            json!({"calls":[
                {"name":"read","args":{}},{"name":"__pi_tool_batch","args":{}}
            ]}),
            // Escaping, not just the raw string length, consumes the wire cap.
            json!({"calls":[
                {"name":"read","args":{"text":"\u{0}".repeat(REQUEST_LIMIT / 6)}}
            ]}),
        ] {
            assert!(invocation.call_tool_batch(&tool, invalid).await.is_err());
            assert_eq!(host.state().started, 0);
            assert!(invocation.tool_receipts.is_empty());
        }
    });
}

#[test]
fn successes_native_error_results_and_host_errors_settle_independently() {
    run_test(async {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        let host = Host::new(2);
        let mut invocation = invocation(&tool, Some(&host)).await;
        let reply = settle_fixture_inner(invocation.call_tool_batch(
            &tool,
            json!({"calls":[
                {"name":"read","args":{"label":"slow-first","waitFor":2}},
                {"name":"fail","args":{}},
                {"name":"error_output","args":{}},
                {"name":"read","args":{"label":"last"}}
            ]}),
        ))
        .unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["schema"], "pi.eval.tool_batch.v1");
        let items = reply["items"].as_array().unwrap();
        assert_eq!(items.len(), 4);
        for (index, item) in items.iter().enumerate() {
            assert_eq!(item["index"], index);
            assert_eq!(
                item["toolCallId"],
                invocation.tool_receipts[index]["toolCallId"]
            );
        }
        assert_eq!(items[0]["result"]["schema"], "pi.eval.tool_result.v1");
        assert_eq!(items[0]["result"]["details"]["label"], "slow-first");
        assert!(
            items[1]["error"]
                .as_str()
                .unwrap()
                .contains("controlled host failure")
        );
        assert!(items[1].get("result").is_none());
        assert_eq!(items[2]["result"]["isError"], true);
        assert!(items[2].get("error").is_none());
        assert_eq!(items[3]["result"]["content"][0]["text"], "last");
        for (index, status) in ["completed", "failed", "failed", "completed"]
            .iter()
            .enumerate()
        {
            assert_eq!(invocation.tool_receipts[index]["status"], *status);
        }
        assert_eq!(host.state().started, 4);
        assert_eq!(host.state().active, 0);
    });
}

#[test]
fn compatible_reads_overlap_but_mutations_are_ordered_barriers() {
    run_test(async {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        let host = Host::new(2);
        let mut invocation = invocation(&tool, Some(&host)).await;
        let reply = settle_fixture_inner(invocation.call_tool_batch(
            &tool,
            json!({"calls":[
                {"name":"read","args":{"label":"before-a","waitFor":2}},
                {"name":"read","args":{"label":"before-b"}},
                {"name":"write","args":{"label":"barrier"}},
                {"name":"read","args":{"label":"after-a","waitFor":5}},
                {"name":"read","args":{"label":"after-b"}}
            ]}),
        ))
        .unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        for (index, expected) in [0, 0, 1, 1, 1].into_iter().enumerate() {
            assert_eq!(
                reply["items"][index]["result"]["details"]["value"],
                expected
            );
        }
        let state = host.state();
        assert_eq!(state.peak, 2);
        assert_eq!(state.parallel_calls, 4);
        let position = |event| state.events.iter().position(|item| item == event).unwrap();
        assert!(position("start:before-b") < position("done:before-a"));
        assert!(position("done:before-a") < position("start:barrier"));
        assert!(position("done:before-b") < position("start:barrier"));
        assert!(position("done:barrier") < position("start:after-a"));
        assert!(position("done:barrier") < position("start:after-b"));
    });
}

#[test]
fn host_parallelism_is_clamped_and_zero_remains_serial() {
    run_test(async {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        for (cap, wait_for, expected_peak) in [(64, 8, 8), (0, 0, 1)] {
            let host = Host::new(cap);
            let mut invocation = invocation(&tool, Some(&host)).await;
            let calls: Vec<_> = (0..16)
                .map(|index| {
                    json!({
                        "name":"read",
                        "args":{"label":index.to_string(),"waitFor":wait_for}
                    })
                })
                .collect();
            settle_fixture_inner(invocation.call_tool_batch(&tool, json!({"calls":calls})))
                .unwrap();
            assert_eq!(host.state().started, 16);
            assert_eq!(host.state().peak, expected_peak);
            if cap == 0 {
                assert_eq!(host.state().parallel_calls, 0);
            }
        }
    });
}

#[test]
fn scalar_and_batch_calls_share_an_atomic_sixty_four_attempt_budget() {
    run_test(async {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        let host = Host::new(2);
        let mut invocation = invocation(&tool, Some(&host)).await;
        // A caught error is still an admitted attempt.
        assert!(invocation.dispatch(&tool, "fail", json!({})).await.is_err());
        for _ in 1..63 {
            invocation.dispatch(&tool, "read", json!({})).await.unwrap();
        }
        let error = invocation
            .call_tool_batch(
                &tool,
                json!({"calls":[
                    {"name":"read","args":{}},{"name":"write","args":{}}
                ]}),
            )
            .await
            .unwrap_err();
        assert!(error.contains("64"));
        assert_eq!(host.state().started, 63);
        assert_eq!(invocation.tool_receipts.len(), 63);
        invocation.dispatch(&tool, "read", json!({})).await.unwrap();
        assert!(invocation.dispatch(&tool, "read", json!({})).await.is_err());
        assert!(
            invocation
                .call_tool_batch(&tool, json!({"calls":[{"name":"write","args":{}}]}))
                .await
                .is_err()
        );
        assert_eq!(host.state().started, 64);
        assert_eq!(host.state().value, 0);
        assert_eq!(invocation.tool_receipts.len(), 64);
    });
}

#[test]
fn dropping_batch_keeps_completed_and_incomplete_native_receipts() {
    run_test(async {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        let host = Host::new(1);
        let mut invocation = invocation(&tool, Some(&host)).await;
        let mut batch = Box::pin(invocation.call_tool_batch(
            &tool,
            json!({"calls":[
                {"name":"read","args":{"label":"completed"}},
                {"name":"read","args":{"label":"pending","hold":true}},
                {"name":"write","args":{"label":"never-started"}}
            ]}),
        ));
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        for _ in 0..32 {
            assert!(batch.as_mut().poll(&mut cx).is_pending());
            if host.state().started == 2 {
                break;
            }
        }
        assert_eq!(host.state().started, 2);
        drop(batch);
        assert_eq!(host.state().active, 0);
        assert_eq!(host.state().value, 0);
        assert_eq!(invocation.tool_receipts.len(), 3);
        assert_eq!(invocation.tool_receipts[0]["status"], "completed");
        assert_eq!(invocation.tool_receipts[1]["status"], "incomplete");
        assert_eq!(invocation.tool_receipts[2]["status"], "incomplete");
        let failure = invocation
            .finish(Err(Error::tool("eval", "cancelled")))
            .unwrap_err();
        assert!(failure.to_string().contains("EVAL_TOOL_CALLS"));
        assert!(failure.to_string().contains("completed"));
        assert!(failure.to_string().contains("incomplete"));
    });
}

#[test]
fn aggregate_overflow_still_settles_a_later_mutation() {
    run_test(async {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        let host = Host::new(2);
        let mut invocation = invocation(&tool, Some(&host)).await;
        let error = invocation
            .call_tool_batch(
                &tool,
                json!({"calls":[
                    {"name":"large","args":{}},
                    {"name":"large","args":{}},
                    {"name":"write","args":{"label":"must-still-run"}}
                ]}),
            )
            .await
            .unwrap_err();
        assert!(error.contains("batch result exceeds 4 MiB"));
        assert_eq!(host.state().started, 3);
        assert_eq!(host.state().value, 1);
        assert_eq!(host.state().active, 0);
        assert!(
            invocation
                .tool_receipts
                .iter()
                .all(|receipt| receipt["status"] == "completed")
        );
    });
}

#[test]
fn standalone_whitelist_and_native_session_capture_are_required() {
    run_test(async {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.txt"), "standalone-batch-proof").unwrap();
        let tool = EvalTool::new(dir.path());
        let mut unbound = Invocation::new(None, &json!({}), None).unwrap();
        let error = unbound
            .call_tool_batch(
                &tool,
                json!({"calls":[{"name":"read","args":{"path":"data.txt"}}]}),
            )
            .await
            .unwrap_err();
        assert!(error.contains("EVAL_SESSION_UNAVAILABLE"));
        assert!(unbound.tool_receipts.is_empty());

        let mut invocation = invocation(&tool, None).await;
        let denied = invocation
            .call_tool_batch(
                &tool,
                json!({"calls":[
                    {"name":"read","args":{"path":"data.txt"}},
                    {"name":"bash","args":{"command":"false"}}
                ]}),
            )
            .await
            .unwrap_err();
        assert!(denied.contains("EVAL_BRIDGE_DENIED"));
        assert!(invocation.tool_receipts.is_empty());
        let reply = invocation
            .call_tool_batch(
                &tool,
                json!({"calls":[{"name":"read","args":{"path":"data.txt"}}]}),
            )
            .await
            .unwrap();
        assert!(reply.contains("standalone-batch-proof"));
        assert_eq!(invocation.tool_receipts.len(), 1);
        assert_eq!(invocation.tool_receipts[0]["status"], "completed");
    });
}

#[test]
fn observed_session_change_remains_sticky_after_returning_to_original_owner() {
    run_test(async {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        let changed = Arc::new(AtomicBool::new(false));
        let resolver_changed = Arc::clone(&changed);
        tool.session_scope.bind(Arc::new(move || {
            let id = if resolver_changed.load(Ordering::SeqCst) {
                "other-session"
            } else {
                "original-session"
            };
            Box::pin(async move { Some(id.to_owned()) })
        }));
        let host = Host::new(1);
        let mut invocation = invocation(&tool, Some(&host)).await;
        let runner_guard = CellSession::new(
            tool.session_scope.clone(),
            tool.eval_session_id().await.unwrap(),
        );
        invocation.bind_cell_session(runner_guard.clone());
        let request = json!({"calls":[{"name":"read","args":{}}]});
        invocation
            .call_tool_batch(&tool, request.clone())
            .await
            .unwrap();
        assert_eq!(host.state().started, 1);

        changed.store(true, Ordering::SeqCst);
        let rejected = invocation
            .call_tool_batch(&tool, request.clone())
            .await
            .unwrap_or_else(|error| error);
        assert!(rejected.contains("EVAL_SESSION_CHANGED"));
        assert_eq!(host.state().started, 1);

        // A new guard per bridge operation would wrongly allow this request.
        changed.store(false, Ordering::SeqCst);
        let still_rejected = invocation
            .call_tool_batch(&tool, request)
            .await
            .unwrap_or_else(|error| error);
        assert!(still_rejected.contains("EVAL_SESSION_CHANGED"));
        assert_eq!(host.state().started, 1);
        assert!(
            runner_guard
                .check_tool()
                .await
                .unwrap_err()
                .to_string()
                .contains("EVAL_SESSION_CHANGED")
        );
        let scalar_error = invocation
            .dispatch(&tool, "read", json!({}))
            .await
            .unwrap_err();
        assert!(scalar_error.contains("EVAL_SESSION_CHANGED"));
        assert_eq!(host.state().started, 1);
    });
}

#[test]
fn unbound_host_scalar_call_fails_before_recording_incomplete_receipt() {
    run_test(async {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        let host = Host::new(1);
        let mut unbound = Invocation::new(None, &json!({}), None)
            .unwrap()
            .with_host(Some(&host));
        let error = unbound
            .dispatch(&tool, "read", json!({}))
            .await
            .unwrap_err();
        assert!(error.contains("EVAL_SESSION_UNAVAILABLE"));
        assert_eq!(host.state().started, 0);
        assert!(
            unbound.tool_receipts.is_empty(),
            "unbound scalar host calls must not consume call budget or record incomplete receipts"
        );
    });
}

#[test]
fn empty_host_error_is_normalized_to_non_empty_batch_item_error() {
    let call = ToolCall {
        id: "eval-empty-err".to_string(),
        name: "fail".to_string(),
        arguments: json!({}),
        thought_signature: None,
    };
    let encoded = encode_outcome(0, &call, &Err(String::new())).expect("encode empty error");
    let parsed: Result<Value, _> = serde_json::from_str(&encoded);
    assert!(
        parsed.is_ok(),
        "batch failure item must be valid JSON: {encoded}"
    );
    if let Ok(item) = parsed {
        assert_eq!(item["index"], 0);
        assert_eq!(item["name"], "fail");
        assert_eq!(item["toolCallId"], "eval-empty-err");
        let err_str = item["error"].as_str().unwrap_or_default();
        assert!(
            !err_str.is_empty(),
            "batch item error must stay non-empty so JS/Python protocol validators accept the batch"
        );
    }
}
