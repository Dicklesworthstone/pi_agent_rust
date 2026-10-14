use super::*;
use crate::tools::{Tool, ToolRegistry};
use serde_json::json;
use std::sync::Arc;

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
        .block_on(future)
}

fn select(scope: &JobSessionScope, id: &str) {
    let id = id.to_string();
    scope.bind(Arc::new(move || {
        let id = id.clone();
        Box::pin(async move { Some(id) })
    }));
}

fn text(output: &ToolOutput) -> String {
    output.content.iter().filter_map(|block| match block {
        crate::model::ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    }).collect()
}

async fn cell(tool: &dyn Tool, language: &str, code: &str) -> ToolOutput {
    let output = tool.execute("cell", json!({"kernel":language,"code":code}), None)
        .await.unwrap();
    assert!(!output.is_error, "{}", text(&output));
    output
}

fn scoped(dir: &std::path::Path) -> (EvalTool, JobSessionScope) {
    let scope = JobSessionScope::fixed("session-a");
    let mut tool = EvalTool::new(dir);
    tool.bind_job_session_scope(scope.clone());
    (tool, scope)
}

fn python_available() -> bool {
    let available = std::process::Command::new("python3").arg("--version")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .status().is_ok_and(|status| status.success());
    if !available { eprintln!("Skipping Python session case: python3 unavailable"); }
    available
}

#[test]
fn actual_registry_binding_isolates_and_restores_lexical_javascript_state() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ToolRegistry::from_tools(vec![Box::new(EvalTool::new(dir.path()))]);
    let scope = registry.job_session_scope();
    let tool = registry.get("eval").unwrap();
    run(async {
        select(&scope, "session-a");
        cell(tool, "js", "const secret = 41; secret").await;
        select(&scope, "session-b");
        let absent = cell(tool, "js", "typeof secret").await;
        assert_eq!(text(&absent).trim(), "\"undefined\"");
        assert_eq!(absent.details.as_ref().unwrap()["cell"], 1);
        cell(tool, "js", "const secret = 99; secret").await;
        select(&scope, "session-a");
        let restored = cell(tool, "js", "secret + 1").await;
        assert_eq!(text(&restored).trim(), "42");
        let details = restored.details.unwrap();
        assert_eq!(details["cell"], 2);
        assert_eq!(details["restarted"], false);
        assert_eq!(details["kernelSession"]["namespaceRestored"], true);
        assert!(!details.to_string().contains("session-b"));
        select(&scope, "session-b");
        assert_eq!(text(&cell(tool, "js", "secret").await).trim(), "99");
    });
}

#[test]
fn python_namespaces_and_both_languages_survive_switch_back_independently() {
    if !python_available() { return; }
    let dir = tempfile::tempdir().unwrap();
    let (tool, scope) = scoped(dir.path());
    run(async {
        cell(&tool, "python", "secret = 42").await;
        cell(&tool, "js", "const secret = 43").await;
        select(&scope, "session-b");
        assert_eq!(text(&cell(&tool, "python", "'secret' in globals()").await).trim(), "False");
        cell(&tool, "python", "secret = 99").await;
        cell(&tool, "js", "const secret = 100").await;
        select(&scope, "session-a");
        assert_eq!(text(&cell(&tool, "python", "secret").await).trim(), "42");
        assert_eq!(text(&cell(&tool, "js", "secret").await).trim(), "43");
        select(&scope, "session-b");
        assert_eq!(text(&cell(&tool, "python", "secret").await).trim(), "99");
        assert_eq!(text(&cell(&tool, "js", "secret").await).trim(), "100");
    });
}

#[test]
fn reset_and_ordinary_failure_touch_only_the_selected_session() {
    let dir = tempfile::tempdir().unwrap();
    let (tool, scope) = scoped(dir.path());
    run(async {
        cell(&tool, "js", "globalThis.keep = 42").await;
        select(&scope, "session-b");
        let failed = tool.execute("error", json!({
            "kernel":"js", "code":"globalThis.keep = 9; throw new Error('expected')"
        }), None).await.unwrap();
        assert!(failed.is_error);
        select(&scope, "session-a");
        assert_eq!(text(&cell(&tool, "js", "keep").await).trim(), "42");
        select(&scope, "session-b");
        assert_eq!(text(&cell(&tool, "js", "keep").await).trim(), "9");
        let reset = tool.execute("reset", json!({
            "kernel":"js", "code":"typeof keep", "reset":true
        }), None).await.unwrap();
        assert!(!reset.is_error);
        assert!(text(&reset).contains("undefined"));
        assert_eq!(reset.details.unwrap()["cell"], 1);
        select(&scope, "session-a");
        assert_eq!(text(&cell(&tool, "js", "keep").await).trim(), "42");
    });
}

#[test]
fn queued_cell_cannot_follow_a_session_switch_or_apply_its_reset() {
    let dir = tempfile::tempdir().unwrap();
    let (tool, scope) = scoped(dir.path());
    run(async {
        cell(&tool, "js", "globalThis.keep = 42").await;
        let lease = tool.execution.lock().await;
        let mut queued = tool.execute("queued", json!({
            "kernel":"js", "code":"globalThis.keep = 99", "reset":true
        }), None);
        assert!(futures::poll!(queued.as_mut()).is_pending());
        select(&scope, "session-b");
        drop(lease);
        let error = queued.await.unwrap_err();
        assert!(error.to_string().contains("EVAL_SESSION_CHANGED"));
        assert!(tool.execution.try_lock().is_some());
        assert_eq!(tool.retained_sessions.lock().unwrap().active.as_deref(), Some("session-a"));
        select(&scope, "session-a");
        assert_eq!(text(&cell(&tool, "js", "keep").await).trim(), "42");
    });
}

#[test]
fn missing_or_oversized_identity_refuses_before_admission_without_state_loss() {
    let dir = tempfile::tempdir().unwrap();
    let (tool, scope) = scoped(dir.path());
    run(async {
        cell(&tool, "js", "const keep = 42").await;
        let lease = tool.execution.lock().await;
        for unavailable in [true, false] {
            if unavailable {
                scope.bind(Arc::new(|| Box::pin(async { None })));
            } else {
                select(&scope, &"x".repeat(MAX_SESSION_ID_BYTES + 1));
            }
            let mut request = tool.execute("no-owner", json!({
                "kernel":"js", "code":"", "reset":true
            }), None);
            match futures::poll!(request.as_mut()) {
                std::task::Poll::Ready(Err(error)) => {
                    assert!(error.to_string().contains("EVAL_SESSION_UNAVAILABLE"));
                }
                _ => panic!("unavailable identity must be refused before queueing"),
            }
        }
        drop(lease);
        select(&scope, "session-a");
        assert_eq!(text(&cell(&tool, "js", "keep").await).trim(), "42");
    });
}

#[test]
fn invalid_input_and_failed_source_preflight_do_not_activate_another_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let (tool, scope) = scoped(dir.path());
    run(async {
        cell(&tool, "js", "const keep = 42").await;
        select(&scope, "session-b");
        for request in [
            json!({"kernel":"typo", "code":"", "reset":true}),
            json!({"kernel":"js", "code":"%load missing-file.js", "reset":true}),
        ] {
            assert!(tool.execute("invalid", request, None).await.is_err());
            let retained = tool.retained_sessions.lock().unwrap();
            assert_eq!(retained.active.as_deref(), Some("session-a"));
            assert!(retained.dormant.is_empty());
        }
        select(&scope, "session-a");
        assert_eq!(text(&cell(&tool, "js", "keep").await).trim(), "42");
    });
}

#[test]
fn idle_cache_is_bounded_lru_and_evicted_sessions_restart_explicitly() {
    let dir = tempfile::tempdir().unwrap();
    let (tool, scope) = scoped(dir.path());
    run(async {
        for index in 0..MAX_RETAINED_SESSIONS {
            select(&scope, &format!("s{index}"));
            cell(&tool, "js", &format!("const keep = {index}")).await;
        }
        select(&scope, "s0");
        assert_eq!(text(&cell(&tool, "js", "keep").await).trim(), "0");
        select(&scope, "new-session");
        let fresh = cell(&tool, "js", "typeof keep").await;
        assert_eq!(fresh.details.unwrap()["kernelSession"]["idleNamespaceEvicted"], true);
        assert_eq!(tool.retained_sessions.lock().unwrap().dormant.len(), MAX_RETAINED_SESSIONS - 1);
        select(&scope, "s0");
        assert_eq!(text(&cell(&tool, "js", "keep").await).trim(), "0");
        select(&scope, "s1");
        let evicted = cell(&tool, "js", "typeof keep").await;
        assert_eq!(text(&evicted).trim(), "\"undefined\"");
        let details = evicted.details.unwrap();
        assert_eq!(details["restarted"], true);
        assert_eq!(details["cell"], 1);
        assert_eq!(details["kernelSession"]["namespaceRestored"], false);
    });
}

#[test]
fn rebinding_the_tool_to_a_registry_retires_the_previous_storage_incarnation() {
    let dir = tempfile::tempdir().unwrap();
    let (mut tool, scope) = scoped(dir.path());
    run(cell(&tool, "js", "const keep = 42"));
    select(&scope, "session-b");
    run(cell(&tool, "js", "const keep = 99"));
    tool.bind_job_session_scope(scope.clone());
    assert!(tool.retained_sessions.lock().unwrap().dormant.is_empty());
    for session_id in ["session-a", "session-b"] {
        select(&scope, session_id);
        let fresh = run(cell(&tool, "js", "typeof keep"));
        assert_eq!(text(&fresh).trim(), "\"undefined\"");
        assert_eq!(fresh.details.unwrap()["restarted"], true);
    }
}

#[test]
fn dropping_a_queued_reset_does_not_rotate_the_namespace_cache() {
    let dir = tempfile::tempdir().unwrap();
    let (tool, scope) = scoped(dir.path());
    run(async {
        cell(&tool, "js", "const keep = 42").await;
        select(&scope, "session-b");
        let lease = tool.execution.lock().await;
        let mut queued = tool.execute("dropped", json!({"kernel":"js", "code":"", "reset":true}), None);
        assert!(futures::poll!(queued.as_mut()).is_pending());
        drop(queued);
        drop(lease);
        assert_eq!(tool.retained_sessions.lock().unwrap().active.as_deref(), Some("session-a"));
        select(&scope, "session-a");
        assert_eq!(text(&cell(&tool, "js", "keep").await).trim(), "42");
    });
}
