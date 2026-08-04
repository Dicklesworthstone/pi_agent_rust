use pi::Error;
use pi::extensions::{
    ExtensionMessage, ExtensionPolicy, ExtensionPolicyMode, HostCallPayload, PolicyDecision,
    required_capability_for_host_call,
};
use serde_json::json;

fn register_message_json(overrides: serde_json::Value) -> String {
    let mut base = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "register",
        "payload": {
            "name": "demo",
            "version": "0.1.0",
            "api_version": "1.0",
            "capabilities": ["read"],
            "tools": [],
            "slash_commands": [],
            "event_hooks": []
        }
    });

    if let serde_json::Value::Object(dst) = &mut base
        && let serde_json::Value::Object(src) = overrides
    {
        for (k, v) in src {
            dst.insert(k, v);
        }
    }

    base.to_string()
}

fn host_call(method: &str, params: serde_json::Value) -> HostCallPayload {
    HostCallPayload {
        call_id: "call-1".to_string(),
        capability: "declared".to_string(),
        method: method.to_string(),
        params,
        timeout_ms: None,
        cancel_token: None,
        context: None,
    }
}

#[test]
fn parse_and_validate_register_ok() {
    let json = register_message_json(json!({}));
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    assert_eq!(parsed.version, pi::extensions::PROTOCOL_VERSION);
}

#[test]
fn parse_and_validate_allows_unknown_fields() {
    let json = register_message_json(json!({
        "unknown_top_level": 123,
        "payload": {
            "name": "demo",
            "version": "0.1.0",
            "api_version": "1.0",
            "capabilities": ["read"],
            "tools": [],
            "slash_commands": [],
            "event_hooks": [],
            "unknown_payload_field": "ok"
        }
    }));
    ExtensionMessage::parse_and_validate(&json).expect("unknown fields should not reject parse");
}

#[test]
fn parse_and_validate_rejects_missing_type_field() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "payload": {
            "name": "demo",
            "version": "0.1.0",
            "api_version": "1.0",
            "capabilities": [],
            "tools": [],
            "slash_commands": [],
            "event_hooks": []
        }
    })
    .to_string();

    let err = ExtensionMessage::parse_and_validate(&json).unwrap_err();
    assert!(
        matches!(err, Error::Json(_)),
        "expected json error, got {err}"
    );
    let message = err.to_string();
    assert!(
        message.contains("missing field `type`"),
        "expected actionable missing-field message, got: {message}"
    );
}

#[test]
fn parse_and_validate_rejects_protocol_version_mismatch() {
    let json = register_message_json(json!({ "version": "999.0" }));
    let err = ExtensionMessage::parse_and_validate(&json).unwrap_err();
    assert!(
        matches!(
            err,
            Error::Validation(ref msg)
                if msg.contains("Unsupported extension protocol version")
        ),
        "expected validation error, got {err}"
    );
}

#[test]
fn parse_and_validate_rejects_empty_message_id() {
    let json = register_message_json(json!({ "id": "   " }));
    let err = ExtensionMessage::parse_and_validate(&json).unwrap_err();
    assert!(
        matches!(err, Error::Validation(ref msg) if msg == "Extension message id is empty"),
        "expected validation error, got {err}"
    );
}

#[test]
fn parse_and_validate_rejects_empty_register_name() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "register",
        "payload": {
            "name": " ",
            "version": "0.1.0",
            "api_version": "1.0",
            "capabilities": [],
            "tools": [],
            "slash_commands": [],
            "event_hooks": []
        }
    })
    .to_string();
    let err = ExtensionMessage::parse_and_validate(&json).unwrap_err();
    assert!(
        matches!(err, Error::Validation(ref msg) if msg == "Extension name is empty"),
        "expected validation error, got {err}"
    );
}

#[test]
fn policy_evaluate_covers_modes_and_deny_list() {
    let mut policy = ExtensionPolicy::default();

    // Prompt mode (default): default_caps are allowed, unknown prompts, deny_caps always deny.
    let read = policy.evaluate("read");
    assert_eq!(read.decision, PolicyDecision::Allow);
    assert_eq!(read.reason, "default_caps");

    let empty = policy.evaluate("   ");
    assert_eq!(empty.decision, PolicyDecision::Deny);
    assert_eq!(empty.reason, "empty_capability");
    assert!(empty.capability.is_empty());

    let http = policy.evaluate("HTTP");
    assert_eq!(http.decision, PolicyDecision::Allow);
    assert_eq!(http.reason, "default_caps");

    let unknown = policy.evaluate("custom_cap");
    assert_eq!(unknown.decision, PolicyDecision::Prompt);
    assert_eq!(unknown.reason, "prompt_required");

    let denied = policy.evaluate("exec");
    assert_eq!(denied.decision, PolicyDecision::Deny);
    assert_eq!(denied.reason, "deny_caps");

    // Strict: unknown is denied (but deny_caps still denies).
    policy.mode = ExtensionPolicyMode::Strict;
    let strict_unknown = policy.evaluate("custom_cap");
    assert_eq!(strict_unknown.decision, PolicyDecision::Deny);
    assert_eq!(strict_unknown.reason, "not_in_default_caps");

    // Permissive: unknown is allowed (but deny_caps still denies).
    policy.mode = ExtensionPolicyMode::Permissive;
    let permissive_unknown = policy.evaluate("custom_cap");
    assert_eq!(permissive_unknown.decision, PolicyDecision::Allow);
    assert_eq!(permissive_unknown.reason, "permissive");

    let permissive_denied = policy.evaluate("ENV");
    assert_eq!(permissive_denied.decision, PolicyDecision::Deny);
    assert_eq!(permissive_denied.reason, "deny_caps");
}

// ============================================================================
// Individual Message Type Parsing Tests (bd-261)
// ============================================================================

#[test]
fn parse_tool_call_message_ok() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "tool_call",
        "payload": {
            "call_id": "call-1",
            "name": "read",
            "input": { "path": "README.md" }
        }
    })
    .to_string();
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    assert!(matches!(
        parsed.body,
        pi::extensions::ExtensionBody::ToolCall(_)
    ));
}

#[test]
fn parse_tool_call_rejects_missing_call_id() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "tool_call",
        "payload": {
            "name": "read",
            "input": { "path": "README.md" }
        }
    })
    .to_string();
    let err = ExtensionMessage::parse_and_validate(&json).unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("call_id") || message.contains("callId"),
        "expected error about call_id, got: {message}"
    );
}

#[test]
fn parse_tool_result_message_ok() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "tool_result",
        "payload": {
            "call_id": "call-1",
            "output": { "content": "file contents" },
            "is_error": false
        }
    })
    .to_string();
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    assert!(matches!(
        parsed.body,
        pi::extensions::ExtensionBody::ToolResult(_)
    ));
}

#[test]
fn parse_tool_result_error_flag_true() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "tool_result",
        "payload": {
            "call_id": "call-1",
            "output": { "error": "file not found" },
            "is_error": true
        }
    })
    .to_string();
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    match parsed.body {
        pi::extensions::ExtensionBody::ToolResult(payload) => assert!(payload.is_error),
        _ => unreachable!("expected ToolResult"),
    }
}

#[test]
fn parse_slash_command_message_ok() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "slash_command",
        "payload": {
            "name": "/hello",
            "args": ["world", "test"]
        }
    })
    .to_string();
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    assert!(matches!(
        parsed.body,
        pi::extensions::ExtensionBody::SlashCommand(_)
    ));
}

#[test]
fn parse_slash_command_with_empty_args() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "slash_command",
        "payload": {
            "name": "/help",
            "args": []
        }
    })
    .to_string();
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    match parsed.body {
        pi::extensions::ExtensionBody::SlashCommand(payload) => assert!(payload.args.is_empty()),
        _ => unreachable!("expected SlashCommand"),
    }
}

#[test]
fn parse_slash_result_message_ok() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "slash_result",
        "payload": {
            "output": { "text": "command executed" },
            "is_error": false
        }
    })
    .to_string();
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    assert!(matches!(
        parsed.body,
        pi::extensions::ExtensionBody::SlashResult(_)
    ));
}

#[test]
fn parse_event_hook_message_ok() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "event_hook",
        "payload": {
            "event": "agent_start",
            "data": { "session_id": "sess-123" }
        }
    })
    .to_string();
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    assert!(matches!(
        parsed.body,
        pi::extensions::ExtensionBody::EventHook(_)
    ));
}

#[test]
fn parse_event_hook_with_null_data() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "event_hook",
        "payload": {
            "event": "agent_end"
        }
    })
    .to_string();
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    match parsed.body {
        pi::extensions::ExtensionBody::EventHook(payload) => assert!(payload.data.is_none()),
        _ => unreachable!("expected EventHook"),
    }
}

#[test]
fn parse_host_result_message_ok() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "host_result",
        "payload": {
            "call_id": "host-1",
            "output": { "status": "success" },
            "is_error": false
        }
    })
    .to_string();
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    assert!(matches!(
        parsed.body,
        pi::extensions::ExtensionBody::HostResult(_)
    ));
}

#[test]
fn parse_host_result_with_error_details() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "host_result",
        "payload": {
            "call_id": "host-1",
            "output": {},
            "is_error": true,
            "error": {
                "code": "denied",
                "message": "capability not allowed",
                "details": { "capability": "exec" },
                "retryable": false
            }
        }
    })
    .to_string();
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    match parsed.body {
        pi::extensions::ExtensionBody::HostResult(payload) => {
            assert!(payload.is_error);
            let error = payload.error.expect("error should be present");
            assert!(matches!(
                error.code,
                pi::extensions::HostCallErrorCode::Denied
            ));
        }
        _ => unreachable!("expected HostResult"),
    }
}

#[test]
fn parse_error_message_ok() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "error",
        "payload": {
            "code": "E_DEMO",
            "message": "Something went wrong"
        }
    })
    .to_string();
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    assert!(matches!(
        parsed.body,
        pi::extensions::ExtensionBody::Error(_)
    ));
}

#[test]
fn parse_error_message_with_details() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "error",
        "payload": {
            "code": "E_CONFIG",
            "message": "Invalid configuration",
            "details": { "field": "api_version", "expected": "1.0" }
        }
    })
    .to_string();
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    match parsed.body {
        pi::extensions::ExtensionBody::Error(payload) => {
            assert_eq!(payload.code, "E_CONFIG");
            assert!(payload.details.is_some());
        }
        _ => unreachable!("expected Error"),
    }
}

#[test]
fn parse_log_message_all_correlation_fields() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "log",
        "payload": {
            "schema": "pi.ext.log.v1",
            "ts": "2026-02-03T12:00:00.000Z",
            "level": "debug",
            "event": "tool_call.complete",
            "message": "Tool call completed successfully",
            "correlation": {
                "extension_id": "ext.demo",
                "scenario_id": "scn-001",
                "session_id": "sess-123",
                "run_id": "run-456",
                "artifact_id": "art-789",
                "tool_call_id": "call-1",
                "trace_id": "trace-abc",
                "span_id": "span-def"
            },
            "source": {
                "component": "runtime",
                "host": "localhost",
                "pid": 12345
            },
            "data": { "duration_ms": 150 }
        }
    })
    .to_string();
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    match parsed.body {
        pi::extensions::ExtensionBody::Log(payload) => {
            assert!(matches!(payload.level, pi::extensions::LogLevel::Debug));
            assert_eq!(payload.correlation.session_id.as_deref(), Some("sess-123"));
            assert!(payload.source.is_some());
        }
        _ => unreachable!("expected Log"),
    }
}

// ============================================================================
// Unicode and Edge Case Tests (bd-261)
// ============================================================================

#[test]
fn parse_message_with_unicode_content() {
    let json = json!({
        "id": "msg-unicode-😀",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "tool_call",
        "payload": {
            "call_id": "call-中文",
            "name": "read",
            "input": { "path": "文件/שלום/مرحبا.txt" }
        }
    })
    .to_string();
    let parsed = ExtensionMessage::parse_and_validate(&json).expect("parse");
    assert_eq!(parsed.id, "msg-unicode-😀");
}

#[test]
fn parse_malformed_json_fails() {
    let json = r#"{ "id": "msg-1", "version": "1.0", "#; // truncated
    let err = ExtensionMessage::parse_and_validate(json).unwrap_err();
    assert!(
        matches!(err, Error::Json(_)),
        "expected json error, got {err}"
    );
}

#[test]
fn parse_empty_json_object_fails() {
    let json = "{}";
    let err = ExtensionMessage::parse_and_validate(json).unwrap_err();
    assert!(
        matches!(err, Error::Json(_)),
        "expected json error, got {err}"
    );
}

#[test]
fn parse_json_array_fails() {
    let json = r#"[{"id": "msg-1"}]"#;
    let err = ExtensionMessage::parse_and_validate(json).unwrap_err();
    assert!(
        matches!(err, Error::Json(_)),
        "expected json error, got {err}"
    );
}

#[test]
fn parse_null_payload_fails() {
    let json = json!({
        "id": "msg-1",
        "version": pi::extensions::PROTOCOL_VERSION,
        "type": "register",
        "payload": null
    })
    .to_string();
    let err = ExtensionMessage::parse_and_validate(&json).unwrap_err();
    assert!(
        matches!(err, Error::Json(_)),
        "expected json error, got {err}"
    );
}

// ============================================================================
// Host Call Capability Mapping Tests
// ============================================================================

#[test]
fn required_capability_for_host_call_maps_tool_to_capability() {
    assert_eq!(
        required_capability_for_host_call(&host_call("exec", json!({}))).as_deref(),
        Some("exec")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call("http", json!({}))).as_deref(),
        Some("http")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call("session", json!({}))).as_deref(),
        Some("session")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call("ui", json!({}))).as_deref(),
        Some("ui")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call("log", json!({}))).as_deref(),
        Some("log")
    );

    assert_eq!(
        required_capability_for_host_call(&host_call("tool", json!({ "name": "read" }))).as_deref(),
        Some("read")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call(" TOOL ", json!({ "name": " READ " })))
            .as_deref(),
        Some("read")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call("tool", json!({ "name": "grep" }))).as_deref(),
        Some("read")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call("tool", json!({ "name": "edit" }))).as_deref(),
        Some("write")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call("tool", json!({ "name": "bash" }))).as_deref(),
        Some("exec")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call("tool", json!({ "name": "unknown-tool" })))
            .as_deref(),
        Some("tool")
    );

    assert_eq!(
        required_capability_for_host_call(&host_call("tool", json!({}))),
        None
    );
    assert_eq!(
        required_capability_for_host_call(&host_call("unknown", json!({}))),
        None
    );
}

#[test]
fn required_capability_for_host_call_maps_events_op_aliases_to_events() {
    assert_eq!(
        required_capability_for_host_call(&host_call(
            "events",
            json!({ "op": "get_active_tools" })
        ))
        .as_deref(),
        Some("events")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call("events", json!({ "method": "set-model" })))
            .as_deref(),
        Some("events")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call(
            "events",
            json!({ "name": "register command" })
        ))
        .as_deref(),
        Some("events")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call(" EVENTS ", json!({ "op": " list_flags " })))
            .as_deref(),
        Some("events")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call("events", json!({ "op": "append.entry" })))
            .as_deref(),
        Some("events")
    );
}

#[test]
fn required_capability_for_host_call_maps_session_op_aliases_to_session() {
    assert_eq!(
        required_capability_for_host_call(&host_call("session", json!({ "op": "get_model" })))
            .as_deref(),
        Some("session")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call(
            "session",
            json!({ "method": "set-thinking_level" })
        ))
        .as_deref(),
        Some("session")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call("session", json!({ "name": "set label" })))
            .as_deref(),
        Some("session")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call(" SESSION ", json!({ "op": " get-file " })))
            .as_deref(),
        Some("session")
    );
}

#[test]
fn required_capability_for_host_call_unsupported_event_session_ops_do_not_escalate() {
    // Unsupported op values should never escalate out of the declared method capability class.
    assert_eq!(
        required_capability_for_host_call(&host_call("events", json!({ "op": "launch_shell" })))
            .as_deref(),
        Some("events")
    );
    assert_eq!(
        required_capability_for_host_call(&host_call("session", json!({ "op": "exec" })))
            .as_deref(),
        Some("session")
    );

    // Unknown method still fails closed.
    assert_eq!(
        required_capability_for_host_call(&host_call("mystery", json!({ "op": "exec" }))),
        None
    );
}
