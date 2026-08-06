//! UI routing, protocol adapter, and typed opcode tests.

use super::*;

// ========================================================================
// bd-2hz.4: UI method routing through shared dispatcher + taxonomy
// ========================================================================

/// UI confirm success path via shared dispatcher.
#[test]
fn shared_dispatch_ui_confirm_success() {
    use asupersync::channel::mpsc;

    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();

    let manager = extension_manager_no_persisted_permissions();
    let (ui_tx, mut ui_rx) = mpsc::channel(8);
    manager.set_ui_sender(ui_tx);

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.ui-test"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let call = HostCallPayload {
        call_id: "ui-confirm-1".to_string(),
        capability: "ui".to_string(),
        method: "ui".to_string(),
        params: json!({ "op": "confirm", "title": "Test?", "message": "Really?" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let cx = asupersync::Cx::for_request();

        let ui_handler = async {
            let req = ui_rx.recv(&cx).await.expect("ui recv");
            assert_eq!(req.method, "confirm");
            manager.respond_ui(ExtensionUiResponse {
                id: req.id,
                value: Some(Value::Bool(true)),
                cancelled: false,
            });
        };

        let dispatch = async { dispatch_host_call_shared(&ctx, call).await };

        let ((), result) = futures::join!(ui_handler, dispatch);
        assert!(
            !result.is_error,
            "expected success, got error: {:?}",
            result.error
        );
        // confirm returns the boolean value
        assert_eq!(result.output, json!(true));
    });
}

/// UI with no manager (shutdown) returns denied.
#[test]
fn shared_dispatch_ui_without_manager_returns_denied() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let ctx = test_host_call_context(&tools, &http, &policy);

    let call = HostCallPayload {
        call_id: "ui-no-mgr".to_string(),
        capability: "ui".to_string(),
        method: "ui".to_string(),
        params: json!({ "op": "confirm" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error);
        let err = result.error.expect("expected error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert!(
            err.message.contains("shutting down"),
            "expected shutdown message, got: {}",
            err.message
        );
    });
}

/// UI with no UI sender configured returns denied.
#[test]
fn shared_dispatch_ui_no_sender_returns_denied() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();

    // Manager exists but no UI sender configured.
    let manager = extension_manager_no_persisted_permissions();
    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.ui-test"),
        tools: &tools,
        http: &http,
        manager: Some(manager),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let call = HostCallPayload {
        call_id: "ui-no-sender".to_string(),
        capability: "ui".to_string(),
        method: "ui".to_string(),
        params: json!({ "op": "confirm", "title": "Test?" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error);
        let err = result.error.expect("expected error payload");
        // "not configured" maps to "denied" via classify_ui_hostcall_error
        assert_eq!(err.code, HostCallErrorCode::Denied);
    });
}

/// UI cancelled response maps to deterministic cancelled output.
#[test]
fn shared_dispatch_ui_cancelled_returns_deterministic_value() {
    use asupersync::channel::mpsc;

    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();

    let manager = extension_manager_no_persisted_permissions();
    let (ui_tx, mut ui_rx) = mpsc::channel(8);
    manager.set_ui_sender(ui_tx);

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.ui-test"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let call = HostCallPayload {
        call_id: "ui-cancel-1".to_string(),
        capability: "ui".to_string(),
        method: "ui".to_string(),
        params: json!({ "op": "confirm", "title": "Cancel me" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let cx = asupersync::Cx::for_request();

        let ui_handler = async {
            let req = ui_rx.recv(&cx).await.expect("ui recv");
            assert_eq!(req.method, "confirm");
            // Simulate user cancellation.
            manager.respond_ui(ExtensionUiResponse {
                id: req.id,
                value: None,
                cancelled: true,
            });
        };

        let dispatch = async { dispatch_host_call_shared(&ctx, call).await };

        let ((), result) = futures::join!(ui_handler, dispatch);
        // Cancelled confirm resolves with false (not an error).
        assert!(!result.is_error, "cancelled should not be an error");
        assert_eq!(
            result.output,
            json!(false),
            "cancelled confirm should resolve to false"
        );
    });
}

#[test]
fn ui_response_value_for_custom_cancelled_returns_closed_payload() {
    let response = ExtensionUiResponse {
        id: "req-custom-cancel".to_string(),
        value: None,
        cancelled: true,
    };

    assert_eq!(
        ui_response_value_for_op("custom", &response),
        json!({ "closed": true })
    );
}

/// UI with invalid (empty) op returns invalid_request.
#[test]
fn shared_dispatch_ui_empty_op_returns_invalid_request() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();

    let manager = extension_manager_no_persisted_permissions();
    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.ui-test"),
        tools: &tools,
        http: &http,
        manager: Some(manager),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let call = HostCallPayload {
        call_id: "ui-empty-op".to_string(),
        capability: "ui".to_string(),
        method: "ui".to_string(),
        params: json!({ "op": "" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error);
        let err = result.error.expect("expected error payload");
        assert_eq!(err.code, HostCallErrorCode::InvalidRequest);
    });
}

/// UI shared dispatch emits structured logs with params_hash and no raw payload.
#[test]
fn shared_dispatch_ui_logs_params_hash_no_raw_payload() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();

    let manager = extension_manager_no_persisted_permissions();
    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.ui-log"),
        tools: &tools,
        http: &http,
        manager: Some(manager),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let call = HostCallPayload {
        call_id: "ui-log-1".to_string(),
        capability: "ui".to_string(),
        method: "ui".to_string(),
        params: json!({
            "op": "confirm",
            "title": "Secret Title",
            "message": "Secret Body"
        }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let (_result, events) =
        capture_tracing_events(|| run_async(async { dispatch_host_call_shared(&ctx, call).await }));

    // Should have host_call.start with params_hash.
    let start = events.iter().find(|e| {
        e.fields
            .get("event")
            .is_some_and(|v| v.contains("host_call.start"))
    });
    let start = start.expect("host_call.start event for ui call");
    assert!(
        start.fields.contains_key("params_hash"),
        "start event must include params_hash"
    );

    // Should have host_call.end with duration_ms.
    let end = events.iter().find(|e| {
        e.fields
            .get("event")
            .is_some_and(|v| v.contains("host_call.end"))
    });
    let end = end.expect("host_call.end event for ui call");
    assert!(
        end.fields.contains_key("duration_ms"),
        "end event must include duration_ms"
    );

    // No raw payload fields should appear in any log event.
    for event in &events {
        for value in event.fields.values() {
            assert!(
                !value.contains("Secret Title"),
                "raw payload leaked into logs: {value}"
            );
            assert!(
                !value.contains("Secret Body"),
                "raw payload leaked into logs: {value}"
            );
        }
    }
}

// ========================================================================
// bd-1uy.1.2: Protocol adapter (handle_extension_message) tests
// ========================================================================

pub(super) fn make_host_call_msg(
    call_id: &str,
    method: &str,
    capability: &str,
    params: Value,
) -> ExtensionMessage {
    ExtensionMessage {
        id: format!("msg-{call_id}"),
        version: PROTOCOL_VERSION.to_string(),
        body: ExtensionBody::HostCall(HostCallPayload {
            call_id: call_id.to_string(),
            capability: capability.to_string(),
            method: method.to_string(),
            params,
            timeout_ms: None,
            cancel_token: None,
            context: None,
        }),
    }
}

/// Round-trip: host_call -> adapter -> host_result validates.
#[test]
fn protocol_adapter_host_call_roundtrip_validates() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&["read"], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let ctx = test_host_call_context(&tools, &http, &policy);

    let msg = make_host_call_msg(
        "call-roundtrip",
        "tool",
        "tool",
        json!({ "name": "nonexistent_tool", "input": {} }),
    );

    let responses = run_async(async { handle_extension_message(&ctx, msg).await });
    assert_eq!(responses.len(), 1);

    let response = &responses[0];
    // Response id should follow the deterministic format.
    assert_eq!(response.id, "host_result:call-roundtrip");
    assert_eq!(response.version, PROTOCOL_VERSION);

    // Validate the response message.
    response.validate().expect("response must be schema-valid");

    // The body should be HostResult.
    let result = match &response.body {
        ExtensionBody::HostResult(result) => result,
        other => panic!(),
    };

    // call_id must be preserved.
    assert_eq!(result.call_id, "call-roundtrip");
    // Unknown tool -> error.
    assert!(result.is_error);
    let err = result.error.as_ref().expect("error payload");
    assert_eq!(err.code, HostCallErrorCode::InvalidRequest);
}

/// Protocol adapter: capability mismatch -> invalid_request.
#[test]
fn protocol_adapter_capability_mismatch_returns_invalid_request() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let ctx = test_host_call_context(&tools, &http, &policy);

    // Claim capability "exec" but method is "tool" with name "read" (requires "read").
    let msg = make_host_call_msg(
        "call-mismatch",
        "tool",
        "exec",
        json!({ "name": "read", "input": {} }),
    );

    let responses = run_async(async { handle_extension_message(&ctx, msg).await });
    assert_eq!(responses.len(), 1);

    let result = match &responses[0].body {
        ExtensionBody::HostResult(result) => result,
        other => panic!(),
    };

    assert!(result.is_error);
    let err = result.error.as_ref().expect("error payload");
    assert_eq!(err.code, HostCallErrorCode::InvalidRequest);
    assert!(
        err.message.contains("mismatch"),
        "expected mismatch in message: {}",
        err.message
    );
}

/// Protocol adapter: denied-by-policy -> denied.
#[test]
fn protocol_adapter_denied_by_policy() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = deny_all_policy();
    let ctx = test_host_call_context(&tools, &http, &policy);

    let msg = make_host_call_msg(
        "call-deny",
        "tool",
        "read",
        json!({ "name": "read", "input": { "path": "/etc/passwd" } }),
    );

    let responses = run_async(async { handle_extension_message(&ctx, msg).await });
    assert_eq!(responses.len(), 1);

    let result = match &responses[0].body {
        ExtensionBody::HostResult(result) => result,
        other => panic!(),
    };

    assert!(result.is_error);
    let err = result.error.as_ref().expect("error payload");
    assert_eq!(err.code, HostCallErrorCode::Denied);
}

/// Protocol adapter: wrong message type -> invalid_request error.
#[test]
fn protocol_adapter_wrong_message_type_returns_error() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let ctx = test_host_call_context(&tools, &http, &policy);

    // Send an error message instead of host_call.
    let msg = ExtensionMessage {
        id: "msg-wrong".to_string(),
        version: PROTOCOL_VERSION.to_string(),
        body: ExtensionBody::Error(ErrorPayload {
            code: "test_error".to_string(),
            message: "this is not a host_call".to_string(),
            details: None,
        }),
    };

    let responses = run_async(async { handle_extension_message(&ctx, msg).await });
    assert_eq!(responses.len(), 1);

    let result = match &responses[0].body {
        ExtensionBody::HostResult(result) => result,
        other => panic!(),
    };

    assert!(result.is_error);
    let err = result.error.as_ref().expect("error payload");
    assert_eq!(err.code, HostCallErrorCode::InvalidRequest);
    assert!(
        err.message.contains("expects host_call"),
        "error should mention expected type: {}",
        err.message
    );
}

/// Protocol adapter: successful tool execution roundtrip.
#[test]
fn protocol_adapter_tool_success_roundtrip() {
    let dir = tempdir().expect("tempdir");
    let cwd = dir.path();

    // Write a file we can read.
    std::fs::write(cwd.join("hello.txt"), "world").expect("write test file");

    let tools = ToolRegistry::new(&["read"], cwd, None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let ctx = HostCallContext {
        runtime_name: "protocol",
        extension_id: Some("ext.test"),
        tools: &tools,
        http: &http,
        manager: None,
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let msg = make_host_call_msg(
        "call-read-ok",
        "tool",
        "read",
        json!({ "name": "read", "input": { "path": cwd.join("hello.txt").to_str().unwrap() } }),
    );

    let responses = run_async(async { handle_extension_message(&ctx, msg).await });
    assert_eq!(responses.len(), 1);

    let response = &responses[0];
    response.validate().expect("response must validate");

    let result = match &response.body {
        ExtensionBody::HostResult(result) => result,
        other => panic!(),
    };

    assert_eq!(result.call_id, "call-read-ok");
    assert!(!result.is_error, "read should succeed: {:?}", result.error);
    // Output should contain the file content.
    let output_str = serde_json::to_string(&result.output).expect("serialize");
    assert!(
        output_str.contains("world"),
        "output should contain file content: {output_str}"
    );
}

#[test]
fn hostcall_request_to_payload_preserves_method_and_capability() {
    let request = HostcallRequest {
        call_id: "call-conv".to_string(),
        kind: HostcallKind::Tool {
            name: "read".to_string(),
        },
        payload: json!({ "path": "test.txt" }),
        trace_id: 42,
        extension_id: Some("ext.test".to_string()),
    };

    let payload = hostcall_request_to_payload(&request);
    assert_eq!(payload.method, "tool");
    assert_eq!(payload.capability, "read");
    assert_eq!(payload.call_id, "call-conv");
    assert_eq!(
        payload.params,
        json!({ "name": "read", "input": { "path": "test.txt" } })
    );
    assert_eq!(
        payload.context,
        Some(json!({
            "typed_opcode": {
                "schema": HOSTCALL_OPCODE_SCHEMA_VERSION,
                "version": HOSTCALL_OPCODE_VERSION,
                "code": "tool.read"
            },
            "io_uring_lane_input": {
                "schema": HOSTCALL_IO_URING_CONTEXT_SCHEMA_VERSION,
                "capability_class": "filesystem",
                "io_hint": "io_heavy"
            },
        }))
    );
}

#[test]
fn hostcall_request_to_payload_exec_shape() {
    let request = HostcallRequest {
        call_id: "call-exec".to_string(),
        kind: HostcallKind::Exec {
            cmd: "ls".to_string(),
        },
        payload: json!({ "args": ["-la"], "timeout": 30000 }),
        trace_id: 1,
        extension_id: None,
    };

    let payload = hostcall_request_to_payload(&request);
    assert_eq!(payload.method, "exec");
    assert_eq!(payload.capability, "exec");
    // Params should have "cmd" injected
    assert_eq!(
        payload.params.get("cmd").and_then(Value::as_str),
        Some("ls")
    );
    assert!(payload.params.get("args").is_some());
    assert_eq!(
        payload.context,
        Some(json!({
            "io_uring_lane_input": {
                "schema": HOSTCALL_IO_URING_CONTEXT_SCHEMA_VERSION,
                "capability_class": "execution",
                "io_hint": "cpu_bound"
            }
        }))
    );
}

#[test]
fn hostcall_request_to_payload_session_shape() {
    let request = HostcallRequest {
        call_id: "call-session".to_string(),
        kind: HostcallKind::Session {
            op: "get_state".to_string(),
        },
        payload: json!({ "key": "value" }),
        trace_id: 1,
        extension_id: None,
    };

    let payload = hostcall_request_to_payload(&request);
    assert_eq!(payload.method, "session");
    assert_eq!(payload.capability, "session");
    // Params should have "op" injected
    assert_eq!(
        payload.params.get("op").and_then(Value::as_str),
        Some("get_state")
    );
    assert_eq!(
        payload.params.get("key").and_then(Value::as_str),
        Some("value")
    );
    // get_state is a recognized typed opcode, so context should be present.
    assert_eq!(
        payload.context,
        Some(json!({
            "typed_opcode": {
                "schema": HOSTCALL_OPCODE_SCHEMA_VERSION,
                "version": HOSTCALL_OPCODE_VERSION,
                "code": "session.get_state"
            },
            "io_uring_lane_input": {
                "schema": HOSTCALL_IO_URING_CONTEXT_SCHEMA_VERSION,
                "capability_class": "session",
                "io_hint": "unknown"
            },
        }))
    );
}

#[test]
fn hostcall_request_to_payload_session_get_name_emits_typed_opcode_context() {
    let request = HostcallRequest {
        call_id: "call-session-name".to_string(),
        kind: HostcallKind::Session {
            op: "get_name".to_string(),
        },
        payload: json!({}),
        trace_id: 7,
        extension_id: None,
    };

    let payload = hostcall_request_to_payload(&request);
    assert_eq!(
        payload.context,
        Some(json!({
            "typed_opcode": {
                "schema": HOSTCALL_OPCODE_SCHEMA_VERSION,
                "version": HOSTCALL_OPCODE_VERSION,
                "code": "session.get_name"
            },
            "io_uring_lane_input": {
                "schema": HOSTCALL_IO_URING_CONTEXT_SCHEMA_VERSION,
                "capability_class": "session",
                "io_hint": "unknown"
            },
        }))
    );
}

// ========================================================================
// bd-3ar8v.4.8.23: Typed opcode round-trip serialization tests for all
//                   hostcall fast-lane matrix entries
// ========================================================================

/// Tool.write round-trip: verifies method, capability, typed opcode context.
#[test]
fn hostcall_request_to_payload_tool_write_roundtrip() {
    let request = HostcallRequest {
        call_id: "rt-tool-write".to_string(),
        kind: HostcallKind::Tool {
            name: "write".to_string(),
        },
        payload: json!({ "path": "/tmp/out.txt", "content": "hello" }),
        trace_id: 10,
        extension_id: None,
    };

    let payload = hostcall_request_to_payload(&request);
    assert_eq!(payload.method, "tool");
    assert_eq!(payload.capability, "write");
    assert_eq!(
        payload.params.get("name").and_then(Value::as_str),
        Some("write")
    );
    let ctx = payload.context.as_ref().expect("context for tool.write");
    assert_eq!(ctx["typed_opcode"]["code"], "tool.write");
    assert_eq!(
        ctx["typed_opcode"]["schema"],
        HOSTCALL_OPCODE_SCHEMA_VERSION
    );
    assert_eq!(ctx["typed_opcode"]["version"], HOSTCALL_OPCODE_VERSION);
}

/// Tool.edit round-trip: verifies typed opcode context and filesystem class.
#[test]
fn hostcall_request_to_payload_tool_edit_roundtrip() {
    let request = HostcallRequest {
        call_id: "rt-tool-edit".to_string(),
        kind: HostcallKind::Tool {
            name: "edit".to_string(),
        },
        payload: json!({ "path": "/tmp/f.txt", "old": "a", "new": "b" }),
        trace_id: 11,
        extension_id: None,
    };

    let payload = hostcall_request_to_payload(&request);
    assert_eq!(payload.method, "tool");
    assert_eq!(payload.capability, "write");
    let ctx = payload.context.as_ref().expect("context for tool.edit");
    assert_eq!(ctx["typed_opcode"]["code"], "tool.edit");
    assert_eq!(ctx["io_uring_lane_input"]["capability_class"], "filesystem");
}

/// Tool.bash round-trip: verifies execution capability class.
#[test]
fn hostcall_request_to_payload_tool_bash_roundtrip() {
    let request = HostcallRequest {
        call_id: "rt-tool-bash".to_string(),
        kind: HostcallKind::Tool {
            name: "bash".to_string(),
        },
        payload: json!({ "command": "echo hello" }),
        trace_id: 12,
        extension_id: None,
    };

    let payload = hostcall_request_to_payload(&request);
    assert_eq!(payload.method, "tool");
    assert_eq!(payload.capability, "exec");
    let ctx = payload.context.as_ref().expect("context for tool.bash");
    assert_eq!(ctx["typed_opcode"]["code"], "tool.bash");
    assert_eq!(ctx["io_uring_lane_input"]["capability_class"], "execution");
}

/// Exec kind round-trip: verifies cmd is placed in params.
#[test]
fn hostcall_request_to_payload_exec_roundtrip() {
    let request = HostcallRequest {
        call_id: "rt-exec".to_string(),
        kind: HostcallKind::Exec {
            cmd: "ls".to_string(),
        },
        payload: json!({ "args": ["-la"], "timeout": 5000 }),
        trace_id: 13,
        extension_id: None,
    };

    let payload = hostcall_request_to_payload(&request);
    assert_eq!(payload.method, "exec");
    assert_eq!(payload.capability, "exec");
    assert_eq!(
        payload.params.get("cmd").and_then(Value::as_str),
        Some("ls")
    );
    assert_eq!(payload.params["args"], json!(["-la"]));
}

/// HTTP kind round-trip: passes payload through.
#[test]
fn hostcall_request_to_payload_http_roundtrip() {
    let request = HostcallRequest {
        call_id: "rt-http".to_string(),
        kind: HostcallKind::Http,
        payload: json!({ "url": "https://example.com", "method": "GET" }),
        trace_id: 14,
        extension_id: None,
    };

    let payload = hostcall_request_to_payload(&request);
    assert_eq!(payload.method, "http");
    assert_eq!(payload.capability, "http");
    assert_eq!(
        payload.params.get("url").and_then(Value::as_str),
        Some("https://example.com")
    );
}

/// Session set_model round-trip: verifies typed opcode.
#[test]
fn hostcall_request_to_payload_session_set_model_roundtrip() {
    let request = HostcallRequest {
        call_id: "rt-session-set-model".to_string(),
        kind: HostcallKind::Session {
            op: "set_model".to_string(),
        },
        payload: json!({ "provider": "anthropic", "model": "claude-sonnet-4-5" }),
        trace_id: 15,
        extension_id: None,
    };

    let payload = hostcall_request_to_payload(&request);
    assert_eq!(payload.method, "session");
    assert_eq!(payload.capability, "session");
    assert_eq!(
        payload.params.get("op").and_then(Value::as_str),
        Some("set_model")
    );
    let ctx = payload.context.as_ref().expect("context");
    assert_eq!(ctx["typed_opcode"]["code"], "session.set_model");
}

/// Session get_model round-trip: verifies typed opcode.
#[test]
fn hostcall_request_to_payload_session_get_model_roundtrip() {
    let request = HostcallRequest {
        call_id: "rt-session-get-model".to_string(),
        kind: HostcallKind::Session {
            op: "get_model".to_string(),
        },
        payload: json!({}),
        trace_id: 16,
        extension_id: None,
    };

    let payload = hostcall_request_to_payload(&request);
    let ctx = payload.context.as_ref().expect("context");
    assert_eq!(ctx["typed_opcode"]["code"], "session.get_model");
    assert_eq!(ctx["io_uring_lane_input"]["capability_class"], "session");
}

/// Session get_thinking_level and set_thinking_level round-trip.
#[test]
fn hostcall_request_to_payload_session_thinking_level_roundtrip() {
    for op in &["get_thinking_level", "set_thinking_level"] {
        let request = HostcallRequest {
            call_id: format!("rt-session-{op}"),
            kind: HostcallKind::Session { op: op.to_string() },
            payload: json!({}),
            trace_id: 17,
            extension_id: None,
        };

        let payload = hostcall_request_to_payload(&request);
        let ctx = payload.context.as_ref().unwrap_or_else(|| {
            panic!();
        });
        assert_eq!(
            ctx["typed_opcode"]["code"],
            format!("session.{op}"),
            "opcode mismatch for {op}"
        );
    }
}

/// Session set_label round-trip.
#[test]
fn hostcall_request_to_payload_session_set_label_roundtrip() {
    let request = HostcallRequest {
        call_id: "rt-session-set-label".to_string(),
        kind: HostcallKind::Session {
            op: "set_label".to_string(),
        },
        payload: json!({ "target_id": "msg-1", "label": "important" }),
        trace_id: 18,
        extension_id: None,
    };

    let payload = hostcall_request_to_payload(&request);
    let ctx = payload.context.as_ref().expect("context");
    assert_eq!(ctx["typed_opcode"]["code"], "session.set_label");
}

/// Session new getters (get_state, get_messages, get_entries, get_branch,
/// `get_file`) round-trip with typed opcodes.
#[test]
fn hostcall_request_to_payload_session_new_getters_roundtrip() {
    let ops = [
        "get_state",
        "get_messages",
        "get_entries",
        "get_branch",
        "get_file",
    ];

    for op in ops {
        let request = HostcallRequest {
            call_id: format!("rt-session-{op}"),
            kind: HostcallKind::Session { op: op.to_string() },
            payload: json!({}),
            trace_id: 19,
            extension_id: None,
        };

        let payload = hostcall_request_to_payload(&request);
        assert_eq!(payload.method, "session", "method mismatch for {op}");
        assert_eq!(
            payload.capability, "session",
            "capability mismatch for {op}"
        );
        let ctx = payload.context.as_ref().unwrap_or_else(|| {
            panic!();
        });
        assert_eq!(
            ctx["typed_opcode"]["code"],
            format!("session.{op}"),
            "opcode code mismatch for {op}"
        );
        assert_eq!(
            ctx["io_uring_lane_input"]["capability_class"], "session",
            "capability_class mismatch for {op}"
        );
    }
}

/// Events round-trip for all declared event operations.
#[test]
fn hostcall_request_to_payload_events_all_ops_roundtrip() {
    let event_ops = [
        "get_active_tools",
        "get_all_tools",
        "set_active_tools",
        "emit",
        "list",
        "get_model",
        "set_model",
        "get_thinking_level",
        "set_thinking_level",
        "get_flag",
        "list_flags",
        "append_entry",
        "register_command",
    ];

    for op in event_ops {
        let request = HostcallRequest {
            call_id: format!("rt-events-{op}"),
            kind: HostcallKind::Events { op: op.to_string() },
            payload: json!({}),
            trace_id: 20,
            extension_id: None,
        };

        let payload = hostcall_request_to_payload(&request);
        assert_eq!(payload.method, "events", "method mismatch for events.{op}");
        assert_eq!(
            payload.capability, "events",
            "capability mismatch for events.{op}"
        );
        assert_eq!(
            payload.params.get("op").and_then(Value::as_str),
            Some(op),
            "op not injected for events.{op}"
        );

        let ctx = payload.context.as_ref().unwrap_or_else(|| {
            panic!();
        });
        assert_eq!(
            ctx["typed_opcode"]["code"],
            format!("events.{op}"),
            "opcode code mismatch for events.{op}"
        );
        assert_eq!(
            ctx["io_uring_lane_input"]["capability_class"], "events",
            "capability_class mismatch for events.{op}"
        );
    }
}

/// UI round-trip: verifies op injection.
#[test]
fn hostcall_request_to_payload_ui_roundtrip() {
    let request = HostcallRequest {
        call_id: "rt-ui-confirm".to_string(),
        kind: HostcallKind::Ui {
            op: "confirm".to_string(),
        },
        payload: json!({ "message": "Are you sure?" }),
        trace_id: 21,
        extension_id: None,
    };

    let payload = hostcall_request_to_payload(&request);
    assert_eq!(payload.method, "ui");
    assert_eq!(payload.capability, "ui");
    assert_eq!(
        payload.params.get("op").and_then(Value::as_str),
        Some("confirm")
    );
}

/// Log kind round-trip: passes through payload.
#[test]
fn hostcall_request_to_payload_log_roundtrip() {
    let request = HostcallRequest {
        call_id: "rt-log".to_string(),
        kind: HostcallKind::Log,
        payload: json!({ "level": "info", "message": "test" }),
        trace_id: 22,
        extension_id: None,
    };

    let payload = hostcall_request_to_payload(&request);
    assert_eq!(payload.method, "log");
    assert_eq!(payload.capability, "log");
}

/// Outcome round-trip: `HostResultPayload` -> `HostcallOutcome` -> `HostResultPayload`
/// for success, error, and stream chunk.
#[test]
fn host_result_to_outcome_and_back_roundtrip() {
    // Success
    let success = HostResultPayload {
        call_id: "rt-s".to_string(),
        output: json!({"data": 42}),
        is_error: false,
        error: None,
        chunk: None,
    };
    let outcome = host_result_to_outcome(success);
    assert!(matches!(outcome, HostcallOutcome::Success(_)));
    let back = outcome_to_host_result("rt-s", &outcome);
    assert!(!back.is_error);
    assert_eq!(back.output, json!({"data": 42}));

    // Error
    let error_result = HostResultPayload {
        call_id: "rt-e".to_string(),
        output: json!({}),
        is_error: true,
        error: Some(HostCallError {
            code: HostCallErrorCode::Denied,
            message: "nope".to_string(),
            details: None,
            retryable: None,
        }),
        chunk: None,
    };
    let outcome = host_result_to_outcome(error_result);
    assert!(matches!(outcome, HostcallOutcome::Error { .. }));
    let back = outcome_to_host_result("rt-e", &outcome);
    assert!(back.is_error);
    let err = back.error.as_ref().expect("error");
    assert_eq!(err.code, HostCallErrorCode::Denied);

    // Stream chunk
    let chunk_result = HostResultPayload {
        call_id: "rt-c".to_string(),
        output: json!({"chunk_data": "piece"}),
        is_error: false,
        error: None,
        chunk: Some(HostStreamChunk {
            index: 3,
            is_last: false,
            backpressure: None,
        }),
    };
    let outcome = host_result_to_outcome(chunk_result);
    assert!(matches!(outcome, HostcallOutcome::StreamChunk { .. }));
    let back = outcome_to_host_result("rt-c", &outcome);
    assert!(!back.is_error);
    let chunk = back.chunk.as_ref().expect("chunk");
    assert_eq!(chunk.index, 3);
    assert!(!chunk.is_last);
}

#[test]
fn validate_host_call_rejects_malformed_typed_opcode_context() {
    let payload = HostCallPayload {
        call_id: "bad-opcode-context".to_string(),
        capability: "read".to_string(),
        method: "tool".to_string(),
        params: json!({ "name": "read", "input": {} }),
        timeout_ms: None,
        cancel_token: None,
        context: Some(json!({
            "typed_opcode": {
                "schema": HOSTCALL_OPCODE_SCHEMA_VERSION,
                "version": HOSTCALL_OPCODE_VERSION,
                "code": "tool.unknown"
            }
        })),
    };

    let err = validate_host_call(&payload).expect_err("unknown opcode code must be rejected");
    assert!(
        err.to_string()
            .contains("Unknown host_call typed opcode code"),
        "unexpected error: {err}"
    );
}

#[test]
fn validate_host_call_rejects_typed_opcode_without_schema() {
    let payload = HostCallPayload {
        call_id: "bad-opcode-no-schema".to_string(),
        capability: "read".to_string(),
        method: "tool".to_string(),
        params: json!({ "name": "read", "input": {} }),
        timeout_ms: None,
        cancel_token: None,
        context: Some(json!({
            "typed_opcode": {
                "version": HOSTCALL_OPCODE_VERSION,
                "code": "tool.read"
            }
        })),
    };

    let err = validate_host_call(&payload).expect_err("missing schema must be rejected");
    assert!(
        err.to_string()
            .contains("context.typed_opcode.schema is required"),
        "unexpected error: {err}"
    );
}

#[test]
fn resolve_hostcall_opcode_fallback_for_unsupported_ops() {
    let payload = HostCallPayload {
        call_id: "fallback-op".to_string(),
        capability: "session".to_string(),
        method: "session".to_string(),
        params: json!({ "op": "append_entry", "customType": "metric", "data": {} }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let resolution = resolve_hostcall_opcode(&payload).expect("opcode resolution");
    assert!(matches!(
        resolution,
        HostcallOpcodeResolution::Fallback {
            reason: "opcode_not_declared_or_not_supported"
        }
    ));
}

#[test]
fn select_hostcall_lane_fast_for_typed_tool_opcode() {
    let payload = HostCallPayload {
        call_id: "lane-fast".to_string(),
        capability: "read".to_string(),
        method: "tool".to_string(),
        params: json!({ "name": "read", "input": {} }),
        timeout_ms: None,
        cancel_token: None,
        context: Some(json!({
            "typed_opcode": {
                "schema": HOSTCALL_OPCODE_SCHEMA_VERSION,
                "version": HOSTCALL_OPCODE_VERSION,
                "code": "tool.read"
            }
        })),
    };

    let lane = select_hostcall_lane(&payload).expect("lane decision");
    assert_eq!(lane.lane, HostcallDispatchLane::Fast);
    assert_eq!(lane.reason, "typed_opcode_context_v1");
    assert_eq!(lane.capability_class, "filesystem");
    assert_eq!(lane.matrix_key, "tool|tool.read|filesystem");
    assert_eq!(lane.opcode, Some(CommonHostcallOpcode::ToolRead));
}

#[test]
fn select_hostcall_lane_fast_when_opcode_is_derived() {
    let payload = HostCallPayload {
        call_id: "lane-fast-derived".to_string(),
        capability: "session".to_string(),
        method: "session".to_string(),
        params: json!({ "op": "get_name" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let lane = select_hostcall_lane(&payload).expect("lane decision");
    assert_eq!(lane.lane, HostcallDispatchLane::Fast);
    assert_eq!(lane.reason, "typed_opcode_derived_v1");
    assert_eq!(lane.capability_class, "session");
    assert_eq!(lane.matrix_key, "session|session.get_name|session");
    assert_eq!(lane.opcode, Some(CommonHostcallOpcode::SessionGetName));
}

#[test]
fn select_hostcall_lane_compat_for_untyped_session_op() {
    let payload = HostCallPayload {
        call_id: "lane-compat".to_string(),
        capability: "session".to_string(),
        method: "session".to_string(),
        params: json!({ "op": "append_entry", "customType": "x", "data": {} }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let lane = select_hostcall_lane(&payload).expect("lane decision");
    assert_eq!(lane.lane, HostcallDispatchLane::Compat);
    assert_eq!(lane.reason, "opcode_not_declared_or_not_supported");
    assert_eq!(lane.capability_class, "session");
    assert_eq!(lane.matrix_key, "session|fallback|session");
    assert!(lane.opcode.is_none());
}

#[test]
fn select_hostcall_lane_compat_for_env_hostcall() {
    let payload = HostCallPayload {
        call_id: "lane-env".to_string(),
        capability: "env".to_string(),
        method: "env".to_string(),
        params: json!({ "name": "HOME" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let lane = select_hostcall_lane(&payload).expect("lane decision");
    assert_eq!(lane.lane, HostcallDispatchLane::Compat);
    assert_eq!(lane.reason, "opcode_not_declared_or_not_supported");
    assert_eq!(lane.capability_class, "environment");
    assert_eq!(lane.matrix_key, "env|fallback|environment");
    assert!(lane.opcode.is_none());
}

#[test]
fn select_hostcall_lane_rejects_capability_mismatch_for_fast_opcode() {
    let payload = HostCallPayload {
        call_id: "lane-cap-mismatch".to_string(),
        capability: "write".to_string(),
        method: "tool".to_string(),
        params: json!({ "name": "read", "input": {} }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let err = select_hostcall_lane(&payload).expect_err("capability mismatch must fail");
    assert!(
        err.to_string().contains("Host call capability mismatch"),
        "unexpected error: {err}"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn hostcall_fast_lane_matrix_entries_are_consistent() {
    let matrix = [
        (
            CommonHostcallOpcode::ToolRead,
            "tool",
            "tool.read",
            "filesystem",
            "tool|tool.read|filesystem",
        ),
        (
            CommonHostcallOpcode::ToolWrite,
            "tool",
            "tool.write",
            "filesystem",
            "tool|tool.write|filesystem",
        ),
        (
            CommonHostcallOpcode::ToolEdit,
            "tool",
            "tool.edit",
            "filesystem",
            "tool|tool.edit|filesystem",
        ),
        (
            CommonHostcallOpcode::ToolBash,
            "tool",
            "tool.bash",
            "execution",
            "tool|tool.bash|execution",
        ),
        (
            CommonHostcallOpcode::SessionGetName,
            "session",
            "session.get_name",
            "session",
            "session|session.get_name|session",
        ),
        (
            CommonHostcallOpcode::SessionSetName,
            "session",
            "session.set_name",
            "session",
            "session|session.set_name|session",
        ),
        (
            CommonHostcallOpcode::SessionGetModel,
            "session",
            "session.get_model",
            "session",
            "session|session.get_model|session",
        ),
        (
            CommonHostcallOpcode::SessionSetModel,
            "session",
            "session.set_model",
            "session",
            "session|session.set_model|session",
        ),
        (
            CommonHostcallOpcode::SessionGetThinkingLevel,
            "session",
            "session.get_thinking_level",
            "session",
            "session|session.get_thinking_level|session",
        ),
        (
            CommonHostcallOpcode::SessionSetThinkingLevel,
            "session",
            "session.set_thinking_level",
            "session",
            "session|session.set_thinking_level|session",
        ),
        (
            CommonHostcallOpcode::SessionSetLabel,
            "session",
            "session.set_label",
            "session",
            "session|session.set_label|session",
        ),
        (
            CommonHostcallOpcode::EventsGetActiveTools,
            "events",
            "events.get_active_tools",
            "events",
            "events|events.get_active_tools|events",
        ),
        (
            CommonHostcallOpcode::EventsGetAllTools,
            "events",
            "events.get_all_tools",
            "events",
            "events|events.get_all_tools|events",
        ),
        (
            CommonHostcallOpcode::EventsSetActiveTools,
            "events",
            "events.set_active_tools",
            "events",
            "events|events.set_active_tools|events",
        ),
        (
            CommonHostcallOpcode::EventsEmit,
            "events",
            "events.emit",
            "events",
            "events|events.emit|events",
        ),
        (
            CommonHostcallOpcode::EventsList,
            "events",
            "events.list",
            "events",
            "events|events.list|events",
        ),
        // --- new session getters ---
        (
            CommonHostcallOpcode::SessionGetState,
            "session",
            "session.get_state",
            "session",
            "session|session.get_state|session",
        ),
        (
            CommonHostcallOpcode::SessionGetMessages,
            "session",
            "session.get_messages",
            "session",
            "session|session.get_messages|session",
        ),
        (
            CommonHostcallOpcode::SessionGetEntries,
            "session",
            "session.get_entries",
            "session",
            "session|session.get_entries|session",
        ),
        (
            CommonHostcallOpcode::SessionGetBranch,
            "session",
            "session.get_branch",
            "session",
            "session|session.get_branch|session",
        ),
        (
            CommonHostcallOpcode::SessionGetFile,
            "session",
            "session.get_file",
            "session",
            "session|session.get_file|session",
        ),
        // --- new events operations ---
        (
            CommonHostcallOpcode::EventsGetModel,
            "events",
            "events.get_model",
            "events",
            "events|events.get_model|events",
        ),
        (
            CommonHostcallOpcode::EventsSetModel,
            "events",
            "events.set_model",
            "events",
            "events|events.set_model|events",
        ),
        (
            CommonHostcallOpcode::EventsGetThinkingLevel,
            "events",
            "events.get_thinking_level",
            "events",
            "events|events.get_thinking_level|events",
        ),
        (
            CommonHostcallOpcode::EventsSetThinkingLevel,
            "events",
            "events.set_thinking_level",
            "events",
            "events|events.set_thinking_level|events",
        ),
        (
            CommonHostcallOpcode::EventsGetFlag,
            "events",
            "events.get_flag",
            "events",
            "events|events.get_flag|events",
        ),
        (
            CommonHostcallOpcode::EventsListFlags,
            "events",
            "events.list_flags",
            "events",
            "events|events.list_flags|events",
        ),
        (
            CommonHostcallOpcode::EventsAppendEntry,
            "events",
            "events.append_entry",
            "events",
            "events|events.append_entry|events",
        ),
        (
            CommonHostcallOpcode::EventsRegisterCommand,
            "events",
            "events.register_command",
            "events",
            "events|events.register_command|events",
        ),
    ];

    for (opcode, method, code, capability_class, matrix_key) in matrix {
        assert_eq!(opcode.method(), method);
        assert_eq!(opcode.code(), code);
        assert_eq!(opcode.capability_class(), capability_class);
        assert_eq!(opcode.lane_matrix_key(), matrix_key);
        assert_eq!(
            opcode.lane_matrix_key(),
            format!("{method}|{code}|{capability_class}")
        );
    }
}

#[test]
fn select_hostcall_lane_compat_unknown_method_uses_unknown_matrix_key() {
    let payload = HostCallPayload {
        call_id: "lane-unknown".to_string(),
        capability: "tool".to_string(),
        method: "mystery".to_string(),
        params: json!({}),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let lane = select_hostcall_lane(&payload).expect("lane decision");
    assert_eq!(lane.lane, HostcallDispatchLane::Compat);
    assert_eq!(lane.reason, "opcode_not_declared_or_not_supported");
    assert_eq!(lane.capability_class, "tool");
    assert_eq!(lane.matrix_key, "unknown|fallback|unknown");
    assert!(lane.opcode.is_none());
}

#[test]
fn select_hostcall_lane_rejects_mismatched_typed_opcode() {
    let payload = HostCallPayload {
        call_id: "lane-bad".to_string(),
        capability: "session".to_string(),
        method: "session".to_string(),
        params: json!({ "op": "set_name", "name": "x" }),
        timeout_ms: None,
        cancel_token: None,
        context: Some(json!({
            "typed_opcode": {
                "schema": HOSTCALL_OPCODE_SCHEMA_VERSION,
                "version": HOSTCALL_OPCODE_VERSION,
                "code": "session.get_name"
            }
        })),
    };

    let err = select_hostcall_lane(&payload).expect_err("mismatch must fail");
    assert!(
        err.to_string()
            .contains("does not match payload-derived opcode"),
        "unexpected error: {err}"
    );
}

#[test]
fn select_hostcall_lane_fast_for_new_session_getters() {
    let cases: &[(&str, CommonHostcallOpcode)] = &[
        ("get_state", CommonHostcallOpcode::SessionGetState),
        ("get_messages", CommonHostcallOpcode::SessionGetMessages),
        ("get_entries", CommonHostcallOpcode::SessionGetEntries),
        ("get_branch", CommonHostcallOpcode::SessionGetBranch),
        ("get_file", CommonHostcallOpcode::SessionGetFile),
    ];
    for (op, expected_opcode) in cases {
        let payload = HostCallPayload {
            call_id: format!("lane-session-{op}"),
            capability: "session".to_string(),
            method: "session".to_string(),
            params: json!({ "op": op }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        let lane = select_hostcall_lane(&payload).unwrap_or_else(|e| panic!());
        assert_eq!(
            lane.lane,
            HostcallDispatchLane::Fast,
            "session op '{op}' should route to fast lane"
        );
        assert_eq!(lane.reason, "typed_opcode_derived_v1");
        assert_eq!(lane.opcode, Some(*expected_opcode));
        assert_eq!(lane.capability_class, "session");
    }
}

#[test]
fn select_hostcall_lane_fast_for_new_events_ops() {
    let cases: &[(&str, CommonHostcallOpcode)] = &[
        ("get_model", CommonHostcallOpcode::EventsGetModel),
        ("set_model", CommonHostcallOpcode::EventsSetModel),
        (
            "get_thinking_level",
            CommonHostcallOpcode::EventsGetThinkingLevel,
        ),
        (
            "set_thinking_level",
            CommonHostcallOpcode::EventsSetThinkingLevel,
        ),
        ("get_flag", CommonHostcallOpcode::EventsGetFlag),
        ("list_flags", CommonHostcallOpcode::EventsListFlags),
        ("append_entry", CommonHostcallOpcode::EventsAppendEntry),
        (
            "register_command",
            CommonHostcallOpcode::EventsRegisterCommand,
        ),
    ];
    for (op, expected_opcode) in cases {
        let payload = HostCallPayload {
            call_id: format!("lane-events-{op}"),
            capability: "events".to_string(),
            method: "events".to_string(),
            params: json!({ "op": op }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        let lane = select_hostcall_lane(&payload).unwrap_or_else(|e| panic!());
        assert_eq!(
            lane.lane,
            HostcallDispatchLane::Fast,
            "events op '{op}' should route to fast lane"
        );
        assert_eq!(lane.reason, "typed_opcode_derived_v1");
        assert_eq!(lane.opcode, Some(*expected_opcode));
        assert_eq!(lane.capability_class, "events");
    }
}

#[test]
fn parse_session_hostcall_op_accepts_alias_variants() {
    let cases: &[(&str, SessionHostcallOp)] = &[
        ("appendMessage", SessionHostcallOp::AppendMessage),
        ("append_message", SessionHostcallOp::AppendMessage),
        ("append-message", SessionHostcallOp::AppendMessage),
        ("append message", SessionHostcallOp::AppendMessage),
        ("setModel", SessionHostcallOp::SetModel),
        ("set_model", SessionHostcallOp::SetModel),
        ("set-model", SessionHostcallOp::SetModel),
        ("setThinkingLevel", SessionHostcallOp::SetThinkingLevel),
        ("set_thinking_level", SessionHostcallOp::SetThinkingLevel),
        ("set-thinking-level", SessionHostcallOp::SetThinkingLevel),
        ("setLabel", SessionHostcallOp::SetLabel),
        ("set_label", SessionHostcallOp::SetLabel),
        ("set-label", SessionHostcallOp::SetLabel),
    ];
    for (raw, expected) in cases {
        assert_eq!(
            parse_session_hostcall_op(raw),
            Some(*expected),
            "session op alias should parse: {raw}"
        );
        assert_eq!(
            parse_session_hostcall_op(&raw.to_ascii_uppercase()),
            Some(*expected),
            "uppercase session op alias should parse: {raw}"
        );
    }

    assert_eq!(parse_session_hostcall_op("unknown"), None);
    assert_eq!(parse_session_hostcall_op(""), None);
}

#[test]
fn parse_events_hostcall_op_accepts_alias_variants() {
    let cases: &[(&str, EventsHostcallOp)] = &[
        ("getActiveTools", EventsHostcallOp::GetActiveTools),
        ("get_active_tools", EventsHostcallOp::GetActiveTools),
        ("get-active-tools", EventsHostcallOp::GetActiveTools),
        ("setModel", EventsHostcallOp::SetModel),
        ("set_model", EventsHostcallOp::SetModel),
        ("set-model", EventsHostcallOp::SetModel),
        ("setThinkingLevel", EventsHostcallOp::SetThinkingLevel),
        ("set_thinking_level", EventsHostcallOp::SetThinkingLevel),
        ("set-thinking-level", EventsHostcallOp::SetThinkingLevel),
        ("appendEntry", EventsHostcallOp::AppendEntry),
        ("append_entry", EventsHostcallOp::AppendEntry),
        ("append-entry", EventsHostcallOp::AppendEntry),
        ("registerCommand", EventsHostcallOp::RegisterCommand),
        ("register_command", EventsHostcallOp::RegisterCommand),
        ("register-command", EventsHostcallOp::RegisterCommand),
        ("sendMessage", EventsHostcallOp::SendMessage),
        ("send_message", EventsHostcallOp::SendMessage),
        ("send-message", EventsHostcallOp::SendMessage),
        ("sendUserMessage", EventsHostcallOp::SendUserMessage),
        ("send_user_message", EventsHostcallOp::SendUserMessage),
        ("send-user-message", EventsHostcallOp::SendUserMessage),
    ];
    for (raw, expected) in cases {
        assert_eq!(
            parse_events_hostcall_op(raw),
            Some(*expected),
            "events op alias should parse: {raw}"
        );
        assert_eq!(
            parse_events_hostcall_op(&raw.to_ascii_uppercase()),
            Some(*expected),
            "uppercase events op alias should parse: {raw}"
        );
    }

    assert_eq!(parse_events_hostcall_op("unknown"), None);
    assert_eq!(parse_events_hostcall_op(""), None);
}

#[test]
fn session_append_entry_still_falls_back_to_compat() {
    // "append_entry" with method="session" is NOT a fast-lane opcode
    // (only method="events" has EventsAppendEntry).
    let payload = HostCallPayload {
        call_id: "session-append-compat".to_string(),
        capability: "session".to_string(),
        method: "session".to_string(),
        params: json!({ "op": "append_entry", "customType": "metric", "data": {} }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };
    let lane = select_hostcall_lane(&payload).expect("lane decision");
    assert_eq!(lane.lane, HostcallDispatchLane::Compat);
    assert!(lane.opcode.is_none());
}

#[test]
fn all_opcodes_have_consistent_round_trip_code_parse() {
    // Every opcode's code() must round-trip through parse_common_hostcall_opcode_code().
    let all_opcodes = [
        CommonHostcallOpcode::ToolRead,
        CommonHostcallOpcode::ToolWrite,
        CommonHostcallOpcode::ToolEdit,
        CommonHostcallOpcode::ToolBash,
        CommonHostcallOpcode::SessionGetState,
        CommonHostcallOpcode::SessionGetMessages,
        CommonHostcallOpcode::SessionGetEntries,
        CommonHostcallOpcode::SessionGetBranch,
        CommonHostcallOpcode::SessionGetFile,
        CommonHostcallOpcode::SessionGetName,
        CommonHostcallOpcode::SessionSetName,
        CommonHostcallOpcode::SessionGetModel,
        CommonHostcallOpcode::SessionSetModel,
        CommonHostcallOpcode::SessionGetThinkingLevel,
        CommonHostcallOpcode::SessionSetThinkingLevel,
        CommonHostcallOpcode::SessionSetLabel,
        CommonHostcallOpcode::EventsGetActiveTools,
        CommonHostcallOpcode::EventsGetAllTools,
        CommonHostcallOpcode::EventsSetActiveTools,
        CommonHostcallOpcode::EventsEmit,
        CommonHostcallOpcode::EventsList,
        CommonHostcallOpcode::EventsGetModel,
        CommonHostcallOpcode::EventsSetModel,
        CommonHostcallOpcode::EventsGetThinkingLevel,
        CommonHostcallOpcode::EventsSetThinkingLevel,
        CommonHostcallOpcode::EventsGetFlag,
        CommonHostcallOpcode::EventsListFlags,
        CommonHostcallOpcode::EventsAppendEntry,
        CommonHostcallOpcode::EventsRegisterCommand,
    ];
    assert_eq!(
        all_opcodes.len(),
        29,
        "expected 29 total opcodes (was 16, added 13 new)"
    );
    for opcode in &all_opcodes {
        let code = opcode.code();
        let parsed = parse_common_hostcall_opcode_code(code);
        assert_eq!(
            parsed,
            Some(*opcode),
            "round-trip failed for opcode code '{code}'"
        );
    }
}
