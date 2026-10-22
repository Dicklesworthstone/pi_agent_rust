use super::*;
use super::super::{EvalTool, transport};
use crate::jobs::JobSessionScope;
use crate::tools::ToolUpdate;
use serde_json::json;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

fn run<F: Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread().blocking_threads(1, 8)
        .build().unwrap().block_on(future)
}

fn session(scope: JobSessionScope) -> CellSession {
    scope.bind(Arc::new(|| Box::pin(async { Some("bridge-owner".to_string()) })));
    CellSession::new(scope, "bridge-owner".to_string())
}

fn frame(sequence: u64, text: &str) -> String {
    json!({"id":1,"stream":{"sequence":sequence,"name":"stdout","text":text}}).to_string()
}

#[test]
fn output_callbacks_release_a_pending_operation_without_recreating_it() {
    run(async {
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(frame(1, "release native operation"))).unwrap();
        let session = session(JobSessionScope::default());
        let polls = Arc::new(AtomicUsize::new(0));
        let (release, gate) = futures::channel::oneshot::channel();
        let release = Mutex::new(Some(release));
        let mut capture = Capture::new("python", Some(Box::new(move |update: ToolUpdate| {
            assert_eq!(update.details.unwrap()["output"]["sequence"], 1);
            if let Some(release) = release.lock().unwrap().take() { release.send(()).unwrap(); }
        })));
        let started = Arc::clone(&polls);
        let operation = async move {
            started.fetch_add(1, Ordering::SeqCst);
            gate.await.unwrap();
            42
        };
        assert_eq!(wait_python(&Mutex::new(rx), 1, Instant::now() + Duration::from_secs(5),
            &mut capture, &session, operation).await.unwrap(), 42);
        assert_eq!(polls.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn a_full_output_slice_still_polls_the_existing_native_future() {
    run(async {
        let (tx, rx) = mpsc::channel();
        for sequence in 1..=32 { tx.send(Ok(frame(sequence, "ready"))).unwrap(); }
        let lines = Mutex::new(rx);
        let mut capture = Capture::new("python", None);
        let session = session(JobSessionScope::default());
        assert_eq!(wait_python(&lines, 1, Instant::now() + Duration::from_secs(5),
            &mut capture, &session, async { "native result" }).await.unwrap(), "native result");
        let next = lines.lock().unwrap().try_recv().unwrap().unwrap();
        assert_eq!(serde_json::from_str::<Value>(&next).unwrap()["stream"]["sequence"], DRAIN_FRAMES + 1);
    });
}

#[test]
fn received_non_output_is_rejected_before_native_dispatch() {
    for line in [
        "not JSON".to_string(),
        json!({"id":1,"ok":true,"result":"premature"}).to_string(),
        json!({"id":1,"bridge":{"call":2,"tool":"read","input":{}}}).to_string(),
        json!({"id":2,"stream":{"sequence":1,"name":"stdout","text":"wrong cell"}}).to_string(),
        json!({"id":1,"stream":{"sequence":2,"name":"stdout","text":"wrong sequence"}}).to_string(),
        json!({"id":1,"stream":{"sequence":1,"name":"stdout","text":"ambiguous"},"ok":true}).to_string(),
    ] {
        run(async {
            let (tx, rx) = mpsc::channel();
            tx.send(Ok(line)).unwrap();
            let mut capture = Capture::new("python", None);
            let session = session(JobSessionScope::default());
            let started = AtomicUsize::new(0);
            let error = wait_python(&Mutex::new(rx), 1, Instant::now() + Duration::from_secs(5),
                &mut capture, &session, async { started.fetch_add(1, Ordering::SeqCst); }).await.unwrap_err();
            assert!(error.to_string().contains("EVAL_PROTOCOL"), "{error}");
            assert_eq!(started.load(Ordering::SeqCst), 0);
        });
    }
}

#[test]
fn read_failure_and_eof_cannot_become_successful_native_replies() {
    for failure in [false, true] {
        run(async {
            let (tx, rx) = mpsc::channel();
            if failure { tx.send(Err("truncated wire frame".to_string())).unwrap(); }
            drop(tx);
            let mut capture = Capture::new("python", None);
            let session = session(JobSessionScope::default());
            let error = wait_python(&Mutex::new(rx), 1, Instant::now() + Duration::from_secs(5),
                &mut capture, &session, async { 42 }).await.unwrap_err();
            let code = if failure { "EVAL_PROTOCOL_READ" } else { "EVAL_KERNEL_CRASH" };
            assert!(error.to_string().contains(code), "{error}");
            assert!(error.to_string().contains("EVAL_STATE_RESET"));
        });
    }
}

#[test]
fn callback_session_switch_refuses_dispatch_after_publication() {
    run(async {
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(frame(1, "switch"))).unwrap();
        let scope = JobSessionScope::default();
        let session = session(scope.clone());
        let mut capture = Capture::new("python", Some(Box::new(move |_| {
            scope.bind(Arc::new(|| Box::pin(async { Some("new-owner".to_string()) })));
        })));
        let error = wait_python(&Mutex::new(rx), 1, Instant::now() + Duration::from_secs(5),
            &mut capture, &session, async { panic!("must not dispatch after switching") }).await.unwrap_err();
        assert!(error.to_string().contains("EVAL_SESSION_CHANGED"));
    });
}

#[test]
fn native_completion_cannot_publish_into_a_replacement_conversation() {
    run(async {
        let (_tx, rx) = mpsc::channel();
        let scope = JobSessionScope::default();
        let session = session(scope.clone());
        let mut capture = Capture::new("python", None);
        let error = wait_python(&Mutex::new(rx), 1, Instant::now() + Duration::from_secs(5),
            &mut capture, &session, async move {
                scope.bind(Arc::new(|| Box::pin(async { Some("replacement".to_string()) })));
                42
            }).await.unwrap_err();
        assert!(error.to_string().contains("EVAL_SESSION_CHANGED"));
    });
}

#[test]
fn quiet_native_wait_observes_the_original_deadline() {
    run(async {
        let (_tx, rx) = mpsc::channel();
        let mut capture = Capture::new("python", None);
        let session = session(JobSessionScope::default());
        let error = wait_python(&Mutex::new(rx), 1, Instant::now() + Duration::from_millis(20),
            &mut capture, &session, futures::future::pending::<()>()).await.unwrap_err();
        assert!(error.to_string().contains("EVAL_TIMEOUT"));
    });
}

#[test]
fn callback_cancellation_is_checked_before_polling_native_work() {
    run(async {
        let owner = AgentCx::for_testing_with_io();
        let _current = owner.cx().clone().set_current_restricted();
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(frame(1, "cancel"))).unwrap();
        let session = session(JobSessionScope::default());
        let cancelled = owner.clone();
        let mut capture = Capture::new("python", Some(Box::new(move |_| cancelled.set_cancel_requested(true))));
        let error = wait_python(&Mutex::new(rx), 1, Instant::now() + Duration::from_secs(5),
            &mut capture, &session, async { panic!("cancelled work was polled") }).await.unwrap_err();
        assert!(error.to_string().contains("EVAL_CANCELLED"));
    });
}

/// This is a real Python producer and the production pump, with a controlled
/// native-boundary delay. It is not a network service or a model fixture.
#[test]
fn actual_python_async_output_unblocks_native_read_and_preserves_final_capture() {
    use std::io::Write as _;
    if !std::process::Command::new("python3").arg("--version")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .status().is_ok_and(|status| status.success())
    {
        eprintln!("SKIP: python3 unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("source.txt"), "native payload\n").unwrap();
    run(async {
        let tool = EvalTool::new(dir.path());
        let scope = JobSessionScope::default();
        let session = session(scope);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut kernel = tool.spawn_python(deadline, &session).await.unwrap();
        let (release, gate) = futures::channel::oneshot::channel();
        let release = Mutex::new(Some(release));
        let mut capture = Capture::new("python", Some(Box::new(move |update| {
            if update.details.as_ref().is_some_and(|details| details["jsonOutputs"] == json!([{"gate":42}])) {
                if let Some(release) = release.lock().unwrap().take() { release.send(()).unwrap(); }
            }
        })));
        let code = r#"import asyncio
async def work():
    return await tool.call('read', {'path':'source.txt'})
async def progress():
    await asyncio.sleep(0.05)
    for index in range(80):
        print('progress-' + str(index) + ':' + 'x' * 4096)
    display({'gate':42})
answer, _ = await asyncio.gather(work(), progress())
assert 'native payload' in answer
42
"#;
        writeln!(kernel.stdin.as_mut().unwrap(), "{}", json!({"id":1,"code":code})).unwrap();
        kernel.stdin.as_mut().unwrap().flush().unwrap();
        let line = kernel.next_line(deadline, &mut capture, &session).await.unwrap().unwrap();
        let bridge: Value = serde_json::from_str(&line).unwrap();
        transport::validate_bridge_owner(&bridge, 1, line.len()).unwrap();
        let mut judgments = super::super::judgment::Invocation::new(None, &json!({}), None).unwrap();
        let pending = async {
            gate.await.unwrap();
            tool.run_bridge_call(&bridge["bridge"], &mut judgments).await
        };
        let reply = wait_python(&kernel.lines, 1, deadline, &mut capture, &session, pending).await.unwrap();
        assert_eq!(reply["ok"], true);
        writeln!(kernel.stdin.as_mut().unwrap(), "{}", json!({"bridge_result":reply})).unwrap();
        kernel.stdin.as_mut().unwrap().flush().unwrap();
        let mut final_frame: Value = loop {
            let line = kernel.next_line(deadline, &mut capture, &session).await.unwrap().unwrap();
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame.get("stream").is_some() { capture.accept(&frame, 1).unwrap(); }
            else { break frame; }
        };
        transport::validate_response(&final_frame, 1).unwrap();
        capture.finish_response(&mut final_frame).unwrap();
        assert_eq!(final_frame["result"], "42");
        assert_eq!(final_frame["displays"][0]["data"], json!({"gate":42}));
        let text = final_frame["stdout"].as_str().unwrap();
        assert_eq!(text.matches("progress-0:").count(), 1);
        assert_eq!(text.matches("progress-79:").count(), 1);
    });
}
