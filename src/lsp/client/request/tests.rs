use super::super::test_server::Fixture;
use super::*;
use serde_json::json;

#[test]
fn zero_budget_never_dispatches_a_request() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({})) else {
        return;
    };
    let error = peer
        .runtime
        .block_on(peer.client.call("test/success", json!({}), Duration::ZERO))
        .unwrap_err();
    assert!(matches!(error, LspCallError::Timeout { timeout_ms: 0 }));
    assert!(
        !peer
            .frames()
            .iter()
            .any(|frame| frame["method"] == "test/success")
    );
}

#[test]
fn serialized_lane_admission_spends_the_request_timeout() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({})) else {
        return;
    };
    peer.runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let guard = OwnedMutexGuard::lock(Arc::clone(&peer.client.request_lane), owner.cx())
            .await
            .unwrap();
        let started = std::time::Instant::now();
        let error = peer
            .client
            .call("test/success", json!({}), Duration::from_millis(30))
            .await
            .unwrap_err();
        assert!(matches!(error, LspCallError::Timeout { timeout_ms: 30 }));
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(guard);
    });
    assert!(
        !peer
            .frames()
            .iter()
            .any(|frame| frame["method"] == "test/success")
    );
}

#[test]
fn dropping_a_posted_request_cancels_it_before_the_next_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({})) else {
        return;
    };
    peer.runtime.block_on(async {
        let mut request = Box::pin(peer.client.call(
            "test/hang",
            json!({}),
            Duration::from_secs(30),
        ));
        assert!(futures::poll!(request.as_mut()).is_pending());
        drop(request);
        assert_eq!(
            peer.client
                .call("test/success", json!({}), Duration::from_secs(5))
                .await
                .unwrap(),
            json!({"ok":true})
        );
    });
    let frames = peer.frames();
    let posted = frames
        .iter()
        .position(|frame| frame["method"] == "test/hang")
        .unwrap();
    let cancelled = frames
        .iter()
        .position(|frame| frame["method"] == "$/cancelRequest")
        .unwrap();
    let successor = frames
        .iter()
        .position(|frame| frame["method"] == "test/success")
        .unwrap();
    assert!(posted < cancelled && cancelled < successor);
    assert_eq!(frames[cancelled]["params"]["id"], frames[posted]["id"]);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["method"] == "$/cancelRequest")
            .count(),
        1
    );
}

#[test]
fn timeout_cancels_once_and_releases_the_lane() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({})) else {
        return;
    };
    let error = peer
        .runtime
        .block_on(
            peer.client
                .call("test/hang", json!({}), Duration::from_millis(30)),
        )
        .unwrap_err();
    assert!(matches!(error, LspCallError::Timeout { timeout_ms: 30 }));
    let frames = peer.frames();
    let request = frames
        .iter()
        .find(|frame| frame["method"] == "test/hang")
        .unwrap();
    let cancellations: Vec<_> = frames
        .iter()
        .filter(|frame| frame["method"] == "$/cancelRequest")
        .collect();
    assert_eq!(cancellations.len(), 1);
    assert_eq!(cancellations[0]["params"]["id"], request["id"]);
    assert!(peer.client.is_alive());
}

#[test]
fn completed_responses_do_not_emit_spurious_cancellation() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({})) else {
        return;
    };
    let result = peer
        .runtime
        .block_on(
            peer.client
                .call("test/success", json!({}), Duration::from_secs(5)),
        )
        .unwrap();
    assert_eq!(result, json!({"ok":true}));
    assert!(
        !peer
            .frames()
            .iter()
            .any(|frame| frame["method"] == "$/cancelRequest")
    );
}

#[test]
fn completed_command_errors_are_not_replayed_or_cancelled() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({})) else {
        return;
    };
    peer.configure(json!({"workspace/executeCommand":[{"error":{"code":-32801,"message":"content modified"}}]}));
    let error = peer
        .runtime
        .block_on(peer.client.call(
            "workspace/executeCommand",
            json!({"command":"fixture"}),
            Duration::from_secs(5),
        ))
        .unwrap_err();
    assert!(matches!(
        error,
        LspCallError::Transport(TransportError::Server(_))
    ));
    let frames = peer.frames();
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["method"] == "workspace/executeCommand")
            .count(),
        1
    );
    assert!(
        !frames
            .iter()
            .any(|frame| frame["method"] == "$/cancelRequest")
    );
}

#[test]
fn owner_cancellation_before_admission_prevents_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({})) else {
        return;
    };
    let owner = AgentCx::for_request();
    let budget = RequestBudget {
        start: now(&owner),
        owner,
        timeout: Duration::from_secs(5),
    };
    budget.owner.cancel_with(
        asupersync::types::CancelKind::User,
        Some("test owner cancelled"),
    );
    let error = peer
        .runtime
        .block_on(
            peer.client
                .call_with_budget("test/success", json!({}), &budget),
        )
        .unwrap_err();
    assert!(matches!(error, LspCallError::Cancelled));
    assert!(
        !peer
            .frames()
            .iter()
            .any(|frame| frame["method"] == "test/success")
    );
}

#[test]
fn prepare_rename_retries_during_warmup_on_no_references_found() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({})) else {
        return;
    };
    peer.configure(json!({
        "textDocument/prepareRename": [
            {"error": {"code": -32602, "message": "No references found at position"}},
            {"result": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}}
        ]
    }));
    let result = peer
        .runtime
        .block_on(peer.client.call(
            "textDocument/prepareRename",
            json!({"textDocument": {"uri": "file:///test.rs"}, "position": {"line": 0, "character": 0}}),
            Duration::from_secs(5),
        ))
        .unwrap();
    assert_eq!(
        result,
        json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}})
    );
    let frames = peer.frames();
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["method"] == "textDocument/prepareRename")
            .count(),
        2
    );
}

#[test]
fn prepare_rename_retries_during_warmup_on_null_until_target_ready() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({})) else {
        return;
    };
    peer.configure(json!({
        "textDocument/prepareRename": [
            {"result": null},
            {"result": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}}
        ]
    }));
    let result = peer
        .runtime
        .block_on(peer.client.call(
            "textDocument/prepareRename",
            json!({"textDocument": {"uri": "file:///test.rs"}, "position": {"line": 0, "character": 0}}),
            Duration::from_secs(5),
        ))
        .unwrap();
    assert_eq!(
        result,
        json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}})
    );
    let frames = peer.frames();
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["method"] == "textDocument/prepareRename")
            .count(),
        2
    );
}

#[test]
fn prepare_rename_does_not_retry_null_when_server_is_quiescent() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({})) else {
        return;
    };
    peer.client.quiescent.store(true, Ordering::SeqCst);
    peer.configure(json!({
        "textDocument/prepareRename": [
            {"result": null}
        ]
    }));
    let result = peer
        .runtime
        .block_on(peer.client.call(
            "textDocument/prepareRename",
            json!({"textDocument": {"uri": "file:///test.rs"}, "position": {"line": 0, "character": 0}}),
            Duration::from_secs(5),
        ))
        .unwrap();
    assert_eq!(result, Value::Null);
    let frames = peer.frames();
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["method"] == "textDocument/prepareRename")
            .count(),
        1
    );
}
