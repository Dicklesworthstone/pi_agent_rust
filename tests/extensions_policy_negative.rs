//! Negative conformance tests for denied capabilities (bd-2ce).
//!
//! Tests verify that the extension policy system correctly denies hostcalls to
//! forbidden capabilities and returns predictable error messages across all
//! three policy modes (Strict, Prompt, Permissive).
//!
//! Scope:
//! - Unit tests for [`ExtensionPolicy::evaluate()`] across all modes
//! - Unit tests for [`required_capability_for_host_call()`] mapping
//! - Integration tests loading a JS extension that attempts denied hostcalls

mod common;

use chrono::{SecondsFormat, Utc};
use pi::extensions::{
    ExtensionManager, ExtensionPolicy, ExtensionPolicyMode, HostCallPayload, JsExtensionLoadSpec,
    JsExtensionRuntimeHandle, PolicyDecision,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn reports_dir() -> PathBuf {
    project_root().join("tests/ext_conformance/reports/negative")
}

fn negative_ext_path() -> PathBuf {
    project_root()
        .join("tests/ext_conformance/artifacts/negative-denied-caps/negative-denied-caps.ts")
}

fn make_hostcall_payload(method: &str, capability: &str, params: Value) -> HostCallPayload {
    HostCallPayload {
        call_id: format!("test-{method}-{capability}"),
        capability: capability.to_string(),
        method: method.to_string(),
        params,
        timeout_ms: None,
        cancel_token: None,
        context: None,
    }
}

// ============================================================================
// Unit tests: ExtensionPolicy::evaluate() across all modes
// ============================================================================

#[test]
fn deny_caps_exec_denied_in_all_modes() {
    for mode in [
        ExtensionPolicyMode::Strict,
        ExtensionPolicyMode::Prompt,
        ExtensionPolicyMode::Permissive,
    ] {
        let policy = ExtensionPolicy {
            mode,
            deny_caps: vec!["exec".to_string(), "env".to_string()],
            ..Default::default()
        };
        let check = policy.evaluate("exec");
        assert_eq!(
            check.decision,
            PolicyDecision::Deny,
            "exec should be denied in {mode:?} mode"
        );
        assert_eq!(check.reason, "deny_caps");
        assert_eq!(check.capability, "exec");
    }
}

#[test]
fn deny_caps_env_denied_in_all_modes() {
    for mode in [
        ExtensionPolicyMode::Strict,
        ExtensionPolicyMode::Prompt,
        ExtensionPolicyMode::Permissive,
    ] {
        let policy = ExtensionPolicy {
            mode,
            deny_caps: vec!["exec".to_string(), "env".to_string()],
            ..Default::default()
        };
        let check = policy.evaluate("env");
        assert_eq!(
            check.decision,
            PolicyDecision::Deny,
            "env should be denied in {mode:?} mode"
        );
        assert_eq!(check.reason, "deny_caps");
    }
}

#[test]
fn deny_caps_case_insensitive() {
    let policy = ExtensionPolicy::default();
    for variant in ["EXEC", "Exec", "eXeC", "ENV", "Env", "eNv"] {
        let check = policy.evaluate(variant);
        assert_eq!(
            check.decision,
            PolicyDecision::Deny,
            "{variant} should be denied (case insensitive)"
        );
        assert_eq!(check.reason, "deny_caps");
    }
}

#[test]
fn deny_caps_with_whitespace_trimmed() {
    let policy = ExtensionPolicy::default();
    for variant in ["  exec  ", "\texec\t", " env "] {
        let check = policy.evaluate(variant);
        assert_eq!(
            check.decision,
            PolicyDecision::Deny,
            "{variant:?} should be denied after trimming"
        );
    }
}

#[test]
fn empty_capability_denied_in_all_modes() {
    for mode in [
        ExtensionPolicyMode::Strict,
        ExtensionPolicyMode::Prompt,
        ExtensionPolicyMode::Permissive,
    ] {
        let policy = ExtensionPolicy {
            mode,
            ..Default::default()
        };
        for empty in ["", "   ", "\t", "  \n  "] {
            let check = policy.evaluate(empty);
            assert_eq!(
                check.decision,
                PolicyDecision::Deny,
                "empty capability {empty:?} should be denied in {mode:?}"
            );
            assert_eq!(check.reason, "empty_capability");
            assert!(check.capability.is_empty());
        }
    }
}

#[test]
fn default_caps_allowed_in_prompt_mode() {
    let policy = ExtensionPolicy::default();
    for cap in ["read", "write", "http", "events", "session"] {
        let check = policy.evaluate(cap);
        assert_eq!(
            check.decision,
            PolicyDecision::Allow,
            "{cap} should be allowed in Prompt mode"
        );
        assert_eq!(check.reason, "default_caps");
    }
}

#[test]
fn default_caps_allowed_in_strict_mode() {
    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        ..Default::default()
    };
    for cap in ["read", "write", "http", "events", "session"] {
        let check = policy.evaluate(cap);
        assert_eq!(
            check.decision,
            PolicyDecision::Allow,
            "{cap} should be allowed in Strict mode"
        );
        assert_eq!(check.reason, "default_caps");
    }
}

#[test]
fn default_caps_allowed_in_permissive_mode() {
    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        ..Default::default()
    };
    for cap in ["read", "write", "http", "events", "session"] {
        let check = policy.evaluate(cap);
        assert_eq!(
            check.decision,
            PolicyDecision::Allow,
            "{cap} should be allowed in Permissive mode"
        );
        // Permissive mode reason is "permissive" for all caps
        assert_eq!(check.reason, "permissive");
    }
}

#[test]
fn unknown_cap_prompt_in_prompt_mode() {
    let policy = ExtensionPolicy::default();
    for cap in ["custom_cap", "gpu", "network", "filesystem"] {
        let check = policy.evaluate(cap);
        assert_eq!(
            check.decision,
            PolicyDecision::Prompt,
            "{cap} should require prompt in Prompt mode"
        );
        assert_eq!(check.reason, "prompt_required");
    }
}

#[test]
fn unknown_cap_denied_in_strict_mode() {
    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        ..Default::default()
    };
    for cap in ["custom_cap", "gpu", "network", "filesystem"] {
        let check = policy.evaluate(cap);
        assert_eq!(
            check.decision,
            PolicyDecision::Deny,
            "{cap} should be denied in Strict mode"
        );
        assert_eq!(check.reason, "not_in_default_caps");
    }
}

#[test]
fn unknown_cap_allowed_in_permissive_mode() {
    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        ..Default::default()
    };
    for cap in ["custom_cap", "gpu", "network", "filesystem"] {
        let check = policy.evaluate(cap);
        assert_eq!(
            check.decision,
            PolicyDecision::Allow,
            "{cap} should be allowed in Permissive mode"
        );
        assert_eq!(check.reason, "permissive");
    }
}

#[test]
fn deny_caps_override_default_caps() {
    // If a capability is in both default_caps and deny_caps, deny_caps wins.
    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Prompt,
        default_caps: vec!["read".to_string(), "exec".to_string()],
        deny_caps: vec!["exec".to_string()],
        ..Default::default()
    };
    let check = policy.evaluate("exec");
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "deny_caps");

    let check = policy.evaluate("read");
    assert_eq!(check.decision, PolicyDecision::Allow);
}

#[test]
fn deny_caps_override_permissive_mode() {
    // Even in Permissive mode, deny_caps are still denied.
    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        deny_caps: vec!["exec".to_string(), "env".to_string()],
        ..Default::default()
    };
    let check = policy.evaluate("exec");
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "deny_caps");

    // But unknown caps are allowed
    let check = policy.evaluate("unknown_thing");
    assert_eq!(check.decision, PolicyDecision::Allow);
    assert_eq!(check.reason, "permissive");
}

#[test]
fn custom_deny_caps_list() {
    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Prompt,
        deny_caps: vec![
            "dangerous".to_string(),
            "forbidden".to_string(),
            "nuclear".to_string(),
        ],
        ..Default::default()
    };
    for cap in ["dangerous", "forbidden", "nuclear"] {
        let check = policy.evaluate(cap);
        assert_eq!(
            check.decision,
            PolicyDecision::Deny,
            "{cap} should be denied"
        );
    }
    // exec is NOT denied here (not in custom deny list)
    let check = policy.evaluate("exec");
    assert_eq!(check.decision, PolicyDecision::Prompt);
}

#[test]
fn empty_deny_caps_list() {
    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Prompt,
        deny_caps: vec![],
        ..Default::default()
    };
    // exec is no longer denied
    let check = policy.evaluate("exec");
    assert_eq!(check.decision, PolicyDecision::Prompt);
    assert_eq!(check.reason, "prompt_required");
}

#[test]
fn empty_default_caps_list() {
    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        default_caps: vec![],
        deny_caps: vec!["exec".to_string()],
        ..Default::default()
    };
    // In Strict mode with empty default_caps, everything unknown is denied
    let check = policy.evaluate("read");
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "not_in_default_caps");

    // deny_caps still works
    let check = policy.evaluate("exec");
    assert_eq!(check.decision, PolicyDecision::Deny);
    assert_eq!(check.reason, "deny_caps");
}

// ============================================================================
// Unit tests: required_capability_for_host_call() mapping
// ============================================================================

#[test]
fn hostcall_exec_maps_to_exec_capability() {
    let payload = make_hostcall_payload("exec", "exec", json!({"cmd": "ls", "args": ["-la"]}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("exec"));
}

#[test]
fn hostcall_env_maps_to_env_capability() {
    let payload = make_hostcall_payload("env", "env", json!({"op": "get", "key": "HOME"}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("env"));
}

#[test]
fn hostcall_tool_bash_maps_to_exec_capability() {
    let payload = make_hostcall_payload("tool", "exec", json!({"name": "bash", "input": {}}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("exec"));
}

#[test]
fn hostcall_tool_read_maps_to_read_capability() {
    let payload = make_hostcall_payload(
        "tool",
        "read",
        json!({"name": "read", "input": {"path": "file.txt"}}),
    );
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("read"));
}

#[test]
fn hostcall_tool_write_maps_to_write_capability() {
    let payload = make_hostcall_payload(
        "tool",
        "write",
        json!({"name": "write", "input": {"path": "file.txt", "content": "hello"}}),
    );
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("write"));
}

#[test]
fn hostcall_http_maps_to_http_capability() {
    let payload = make_hostcall_payload("http", "http", json!({"url": "https://example.com"}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("http"));
}

#[test]
fn hostcall_session_maps_to_session_capability() {
    let payload = make_hostcall_payload("session", "session", json!({"op": "getSessionName"}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("session"));
}

#[test]
fn hostcall_events_maps_to_events_capability() {
    let payload = make_hostcall_payload("events", "events", json!({"op": "emit", "event": "test"}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("events"));
}

#[test]
fn hostcall_ui_maps_to_ui_capability() {
    let payload = make_hostcall_payload("ui", "ui", json!({"op": "notify", "message": "hi"}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("ui"));
}

#[test]
fn hostcall_fs_read_maps_to_read_capability() {
    let payload = make_hostcall_payload("fs", "read", json!({"op": "read", "path": "/tmp/x"}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("read"));
}

#[test]
fn hostcall_fs_write_maps_to_write_capability() {
    let payload = make_hostcall_payload(
        "fs",
        "write",
        json!({"op": "write", "path": "/tmp/x", "data": "hello"}),
    );
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("write"));
}

#[test]
fn hostcall_fs_delete_maps_to_write_capability() {
    let payload = make_hostcall_payload("fs", "write", json!({"op": "delete", "path": "/tmp/x"}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("write"));
}

#[test]
fn hostcall_fs_stat_maps_to_read_capability() {
    let payload = make_hostcall_payload("fs", "read", json!({"op": "stat", "path": "/tmp/x"}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("read"));
}

#[test]
fn hostcall_fs_list_maps_to_read_capability() {
    let payload = make_hostcall_payload("fs", "read", json!({"op": "list", "path": "/tmp"}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("read"));
}

#[test]
fn hostcall_fs_mkdir_maps_to_write_capability() {
    let payload =
        make_hostcall_payload("fs", "write", json!({"op": "mkdir", "path": "/tmp/newdir"}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("write"));
}

#[test]
fn hostcall_unknown_method_returns_none() {
    let payload = make_hostcall_payload("nonsense", "nonsense", json!({}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert!(cap.is_none(), "unknown method should return None");
}

#[test]
fn hostcall_empty_method_returns_none() {
    let payload = make_hostcall_payload("", "", json!({}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert!(cap.is_none(), "empty method should return None");
}

#[test]
fn hostcall_log_maps_to_log_capability() {
    let payload = make_hostcall_payload("log", "log", json!({"level": "info", "msg": "test"}));
    let cap = pi::extensions::required_capability_for_host_call(&payload);
    assert_eq!(cap.as_deref(), Some("log"));
}

// ============================================================================
// Integration: Load JS extension that attempts denied exec
// ============================================================================

fn load_negative_extension() -> (ExtensionManager, JsExtensionRuntimeHandle) {
    let ext_path = negative_ext_path();
    assert!(ext_path.exists(), "negative test fixture must exist");

    let spec = JsExtensionLoadSpec::from_entry_path(&ext_path).expect("load spec for negative ext");

    let manager = ExtensionManager::new();
    let cwd = PathBuf::from("/tmp/negative-test");
    let _ = std::fs::create_dir_all(&cwd);
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));

    let env = HashMap::new();
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        env,
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start JS runtime for negative test")
        }
    });
    manager.set_js_runtime(runtime.clone());

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load negative extension");
        }
    });

    (manager, runtime)
}

fn execute_tool(
    runtime: &JsExtensionRuntimeHandle,
    tool_name: &str,
    input: Value,
) -> Result<Value, String> {
    let ctx = json!({
        "hasUI": false,
        "cwd": "/tmp/negative-test",
        "sessionEntries": [],
        "sessionBranch": [],
        "sessionLeafEntry": null,
        "modelRegistry": {},
    });

    common::run_async({
        let runtime = runtime.clone();
        let tool_name = tool_name.to_string();
        let tool_call_id = format!("tc-neg-{tool_name}");
        async move {
            runtime
                .execute_tool(tool_name, tool_call_id, input, ctx, 10_000)
                .await
                .map_err(|e| format!("{e}"))
        }
    })
}

fn dispatch_event(
    runtime: &JsExtensionRuntimeHandle,
    event_name: &str,
    payload: Value,
) -> Result<Value, String> {
    let ctx = json!({
        "hasUI": false,
        "cwd": "/tmp/negative-test",
        "sessionEntries": [],
        "sessionBranch": [],
        "sessionLeafEntry": null,
        "modelRegistry": {},
    });

    common::run_async({
        let runtime = runtime.clone();
        let event_name = event_name.to_string();
        async move {
            runtime
                .dispatch_event(event_name, payload, ctx, 10_000)
                .await
                .map_err(|e| format!("{e}"))
        }
    })
}

fn extract_text(result: &Value) -> String {
    result.get("content").and_then(Value::as_array).map_or_else(
        || {
            result
                .as_str()
                .map_or_else(|| result.to_string(), String::from)
        },
        |arr| {
            arr.iter()
                .filter_map(|b| {
                    if b.get("type").and_then(Value::as_str) == Some("text") {
                        b.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        },
    )
}

#[test]
fn exec_tool_denied_by_default_policy() {
    let (_manager, runtime) = load_negative_extension();

    let result = execute_tool(&runtime, "try-exec", json!({}));
    let text = match &result {
        Ok(val) => extract_text(val),
        Err(e) => e.clone(),
    };

    // The tool catches the error and returns it as EXEC_DENIED: <message>
    assert!(
        text.contains("EXEC_DENIED") || text.to_lowercase().contains("denied"),
        "exec should be denied by policy, got: {text}"
    );

    // Verify the error message format includes the capability name
    let lower = text.to_lowercase();
    assert!(
        lower.contains("exec") || lower.contains("denied"),
        "denial error should mention 'exec' or 'denied', got: {text}"
    );
}

#[test]
fn session_tool_allowed_by_default_policy() {
    let (_manager, runtime) = load_negative_extension();

    let result = execute_tool(&runtime, "try-session", json!({}));
    let text = match &result {
        Ok(val) => extract_text(val),
        Err(e) => e.clone(),
    };

    // Session is in default_caps, so the hostcall should NOT be denied by policy.
    // It may fail with "No session configured" (which proves it passed the policy check),
    // or succeed with SESSION_OK if a session happens to be configured.
    assert!(
        !text.contains("denied by policy"),
        "session capability should not be denied by policy, got: {text}"
    );
    // Verify we got through to the actual session operation (either success or session-level error)
    assert!(
        text.contains("SESSION_OK") || text.contains("SESSION_ERROR"),
        "session tool should have been dispatched (not blocked by policy), got: {text}"
    );
}

#[test]
fn exec_denied_in_event_handler() {
    let (_manager, runtime) = load_negative_extension();

    let result = dispatch_event(&runtime, "session:start", json!({}));
    match result {
        Ok(val) => {
            // The event handler catches the error and returns { blocked: true, error: ... }
            let blocked = val.get("blocked").and_then(Value::as_bool).unwrap_or(false);
            let error = val.get("error").and_then(Value::as_str).unwrap_or_default();
            assert!(
                blocked,
                "exec in event handler should be blocked, got: {val}"
            );
            assert!(
                error.to_lowercase().contains("denied") || error.to_lowercase().contains("exec"),
                "error should mention denial, got: {error}"
            );
        }
        Err(e) => {
            // If the event dispatch itself errors, the exec was likely denied
            let lower = e.to_lowercase();
            assert!(
                lower.contains("denied") || lower.contains("exec"),
                "event dispatch error should mention denial, got: {e}"
            );
        }
    }
}

// ============================================================================
// JSONL report generation (bd-4u9 artifacts)
// ============================================================================

#[derive(Debug, Serialize)]
struct NegativeTestResult {
    test_name: String,
    capability: String,
    mode: String,
    expected_decision: String,
    actual_decision: String,
    reason: String,
    status: String, // "pass" or "fail"
    duration_ms: u64,
}

#[test]
#[allow(clippy::too_many_lines)]
fn negative_conformance_report() {
    let report_dir = reports_dir();
    let _ = std::fs::create_dir_all(&report_dir);

    let mut results: Vec<NegativeTestResult> = Vec::new();
    let modes = [
        ("strict", ExtensionPolicyMode::Strict),
        ("prompt", ExtensionPolicyMode::Prompt),
        ("permissive", ExtensionPolicyMode::Permissive),
    ];

    let denied_caps = ["exec", "env"];
    let allowed_default_caps = ["read", "write", "http", "events", "session"];
    let unknown_caps = ["custom", "gpu"];

    // Test denied capabilities across all modes
    for (mode_name, mode) in &modes {
        let policy = ExtensionPolicy {
            mode: *mode,
            ..Default::default()
        };

        for cap in &denied_caps {
            let start = Instant::now();
            let check = policy.evaluate(cap);
            let elapsed = start.elapsed().as_millis();
            let pass = check.decision == PolicyDecision::Deny;
            results.push(NegativeTestResult {
                test_name: format!("deny_caps_{cap}_{mode_name}"),
                capability: cap.to_string(),
                mode: mode_name.to_string(),
                expected_decision: "deny".to_string(),
                actual_decision: format!("{:?}", check.decision),
                reason: check.reason.clone(),
                status: if pass { "pass" } else { "fail" }.to_string(),
                duration_ms: u64::try_from(elapsed).unwrap_or(0),
            });
        }

        // Test allowed default capabilities
        for cap in &allowed_default_caps {
            let start = Instant::now();
            let check = policy.evaluate(cap);
            let elapsed = start.elapsed().as_millis();
            let pass = check.decision == PolicyDecision::Allow;
            results.push(NegativeTestResult {
                test_name: format!("default_caps_{cap}_{mode_name}"),
                capability: cap.to_string(),
                mode: mode_name.to_string(),
                expected_decision: "allow".to_string(),
                actual_decision: format!("{:?}", check.decision),
                reason: check.reason.clone(),
                status: if pass { "pass" } else { "fail" }.to_string(),
                duration_ms: u64::try_from(elapsed).unwrap_or(0),
            });
        }

        // Test unknown capabilities (mode-dependent behavior)
        for cap in &unknown_caps {
            let start = Instant::now();
            let check = policy.evaluate(cap);
            let elapsed = start.elapsed().as_millis();

            let expected = match mode {
                ExtensionPolicyMode::Strict => PolicyDecision::Deny,
                ExtensionPolicyMode::Prompt => PolicyDecision::Prompt,
                ExtensionPolicyMode::Permissive => PolicyDecision::Allow,
            };
            let pass = check.decision == expected;
            results.push(NegativeTestResult {
                test_name: format!("unknown_cap_{cap}_{mode_name}"),
                capability: cap.to_string(),
                mode: mode_name.to_string(),
                expected_decision: format!("{expected:?}").to_lowercase(),
                actual_decision: format!("{:?}", check.decision),
                reason: check.reason.clone(),
                status: if pass { "pass" } else { "fail" }.to_string(),
                duration_ms: u64::try_from(elapsed).unwrap_or(0),
            });
        }
    }

    // Test empty capability
    for (mode_name, mode) in &modes {
        let policy = ExtensionPolicy {
            mode: *mode,
            ..Default::default()
        };
        let start = Instant::now();
        let check = policy.evaluate("");
        let elapsed = start.elapsed().as_millis();
        let pass = check.decision == PolicyDecision::Deny;
        results.push(NegativeTestResult {
            test_name: format!("empty_cap_{mode_name}"),
            capability: String::new(),
            mode: mode_name.to_string(),
            expected_decision: "deny".to_string(),
            actual_decision: format!("{:?}", check.decision),
            reason: check.reason.clone(),
            status: if pass { "pass" } else { "fail" }.to_string(),
            duration_ms: u64::try_from(elapsed).unwrap_or(0),
        });
    }

    // Write JSONL report
    let events_path = report_dir.join("negative_events.jsonl");
    let mut lines: Vec<String> = Vec::new();
    for r in &results {
        let entry = json!({
            "schema": "pi.ext.negative_conformance.v1",
            "test_name": r.test_name,
            "capability": r.capability,
            "mode": r.mode,
            "expected_decision": r.expected_decision,
            "actual_decision": r.actual_decision,
            "reason": r.reason,
            "status": r.status,
            "duration_ms": r.duration_ms,
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        lines.push(serde_json::to_string(&entry).unwrap_or_default());
    }
    let _ = std::fs::write(&events_path, lines.join("\n") + "\n");

    // Write triage summary
    let pass_count = results.iter().filter(|r| r.status == "pass").count();
    let fail_count = results.iter().filter(|r| r.status == "fail").count();
    let triage = json!({
        "schema": "pi.ext.negative_triage.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "counts": {
            "total": results.len(),
            "pass": pass_count,
            "fail": fail_count,
        },
        "pass_rate_pct": if results.is_empty() {
            100.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            { (pass_count as f64) / (results.len() as f64) * 100.0 }
        },
    });
    let triage_path = report_dir.join("triage.json");
    let _ = std::fs::write(
        &triage_path,
        serde_json::to_string_pretty(&triage).unwrap_or_default(),
    );

    // Summary output
    eprintln!("\n=== Negative Conformance Report ===");
    eprintln!("  Total: {}", results.len());
    eprintln!("  Pass:  {pass_count}");
    eprintln!("  Fail:  {fail_count}");
    eprintln!("  Report: {}", events_path.display());
    eprintln!("  Triage: {}\n", triage_path.display());

    // Verify all pass
    assert_eq!(
        fail_count, 0,
        "all negative conformance checks should pass, {fail_count} failed"
    );
}
