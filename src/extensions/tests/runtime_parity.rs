//! JavaScript hostcall, cross-runtime parity, and streaming protocol tests.

use super::*;

// ========================================================================
// bd-1uy.1.3: JS-origin hostcalls produce taxonomy-only error codes
// ========================================================================

#[test]
fn js_hostcall_log_defaults_correlation_and_succeeds() {
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();
    let manager = extension_manager_no_persisted_permissions();

    let host = JsRuntimeHost {
        tools: Arc::new(crate::tools::ToolRegistry::new(&[], &cwd, None)),
        manager_ref: Arc::downgrade(&manager.inner),
        manager_snapshot: Arc::clone(&manager.snapshot),
        manager_snapshot_version: Arc::clone(&manager.snapshot_version),
        http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
        policy: ExtensionPolicy {
            mode: ExtensionPolicyMode::Permissive,
            max_memory_mb: 256,
            default_caps: Vec::new(),
            deny_caps: Vec::new(),
            ..Default::default()
        },
        interceptor: None,
    };

    let request = crate::extensions_js::HostcallRequest {
        call_id: "call-log-ok".to_string(),
        kind: crate::extensions_js::HostcallKind::Log,
        payload: serde_json::json!({
            "level": "info",
            "event": "unit.log",
            "message": "hello from extension"
        }),
        trace_id: 0,
        extension_id: Some("ext.test".to_string()),
    };

    let outcome = run_async(async { super::dispatch_hostcall(&host, request).await });
    match outcome {
        HostcallOutcome::Success(value) => {
            assert_eq!(value["ok"], true);
            assert_eq!(value["schema"], LOG_SCHEMA_VERSION);
            assert_eq!(value["event"], "unit.log");
        }
        other => panic!(),
    }
}

#[test]
fn js_hostcall_log_missing_required_fields_is_invalid_request() {
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();
    let manager = extension_manager_no_persisted_permissions();

    let host = JsRuntimeHost {
        tools: Arc::new(crate::tools::ToolRegistry::new(&[], &cwd, None)),
        manager_ref: Arc::downgrade(&manager.inner),
        manager_snapshot: Arc::clone(&manager.snapshot),
        manager_snapshot_version: Arc::clone(&manager.snapshot_version),
        http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
        policy: ExtensionPolicy {
            mode: ExtensionPolicyMode::Permissive,
            max_memory_mb: 256,
            default_caps: Vec::new(),
            deny_caps: Vec::new(),
            ..Default::default()
        },
        interceptor: None,
    };

    let request = crate::extensions_js::HostcallRequest {
        call_id: "call-log-bad".to_string(),
        kind: crate::extensions_js::HostcallKind::Log,
        payload: serde_json::json!({
            "level": "info",
            "message": "missing event"
        }),
        trace_id: 0,
        extension_id: Some("ext.test".to_string()),
    };

    let outcome = run_async(async { super::dispatch_hostcall(&host, request).await });
    match outcome {
        HostcallOutcome::Error { code, message } => {
            assert_eq!(code, "invalid_request");
            assert!(
                message.contains("validation failed") || message.contains("payload is invalid"),
                "unexpected error message: {message}"
            );
        }
        other => panic!(),
    }
}

/// Unknown tool → `invalid_request` (not `tool_error`).
#[test]
fn js_hostcall_unknown_tool_returns_invalid_request() {
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();
    let manager = extension_manager_no_persisted_permissions();

    let host = JsRuntimeHost {
        tools: Arc::new(crate::tools::ToolRegistry::new(&["read"], &cwd, None)),
        manager_ref: Arc::downgrade(&manager.inner),
        manager_snapshot: Arc::clone(&manager.snapshot),
        manager_snapshot_version: Arc::clone(&manager.snapshot_version),
        http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
        policy: ExtensionPolicy {
            mode: ExtensionPolicyMode::Permissive,
            max_memory_mb: 256,
            default_caps: Vec::new(),
            deny_caps: Vec::new(),
            ..Default::default()
        },
        interceptor: None,
    };

    let request = crate::extensions_js::HostcallRequest {
        call_id: "call-unknown-tool".to_string(),
        kind: crate::extensions_js::HostcallKind::Tool {
            name: "nonexistent_tool_xyz".to_string(),
        },
        payload: serde_json::json!({}),
        trace_id: 0,
        extension_id: Some("ext.test".to_string()),
    };

    let outcome = run_async(async { super::dispatch_hostcall(&host, request).await });
    match &outcome {
        HostcallOutcome::Error { code, message } => {
            assert_eq!(
                code, "invalid_request",
                "expected taxonomy code, got: {code}"
            );
            assert!(
                message.contains("nonexistent_tool_xyz"),
                "error should mention tool name: {message}"
            );
        }
        other => panic!(),
    }
}

/// Tool execution failure → `io` (not `tool_error`).
#[test]
fn js_hostcall_tool_execution_failure_maps_to_taxonomy() {
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();
    let manager = extension_manager_no_persisted_permissions();

    let host = JsRuntimeHost {
        tools: Arc::new(crate::tools::ToolRegistry::new(&["read"], &cwd, None)),
        manager_ref: Arc::downgrade(&manager.inner),
        manager_snapshot: Arc::clone(&manager.snapshot),
        manager_snapshot_version: Arc::clone(&manager.snapshot_version),
        http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
        policy: ExtensionPolicy {
            mode: ExtensionPolicyMode::Permissive,
            max_memory_mb: 256,
            default_caps: Vec::new(),
            deny_caps: Vec::new(),
            ..Default::default()
        },
        interceptor: None,
    };

    // Read a nonexistent file to trigger a tool execution error.
    let request = crate::extensions_js::HostcallRequest {
        call_id: "call-tool-fail".to_string(),
        kind: crate::extensions_js::HostcallKind::Tool {
            name: "read".to_string(),
        },
        payload: serde_json::json!({
            "path": "/nonexistent/path/that/does/not/exist.txt"
        }),
        trace_id: 0,
        extension_id: Some("ext.test".to_string()),
    };

    let outcome = run_async(async { super::dispatch_hostcall(&host, request).await });
    match &outcome {
        HostcallOutcome::Error { code, .. } => {
            // Must be a taxonomy code, never "tool_error".
            assert!(
                ["timeout", "denied", "io", "invalid_request", "internal"].contains(&code.as_str()),
                "expected taxonomy error code, got non-taxonomy code: {code}"
            );
            assert_ne!(code, "tool_error", "must not emit legacy tool_error code");
        }
        // Tool may succeed with an error message in output (depends on implementation).
        HostcallOutcome::Success(_) => {}
        HostcallOutcome::StreamChunk { .. } => {
            panic!();
        }
    }
}

/// Manager shutdown → `denied` (not `SHUTDOWN`).
#[test]
fn js_hostcall_manager_shutdown_maps_to_denied() {
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();

    // Create a manager then drop the inner Arc so manager() returns None.
    let tools = Arc::new(crate::tools::ToolRegistry::new(&[], &cwd, None));
    let http = Arc::new(crate::connectors::http::HttpConnector::with_defaults());

    // Create a manager we intentionally don't hold, so the Weak ref is dead.
    let (dead_manager_ref, dead_snapshot, dead_version) = {
        let manager = extension_manager_no_persisted_permissions();
        (
            Arc::downgrade(&manager.inner),
            Arc::clone(&manager.snapshot),
            Arc::clone(&manager.snapshot_version),
        )
        // manager dropped here → Weak upgrades fail
    };

    let host = JsRuntimeHost {
        tools,
        manager_ref: dead_manager_ref,
        manager_snapshot: dead_snapshot,
        manager_snapshot_version: dead_version,
        http,
        policy: ExtensionPolicy {
            mode: ExtensionPolicyMode::Permissive,
            max_memory_mb: 256,
            default_caps: Vec::new(),
            deny_caps: Vec::new(),
            ..Default::default()
        },
        interceptor: None,
    };

    // Session call with dead manager should yield "denied", not "SHUTDOWN".
    let request = crate::extensions_js::HostcallRequest {
        call_id: "call-shutdown".to_string(),
        kind: crate::extensions_js::HostcallKind::Session {
            op: "get_state".to_string(),
        },
        payload: serde_json::json!({}),
        trace_id: 0,
        extension_id: Some("ext.test".to_string()),
    };

    let outcome = run_async(async { super::dispatch_hostcall(&host, request).await });
    match &outcome {
        HostcallOutcome::Error { code, .. } => {
            assert_eq!(
                code, "denied",
                "shutdown path must map to 'denied', got: {code}"
            );
            assert_ne!(code, "SHUTDOWN", "must not emit legacy SHUTDOWN code");
        }
        other => panic!(),
    }
}

/// Verify that all error codes emitted by the shared dispatcher are taxonomy-only.
#[test]
fn js_hostcall_all_error_codes_are_taxonomy_only() {
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();
    let manager = extension_manager_no_persisted_permissions();

    let host = JsRuntimeHost {
        tools: Arc::new(crate::tools::ToolRegistry::new(&["read"], &cwd, None)),
        manager_ref: Arc::downgrade(&manager.inner),
        manager_snapshot: Arc::clone(&manager.snapshot),
        manager_snapshot_version: Arc::clone(&manager.snapshot_version),
        http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
        policy: ExtensionPolicy {
            mode: ExtensionPolicyMode::Strict,
            max_memory_mb: 256,
            default_caps: vec!["read".to_string()],
            deny_caps: vec!["exec".to_string()],
            ..Default::default()
        },
        interceptor: None,
    };

    let taxonomy_codes = ["timeout", "denied", "io", "invalid_request", "internal"];
    let legacy_codes = ["tool_error", "SHUTDOWN", "CANCELLED", "cancelled"];

    // Denied-by-policy (exec denied).
    let denied_req = crate::extensions_js::HostcallRequest {
        call_id: "call-denied".to_string(),
        kind: crate::extensions_js::HostcallKind::Exec {
            cmd: "ls".to_string(),
        },
        payload: serde_json::json!({}),
        trace_id: 0,
        extension_id: Some("ext.test".to_string()),
    };

    let outcome = run_async(async { super::dispatch_hostcall(&host, denied_req).await });
    if let HostcallOutcome::Error { code, .. } = &outcome {
        assert!(
            taxonomy_codes.contains(&code.as_str()),
            "denied-by-policy produced non-taxonomy code: {code}"
        );
        for legacy in &legacy_codes {
            assert_ne!(code, *legacy, "emitted legacy code: {code}");
        }
    }

    // Unknown tool.
    let unknown_req = crate::extensions_js::HostcallRequest {
        call_id: "call-unknown".to_string(),
        kind: crate::extensions_js::HostcallKind::Tool {
            name: "no_such_tool".to_string(),
        },
        payload: serde_json::json!({}),
        trace_id: 0,
        extension_id: Some("ext.test".to_string()),
    };

    let outcome = run_async(async { super::dispatch_hostcall(&host, unknown_req).await });
    if let HostcallOutcome::Error { code, .. } = &outcome {
        assert!(
            taxonomy_codes.contains(&code.as_str()),
            "unknown-tool produced non-taxonomy code: {code}"
        );
        for legacy in &legacy_codes {
            assert_ne!(code, *legacy, "emitted legacy code: {code}");
        }
    }
}

// ========================================================================
// Cross-Runtime Parity Tests (bd-1uy.1.4)
// ========================================================================
//
// These tests exercise the same canonical `HostCallPayload` through both
// the shared dispatcher and the protocol adapter, then assert:
// 1. Outputs match (same `is_error`, same error code, same output shape)
// 2. Schema validity (`validate_host_result` passes)
// 3. Taxonomy-only error codes
// 4. Params hash parity between JS-origin and canonical payloads

const TAXONOMY_CODES: [HostCallErrorCode; 5] = [
    HostCallErrorCode::Timeout,
    HostCallErrorCode::Denied,
    HostCallErrorCode::Io,
    HostCallErrorCode::InvalidRequest,
    HostCallErrorCode::Internal,
];

/// A canonical test case for parity verification.
struct ParityCase {
    name: &'static str,
    call: HostCallPayload,
    /// JS-origin request that should produce the same canonical payload.
    js_request: Option<HostcallRequest>,
    /// True if this case specifically tests manager-absent behaviour.
    /// JS dispatch always has a manager via `JsRuntimeHost`, so these
    /// cases are skipped in JS-vs-protocol parity (tested separately).
    needs_no_manager: bool,
}

/// Assert structural parity between two `HostResultPayload` values.
fn assert_result_parity(label: &str, shared: &HostResultPayload, protocol: &HostResultPayload) {
    assert_eq!(
        shared.is_error, protocol.is_error,
        "[{label}] is_error mismatch: shared={}, protocol={}",
        shared.is_error, protocol.is_error
    );
    assert_eq!(
        shared.call_id, protocol.call_id,
        "[{label}] call_id mismatch"
    );
    match (&shared.error, &protocol.error) {
        (Some(se), Some(pe)) => {
            assert_eq!(
                se.code, pe.code,
                "[{label}] error code mismatch: shared={:?}, protocol={:?}",
                se.code, pe.code
            );
        }
        (None, None) => {}
        _ => panic!(),
    }
}

/// Validate a `HostResultPayload` against schema invariants.
fn assert_schema_valid(label: &str, result: &HostResultPayload) {
    assert!(
        result.output.is_object(),
        "[{label}] output must be object, got: {:?}",
        result.output
    );
    if result.is_error {
        assert!(
            result.error.is_some(),
            "[{label}] is_error=true but error is None"
        );
    } else {
        assert!(
            result.error.is_none(),
            "[{label}] is_error=false but error is Some: {:?}",
            result.error
        );
    }
    if let Some(ref err) = result.error {
        assert!(
            TAXONOMY_CODES.contains(&err.code),
            "[{label}] non-taxonomy error code: {:?}",
            err.code
        );
    }
    super::validate_host_result(result).unwrap_or_else(|e| panic!());
}

/// Extract `HostResultPayload` from a protocol adapter response.
fn extract_protocol_result(responses: &[ExtensionMessage]) -> &HostResultPayload {
    assert_eq!(responses.len(), 1, "expected exactly 1 response");
    match &responses[0].body {
        ExtensionBody::HostResult(result) => result,
        other => panic!(),
    }
}

/// Build canonical test cases for parity verification.
#[allow(clippy::too_many_lines)]
fn parity_cases(cwd: &std::path::Path) -> Vec<ParityCase> {
    vec![
        ParityCase {
            name: "tool_unknown",
            call: HostCallPayload {
                call_id: "parity-tool-unknown".to_string(),
                capability: "tool".to_string(),
                method: "tool".to_string(),
                params: json!({ "name": "nonexistent_tool_xyz", "input": {} }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
            js_request: Some(HostcallRequest {
                call_id: "parity-tool-unknown".to_string(),
                kind: HostcallKind::Tool {
                    name: "nonexistent_tool_xyz".to_string(),
                },
                payload: json!({}),
                trace_id: 0,
                extension_id: Some("ext.parity".to_string()),
            }),
            needs_no_manager: false,
        },
        ParityCase {
            name: "tool_read_success",
            call: HostCallPayload {
                call_id: "parity-tool-read".to_string(),
                capability: "read".to_string(),
                method: "tool".to_string(),
                params: json!({
                    "name": "read",
                    "input": { "path": cwd.join("parity_test.txt").to_str().unwrap() }
                }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
            js_request: Some(HostcallRequest {
                call_id: "parity-tool-read".to_string(),
                kind: HostcallKind::Tool {
                    name: "read".to_string(),
                },
                payload: json!({
                    "path": cwd.join("parity_test.txt").to_str().unwrap()
                }),
                trace_id: 0,
                extension_id: Some("ext.parity".to_string()),
            }),
            needs_no_manager: false,
        },
        ParityCase {
            name: "exec_empty_cmd",
            call: HostCallPayload {
                call_id: "parity-exec-empty".to_string(),
                capability: "exec".to_string(),
                method: "exec".to_string(),
                params: json!({ "cmd": "" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
            js_request: Some(HostcallRequest {
                call_id: "parity-exec-empty".to_string(),
                kind: HostcallKind::Exec { cmd: String::new() },
                payload: json!({}),
                trace_id: 0,
                extension_id: Some("ext.parity".to_string()),
            }),
            needs_no_manager: false,
        },
        ParityCase {
            name: "session_missing_op",
            call: HostCallPayload {
                call_id: "parity-session-noop".to_string(),
                capability: "session".to_string(),
                method: "session".to_string(),
                params: json!({ "key": "value" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
            js_request: None,
            needs_no_manager: false,
        },
        ParityCase {
            name: "session_no_manager",
            call: HostCallPayload {
                call_id: "parity-session-mgr".to_string(),
                capability: "session".to_string(),
                method: "session".to_string(),
                params: json!({ "op": "get_state" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
            js_request: Some(HostcallRequest {
                call_id: "parity-session-mgr".to_string(),
                kind: HostcallKind::Session {
                    op: "get_state".to_string(),
                },
                payload: json!({}),
                trace_id: 0,
                extension_id: Some("ext.parity".to_string()),
            }),
            needs_no_manager: true,
        },
        ParityCase {
            name: "ui_no_manager",
            call: HostCallPayload {
                call_id: "parity-ui-mgr".to_string(),
                capability: "ui".to_string(),
                method: "ui".to_string(),
                params: json!({ "op": "confirm", "message": "test?" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
            js_request: Some(HostcallRequest {
                call_id: "parity-ui-mgr".to_string(),
                kind: HostcallKind::Ui {
                    op: "confirm".to_string(),
                },
                payload: json!({ "message": "test?" }),
                trace_id: 0,
                extension_id: Some("ext.parity".to_string()),
            }),
            needs_no_manager: true,
        },
        ParityCase {
            name: "ui_empty_op",
            call: HostCallPayload {
                call_id: "parity-ui-noop".to_string(),
                capability: "ui".to_string(),
                method: "ui".to_string(),
                params: json!({ "data": 1 }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
            js_request: None,
            needs_no_manager: false,
        },
        ParityCase {
            name: "events_no_manager",
            call: HostCallPayload {
                call_id: "parity-events-mgr".to_string(),
                capability: "events".to_string(),
                method: "events".to_string(),
                params: json!({ "op": "emit", "event": "test" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
            js_request: Some(HostcallRequest {
                call_id: "parity-events-mgr".to_string(),
                kind: HostcallKind::Events {
                    op: "emit".to_string(),
                },
                payload: json!({ "event": "test" }),
                trace_id: 0,
                extension_id: Some("ext.parity".to_string()),
            }),
            needs_no_manager: true,
        },
        ParityCase {
            name: "capability_mismatch",
            call: HostCallPayload {
                call_id: "parity-cap-mismatch".to_string(),
                capability: "exec".to_string(),
                method: "tool".to_string(),
                params: json!({ "name": "read", "input": {} }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
            js_request: None,
            needs_no_manager: false,
        },
        ParityCase {
            name: "empty_call_id",
            call: HostCallPayload {
                call_id: String::new(),
                capability: "tool".to_string(),
                method: "tool".to_string(),
                params: json!({ "name": "read", "input": {} }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
            js_request: None,
            needs_no_manager: false,
        },
        ParityCase {
            name: "unsupported_method",
            call: HostCallPayload {
                call_id: "parity-bad-method".to_string(),
                capability: "tool".to_string(),
                method: "quantum_compute".to_string(),
                params: json!({}),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
            js_request: None,
            needs_no_manager: false,
        },
    ]
}

#[test]
#[allow(clippy::too_many_lines)]
fn parity_shared_vs_protocol_all_cases() {
    let dir = tempdir().expect("tempdir");
    let cwd = dir.path();
    std::fs::write(cwd.join("parity_test.txt"), "parity_data").expect("write test file");

    let tools = ToolRegistry::new(&["read"], cwd, None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let ctx = test_host_call_context(&tools, &http, &policy);

    let cases = parity_cases(cwd);

    for case in &cases {
        run_async(async {
            let shared_result = dispatch_host_call_shared(&ctx, case.call.clone()).await;

            let msg = make_host_call_msg(
                &case.call.call_id,
                &case.call.method,
                &case.call.capability,
                case.call.params.clone(),
            );
            let responses = handle_extension_message(&ctx, msg).await;
            let protocol_result = extract_protocol_result(&responses);

            assert_result_parity(case.name, &shared_result, protocol_result);

            if !case.call.call_id.is_empty() {
                assert_schema_valid(&format!("{}/shared", case.name), &shared_result);
                assert_schema_valid(&format!("{}/protocol", case.name), protocol_result);
            }
        });
    }
}

#[test]
fn parity_params_hash_all_js_cases() {
    let dir = tempdir().expect("tempdir");
    let cwd = dir.path();
    std::fs::write(cwd.join("parity_test.txt"), "parity_data").expect("write test file");

    let cases = parity_cases(cwd);

    for case in &cases {
        let Some(ref js_req) = case.js_request else {
            continue;
        };
        let converted = hostcall_request_to_payload(js_req);
        let js_hash = js_req.params_hash();
        let canonical_hash = hostcall_params_hash(&converted.method, &converted.params);

        assert_eq!(
            js_hash, canonical_hash,
            "[{}] params_hash mismatch: JS={}, canonical={}",
            case.name, js_hash, canonical_hash
        );

        assert_eq!(
            converted.method, case.call.method,
            "[{}] method mismatch after JS conversion",
            case.name
        );
    }
}

#[test]
fn parity_js_conversion_vs_protocol() {
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let cwd = dir.path();
    std::fs::write(cwd.join("parity_test.txt"), "parity_data").expect("write test file");

    let tools = ToolRegistry::new(&["read"], cwd, None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let ctx = test_host_call_context(&tools, &http, &policy);

    let manager = extension_manager_no_persisted_permissions();
    let host = JsRuntimeHost {
        tools: Arc::new(ToolRegistry::new(&["read"], cwd, None)),
        manager_ref: Arc::downgrade(&manager.inner),
        manager_snapshot: Arc::clone(&manager.snapshot),
        manager_snapshot_version: Arc::clone(&manager.snapshot_version),
        http: Arc::new(HttpConnector::with_defaults()),
        policy: permissive_policy(),
        interceptor: None,
    };

    let cases = parity_cases(cwd);

    for case in &cases {
        let Some(ref js_req) = case.js_request else {
            continue;
        };
        // JS dispatch always has a manager via JsRuntimeHost; skip cases
        // that specifically test manager-absent behaviour (tested separately
        // in `parity_shared_vs_protocol_all_cases`).
        if case.needs_no_manager {
            continue;
        }

        run_async(async {
            let js_outcome = super::dispatch_hostcall(&host, js_req.clone()).await;

            let msg = make_host_call_msg(
                &case.call.call_id,
                &case.call.method,
                &case.call.capability,
                case.call.params.clone(),
            );
            let responses = handle_extension_message(&ctx, msg).await;
            let protocol_result = extract_protocol_result(&responses);

            let js_result = outcome_to_host_result(&case.call.call_id, &js_outcome);

            assert_result_parity(
                &format!("{}/js_vs_protocol", case.name),
                &js_result,
                protocol_result,
            );

            if !case.call.call_id.is_empty() {
                assert_schema_valid(&format!("{}/js_result", case.name), &js_result);
            }
        });
    }
}

#[test]
fn parity_all_errors_are_taxonomy_only() {
    let dir = tempdir().expect("tempdir");
    let cwd = dir.path();

    let tools = ToolRegistry::new(&["read"], cwd, None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let ctx = test_host_call_context(&tools, &http, &policy);

    let cases = parity_cases(cwd);

    for case in &cases {
        run_async(async {
            let result = dispatch_host_call_shared(&ctx, case.call.clone()).await;
            if let Some(ref err) = result.error {
                assert!(
                    TAXONOMY_CODES.contains(&err.code),
                    "[{}] non-taxonomy error code: {:?} (message: {})",
                    case.name,
                    err.code,
                    err.message
                );
            }
        });
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn parity_denied_by_policy_shared_vs_protocol() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = deny_all_policy();
    let ctx = test_host_call_context(&tools, &http, &policy);

    let denied_cases = vec![
        // name=read → required capability "read", not "tool"
        (
            "tool_denied",
            "tool",
            "read",
            json!({ "name": "read", "input": {} }),
        ),
        ("exec_denied", "exec", "exec", json!({ "cmd": "ls" })),
        (
            "http_denied",
            "http",
            "http",
            json!({ "url": "https://example.com" }),
        ),
        (
            "session_denied",
            "session",
            "session",
            json!({ "op": "get_state" }),
        ),
        (
            "ui_denied",
            "ui",
            "ui",
            json!({ "op": "confirm", "message": "test" }),
        ),
        (
            "events_denied",
            "events",
            "events",
            json!({ "op": "emit", "event": "test" }),
        ),
    ];

    for (name, method, capability, params) in &denied_cases {
        let call = HostCallPayload {
            call_id: format!("parity-deny-{name}"),
            capability: capability.to_string(),
            method: method.to_string(),
            params: params.clone(),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        run_async(async {
            let shared_result = dispatch_host_call_shared(&ctx, call.clone()).await;
            let msg = make_host_call_msg(
                &call.call_id,
                &call.method,
                &call.capability,
                call.params.clone(),
            );
            let responses = handle_extension_message(&ctx, msg).await;
            let protocol_result = extract_protocol_result(&responses);

            assert!(
                shared_result.is_error,
                "[{name}] shared: expected error for denied call"
            );
            assert!(
                protocol_result.is_error,
                "[{name}] protocol: expected error for denied call"
            );

            let shared_code = shared_result.error.as_ref().expect("shared error").code;
            let protocol_code = protocol_result.error.as_ref().expect("protocol error").code;
            assert_eq!(
                shared_code,
                HostCallErrorCode::Denied,
                "[{name}] shared: expected Denied, got {shared_code:?}"
            );
            assert_eq!(
                protocol_code,
                HostCallErrorCode::Denied,
                "[{name}] protocol: expected Denied, got {protocol_code:?}"
            );

            assert_result_parity(name, &shared_result, protocol_result);
            assert_schema_valid(&format!("{name}/shared"), &shared_result);
            assert_schema_valid(&format!("{name}/protocol"), protocol_result);
        });
    }
}

#[test]
fn parity_tool_read_success_shared_vs_protocol() {
    let dir = tempdir().expect("tempdir");
    let cwd = dir.path();
    std::fs::write(cwd.join("hello_parity.txt"), "parity_content_42").expect("write test file");

    let tools = ToolRegistry::new(&["read"], cwd, None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let ctx = HostCallContext {
        runtime_name: "parity_test",
        extension_id: Some("ext.parity"),
        tools: &tools,
        http: &http,
        manager: None,
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let call = HostCallPayload {
        call_id: "parity-read-ok".to_string(),
        capability: "read".to_string(),
        method: "tool".to_string(),
        params: json!({
            "name": "read",
            "input": { "path": cwd.join("hello_parity.txt").to_str().unwrap() }
        }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let shared_result = dispatch_host_call_shared(&ctx, call.clone()).await;
        let msg = make_host_call_msg(
            &call.call_id,
            &call.method,
            &call.capability,
            call.params.clone(),
        );
        let responses = handle_extension_message(&ctx, msg).await;
        let protocol_result = extract_protocol_result(&responses);

        assert!(
            !shared_result.is_error,
            "shared: expected success, got: {:?}",
            shared_result.error
        );
        assert!(
            !protocol_result.is_error,
            "protocol: expected success, got: {:?}",
            protocol_result.error
        );

        assert_result_parity("read_success", &shared_result, protocol_result);
        assert_schema_valid("read_success/shared", &shared_result);
        assert_schema_valid("read_success/protocol", protocol_result);

        let shared_str = serde_json::to_string(&shared_result.output).unwrap();
        let protocol_str = serde_json::to_string(&protocol_result.output).unwrap();
        assert!(
            shared_str.contains("parity_content_42"),
            "shared output missing file content: {shared_str}"
        );
        assert!(
            protocol_str.contains("parity_content_42"),
            "protocol output missing file content: {protocol_str}"
        );
    });
}

#[test]
fn parity_outcome_roundtrip_error_preserves_taxonomy() {
    for code in &TAXONOMY_CODES {
        let code_str = host_call_error_code_str(*code);
        let outcome = HostcallOutcome::Error {
            code: code_str.to_string(),
            message: format!("test {code_str}"),
        };

        let result = outcome_to_host_result("rt-test", &outcome);
        assert_schema_valid(&format!("roundtrip/{code_str}"), &result);

        let back = host_result_to_outcome(result);
        match back {
            HostcallOutcome::Error {
                code: back_code,
                message: back_msg,
            } => {
                assert_eq!(
                    back_code, code_str,
                    "roundtrip code mismatch: {back_code} != {code_str}"
                );
                assert!(
                    back_msg.contains(code_str),
                    "roundtrip message lost: {back_msg}"
                );
            }
            other => panic!(),
        }
    }
}

#[test]
fn parity_outcome_roundtrip_success_preserves_output() {
    let output = json!({"key": "value", "count": 42});
    let outcome = HostcallOutcome::Success(output.clone());

    let result = outcome_to_host_result("rt-ok", &outcome);
    assert_schema_valid("roundtrip/success", &result);
    assert_eq!(result.output, output);

    let back = host_result_to_outcome(result);
    match back {
        HostcallOutcome::Success(v) => assert_eq!(v, output),
        other => panic!(),
    }
}

#[test]
fn parity_outcome_roundtrip_stream_chunk() {
    let chunk = json!({"data": "partial"});
    let outcome = HostcallOutcome::StreamChunk {
        sequence: 7,
        chunk: chunk.clone(),
        is_final: false,
    };

    let result = outcome_to_host_result("rt-stream", &outcome);
    assert!(!result.is_error);
    assert!(result.error.is_none());
    assert_eq!(result.output, chunk);
    let stream_info = result.chunk.as_ref().expect("chunk info");
    assert_eq!(stream_info.index, 7);
    assert!(!stream_info.is_last);

    let back = host_result_to_outcome(result);
    match back {
        HostcallOutcome::StreamChunk {
            sequence,
            chunk: c,
            is_final,
        } => {
            assert_eq!(sequence, 7);
            assert_eq!(c, chunk);
            assert!(!is_final);
        }
        other => panic!(),
    }
}

#[test]
fn parity_empty_call_id_rejected_both_paths() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let ctx = test_host_call_context(&tools, &http, &policy);

    let call = HostCallPayload {
        call_id: String::new(),
        capability: "tool".to_string(),
        method: "tool".to_string(),
        params: json!({ "name": "read", "input": {} }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let shared = dispatch_host_call_shared(&ctx, call.clone()).await;
        assert!(shared.is_error, "shared must reject empty call_id");
        let shared_err = shared.error.as_ref().expect("shared error");
        assert_eq!(shared_err.code, HostCallErrorCode::InvalidRequest);

        let msg = make_host_call_msg("", "tool", "tool", json!({ "name": "read", "input": {} }));
        let responses = handle_extension_message(&ctx, msg).await;
        let protocol = extract_protocol_result(&responses);
        assert!(protocol.is_error, "protocol must reject empty call_id");
        let protocol_err = protocol.error.as_ref().expect("protocol error");
        assert_eq!(protocol_err.code, HostCallErrorCode::InvalidRequest);
    });
}

#[test]
fn parity_non_object_params_rejected() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let ctx = test_host_call_context(&tools, &http, &policy);

    let call = HostCallPayload {
        call_id: "parity-badparams".to_string(),
        capability: "tool".to_string(),
        method: "tool".to_string(),
        params: json!("not an object"),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let shared = dispatch_host_call_shared(&ctx, call.clone()).await;
        assert!(shared.is_error, "shared must reject non-object params");
        let shared_err = shared.error.as_ref().expect("shared error");
        assert_eq!(shared_err.code, HostCallErrorCode::InvalidRequest);

        let msg = ExtensionMessage {
            id: "msg-badparams".to_string(),
            version: PROTOCOL_VERSION.to_string(),
            body: ExtensionBody::HostCall(call),
        };
        let responses = handle_extension_message(&ctx, msg).await;
        let protocol = extract_protocol_result(&responses);
        assert!(protocol.is_error, "protocol must reject non-object params");
        let protocol_err = protocol.error.as_ref().expect("protocol error");
        assert_eq!(protocol_err.code, HostCallErrorCode::InvalidRequest);
    });
}

// ========================================================================
// bd-2tl1.5: Streaming Hostcall Protocol Invariants
// ========================================================================

#[test]
fn stream_chunk_serde_roundtrip() {
    let chunk = HostStreamChunk {
        index: 42,
        is_last: false,
        backpressure: None,
    };
    let json = serde_json::to_string(&chunk).unwrap();
    let back: HostStreamChunk = serde_json::from_str(&json).unwrap();
    assert_eq!(back.index, 42);
    assert!(!back.is_last);
    assert!(back.backpressure.is_none());
}

#[test]
fn stream_chunk_serde_with_backpressure() {
    let chunk = HostStreamChunk {
        index: 0,
        is_last: true,
        backpressure: Some(HostStreamBackpressure {
            credits: Some(10),
            delay_ms: Some(500),
        }),
    };
    let json = serde_json::to_value(&chunk).unwrap();
    assert_eq!(json["index"], 0);
    assert_eq!(json["is_last"], true);
    assert_eq!(json["backpressure"]["credits"], 10);
    assert_eq!(json["backpressure"]["delay_ms"], 500);

    let back: HostStreamChunk = serde_json::from_value(json).unwrap();
    assert!(back.is_last);
    let bp = back.backpressure.unwrap();
    assert_eq!(bp.credits, Some(10));
    assert_eq!(bp.delay_ms, Some(500));
}

#[test]
fn stream_chunk_serde_skips_none_backpressure() {
    let chunk = HostStreamChunk {
        index: 5,
        is_last: false,
        backpressure: None,
    };
    let json = serde_json::to_value(&chunk).unwrap();
    assert!(
        json.get("backpressure").is_none(),
        "None backpressure should be omitted from serialized JSON"
    );
}

#[test]
fn stream_backpressure_serde_roundtrip() {
    let bp = HostStreamBackpressure {
        credits: Some(100),
        delay_ms: None,
    };
    let json = serde_json::to_value(&bp).unwrap();
    assert_eq!(json["credits"], 100);
    assert!(
        json.get("delay_ms").is_none(),
        "None delay_ms should be omitted"
    );

    let back: HostStreamBackpressure = serde_json::from_value(json).unwrap();
    assert_eq!(back.credits, Some(100));
    assert!(back.delay_ms.is_none());
}

#[test]
fn stream_backpressure_both_none_serde() {
    let bp = HostStreamBackpressure {
        credits: None,
        delay_ms: None,
    };
    let json = serde_json::to_value(&bp).unwrap();
    assert_eq!(
        json,
        json!({}),
        "both-None backpressure should serialize to empty object"
    );

    let back: HostStreamBackpressure = serde_json::from_value(json).unwrap();
    assert!(back.credits.is_none());
    assert!(back.delay_ms.is_none());
}

#[test]
fn validate_host_result_accepts_stream_chunk_with_object_output() {
    let result = HostResultPayload {
        call_id: "stream-valid".to_string(),
        output: json!({"data": "chunk"}),
        is_error: false,
        error: None,
        chunk: Some(HostStreamChunk {
            index: 0,
            is_last: false,
            backpressure: None,
        }),
    };
    super::validate_host_result(&result)
        .expect("valid stream chunk with object output should pass validation");
}

#[test]
fn validate_host_result_rejects_stream_chunk_non_object_output() {
    // Stream chunks in practice may carry string output (e.g., "line 1\n"),
    // but `validate_host_result` enforces object output uniformly.
    let result = HostResultPayload {
        call_id: "stream-bad-output".to_string(),
        output: json!("string output"),
        is_error: false,
        error: None,
        chunk: Some(HostStreamChunk {
            index: 0,
            is_last: false,
            backpressure: None,
        }),
    };
    assert!(
        super::validate_host_result(&result).is_err(),
        "non-object output should be rejected even for stream chunks"
    );
}

#[test]
fn stream_final_chunk_roundtrip_preserves_is_last() {
    let outcome = HostcallOutcome::StreamChunk {
        sequence: 99,
        chunk: json!({"final": true}),
        is_final: true,
    };
    let result = outcome_to_host_result("final-test", &outcome);
    let chunk_info = result.chunk.as_ref().expect("chunk info");
    assert!(chunk_info.is_last);
    assert_eq!(chunk_info.index, 99);

    let back = host_result_to_outcome(result);
    match back {
        HostcallOutcome::StreamChunk {
            sequence, is_final, ..
        } => {
            assert_eq!(sequence, 99);
            assert!(is_final);
        }
        other => panic!(),
    }
}

#[test]
fn stream_outcome_roundtrip_backpressure_not_preserved() {
    // Backpressure is lost in the outcome roundtrip because
    // `HostcallOutcome::StreamChunk` does not carry backpressure.
    let result = HostResultPayload {
        call_id: "bp-test".to_string(),
        output: json!({"data": "x"}),
        is_error: false,
        error: None,
        chunk: Some(HostStreamChunk {
            index: 3,
            is_last: false,
            backpressure: Some(HostStreamBackpressure {
                credits: Some(5),
                delay_ms: Some(100),
            }),
        }),
    };

    let outcome = host_result_to_outcome(result);
    let back = outcome_to_host_result("bp-test", &outcome);

    // Backpressure is lost (`outcome_to_host_result` always sets None).
    assert!(
        back.chunk.as_ref().unwrap().backpressure.is_none(),
        "backpressure should not survive outcome roundtrip"
    );
    // But sequence and is_last are preserved.
    assert_eq!(back.chunk.as_ref().unwrap().index, 3);
    assert!(!back.chunk.as_ref().unwrap().is_last);
}

#[test]
fn stream_chunk_call_id_preserved_through_conversion() {
    let outcome = HostcallOutcome::StreamChunk {
        sequence: 0,
        chunk: json!({}),
        is_final: false,
    };
    let result = outcome_to_host_result("my-call-id-42", &outcome);
    assert_eq!(result.call_id, "my-call-id-42");
}

#[test]
fn stream_chunk_zero_index_roundtrip() {
    let chunk = HostStreamChunk {
        index: 0,
        is_last: false,
        backpressure: None,
    };
    let json = serde_json::to_value(&chunk).unwrap();
    assert_eq!(json["index"], 0);
    let back: HostStreamChunk = serde_json::from_value(json).unwrap();
    assert_eq!(back.index, 0);
}

#[test]
fn stream_chunk_max_index_roundtrip() {
    let chunk = HostStreamChunk {
        index: u64::MAX,
        is_last: true,
        backpressure: None,
    };
    let json = serde_json::to_value(&chunk).unwrap();
    let back: HostStreamChunk = serde_json::from_value(json).unwrap();
    assert_eq!(back.index, u64::MAX);
    assert!(back.is_last);
}
