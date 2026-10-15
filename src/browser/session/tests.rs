use super::*;
use crate::tools::{Tool, ToolRegistry};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn run<F: Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build().unwrap().block_on(future)
}

fn select(scope: &JobSessionScope, id: &str) {
    let id = id.to_owned();
    scope.bind(Arc::new(move || {
        let id = id.clone();
        Box::pin(async move { Some(id) })
    }));
}

fn scoped(path: &std::path::Path) -> (BrowserTool, JobSessionScope) {
    let scope = JobSessionScope::fixed("owner-a");
    let mut tool = BrowserTool::new(path).with_mock(true);
    tool.bind_job_session_scope(scope.clone());
    (tool, scope)
}

async fn call(tool: &dyn Tool, args: Value) -> ToolOutput {
    let result = tool.execute("session-test", args, None).await.unwrap();
    assert!(!result.is_error);
    result
}

async fn location(tool: &dyn Tool) -> String {
    let result = call(tool, json!({"action":"list_tabs"})).await;
    result.details.unwrap()["tabs"].as_array().unwrap().iter()
        .find(|tab| tab["name"] == "work").unwrap()["url"].as_str().unwrap().to_owned()
}

#[test]
fn registry_binding_keeps_same_named_mock_tabs_in_independent_conversations() {
    // This explicitly selected fixture backend tests namespace routing, not Chromium.
    let dir = tempfile::tempdir().unwrap();
    let registry = ToolRegistry::from_tools(vec![Box::new(BrowserTool::new(dir.path()).with_mock(true))]);
    let scope = registry.job_session_scope();
    let tool = registry.get("browser").unwrap();
    run(async {
        select(&scope, "owner-a");
        call(tool, json!({"action":"open","tab":"work","url":"https://example.com/a"})).await;
        select(&scope, "owner-b");
        let empty = call(tool, json!({"action":"list_tabs"})).await;
        assert!(!empty.details.unwrap().to_string().contains("example.com/a"));
        call(tool, json!({"action":"open","tab":"work","url":"https://example.com/b"})).await;
        select(&scope, "owner-a");
        assert_eq!(location(tool).await, "https://example.com/a");
        select(&scope, "owner-b");
        assert_eq!(location(tool).await, "https://example.com/b");
        call(tool, json!({"action":"release"})).await;
        select(&scope, "owner-a");
        assert_eq!(location(tool).await, "https://example.com/a");
    });
}

#[test]
fn capacity_refuses_without_eviction_and_release_only_frees_the_selected_owner() {
    let dir = tempfile::tempdir().unwrap();
    let (tool, scope) = scoped(dir.path());
    run(async {
        for index in 0..CAPACITY {
            select(&scope, &format!("owner-{index}"));
            call(&tool, json!({"action":"open","tab":"work","url":format!("https://example.com/{index}")})).await;
        }
        select(&scope, "extra");
        let failure = tool.execute("full", json!({"action":"open","url":"https://example.com/extra"}), None).await.unwrap_err();
        assert!(failure.to_string().contains("BROWSER_SESSION_CAPACITY"));
        assert_eq!(tool.sessions.namespaces.lock().unwrap().len(), CAPACITY);
        assert!(!call(&tool, json!({"action":"release"})).await.details.unwrap()["released"].as_bool().unwrap());
        select(&scope, "owner-0");
        assert_eq!(location(&tool).await, "https://example.com/0");
        assert_eq!(call(&tool, json!({"action":"release"})).await.details.unwrap()["released"], true);
        select(&scope, "extra");
        call(&tool, json!({"action":"open","tab":"work","url":"https://example.com/extra"})).await;
        select(&scope, "owner-1");
        assert_eq!(location(&tool).await, "https://example.com/1");
    });
}

#[test]
fn unknown_native_status_and_stop_do_not_launch_or_consume_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let scope = JobSessionScope::fixed("unused");
    let mut tool = BrowserTool::new(dir.path()).with_mock(false)
        .with_launch_options(super::super::BrowserLaunchOptions {
            executable_path: Some(dir.path().join("must-not-launch")),
            ..Default::default()
        });
    tool.bind_job_session_scope(scope.clone());
    run(async {
        for index in 0..32 {
            select(&scope, &format!("empty-{index}"));
            for action in ["status", "stop"] {
                let result = call(&tool, json!({"action":action})).await;
                assert_eq!(result.details.as_ref().unwrap()["running"], false);
                assert_eq!(result.details.unwrap()["browserSession"]["retained"], false);
            }
        }
    });
    assert!(tool.sessions.namespaces.lock().unwrap().is_empty());
    assert!(!dir.path().join("must-not-launch").exists());
}

#[test]
fn stale_queued_work_is_rejected_without_selecting_or_releasing_an_owner() {
    let dir = tempfile::tempdir().unwrap();
    let (tool, scope) = scoped(dir.path());
    run(async {
        call(&tool, json!({"action":"open","tab":"work","url":"https://example.com/a"})).await;
        let lease = tool.sessions.execution.lock().await;
        let mut queued = tool.execute("queued", json!({"action":"release"}), None);
        assert!(futures::poll!(queued.as_mut()).is_pending());
        select(&scope, "owner-b");
        drop(lease);
        let failure = queued.await.unwrap_err();
        assert!(failure.to_string().contains("BROWSER_SESSION_CHANGED"));
        assert_eq!(tool.sessions.namespaces.lock().unwrap().len(), 1);
        select(&scope, "owner-a");
        assert_eq!(location(&tool).await, "https://example.com/a");
    });
}

#[test]
fn abandoned_queue_and_missing_identity_never_allocate_a_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let (tool, scope) = scoped(dir.path());
    run(async {
        let lease = tool.sessions.execution.lock().await;
        let mut queued = tool.execute("queued", json!({"action":"open","url":"https://example.com"}), None);
        assert!(futures::poll!(queued.as_mut()).is_pending());
        drop(queued);
        for id in [None, Some(String::new()), Some(" \n\t".into()), Some("x".repeat(MAX_OWNER_BYTES + 1))] {
            scope.bind(Arc::new(move || {
                let id = id.clone();
                Box::pin(async move { id })
            }));
            let failure = tool.execute("missing", json!({"action":"release"}), None).await.unwrap_err();
            assert!(failure.to_string().contains("BROWSER_SESSION_UNAVAILABLE"));
        }
        drop(lease);
    });
    assert!(tool.sessions.namespaces.lock().unwrap().is_empty());
}

#[test]
fn binding_an_owned_tool_to_another_registry_retires_all_prior_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    let (mut tool, scope) = scoped(dir.path());
    let weak = run(async {
        call(&tool, json!({"action":"open","url":"https://example.com/a"})).await;
        select(&scope, "owner-b");
        call(&tool, json!({"action":"open","url":"https://example.com/b"})).await;
        tool.sessions.namespaces.lock().unwrap().values().map(Arc::downgrade).collect::<Vec<_>>()
    });
    tool.bind_job_session_scope(JobSessionScope::fixed("another-registry"));
    assert!(weak.iter().all(|namespace| namespace.upgrade().is_none()));
    assert!(tool.sessions.namespaces.lock().unwrap().is_empty());
}

#[test]
fn release_never_accepts_a_model_selected_owner_or_ignores_foreign_fields() {
    let dir = tempfile::tempdir().unwrap();
    let (tool, _) = scoped(dir.path());
    run(async {
        call(&tool, json!({"action":"open","url":"https://example.com/a"})).await;
        for field in ["session", "session_id", "tab", "frame", "diagnostics", "accept", "url"] {
            let mut args = json!({"action":"release"});
            args[field] = json!("PRIVATE-CANARY");
            let failure = tool.execute("invalid", args, None).await.unwrap_err().to_string();
            assert!(failure.contains("BROWSER_SESSION_INVALID"));
            assert!(!failure.contains("PRIVATE-CANARY"));
            assert_eq!(tool.sessions.namespaces.lock().unwrap().len(), 1);
        }
        assert_eq!(call(&tool, json!({"action":"release"})).await.details.unwrap()["released"], true);
        assert_eq!(call(&tool, json!({"action":"release"})).await.details.unwrap()["released"], false);
    });
}

#[test]
fn cancelled_identity_wait_drops_the_resolver_and_releases_the_execution_queue() {
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); }
    }
    let dir = tempfile::tempdir().unwrap();
    let (tool, scope) = scoped(dir.path());
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    let dropped = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&dropped);
    scope.bind(Arc::new(move || {
        let first = seen.fetch_add(1, Ordering::SeqCst) == 0;
        let drop_guard = Dropped(Arc::clone(&observed));
        Box::pin(async move {
            if first { return Some("owner-a".into()); }
            let _guard = drop_guard;
            std::future::pending::<Option<String>>().await
        })
    }));
    let owner = AgentCx::for_request();
    run(async {
        let mut pending = Box::pin(owner.with_current(tool.execute("pending", json!({"action":"release"}), None)));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        assert_eq!(count.load(Ordering::SeqCst), 2);
        dropped.store(false, Ordering::SeqCst);
        owner.set_cancel_requested(true);
        assert!(pending.await.unwrap_err().to_string().contains("BROWSER_SESSION_CANCELLED"));
        assert!(dropped.load(Ordering::SeqCst));
        assert!(tool.sessions.execution.try_lock().is_some());
        assert!(tool.sessions.namespaces.lock().unwrap().is_empty());
        select(&scope, "owner-b");
        let fresh = AgentCx::for_request();
        assert!(fresh.with_current(tool.execute("fresh", json!({"action":"release"}), None)).await.is_ok());
    });
}

#[test]
fn queue_wait_uses_the_same_deadline_and_never_acquires_a_namespace_after_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let (tool, _) = scoped(dir.path());
    run(async {
        let lease = tool.sessions.execution.lock().await;
        let error = tool.execute("expired", json!({"action":"open","url":"https://example.com","timeout_ms":30}), None).await.unwrap_err();
        assert!(error.to_string().contains("BROWSER_SESSION_TIMEOUT"));
        drop(lease);
        assert!(tool.sessions.namespaces.lock().unwrap().is_empty());
        assert!(tool.sessions.execution.try_lock().is_some());
    });
}
