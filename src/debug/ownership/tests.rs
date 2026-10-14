use super::*;
use crate::tools::Tool;
use asupersync::sync::OwnedMutexGuard;
use serde_json::json;

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
        .block_on(future)
}

fn select(scope: &JobSessionScope, value: Option<String>) {
    scope.bind(Arc::new(move || {
        let value = value.clone();
        Box::pin(async move { value })
    }));
}

fn scoped(path: &std::path::Path) -> (DebugTool, JobSessionScope) {
    let scope = JobSessionScope::fixed("owner-a");
    let mut tool = DebugTool::new(path, None);
    tool.bind_job_session_scope(scope.clone());
    (tool, scope)
}

#[test]
fn stale_queued_request_does_not_select_another_owner() {
    let directory = tempfile::tempdir().unwrap();
    let (tool, scope) = scoped(directory.path());
    run(async {
        let owner = AgentCx::for_current_or_request();
        let lease = OwnedMutexGuard::lock(Arc::clone(&tool.operations), owner.cx()).await.unwrap();
        let mut queued = Box::pin(tool.execute("queued", json!({"action":"sessions"}), None));
        assert!(futures::poll!(queued.as_mut()).is_pending());
        select(&scope, Some("owner-b".into()));
        drop(lease);
        let error = queued.await.unwrap_err();
        assert!(error.to_string().contains("DAP_SESSION_CHANGED"));
        assert!(lock(&tool.session_owners).active.is_none());
        assert!(lock(&tool.session_owners).parked.is_empty());
    });
}

#[test]
fn abandoned_queued_request_never_activates_a_session() {
    let directory = tempfile::tempdir().unwrap();
    let (tool, scope) = scoped(directory.path());
    run(async {
        let owner = AgentCx::for_current_or_request();
        let lease = OwnedMutexGuard::lock(Arc::clone(&tool.operations), owner.cx()).await.unwrap();
        let mut queued = Box::pin(tool.execute("queued", json!({"action":"sessions"}), None));
        assert!(futures::poll!(queued.as_mut()).is_pending());
        drop(queued);
        select(&scope, Some("owner-b".into()));
        drop(lease);
        assert!(lock(&tool.session_owners).active.is_none());
        let output = tool.execute("current", json!({"action":"sessions"}), None).await.unwrap();
        assert_eq!(output.details.unwrap()["sessions"], json!([]));
        assert_eq!(lock(&tool.session_owners).active.as_deref(), Some("owner-b"));
    });
}

#[test]
fn missing_blank_and_oversized_owners_never_fall_back() {
    let directory = tempfile::tempdir().unwrap();
    let (tool, scope) = scoped(directory.path());
    run(async {
        tool.execute("first", json!({"action":"sessions"}), None).await.unwrap();
        for value in [None, Some(String::new()), Some(" \n\t".into()), Some("x".repeat(MAX_OWNER_BYTES + 1))] {
            select(&scope, value);
            let error = tool.execute("invalid-owner", json!({"action":"sessions"}), None).await.unwrap_err();
            assert!(error.to_string().contains("DAP_SESSION_UNAVAILABLE"));
            assert_eq!(lock(&tool.session_owners).active.as_deref(), Some("owner-a"));
            assert!(lock(&tool.session_owners).parked.is_empty());
        }
        select(&scope, Some("owner-a".into()));
        tool.execute("restored", json!({"action":"sessions"}), None).await.unwrap();
    });
}

#[test]
fn empty_owner_visits_are_not_retained_and_rebinding_clears_selection() {
    let directory = tempfile::tempdir().unwrap();
    let (mut tool, scope) = scoped(directory.path());
    run(async {
        for index in 0..128 {
            select(&scope, Some(format!("owner-{index}")));
            tool.execute("empty", json!({"action":"sessions"}), None).await.unwrap();
            assert!(lock(&tool.session_owners).parked.is_empty());
            assert!(lock(&tool.session).is_none());
            tool.require_debug_capacity().unwrap();
        }
    });
    tool.bind_job_session_scope(JobSessionScope::fixed("new-registry"));
    assert!(lock(&tool.session_owners).active.is_none());
    assert!(lock(&tool.session_owners).parked.is_empty());
    assert!(lock(&tool.session).is_none());
}
