use super::*;
use crate::tools::Tool;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread().blocking_threads(1, 8)
        .build().unwrap().block_on(future)
}

async fn js(tool: &EvalTool, code: &str, mode: &str) -> ToolOutput {
    tool.execute("test", json!({"kernel":"js", "kernel_mode":mode, "code":code}), None)
        .await.unwrap()
}

fn text(output: &ToolOutput) -> String {
    output.content.iter().filter_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    }).collect::<Vec<_>>().join("\n")
}

#[test]
fn per_call_never_reads_or_replaces_retained_javascript_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        assert!(!js(&tool, "const keep = 41", "session").await.is_error);
        let fresh = js(&tool, "typeof keep", "per_call").await;
        assert!(!fresh.is_error);
        assert!(text(&fresh).contains("undefined"));
        assert_eq!(fresh.details.as_ref().unwrap()["cell"], 1);
        assert_eq!(fresh.details.as_ref().unwrap()["stateRetained"], false);
        assert!(!js(&tool, "const keep = 99", "per_call").await.is_error);
        let kept = js(&tool, "keep + 1", "session").await;
        assert!(text(&kept).trim().ends_with("42"));
        assert_eq!(kept.details.as_ref().unwrap()["cell"], 2);
        assert!(text(&js(&tool, "typeof keep", "per_call").await).contains("undefined"));
    });
}

#[test]
fn invalid_mode_fails_before_admission_or_reset() {
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        js(&tool, "globalThis.keep = 42", "session").await;
        let lease = tool.execution.lock().await;
        for mode in [Value::Null, json!(false), json!(3), json!("per-call"), json!("SESSION")] {
            let mut pending = tool.execute("bad", json!({
                "kernel":"js", "kernel_mode":mode, "code":"", "reset":true
            }), None);
            let std::task::Poll::Ready(Err(error)) = futures::poll!(pending.as_mut()) else {
                panic!("invalid mode waited or ran");
            };
            assert!(error.to_string().contains("EVAL_INPUT"));
        }
        drop(lease);
        assert!(text(&js(&tool, "keep", "session").await).trim().ends_with("42"));
    });
}

#[test]
fn isolated_calls_share_admission_and_cannot_escape_waiting_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    let owner = AgentCx::for_testing();
    run(async {
        js(&tool, "globalThis.keep = 42", "session").await;
        let lease = tool.execution.lock().await;
        let mut pending = Box::pin(owner.with_current(tool.execute("queued", json!({
            "kernel":"js", "kernel_mode":"per_call", "code":"globalThis.keep = 99", "reset":true
        }), None)));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        owner.set_cancel_requested(true);
        assert!(pending.await.unwrap_err().to_string().contains("EVAL_CANCELLED"));
        drop(lease);
        assert!(text(&js(&tool, "keep", "session").await).trim().ends_with("42"));
    });
}

#[test]
fn active_private_cancellation_and_abandonment_preserve_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        js(&tool, "globalThis.keep = 42", "session").await;
        for cancel in [false, true] {
            let owner = AgentCx::for_testing();
            let started = Arc::new(AtomicBool::new(false));
            let observed = Arc::clone(&started);
            let mut active = Box::pin(owner.with_current(tool.execute("private", json!({
                "kernel":"js", "kernel_mode":"per_call", "timeout_secs":10,
                "code":"console.log('started'); await sleep(60000)"
            }), Some(Box::new(move |_| { observed.store(true, Ordering::SeqCst); })))));
            // Poll to an actual producer update, not just source preparation.
            let end = Instant::now() + Duration::from_secs(5);
            while !started.load(Ordering::SeqCst) {
                assert!(futures::poll!(active.as_mut()).is_pending());
                assert!(Instant::now() < end);
                AgentCx::for_current_or_request().time().sleep(Duration::from_millis(10)).await;
            }
            if cancel {
                owner.set_cancel_requested(true);
                let stopped = active.await.unwrap();
                assert!(stopped.is_error);
                assert_eq!(stopped.details.unwrap()["stateRetained"], false);
            } else {
                drop(active);
            }
            assert!(tool.execution.try_lock().is_some());
            assert!(text(&js(&tool, "keep", "session").await).trim().ends_with("42"));
        }
    });
}

#[test]
fn per_call_reloads_modules_without_invalidating_the_retained_cache() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("counter.js");
    std::fs::write(&module, "export let value = 1; export function bump() { value += 1; }").unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        assert!(!js(&tool, "const m = await import('./counter.js'); m.bump(); m.value", "session").await.is_error);
        let fresh = js(&tool, "const m = await import('./counter.js'); m.value", "per_call").await;
        assert!(text(&fresh).trim().ends_with('1'));
        let failed = js(&tool, "globalThis.keep = 99; throw new Error('private failure')", "per_call").await;
        assert!(failed.is_error);
        assert!(text(&failed).contains("private failure"));
        assert!(text(&js(&tool, "m.value", "session").await).trim().ends_with('2'));
        assert!(text(&js(&tool, "typeof keep", "session").await).contains("undefined"));
    });
}

#[test]
fn cancelled_or_changed_session_cannot_publish_a_private_result() {
    let dir = tempfile::tempdir().unwrap();
    let mut tool = EvalTool::new(dir.path());
    let scope = crate::jobs::JobSessionScope::fixed("original");
    tool.bind_job_session_scope(scope.clone());
    run(async {
        let output = tool.execute("switch", json!({
            "kernel":"js", "kernel_mode":"per_call", "code":"console.log('switch'); await sleep(60000)", "timeout_secs":5
        }), Some(Box::new(move |_| {
            scope.bind(Arc::new(|| Box::pin(async { Some("replacement".into()) })));
        }))).await.unwrap();
        assert!(output.is_error);
        assert!(text(&output).contains("EVAL_SESSION_CHANGED"));
        assert_eq!(output.details.as_ref().unwrap()["stateRetained"], false);
    });
}

#[test]
fn explicit_python_configuration_and_fresh_storage_are_copied_without_recapture() {
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new_with_python(dir.path(), "unavailable-literal-runtime").unwrap();
    let private = tool.isolated_executor();
    assert_eq!(private.python_path, tool.python_path);
    assert!(Arc::ptr_eq(&private.execution, &tool.execution));
    assert!(private.kernel.lock().unwrap().is_none());
    assert!(private.js.lock().unwrap().is_none());
    let schema = tool.parameters();
    assert_eq!(schema["properties"]["kernel_mode"]["enum"], json!(["session", "per_call"]));
}
