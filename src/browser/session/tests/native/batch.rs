//! Batch workflows reuse the existing bounded native HTTP/WebSocket peer.
//! No alternate backend or synthetic batch executor is involved.

use super::*;
use futures::FutureExt as _;

fn workflow(calls: Vec<Value>) -> Value {
    let steps: Vec<_> = calls.into_iter().map(|call| json!({"call":call})).collect();
    json!({"action":"batch", "steps":steps})
}

fn open_call() -> Value {
    json!({"action":"open", "tab":"work", "url":"about:blank"})
}

fn eval_call(script: &str) -> Value {
    json!({"action":"evaluate", "tab":"work", "script":script})
}

#[test]
fn batch_open_and_evaluations_use_the_same_namespace_in_order() {
    let (endpoint, worker) = peer(vec![
        open(true, &[], "page-a"),
        evaluate(false, &["page-a"], "page-a", "globalThis.answer = 41", json!(41)),
        evaluate(false, &["page-a"], "page-a", "answer + 1", json!(42)),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let registry = registry(directory.path(), &endpoint);
    let tool = registry.get("browser").unwrap();
    let output = run(call(tool, workflow(vec![open_call(), eval_call("globalThis.answer = 41"), eval_call("answer + 1")])));
    let details = output.details.unwrap();
    assert_eq!(details["schema"], "pi.browser.batch.v1");
    assert_eq!(details["completed_steps"], 3);
    assert_eq!(details["attempted_steps"], 3);
    assert_eq!(details["remaining_steps"], 0);
    assert_eq!(details["steps"][0]["details"]["loaded"], true);
    assert_eq!(details["steps"][1]["details"]["result"], 41);
    assert_eq!(details["steps"][2]["details"]["result"], 42);
    assert_eq!(details["browserSession"]["retained"], true);
    assert!(details["failed_step"].is_null());
    worker.join().unwrap();
}

#[test]
fn batch_retains_prior_success_and_stops_after_native_failure() {
    let mut failed = evaluate(false, &["page-a"], "page-a", "failing()", Value::Null);
    failed.last_mut().unwrap().reply = Some(json!({"error":{"code":-32000,"message":"fixture failure"}}));
    let (endpoint, worker) = peer(vec![open(true, &[], "page-a"), failed]);
    let directory = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(directory.path()).with_mock(false).with_cdp_endpoint(endpoint);
    let output = run(tool.execute("batch-failure", workflow(vec![
        open_call(), eval_call("failing()"), eval_call("mustNotRun()"),
    ]), None)).unwrap();
    assert!(output.is_error);
    let details = output.details.unwrap();
    assert_eq!(details["completed_steps"], 1);
    assert_eq!(details["attempted_steps"], 2);
    assert_eq!(details["failed_step"], 1);
    assert_eq!(details["remaining_steps"], 1);
    assert_eq!(details["stop_reason"], "tool_error");
    assert_eq!(details["steps"][0]["details"]["loaded"], true);
    assert_eq!(details["steps"][1]["outcome"], "tool_error");
    assert!(output.content.iter().any(|block| matches!(block,
        crate::model::ContentBlock::Text(text) if text.text.contains("fixture failure"))));
    worker.join().unwrap();
}

#[test]
fn oversized_native_result_is_reported_without_executing_later_steps() {
    let large = "PRIVATE-RESULT".repeat(200);
    let (endpoint, worker) = peer(vec![evaluate(true, &["page-a"], "page-a", "largeResult", json!(large))]);
    let directory = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(directory.path()).with_mock(false).with_cdp_endpoint(endpoint);
    let mut input = workflow(vec![
        json!({"action":"evaluate","tab":"page-a","script":"largeResult"}),
        json!({"action":"close","tab":"page-a"}),
    ]);
    input["max_result_bytes"] = json!(1024);
    let output = run(tool.execute("batch-limit", input, None)).unwrap();
    assert!(output.is_error);
    let serialized = serde_json::to_string(&output).unwrap();
    assert!(!serialized.contains("PRIVATE-RESULT"));
    let details = output.details.unwrap();
    assert_eq!(details["stop_reason"], "result_limit");
    assert_eq!(details["attempted_steps"], 1);
    assert_eq!(details["remaining_steps"], 1);
    assert_eq!(details["steps"][0]["tool_is_error"], false);
    assert_eq!(details["steps"][0]["result_omitted"], true);
    worker.join().unwrap();
}

#[test]
fn invalid_later_envelopes_and_mock_batches_fail_before_reservation_or_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(directory.path()).with_mock(false)
        .with_cdp_endpoint(format!("http://{}", listener.local_addr().unwrap()));
    for action in ["batch", "stop", "release", "unknown"] {
        let error = run(tool.execute("invalid-batch", workflow(vec![open_call(), json!({"action":action})]), None)).unwrap_err();
        assert!(error.to_string().contains("BROWSER_BATCH_INVALID"));
        assert!(tool.sessions.namespaces.lock().unwrap().is_empty());
    }
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    let mock = BrowserTool::new(directory.path()).with_mock(true);
    assert!(run(mock.execute("no-mock", workflow(vec![open_call()]), None)).unwrap_err()
        .to_string().contains("BROWSER_BATCH_NATIVE_REQUIRED"));
    assert!(mock.sessions.namespaces.lock().unwrap().is_empty());
    let schema = tool.parameters();
    assert!(schema["properties"]["action"]["enum"].as_array().unwrap().contains(&json!("batch")));
    assert_eq!(schema["properties"]["steps"]["maxItems"], 32);
}

#[test]
fn batch_owner_change_cannot_publish_partial_payload_or_start_the_next_step() {
    let scope = JobSessionScope::fixed("owner-a");
    let changed = scope.clone();
    let (sent, received) = futures::channel::oneshot::channel::<String>();
    let new_identity = received.map(|result| result.ok()).shared();
    let mut completed = evaluate(false, &["page-a"], "page-a", "secret", json!("PRIVATE-CANARY"));
    completed.last_mut().unwrap().hook = Some(Box::new(move || {
        changed.bind(Arc::new(move || Box::pin(new_identity.clone())));
    }));
    completed.last_mut().unwrap().after_reply = Some(Box::new(move || { sent.send("owner-b".into()).unwrap(); }));
    let (endpoint, worker) = peer(vec![open(true, &[], "page-a"), completed]);
    let directory = tempfile::tempdir().unwrap();
    let mut tool = BrowserTool::new(directory.path()).with_mock(false).with_cdp_endpoint(endpoint);
    tool.bind_job_session_scope(scope.clone());
    let error = run(tool.execute("batch-owner-change", workflow(vec![
        open_call(), eval_call("secret"), eval_call("mustNotRun()"),
    ]), None)).unwrap_err().to_string();
    assert!(error.contains("BROWSER_SESSION_CHANGED"));
    assert!(!error.contains("PRIVATE-CANARY"));
    assert!(tool.sessions.execution.try_lock().is_some());
    assert_eq!(tool.sessions.namespaces.lock().unwrap().len(), 1);
    worker.join().unwrap();
}

#[test]
fn batch_holds_the_outer_lease_and_dropping_it_unblocks_queued_work_without_replay() {
    let (entered, arrived) = futures::channel::oneshot::channel::<()>();
    let mut stalled = evaluate(false, &["page-a"], "page-a", "pending", Value::Null);
    stalled.last_mut().unwrap().reply = None;
    stalled.last_mut().unwrap().hook = Some(Box::new(move || { entered.send(()).unwrap(); }));
    let (endpoint, worker) = peer(vec![
        open(true, &[], "page-a"), stalled,
        evaluate(false, &["page-a"], "page-a", "queuedAfterBatch", json!(84)),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(directory.path()).with_mock(false).with_cdp_endpoint(endpoint);
    run(async {
        let pending = tool.execute("batch-drop", workflow(vec![
            open_call(), eval_call("pending"), eval_call("mustNotRun()"),
        ]), None);
        let pending = match futures::future::select(pending, arrived).await {
            futures::future::Either::Right((Ok(()), pending)) => pending,
            _ => panic!("batch must be awaiting the withheld native reply"),
        };
        let mut queued = tool.execute("queued", eval_call("queuedAfterBatch"), None);
        assert!(futures::poll!(queued.as_mut()).is_pending());
        assert!(tool.sessions.execution.try_lock().is_none());
        drop(pending);
        let recovered = queued.await.unwrap();
        assert!(!recovered.is_error);
        assert_eq!(recovered.details.unwrap()["result"], 84);
    });
    worker.join().unwrap();
}
