//! JSON Mode Event Parity Validation (bd-37u8a: PARITY-V1).
//!
//! Validates that the Rust JSON mode output (--mode json) is event-for-event
//! compatible with pi-mono. Verifies:
//! - All 15 event types are emitted with correct JSON schema
//! - camelCase field naming throughout
//! - Events appear in correct lifecycle order
//! - `SessionHeader` is the first output line
//!
//! This is a SMOKE TEST. The comprehensive test suite is DROPIN-172 (bd-3p29k).

mod common;

use common::TestHarness;
use pi::agent::{AgentEvent, TURN_LATENCY_BREAKDOWN_SCHEMA_V1, TurnLatencyBreakdown};
use pi::extensions::{ExtensionEventName, ExtensionUiRequest, extension_event_from_agent};
use pi::model::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, ImageContent, Message, StopReason,
    TextContent, ThinkingContent, ToolCall, Usage,
};
use pi::tools::ToolOutput;
use serde_json::{Value, json};
use std::sync::Arc;

// ============================================================================
// Helpers
// ============================================================================

/// Serialize an `AgentEvent` and return the parsed JSON value.
fn event_to_json(event: &AgentEvent) -> Value {
    serde_json::to_value(event).expect("serialize AgentEvent")
}

/// Assert a JSON value has a "type" field with the expected value.
fn assert_event_type(value: &Value, expected: &str) {
    let actual = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("<missing>");
    assert_eq!(actual, expected, "event type mismatch");
}

/// Assert that a JSON object has a specific key.
fn assert_has_field(value: &Value, field: &str) {
    assert!(
        value.get(field).is_some(),
        "expected field '{field}' in {value}"
    );
}

/// Assert that a JSON field is a non-empty string.
fn assert_non_empty_string(value: &Value, field: &str) {
    let Some(s) = value.get(field).and_then(Value::as_str) else {
        assert_eq!(
            value.get(field).and_then(Value::as_str),
            Some("<non-empty string>"),
            "expected string field '{field}' in {value}"
        );
        return;
    };
    assert!(
        !s.is_empty(),
        "expected non-empty string for '{field}', got empty"
    );
}

fn test_assistant_message() -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text(TextContent::new("hello"))],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-20250514".to_string(),
        usage: Usage {
            total_tokens: 50,
            input: 20,
            output: 30,
            ..Usage::default()
        },
        stop_reason: StopReason::Stop,
        stop_details: None,
        error_message: None,
        timestamp: 1_700_000_000,
    }
}

fn test_user_message() -> Message {
    Message::User(pi::model::UserMessage {
        content: pi::model::UserContent::Text("test prompt".to_string()),
        timestamp: 1_700_000_000,
    })
}

fn test_tool_output() -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new("tool output"))],
        details: None,
        is_error: false,
    }
}

// ============================================================================
// 1. AgentStart schema
// ============================================================================

#[test]
fn json_parity_agent_start_schema() {
    let harness = TestHarness::new("json_parity_agent_start_schema");
    let event = AgentEvent::AgentStart {
        session_id: "session-abc".to_string().into(),
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "agent_start");
    assert_non_empty_string(&json, "sessionId");
    assert_eq!(json["sessionId"], "session-abc");

    // Verify no snake_case version exists.
    assert!(
        json.get("session_id").is_none(),
        "should use camelCase 'sessionId', not 'session_id'"
    );

    harness
        .log()
        .info_ctx("json_parity", "agent_start schema ok", |ctx| {
            ctx.push(("type".to_string(), "agent_start".to_string()));
        });
}

// ============================================================================
// 2. AgentEnd schema
// ============================================================================

#[test]
fn json_parity_agent_end_schema() {
    let harness = TestHarness::new("json_parity_agent_end_schema");
    let event = AgentEvent::AgentEnd {
        session_id: "session-abc".to_string().into(),
        messages: vec![test_user_message()],
        error: None,
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "agent_end");
    assert_non_empty_string(&json, "sessionId");
    assert!(json["messages"].is_array(), "messages should be array");
    assert!(
        json.get("error").is_none() || json["error"].is_null(),
        "error should be absent or null when no error"
    );

    // With error
    let event_err = AgentEvent::AgentEnd {
        session_id: "s".to_string().into(),
        messages: vec![],
        error: Some("provider timeout".to_string()),
    };
    let json_err = event_to_json(&event_err);
    assert_eq!(json_err["error"], "provider timeout");

    harness
        .log()
        .info_ctx("json_parity", "agent_end schema ok", |ctx| {
            ctx.push(("messages_count".to_string(), "1".to_string()));
        });
}

// ============================================================================
// 3. TurnStart schema
// ============================================================================

#[test]
fn json_parity_turn_start_schema() {
    let harness = TestHarness::new("json_parity_turn_start_schema");
    let event = AgentEvent::TurnStart {
        session_id: "session-abc".to_string().into(),
        turn_index: 0,
        timestamp: 1_700_000_000,
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "turn_start");
    assert_non_empty_string(&json, "sessionId");
    assert_has_field(&json, "turnIndex");
    assert_has_field(&json, "timestamp");

    assert_eq!(json["turnIndex"], 0);
    assert!(json["timestamp"].is_i64(), "timestamp should be i64");

    // Verify camelCase
    assert!(json.get("turn_index").is_none());
    assert!(json.get("session_id").is_none());

    harness
        .log()
        .info_ctx("json_parity", "turn_start schema ok", |ctx| {
            ctx.push(("turnIndex".to_string(), "0".to_string()));
        });
}

// ============================================================================
// 4. TurnEnd schema
// ============================================================================

#[test]
fn json_parity_turn_end_schema() {
    let harness = TestHarness::new("json_parity_turn_end_schema");
    let assistant_msg = Message::Assistant(Arc::new(test_assistant_message()));
    let event = AgentEvent::TurnEnd {
        session_id: "session-abc".to_string().into(),
        turn_index: 0,
        message: assistant_msg,
        tool_results: vec![],
        latency_breakdown: None,
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "turn_end");
    assert_non_empty_string(&json, "sessionId");
    assert_has_field(&json, "turnIndex");
    assert_has_field(&json, "message");
    assert_has_field(&json, "toolResults");
    assert!(json["toolResults"].is_array());

    // Verify camelCase
    assert!(json.get("tool_results").is_none());

    harness
        .log()
        .info_ctx("json_parity", "turn_end schema ok", |ctx| {
            ctx.push(("turnIndex".to_string(), "0".to_string()));
        });
}

#[test]
fn json_parity_turn_end_latency_breakdown_schema() {
    let assistant_msg = Message::Assistant(Arc::new(test_assistant_message()));
    let event = AgentEvent::TurnEnd {
        session_id: "session-latency".to_string().into(),
        turn_index: 7,
        message: assistant_msg,
        tool_results: vec![],
        latency_breakdown: Some(Box::new(TurnLatencyBreakdown::from_component_samples(
            160,
            &[90],
            &[40, 5],
            &[10],
            &[15],
        ))),
    };
    let json = event_to_json(&event);
    let breakdown = json
        .get("latencyBreakdown")
        .expect("turn_end should expose latency breakdown");

    assert_eq!(breakdown["schema"], TURN_LATENCY_BREAKDOWN_SCHEMA_V1);
    assert_eq!(breakdown["totalMs"], 160);
    assert_eq!(breakdown["providerStreaming"]["durationMs"], 90);
    assert_eq!(
        breakdown["providerStreaming"]["tailPercentiles"]["p99Ms"],
        90
    );
    assert_eq!(
        breakdown["providerStreaming"]["tailPercentiles"]["p999Ms"],
        90
    );
    assert_eq!(breakdown["localTools"]["durationMs"], 45);
    assert_eq!(breakdown["extensionHostcalls"]["samples"], 1);
    assert_eq!(breakdown["persistence"]["durationMs"], 15);
    assert_eq!(breakdown["dominantComponent"], "provider_streaming");

    let serialized = serde_json::to_string(&json).expect("serialize event json");
    assert!(!serialized.contains("api_key"));
    assert!(!serialized.contains("authorization"));
    assert!(!serialized.contains("sk-"));
}

// ============================================================================
// 5. MessageStart schema
// ============================================================================

#[test]
fn json_parity_message_start_schema() {
    let harness = TestHarness::new("json_parity_message_start_schema");
    let event = AgentEvent::MessageStart {
        message: test_user_message(),
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "message_start");
    assert_has_field(&json, "message");
    assert!(json["message"].is_object());

    harness
        .log()
        .info_ctx("json_parity", "message_start schema ok", |ctx| {
            ctx.push(("has_message".to_string(), "true".to_string()));
        });
}

// ============================================================================
// 6. MessageUpdate schema
// ============================================================================

#[test]
fn json_parity_message_update_schema() {
    let harness = TestHarness::new("json_parity_message_update_schema");
    let partial = Arc::new(test_assistant_message());
    let event = AgentEvent::MessageUpdate {
        message: Message::Assistant(Arc::clone(&partial)),
        assistant_message_event: AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "hello".to_string(),
            partial,
        },
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "message_update");
    assert_has_field(&json, "message");
    assert_has_field(&json, "assistantMessageEvent");

    // Verify camelCase
    assert!(json.get("assistant_message_event").is_none());

    // Verify nested event has correct type tag
    let ame = &json["assistantMessageEvent"];
    assert_eq!(ame["type"], "text_delta");
    assert_has_field(ame, "contentIndex");
    assert_has_field(ame, "delta");

    harness
        .log()
        .info_ctx("json_parity", "message_update schema ok", |ctx| {
            ctx.push(("ame_type".to_string(), "text_delta".to_string()));
        });
}

// ============================================================================
// 7. MessageEnd schema
// ============================================================================

#[test]
fn json_parity_message_end_schema() {
    let harness = TestHarness::new("json_parity_message_end_schema");
    let event = AgentEvent::MessageEnd {
        message: Message::Assistant(Arc::new(test_assistant_message())),
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "message_end");
    assert_has_field(&json, "message");

    harness
        .log()
        .info_ctx("json_parity", "message_end schema ok", |ctx| {
            ctx.push(("has_message".to_string(), "true".to_string()));
        });
}

// ============================================================================
// 8. ToolExecutionStart schema
// ============================================================================

#[test]
fn json_parity_tool_execution_start_schema() {
    let harness = TestHarness::new("json_parity_tool_execution_start_schema");
    let event = AgentEvent::ToolExecutionStart {
        tool_call_id: "tc-1".to_string(),
        tool_name: "read".to_string(),
        args: json!({"path": "/tmp/test.txt"}),
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "tool_execution_start");
    assert_non_empty_string(&json, "toolCallId");
    assert_non_empty_string(&json, "toolName");
    assert_has_field(&json, "args");

    assert_eq!(json["toolCallId"], "tc-1");
    assert_eq!(json["toolName"], "read");

    // Verify camelCase
    assert!(json.get("tool_call_id").is_none());
    assert!(json.get("tool_name").is_none());

    harness
        .log()
        .info_ctx("json_parity", "tool_execution_start schema ok", |ctx| {
            ctx.push(("tool".to_string(), "read".to_string()));
        });
}

// ============================================================================
// 9. ToolExecutionUpdate schema
// ============================================================================

#[test]
fn json_parity_tool_execution_update_schema() {
    let harness = TestHarness::new("json_parity_tool_execution_update_schema");
    let event = AgentEvent::ToolExecutionUpdate {
        tool_call_id: "tc-1".to_string(),
        tool_name: "bash".to_string(),
        args: json!({"command": "ls"}),
        partial_result: test_tool_output(),
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "tool_execution_update");
    assert_non_empty_string(&json, "toolCallId");
    assert_non_empty_string(&json, "toolName");
    assert_has_field(&json, "args");
    assert_has_field(&json, "partialResult");

    // Verify camelCase
    assert!(json.get("partial_result").is_none());

    harness
        .log()
        .info_ctx("json_parity", "tool_execution_update schema ok", |ctx| {
            ctx.push(("tool".to_string(), "bash".to_string()));
        });
}

// ============================================================================
// 10. ToolExecutionEnd schema
// ============================================================================

#[test]
fn json_parity_tool_execution_end_schema() {
    let harness = TestHarness::new("json_parity_tool_execution_end_schema");
    let event = AgentEvent::ToolExecutionEnd {
        tool_call_id: "tc-1".to_string(),
        tool_name: "read".to_string(),
        result: test_tool_output(),
        is_error: false,
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "tool_execution_end");
    assert_non_empty_string(&json, "toolCallId");
    assert_non_empty_string(&json, "toolName");
    assert_has_field(&json, "result");
    assert_has_field(&json, "isError");
    assert_eq!(json["isError"], false);

    // Verify camelCase
    assert!(json.get("is_error").is_none());

    // With error
    let event_err = AgentEvent::ToolExecutionEnd {
        tool_call_id: "tc-2".to_string(),
        tool_name: "bash".to_string(),
        result: ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("error msg"))],
            details: None,
            is_error: true,
        },
        is_error: true,
    };
    let json_err = event_to_json(&event_err);
    assert_eq!(json_err["isError"], true);

    harness
        .log()
        .info_ctx("json_parity", "tool_execution_end schema ok", |ctx| {
            ctx.push(("error_case".to_string(), "true".to_string()));
        });
}

// ============================================================================
// 11. AutoCompactionStart schema
// ============================================================================

#[test]
fn json_parity_auto_compaction_start_schema() {
    let harness = TestHarness::new("json_parity_auto_compaction_start_schema");
    let event = AgentEvent::AutoCompactionStart {
        reason: "context window exceeded".to_string(),
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "auto_compaction_start");
    assert_non_empty_string(&json, "reason");

    harness
        .log()
        .info_ctx("json_parity", "auto_compaction_start schema ok", |ctx| {
            ctx.push(("reason".to_string(), "context window exceeded".to_string()));
        });
}

// ============================================================================
// 12. AutoCompactionEnd schema
// ============================================================================

#[test]
fn json_parity_auto_compaction_end_schema() {
    let harness = TestHarness::new("json_parity_auto_compaction_end_schema");

    // Success case
    let event = AgentEvent::AutoCompactionEnd {
        result: Some(json!({"summary": "compacted 10 messages"})),
        aborted: false,
        will_retry: false,
        error_message: None,
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "auto_compaction_end");
    assert_has_field(&json, "aborted");
    assert_has_field(&json, "willRetry");
    assert_eq!(json["aborted"], false);
    assert_eq!(json["willRetry"], false);

    // Verify camelCase
    assert!(json.get("will_retry").is_none());
    assert!(json.get("error_message").is_none());

    // Error case with retry
    let event_err = AgentEvent::AutoCompactionEnd {
        result: None,
        aborted: false,
        will_retry: true,
        error_message: Some("provider error".to_string()),
    };
    let json_err = event_to_json(&event_err);
    assert_eq!(json_err["willRetry"], true);
    assert_eq!(json_err["errorMessage"], "provider error");
    assert!(
        json_err.get("result").is_none(),
        "failed auto_compaction_end must omit absent result"
    );

    harness
        .log()
        .info_ctx("json_parity", "auto_compaction_end schema ok", |ctx| {
            ctx.push(("variants_tested".to_string(), "2".to_string()));
        });
}

// ============================================================================
// 13. AutoRetryStart schema
// ============================================================================

#[test]
fn json_parity_auto_retry_start_schema() {
    let harness = TestHarness::new("json_parity_auto_retry_start_schema");
    let event = AgentEvent::AutoRetryStart {
        attempt: 1,
        max_attempts: 3,
        delay_ms: 1000,
        error_message: "rate limited".to_string(),
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "auto_retry_start");
    assert_has_field(&json, "attempt");
    assert_has_field(&json, "maxAttempts");
    assert_has_field(&json, "delayMs");
    assert_has_field(&json, "errorMessage");
    assert_eq!(json["attempt"], 1);
    assert_eq!(json["maxAttempts"], 3);
    assert_eq!(json["delayMs"], 1000);
    assert_eq!(json["errorMessage"], "rate limited");

    // Verify camelCase
    assert!(json.get("max_attempts").is_none());
    assert!(json.get("delay_ms").is_none());
    assert!(json.get("error_message").is_none());

    harness
        .log()
        .info_ctx("json_parity", "auto_retry_start schema ok", |ctx| {
            ctx.push(("attempt".to_string(), "1".to_string()));
        });
}

// ============================================================================
// 14. AutoRetryEnd schema
// ============================================================================

#[test]
fn json_parity_auto_retry_end_schema() {
    let harness = TestHarness::new("json_parity_auto_retry_end_schema");

    // Success case
    let event = AgentEvent::AutoRetryEnd {
        success: true,
        attempt: 2,
        final_error: None,
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "auto_retry_end");
    assert_has_field(&json, "success");
    assert_has_field(&json, "attempt");
    assert_eq!(json["success"], true);
    assert_eq!(json["attempt"], 2);

    // Verify camelCase
    assert!(json.get("final_error").is_none());
    assert!(
        json.get("finalError").is_none(),
        "successful auto_retry_end must omit absent finalError"
    );

    // Failure case
    let event_fail = AgentEvent::AutoRetryEnd {
        success: false,
        attempt: 3,
        final_error: Some("max retries exceeded".to_string()),
    };
    let json_fail = event_to_json(&event_fail);
    assert_eq!(json_fail["success"], false);
    assert_eq!(json_fail["finalError"], "max retries exceeded");

    harness
        .log()
        .info_ctx("json_parity", "auto_retry_end schema ok", |ctx| {
            ctx.push(("variants_tested".to_string(), "2".to_string()));
        });
}

// ============================================================================
// 15. ExtensionError schema
// ============================================================================

#[test]
fn json_parity_extension_error_schema() {
    let harness = TestHarness::new("json_parity_extension_error_schema");
    let event = AgentEvent::ExtensionError {
        extension_id: Some("ext-foo".to_string()),
        event: "on_tool_start".to_string(),
        error: "TypeError: undefined is not a function".to_string(),
    };
    let json = event_to_json(&event);

    assert_event_type(&json, "extension_error");
    assert_has_field(&json, "extensionId");
    assert_non_empty_string(&json, "event");
    assert_non_empty_string(&json, "error");
    assert_eq!(json["extensionId"], "ext-foo");

    // Verify camelCase
    assert!(json.get("extension_id").is_none());

    // Without extension_id
    let event_no_id = AgentEvent::ExtensionError {
        extension_id: None,
        event: "lifecycle".to_string(),
        error: "load failed".to_string(),
    };
    let json_no_id = event_to_json(&event_no_id);
    assert!(
        json_no_id.get("extensionId").is_none() || json_no_id["extensionId"].is_null(),
        "extensionId should be absent or null when None"
    );

    harness
        .log()
        .info_ctx("json_parity", "extension_error schema ok", |ctx| {
            ctx.push(("with_id".to_string(), "true".to_string()));
            ctx.push(("without_id".to_string(), "true".to_string()));
        });
}

// ============================================================================
// 16. Complete lifecycle ordering
// ============================================================================

#[test]
fn json_parity_complete_lifecycle_ordering() {
    let harness = TestHarness::new("json_parity_complete_lifecycle_ordering");

    // Simulate a full agent run lifecycle and verify ordering.
    let session_id = "session-lifecycle-test";
    let partial = Arc::new(test_assistant_message());

    let events: Vec<AgentEvent> = vec![
        AgentEvent::AgentStart {
            session_id: session_id.to_string().into(),
        },
        AgentEvent::MessageStart {
            message: test_user_message(),
        },
        AgentEvent::MessageEnd {
            message: test_user_message(),
        },
        AgentEvent::TurnStart {
            session_id: session_id.to_string().into(),
            turn_index: 0,
            timestamp: 1_700_000_000,
        },
        AgentEvent::MessageStart {
            message: Message::Assistant(Arc::clone(&partial)),
        },
        AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "hello".to_string(),
                partial: Arc::clone(&partial),
            },
        },
        AgentEvent::MessageEnd {
            message: Message::Assistant(Arc::clone(&partial)),
        },
        AgentEvent::TurnEnd {
            session_id: session_id.to_string().into(),
            turn_index: 0,
            message: Message::Assistant(Arc::clone(&partial)),
            tool_results: vec![],
            latency_breakdown: None,
        },
        AgentEvent::AgentEnd {
            session_id: session_id.to_string().into(),
            messages: vec![test_user_message(), Message::Assistant(partial)],
            error: None,
        },
    ];

    let json_lines: Vec<Value> = events.iter().map(event_to_json).collect();

    let expected_order = [
        "agent_start",
        "message_start", // user
        "message_end",   // user
        "turn_start",
        "message_start", // assistant
        "message_update",
        "message_end", // assistant
        "turn_end",
        "agent_end",
    ];

    for (i, expected_type) in expected_order.iter().enumerate() {
        let actual_type = json_lines[i]["type"].as_str().unwrap_or("<missing>");
        assert_eq!(
            actual_type, *expected_type,
            "event at index {i}: expected '{expected_type}', got '{actual_type}'"
        );
    }

    // Verify sessionId consistency across events that have it.
    for line in &json_lines {
        if let Some(sid) = line.get("sessionId").and_then(Value::as_str) {
            assert_eq!(
                sid, session_id,
                "sessionId should be consistent across events"
            );
        }
    }

    harness
        .log()
        .info_ctx("json_parity", "lifecycle ordering ok", |ctx| {
            ctx.push(("events".to_string(), json_lines.len().to_string()));
            ctx.push(("order".to_string(), expected_order.join(",")));
        });
}

// ============================================================================
// 17. AssistantMessageEvent sub-types
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn json_parity_assistant_message_event_all_subtypes() {
    let harness = TestHarness::new("json_parity_assistant_message_event_all_subtypes");
    let partial = Arc::new(test_assistant_message());

    // Each AssistantMessageEvent variant serialized through MessageUpdate
    let variants: Vec<(&str, AssistantMessageEvent)> = vec![
        (
            "start",
            AssistantMessageEvent::Start {
                partial: Arc::clone(&partial),
            },
        ),
        (
            "text_start",
            AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: Arc::clone(&partial),
            },
        ),
        (
            "text_delta",
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "hello".to_string(),
                partial: Arc::clone(&partial),
            },
        ),
        (
            "text_end",
            AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: "hello world".to_string(),
                partial: Arc::clone(&partial),
            },
        ),
        (
            "thinking_start",
            AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                partial: Arc::clone(&partial),
            },
        ),
        (
            "thinking_delta",
            AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "thinking...".to_string(),
                partial: Arc::clone(&partial),
            },
        ),
        (
            "thinking_end",
            AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: "full thought".to_string(),
                partial: Arc::clone(&partial),
            },
        ),
        (
            "toolcall_start",
            AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: Arc::clone(&partial),
            },
        ),
        (
            "toolcall_delta",
            AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"path\"".to_string(),
                partial: Arc::clone(&partial),
            },
        ),
        (
            "toolcall_end",
            AssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call: ToolCall {
                    id: "tc-1".to_string(),
                    name: "read".to_string(),
                    arguments: json!({"path": "/tmp"}),
                    thought_signature: None,
                },
                partial: Arc::clone(&partial),
            },
        ),
        (
            "done",
            AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: Arc::clone(&partial),
            },
        ),
        (
            "error",
            AssistantMessageEvent::Error {
                reason: StopReason::Stop,
                error: Arc::clone(&partial),
            },
        ),
    ];

    let mut tested = 0;
    for (expected_type, ame) in &variants {
        let event = AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: ame.clone(),
        };
        let json = event_to_json(&event);
        let ame_json = &json["assistantMessageEvent"];

        let actual_type = ame_json["type"].as_str().unwrap_or("<missing>");
        assert_eq!(
            actual_type, *expected_type,
            "AME variant type mismatch for {expected_type}"
        );
        tested += 1;
    }

    assert_eq!(tested, 12, "should test all 12 AME variants");

    harness
        .log()
        .info_ctx("json_parity", "AME subtypes ok", |ctx| {
            ctx.push(("variants_tested".to_string(), tested.to_string()));
        });
}

// ============================================================================
// 18. No snake_case leak check (comprehensive)
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn json_parity_no_snake_case_leak() {
    let harness = TestHarness::new("json_parity_no_snake_case_leak");
    let partial = Arc::new(test_assistant_message());

    // Test every event type for snake_case field leaks.
    let events: Vec<AgentEvent> = vec![
        AgentEvent::AgentStart {
            session_id: "s".to_string().into(),
        },
        AgentEvent::AgentEnd {
            session_id: "s".to_string().into(),
            messages: vec![],
            error: None,
        },
        AgentEvent::TurnStart {
            session_id: "s".to_string().into(),
            turn_index: 0,
            timestamp: 0,
        },
        AgentEvent::TurnEnd {
            session_id: "s".to_string().into(),
            turn_index: 0,
            message: test_user_message(),
            tool_results: vec![],
            latency_breakdown: None,
        },
        AgentEvent::MessageStart {
            message: test_user_message(),
        },
        AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "x".to_string(),
                partial: Arc::clone(&partial),
            },
        },
        AgentEvent::MessageEnd {
            message: test_user_message(),
        },
        AgentEvent::ToolExecutionStart {
            tool_call_id: "tc".to_string(),
            tool_name: "read".to_string(),
            args: json!({}),
        },
        AgentEvent::ToolExecutionUpdate {
            tool_call_id: "tc".to_string(),
            tool_name: "bash".to_string(),
            args: json!({}),
            partial_result: test_tool_output(),
        },
        AgentEvent::ToolExecutionEnd {
            tool_call_id: "tc".to_string(),
            tool_name: "read".to_string(),
            result: test_tool_output(),
            is_error: false,
        },
        AgentEvent::AutoCompactionStart {
            reason: "r".to_string(),
        },
        AgentEvent::AutoCompactionEnd {
            result: None,
            aborted: false,
            will_retry: false,
            error_message: Some("err".to_string()),
        },
        AgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 100,
            error_message: "e".to_string(),
        },
        AgentEvent::AutoRetryEnd {
            success: true,
            attempt: 1,
            final_error: None,
        },
        AgentEvent::ExtensionError {
            extension_id: Some("ext".to_string()),
            event: "e".to_string(),
            error: "err".to_string(),
        },
    ];

    // Known snake_case fields that should NOT appear.
    let banned_snake_case = [
        "session_id",
        "turn_index",
        "tool_results",
        "tool_call_id",
        "tool_name",
        "is_error",
        "partial_result",
        "assistant_message_event",
        "max_attempts",
        "delay_ms",
        "error_message",
        "will_retry",
        "final_error",
        "extension_id",
        "content_index",
    ];

    for event in &events {
        let json = event_to_json(event);
        let json_string = serde_json::to_string(&json).expect("to_string");

        for banned in &banned_snake_case {
            // Check if the banned key appears as a JSON key ("key":)
            let key_pattern = format!("\"{banned}\":");
            assert!(
                !json_string.contains(&key_pattern),
                "found banned snake_case field '{banned}' in event {:?}: {json_string}",
                json["type"]
            );
        }
    }

    harness
        .log()
        .info_ctx("json_parity", "no snake_case leak ok", |ctx| {
            ctx.push(("events_checked".to_string(), events.len().to_string()));
            ctx.push((
                "banned_fields".to_string(),
                banned_snake_case.len().to_string(),
            ));
        });
}

// ============================================================================
// 19. SessionHeader schema validation
// ============================================================================

#[test]
fn json_parity_session_header_schema() {
    let harness = TestHarness::new("json_parity_session_header_schema");

    let header = pi::session::SessionHeader::new();
    let json = serde_json::to_value(&header).expect("serialize header");

    assert_eq!(json["type"], "session");
    assert!(
        json["id"].as_str().is_some_and(|s| !s.is_empty()),
        "id should be non-empty string"
    );
    assert!(
        json["timestamp"].as_str().is_some_and(|s| !s.is_empty()),
        "timestamp should be non-empty string"
    );
    assert!(
        json["cwd"].as_str().is_some_and(|s| !s.is_empty()),
        "cwd should be non-empty string"
    );

    // Verify camelCase naming
    assert!(
        json.get("parent_session").is_none(),
        "should use camelCase 'branchedFrom' not 'parent_session'"
    );

    harness
        .log()
        .info_ctx("json_parity", "session header schema ok", |ctx| {
            ctx.push(("type".to_string(), "session".to_string()));
            ctx.push(("has_id".to_string(), "true".to_string()));
        });
}

// ============================================================================
// 20. Event type string stability
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn json_parity_all_event_type_strings() {
    let harness = TestHarness::new("json_parity_all_event_type_strings");
    let partial = Arc::new(test_assistant_message());

    // Map of (variant, expected_type_string)
    let cases: Vec<(AgentEvent, &str)> = vec![
        (
            AgentEvent::AgentStart {
                session_id: "s".to_string().into(),
            },
            "agent_start",
        ),
        (
            AgentEvent::AgentEnd {
                session_id: "s".to_string().into(),
                messages: vec![],
                error: None,
            },
            "agent_end",
        ),
        (
            AgentEvent::TurnStart {
                session_id: "s".to_string().into(),
                turn_index: 0,
                timestamp: 0,
            },
            "turn_start",
        ),
        (
            AgentEvent::TurnEnd {
                session_id: "s".to_string().into(),
                turn_index: 0,
                message: test_user_message(),
                tool_results: vec![],
                latency_breakdown: None,
            },
            "turn_end",
        ),
        (
            AgentEvent::MessageStart {
                message: test_user_message(),
            },
            "message_start",
        ),
        (
            AgentEvent::MessageUpdate {
                message: Message::Assistant(Arc::clone(&partial)),
                assistant_message_event: AssistantMessageEvent::Start {
                    partial: Arc::clone(&partial),
                },
            },
            "message_update",
        ),
        (
            AgentEvent::MessageEnd {
                message: test_user_message(),
            },
            "message_end",
        ),
        (
            AgentEvent::ToolExecutionStart {
                tool_call_id: "t".to_string(),
                tool_name: "r".to_string(),
                args: json!({}),
            },
            "tool_execution_start",
        ),
        (
            AgentEvent::ToolExecutionUpdate {
                tool_call_id: "t".to_string(),
                tool_name: "r".to_string(),
                args: json!({}),
                partial_result: test_tool_output(),
            },
            "tool_execution_update",
        ),
        (
            AgentEvent::ToolExecutionEnd {
                tool_call_id: "t".to_string(),
                tool_name: "r".to_string(),
                result: test_tool_output(),
                is_error: false,
            },
            "tool_execution_end",
        ),
        (
            AgentEvent::AutoCompactionStart {
                reason: "r".to_string(),
            },
            "auto_compaction_start",
        ),
        (
            AgentEvent::AutoCompactionEnd {
                result: None,
                aborted: false,
                will_retry: false,
                error_message: None,
            },
            "auto_compaction_end",
        ),
        (
            AgentEvent::AutoRetryStart {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 0,
                error_message: "e".to_string(),
            },
            "auto_retry_start",
        ),
        (
            AgentEvent::AutoRetryEnd {
                success: true,
                attempt: 1,
                final_error: None,
            },
            "auto_retry_end",
        ),
        (
            AgentEvent::ExtensionError {
                extension_id: None,
                event: "e".to_string(),
                error: "err".to_string(),
            },
            "extension_error",
        ),
    ];

    for (event, expected_type) in &cases {
        let json = event_to_json(event);
        assert_event_type(&json, expected_type);
    }

    assert_eq!(cases.len(), 15, "should cover all 15 AgentEvent variants");

    harness
        .log()
        .info_ctx("json_parity", "all event type strings ok", |ctx| {
            ctx.push(("variants".to_string(), cases.len().to_string()));
        });
}

// ============================================================================
// Extension UI Request Parity (DROPIN-124: bd-359pl)
//
// Validates that `ExtensionUiRequest::to_rpc_event()` produces JSON events
// matching the TypeScript `RpcExtensionUIRequest` schema from pi-mono.
// Reference: legacy_pi_mono_code/.../rpc-types.ts lines 207-247.
// ============================================================================

// ---------------------------------------------------------------------------
// 1. Select method — dialog, expects response
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_select_schema() {
    let harness = TestHarness::new("json_parity_extension_ui_select_schema");
    let req = ExtensionUiRequest::new(
        "sel-1",
        "select",
        json!({"title": "Pick a tool", "options": ["read", "write", "bash"]}),
    );
    let event = req.to_rpc_event();

    assert_eq!(event["type"], "extension_ui_request");
    assert_eq!(event["id"], "sel-1");
    assert_eq!(event["method"], "select");
    assert_eq!(event["title"], "Pick a tool");
    assert_eq!(event["options"], json!(["read", "write", "bash"]));
    // Payload fields are flattened — no nested "payload" key.
    assert!(event.get("payload").is_none(), "payload must be flattened");

    harness
        .log()
        .info_ctx("json_parity", "extension_ui select schema ok", |ctx| {
            ctx.push(("method".to_string(), "select".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 2. Confirm method — dialog, expects response
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_confirm_schema() {
    let harness = TestHarness::new("json_parity_extension_ui_confirm_schema");
    let req = ExtensionUiRequest::new(
        "cfm-1",
        "confirm",
        json!({"title": "Allow exec?", "message": "Extension wants to run commands"}),
    );
    let event = req.to_rpc_event();

    assert_eq!(event["type"], "extension_ui_request");
    assert_eq!(event["id"], "cfm-1");
    assert_eq!(event["method"], "confirm");
    assert_eq!(event["title"], "Allow exec?");
    assert_eq!(event["message"], "Extension wants to run commands");

    harness
        .log()
        .info_ctx("json_parity", "extension_ui confirm schema ok", |ctx| {
            ctx.push(("method".to_string(), "confirm".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 3. Input method — dialog, expects response
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_input_schema() {
    let harness = TestHarness::new("json_parity_extension_ui_input_schema");
    let req = ExtensionUiRequest::new(
        "inp-1",
        "input",
        json!({"title": "API Key", "placeholder": "sk-ant-..."}),
    );
    let event = req.to_rpc_event();

    assert_eq!(event["type"], "extension_ui_request");
    assert_eq!(event["id"], "inp-1");
    assert_eq!(event["method"], "input");
    assert_eq!(event["title"], "API Key");
    assert_eq!(event["placeholder"], "sk-ant-...");

    harness
        .log()
        .info_ctx("json_parity", "extension_ui input schema ok", |ctx| {
            ctx.push(("method".to_string(), "input".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 4. Editor method — dialog, expects response
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_editor_schema() {
    let harness = TestHarness::new("json_parity_extension_ui_editor_schema");
    let req = ExtensionUiRequest::new(
        "edt-1",
        "editor",
        json!({"title": "Edit prompt", "prefill": "Hello, world!"}),
    );
    let event = req.to_rpc_event();

    assert_eq!(event["type"], "extension_ui_request");
    assert_eq!(event["id"], "edt-1");
    assert_eq!(event["method"], "editor");
    assert_eq!(event["title"], "Edit prompt");
    assert_eq!(event["prefill"], "Hello, world!");

    harness
        .log()
        .info_ctx("json_parity", "extension_ui editor schema ok", |ctx| {
            ctx.push(("method".to_string(), "editor".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 5. Notify method — fire-and-forget
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_notify_schema() {
    let harness = TestHarness::new("json_parity_extension_ui_notify_schema");
    let req = ExtensionUiRequest::new(
        "ntf-1",
        "notify",
        json!({"message": "Build complete", "notifyType": "info"}),
    );
    let event = req.to_rpc_event();

    assert_eq!(event["type"], "extension_ui_request");
    assert_eq!(event["id"], "ntf-1");
    assert_eq!(event["method"], "notify");
    assert_eq!(event["message"], "Build complete");
    assert_eq!(event["notifyType"], "info");

    // Also test warning and error notify types.
    let req_warn = ExtensionUiRequest::new(
        "ntf-2",
        "notify",
        json!({"message": "Deprecation", "notifyType": "warning"}),
    );
    assert_eq!(req_warn.to_rpc_event()["notifyType"], "warning");

    let req_err = ExtensionUiRequest::new(
        "ntf-3",
        "notify",
        json!({"message": "Failed", "notifyType": "error"}),
    );
    assert_eq!(req_err.to_rpc_event()["notifyType"], "error");

    harness
        .log()
        .info_ctx("json_parity", "extension_ui notify schema ok", |ctx| {
            ctx.push(("variants".to_string(), "3".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 6. setStatus method — fire-and-forget
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_set_status_schema() {
    let harness = TestHarness::new("json_parity_extension_ui_set_status_schema");
    let req = ExtensionUiRequest::new(
        "sts-1",
        "setStatus",
        json!({"statusKey": "build", "statusText": "compiling..."}),
    );
    let event = req.to_rpc_event();

    assert_eq!(event["type"], "extension_ui_request");
    assert_eq!(event["id"], "sts-1");
    assert_eq!(event["method"], "setStatus");
    assert_eq!(event["statusKey"], "build");
    assert_eq!(event["statusText"], "compiling...");

    // Test clearing status (statusText undefined → null in JSON).
    let req_clear = ExtensionUiRequest::new(
        "sts-2",
        "setStatus",
        json!({"statusKey": "build", "statusText": null}),
    );
    let event_clear = req_clear.to_rpc_event();
    assert_eq!(event_clear["statusKey"], "build");
    assert!(event_clear["statusText"].is_null());

    harness
        .log()
        .info_ctx("json_parity", "extension_ui setStatus schema ok", |ctx| {
            ctx.push(("method".to_string(), "setStatus".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 7. setWidget method — fire-and-forget
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_set_widget_schema() {
    let harness = TestHarness::new("json_parity_extension_ui_set_widget_schema");
    let req = ExtensionUiRequest::new(
        "wdg-1",
        "setWidget",
        json!({
            "widgetKey": "metrics",
            "widgetLines": ["CPU: 42%", "RAM: 8GB"],
            "widgetPlacement": "aboveEditor"
        }),
    );
    let event = req.to_rpc_event();

    assert_eq!(event["type"], "extension_ui_request");
    assert_eq!(event["id"], "wdg-1");
    assert_eq!(event["method"], "setWidget");
    assert_eq!(event["widgetKey"], "metrics");
    assert_eq!(event["widgetLines"], json!(["CPU: 42%", "RAM: 8GB"]));
    assert_eq!(event["widgetPlacement"], "aboveEditor");

    // Test clearing widget (widgetLines undefined → null).
    let req_clear = ExtensionUiRequest::new(
        "wdg-2",
        "setWidget",
        json!({"widgetKey": "metrics", "widgetLines": null}),
    );
    let event_clear = req_clear.to_rpc_event();
    assert_eq!(event_clear["widgetKey"], "metrics");
    assert!(event_clear["widgetLines"].is_null());

    harness
        .log()
        .info_ctx("json_parity", "extension_ui setWidget schema ok", |ctx| {
            ctx.push(("method".to_string(), "setWidget".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 8. setTitle method — fire-and-forget
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_set_title_schema() {
    let harness = TestHarness::new("json_parity_extension_ui_set_title_schema");
    let req = ExtensionUiRequest::new("ttl-1", "setTitle", json!({"title": "My Agent Session"}));
    let event = req.to_rpc_event();

    assert_eq!(event["type"], "extension_ui_request");
    assert_eq!(event["id"], "ttl-1");
    assert_eq!(event["method"], "setTitle");
    assert_eq!(event["title"], "My Agent Session");

    harness
        .log()
        .info_ctx("json_parity", "extension_ui setTitle schema ok", |ctx| {
            ctx.push(("method".to_string(), "setTitle".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 9. set_editor_text method — fire-and-forget
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_set_editor_text_schema() {
    let harness = TestHarness::new("json_parity_extension_ui_set_editor_text_schema");
    let req = ExtensionUiRequest::new(
        "set-1",
        "set_editor_text",
        json!({"text": "prefilled prompt text"}),
    );
    let event = req.to_rpc_event();

    assert_eq!(event["type"], "extension_ui_request");
    assert_eq!(event["id"], "set-1");
    assert_eq!(event["method"], "set_editor_text");
    assert_eq!(event["text"], "prefilled prompt text");

    harness.log().info_ctx(
        "json_parity",
        "extension_ui set_editor_text schema ok",
        |ctx| ctx.push(("method".to_string(), "set_editor_text".to_string())),
    );
}

// ---------------------------------------------------------------------------
// 10. Dialog vs fire-and-forget classification matches TypeScript
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_expects_response_classification() {
    let harness = TestHarness::new("json_parity_extension_ui_expects_response_classification");

    // Dialog methods (expects response = true).
    for method in &["select", "confirm", "input", "editor"] {
        let req = ExtensionUiRequest::new("x", *method, json!({}));
        assert!(
            req.expects_response(),
            "{method} should expect a response (dialog)"
        );
    }

    // Fire-and-forget methods (expects response = false).
    for method in &[
        "notify",
        "setStatus",
        "setWidget",
        "setTitle",
        "set_editor_text",
    ] {
        let req = ExtensionUiRequest::new("x", *method, json!({}));
        assert!(
            !req.expects_response(),
            "{method} should NOT expect a response (fire-and-forget)"
        );
    }

    harness.log().info_ctx(
        "json_parity",
        "extension_ui dialog classification ok",
        |ctx| ctx.push(("dialog_count".to_string(), "4".to_string())),
    );
}

// ---------------------------------------------------------------------------
// 11. All 9 methods produce extension_ui_request type tag
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_all_methods_type_tag() {
    let harness = TestHarness::new("json_parity_extension_ui_all_methods_type_tag");
    let methods = [
        "select",
        "confirm",
        "input",
        "editor",
        "notify",
        "setStatus",
        "setWidget",
        "setTitle",
        "set_editor_text",
    ];

    for method in &methods {
        let req = ExtensionUiRequest::new("id-1", *method, json!({}));
        let event = req.to_rpc_event();
        assert_eq!(
            event["type"], "extension_ui_request",
            "method {method} should produce extension_ui_request type"
        );
        assert_eq!(event["method"], *method);
    }

    harness
        .log()
        .info_ctx("json_parity", "all 9 extension_ui methods ok", |ctx| {
            ctx.push(("methods".to_string(), methods.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 12. Payload flattening — object payloads merge into top-level
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_payload_flattening() {
    let harness = TestHarness::new("json_parity_extension_ui_payload_flattening");

    // Object payload: fields should appear at top level.
    let req = ExtensionUiRequest::new(
        "r1",
        "select",
        json!({"title": "Pick", "options": ["A"], "custom_key": 42}),
    );
    let event = req.to_rpc_event();
    assert!(
        event.get("payload").is_none(),
        "object payload must be flattened"
    );
    assert_eq!(event["title"], "Pick");
    assert_eq!(event["custom_key"], 42);

    // Non-object payload: falls back to a "payload" key.
    let req_str = ExtensionUiRequest::new("r2", "notify", Value::String("raw".to_string()));
    let event_str = req_str.to_rpc_event();
    assert_eq!(event_str["payload"], "raw");

    harness
        .log()
        .info_ctx("json_parity", "extension_ui payload flattening ok", |_| {});
}

// ---------------------------------------------------------------------------
// 13. No snake_case leaks in extension UI events
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_no_snake_case_leaks() {
    let harness = TestHarness::new("json_parity_extension_ui_no_snake_case_leaks");

    // Build events for all methods and verify no keys contain underscores
    // (except the event "type" value and method names which are allowed).
    let cases = vec![
        ExtensionUiRequest::new("r1", "select", json!({"title": "T", "options": ["A"]})),
        ExtensionUiRequest::new("r2", "confirm", json!({"title": "T", "message": "M"})),
        ExtensionUiRequest::new("r3", "input", json!({"title": "T", "placeholder": "P"})),
        ExtensionUiRequest::new("r4", "editor", json!({"title": "T", "prefill": "P"})),
        ExtensionUiRequest::new(
            "r5",
            "notify",
            json!({"message": "M", "notifyType": "info"}),
        ),
        ExtensionUiRequest::new(
            "r6",
            "setStatus",
            json!({"statusKey": "K", "statusText": "V"}),
        ),
        ExtensionUiRequest::new(
            "r7",
            "setWidget",
            json!({"widgetKey": "K", "widgetLines": ["L"], "widgetPlacement": "aboveEditor"}),
        ),
        ExtensionUiRequest::new("r8", "setTitle", json!({"title": "T"})),
        ExtensionUiRequest::new("r9", "set_editor_text", json!({"text": "T"})),
    ];

    // Known keys that intentionally use snake_case (matching TS reference exactly).
    let allowed_snake_keys = ["set_editor_text"]; // method value, not a key

    for req in &cases {
        let event = req.to_rpc_event();
        if let Value::Object(map) = &event {
            for key in map.keys() {
                // The "type" key is a standard JSON discriminator.
                if key == "type" {
                    continue;
                }
                // Keys themselves should use camelCase (like TypeScript).
                // Exception: keys from the payload that match TS reference exactly.
                let has_underscore = key.contains('_');
                if has_underscore {
                    // Only allow keys that are intentional in the TS schema.
                    let allowed = allowed_snake_keys.contains(&key.as_str());
                    assert!(
                        allowed,
                        "unexpected snake_case key '{key}' in {method} event",
                        method = req.method
                    );
                }
            }
        }
    }

    harness.log().info_ctx(
        "json_parity",
        "no snake_case leaks in extension_ui events",
        |_| {},
    );
}

// ---------------------------------------------------------------------------
// 14. Extension ID provenance — when set, included in event
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_extension_id_provenance() {
    let harness = TestHarness::new("json_parity_extension_ui_extension_id_provenance");

    // The extension_id is part of the struct but NOT part of `to_rpc_event()` output
    // because it's tracked on the Rust side for provenance, not emitted to RPC clients.
    // The payload may carry an extension_id field if the extension puts it there.
    let req = ExtensionUiRequest::new(
        "r1",
        "confirm",
        json!({"title": "T", "message": "M", "extension_id": "my-ext"}),
    )
    .with_extension_id(Some("my-ext".to_string()));

    let event = req.to_rpc_event();
    // Payload-provided extension_id should be flattened.
    assert_eq!(event["extension_id"], "my-ext");

    harness.log().info_ctx(
        "json_parity",
        "extension_ui extension_id provenance ok",
        |_| {},
    );
}

// ---------------------------------------------------------------------------
// 15. Timeout field in dialog requests
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_timeout_field() {
    let harness = TestHarness::new("json_parity_extension_ui_timeout_field");

    // TypeScript schema: `timeout?: number` on select/confirm/input methods.
    let mut req = ExtensionUiRequest::new(
        "r1",
        "select",
        json!({"title": "Pick", "options": ["A"], "timeout": 5000}),
    );
    let event = req.to_rpc_event();
    assert_eq!(event["timeout"], 5000);

    // effective_timeout_ms reads from payload.
    assert_eq!(req.effective_timeout_ms(), Some(5000));

    // Explicit timeout_ms on struct takes precedence.
    req.timeout_ms = Some(3000);
    assert_eq!(req.effective_timeout_ms(), Some(3000));

    harness
        .log()
        .info_ctx("json_parity", "extension_ui timeout field ok", |_| {});
}

// ---------------------------------------------------------------------------
// 16. Select with label/value objects (rich options)
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_select_rich_options() {
    let harness = TestHarness::new("json_parity_extension_ui_select_rich_options");
    let req = ExtensionUiRequest::new(
        "sel-rich",
        "select",
        json!({
            "title": "Choose provider",
            "options": [
                {"label": "Anthropic", "value": "anthropic"},
                {"label": "OpenAI", "value": "openai"},
            ]
        }),
    );
    let event = req.to_rpc_event();

    assert_eq!(event["method"], "select");
    let options = event["options"]
        .as_array()
        .expect("options should be array");
    assert_eq!(options.len(), 2);
    assert_eq!(options[0]["label"], "Anthropic");
    assert_eq!(options[0]["value"], "anthropic");

    harness
        .log()
        .info_ctx("json_parity", "extension_ui select rich options ok", |_| {});
}

// ---------------------------------------------------------------------------
// 17. Tool + Extension UI event ordering in complete lifecycle
// ---------------------------------------------------------------------------

#[test]
fn json_parity_complete_lifecycle_with_extension_ui() {
    let harness = TestHarness::new("json_parity_complete_lifecycle_with_extension_ui");

    // Validate that tool events and extension UI events can coexist in a
    // complete agent lifecycle without schema conflicts.
    let tool_start = event_to_json(&AgentEvent::ToolExecutionStart {
        tool_call_id: "tc-1".to_string(),
        tool_name: "bash".to_string(),
        args: json!({"command": "echo hi"}),
    });
    let tool_end = event_to_json(&AgentEvent::ToolExecutionEnd {
        tool_call_id: "tc-1".to_string(),
        tool_name: "bash".to_string(),
        result: test_tool_output(),
        is_error: false,
    });
    let ui_req = ExtensionUiRequest::new(
        "ui-1",
        "confirm",
        json!({"title": "Proceed?", "message": "Continue with deployment?"}),
    )
    .to_rpc_event();

    // All three should have distinct type tags.
    assert_eq!(tool_start["type"], "tool_execution_start");
    assert_eq!(tool_end["type"], "tool_execution_end");
    assert_eq!(ui_req["type"], "extension_ui_request");

    // No field name collisions between event types.
    // Tool events use toolCallId/toolName, UI events use id/method.
    assert!(tool_start.get("id").is_none());
    assert!(ui_req.get("toolCallId").is_none());
    assert!(ui_req.get("toolName").is_none());

    harness
        .log()
        .info_ctx("json_parity", "tool + extension_ui lifecycle ok", |_| {});
}

// ---------------------------------------------------------------------------
// 18. extension_event_from_agent mapping correctness (DROPIN-124)
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn json_parity_extension_event_from_agent_mapping() {
    let harness = TestHarness::new("json_parity_extension_event_from_agent_mapping");
    let partial = Arc::new(test_assistant_message());

    // Events that SHOULD be forwarded to extensions (10 of 15).
    let forwarded: Vec<(AgentEvent, ExtensionEventName)> = vec![
        (
            AgentEvent::AgentStart {
                session_id: "s".to_string().into(),
            },
            ExtensionEventName::AgentStart,
        ),
        (
            AgentEvent::AgentEnd {
                session_id: "s".to_string().into(),
                messages: vec![],
                error: None,
            },
            ExtensionEventName::AgentEnd,
        ),
        (
            AgentEvent::TurnStart {
                session_id: "s".to_string().into(),
                turn_index: 0,
                timestamp: 0,
            },
            ExtensionEventName::TurnStart,
        ),
        (
            AgentEvent::TurnEnd {
                session_id: "s".to_string().into(),
                turn_index: 0,
                message: test_user_message(),
                tool_results: vec![],
                latency_breakdown: None,
            },
            ExtensionEventName::TurnEnd,
        ),
        (
            AgentEvent::MessageStart {
                message: test_user_message(),
            },
            ExtensionEventName::MessageStart,
        ),
        (
            AgentEvent::MessageUpdate {
                message: Message::Assistant(Arc::clone(&partial)),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "x".to_string(),
                    partial: Arc::clone(&partial),
                },
            },
            ExtensionEventName::MessageUpdate,
        ),
        (
            AgentEvent::MessageEnd {
                message: test_user_message(),
            },
            ExtensionEventName::MessageEnd,
        ),
        (
            AgentEvent::ToolExecutionStart {
                tool_call_id: "tc".to_string(),
                tool_name: "read".to_string(),
                args: json!({}),
            },
            ExtensionEventName::ToolExecutionStart,
        ),
        (
            AgentEvent::ToolExecutionUpdate {
                tool_call_id: "tc".to_string(),
                tool_name: "bash".to_string(),
                args: json!({}),
                partial_result: test_tool_output(),
            },
            ExtensionEventName::ToolExecutionUpdate,
        ),
        (
            AgentEvent::ToolExecutionEnd {
                tool_call_id: "tc".to_string(),
                tool_name: "read".to_string(),
                result: test_tool_output(),
                is_error: false,
            },
            ExtensionEventName::ToolExecutionEnd,
        ),
    ];

    for (event, expected_name) in &forwarded {
        let result = extension_event_from_agent(event);
        assert!(
            result.is_some(),
            "event {:?} should be forwarded to extensions",
            event_to_json(event)["type"]
        );
        let (name, payload) = result.unwrap();
        assert_eq!(
            name,
            *expected_name,
            "extension event name mismatch for {:?}",
            event_to_json(event)["type"]
        );
        assert!(
            payload.is_some(),
            "forwarded event should include serialized payload"
        );
    }

    // Events that should NOT be forwarded to extensions (5 of 15).
    let excluded: Vec<AgentEvent> = vec![
        AgentEvent::AutoCompactionStart {
            reason: "r".to_string(),
        },
        AgentEvent::AutoCompactionEnd {
            result: None,
            aborted: false,
            will_retry: false,
            error_message: None,
        },
        AgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 0,
            error_message: "e".to_string(),
        },
        AgentEvent::AutoRetryEnd {
            success: true,
            attempt: 1,
            final_error: None,
        },
        AgentEvent::ExtensionError {
            extension_id: None,
            event: "e".to_string(),
            error: "err".to_string(),
        },
    ];

    for event in &excluded {
        let result = extension_event_from_agent(event);
        assert!(
            result.is_none(),
            "event {:?} should NOT be forwarded to extensions",
            event_to_json(event)["type"]
        );
    }

    assert_eq!(
        forwarded.len() + excluded.len(),
        15,
        "should cover all 15 AgentEvent variants"
    );

    harness.log().info_ctx(
        "json_parity",
        "extension_event_from_agent mapping ok",
        |ctx| {
            ctx.push(("forwarded".to_string(), forwarded.len().to_string()));
            ctx.push(("excluded".to_string(), excluded.len().to_string()));
        },
    );
}

// ---------------------------------------------------------------------------
// 19. Extension event payload preserves camelCase fields
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_event_payload_camel_case() {
    let harness = TestHarness::new("json_parity_extension_event_payload_camel_case");

    // Verify the serialized payload passed to extensions uses camelCase.
    let event = AgentEvent::ToolExecutionStart {
        tool_call_id: "tc-1".to_string(),
        tool_name: "bash".to_string(),
        args: json!({"command": "ls"}),
    };

    let (_, payload) = extension_event_from_agent(&event).expect("should be forwarded");
    let payload = payload.expect("should have payload");

    // The payload should use camelCase field names.
    assert_eq!(payload["toolCallId"], "tc-1");
    assert_eq!(payload["toolName"], "bash");
    assert!(
        payload.get("tool_call_id").is_none(),
        "extension payload should use camelCase"
    );
    assert!(
        payload.get("tool_name").is_none(),
        "extension payload should use camelCase"
    );

    harness.log().info_ctx(
        "json_parity",
        "extension event payload camelCase ok",
        |_| {},
    );
}

// ---------------------------------------------------------------------------
// 20. Tool execution event with extension tool (non-builtin)
// ---------------------------------------------------------------------------

#[test]
fn json_parity_tool_execution_extension_tool() {
    let harness = TestHarness::new("json_parity_tool_execution_extension_tool");

    // Extension tools have names prefixed with the extension ID.
    let event = AgentEvent::ToolExecutionStart {
        tool_call_id: "tc-ext-1".to_string(),
        tool_name: "my-extension__custom_tool".to_string(),
        args: json!({"input": "test data", "mode": "fast"}),
    };
    let json = event_to_json(&event);

    assert_eq!(json["type"], "tool_execution_start");
    assert_eq!(json["toolName"], "my-extension__custom_tool");
    assert_eq!(json["args"]["input"], "test data");

    // Extension tool end with error.
    let event_end = AgentEvent::ToolExecutionEnd {
        tool_call_id: "tc-ext-1".to_string(),
        tool_name: "my-extension__custom_tool".to_string(),
        result: ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("extension error"))],
            details: None,
            is_error: true,
        },
        is_error: true,
    };
    let json_end = event_to_json(&event_end);
    assert_eq!(json_end["isError"], true);
    assert_eq!(json_end["toolName"], "my-extension__custom_tool");

    // Should still be forwarded to extensions.
    let (name, _) = extension_event_from_agent(&event).expect("forwarded");
    assert_eq!(name, ExtensionEventName::ToolExecutionStart);

    harness
        .log()
        .info_ctx("json_parity", "extension tool execution events ok", |_| {});
}

// ============================================================================
// DROPIN-124: Tool Lifecycle Event Payload Detail Tests (bd-359pl)
// ============================================================================

// ---------------------------------------------------------------------------
// 21. Tool error events: isError at both top-level and result level
// ---------------------------------------------------------------------------

#[test]
fn json_parity_tool_error_consistency() {
    let harness = TestHarness::new("json_parity_tool_error_consistency");
    let event = AgentEvent::ToolExecutionEnd {
        tool_call_id: "tc-err".to_string(),
        tool_name: "bash".to_string(),
        result: ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("command not found"))],
            details: Some(json!({"exitCode": 127})),
            is_error: true,
        },
        is_error: true,
    };
    let json = event_to_json(&event);
    assert_eq!(json["isError"], true, "top-level isError");
    assert_eq!(json["result"]["isError"], true, "result.isError");

    harness
        .log()
        .info_ctx("json_parity", "tool error consistency ok", |_| {});
}

// ---------------------------------------------------------------------------
// 22. All 8 built-in tool names serialize correctly
// ---------------------------------------------------------------------------

#[test]
fn json_parity_all_builtin_tool_names() {
    let harness = TestHarness::new("json_parity_all_builtin_tool_names");
    for name in &[
        "read",
        "write",
        "edit",
        "bash",
        "grep",
        "find",
        "ls",
        "hashline_edit",
    ] {
        let event = AgentEvent::ToolExecutionStart {
            tool_call_id: format!("tc-{name}"),
            tool_name: (*name).to_string(),
            args: json!({}),
        };
        let json = event_to_json(&event);
        assert_eq!(json["toolName"].as_str(), Some(*name));
    }

    harness
        .log()
        .info_ctx("json_parity", "all 8 built-in tool names ok", |_| {});
}

// ---------------------------------------------------------------------------
// 23. Tool result with details=None omits details correctly
// ---------------------------------------------------------------------------

#[test]
fn json_parity_tool_result_no_details() {
    let harness = TestHarness::new("json_parity_tool_result_no_details");
    let event = AgentEvent::ToolExecutionEnd {
        tool_call_id: "tc-nd".to_string(),
        tool_name: "write".to_string(),
        result: ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("written"))],
            details: None,
            is_error: false,
        },
        is_error: false,
    };
    let json = event_to_json(&event);
    // Pi-mono: write tool has details: undefined → null in JSON.
    assert!(
        json["result"]["details"].is_null(),
        "details should be null when None"
    );

    harness
        .log()
        .info_ctx("json_parity", "tool result no details ok", |_| {});
}

// ---------------------------------------------------------------------------
// 24. ToolOutput content structure matches pi-mono TextContent
// ---------------------------------------------------------------------------

#[test]
fn json_parity_tool_output_content_structure() {
    let harness = TestHarness::new("json_parity_tool_output_content_structure");
    let output = ToolOutput {
        content: vec![
            ContentBlock::Text(TextContent::new("line 1")),
            ContentBlock::Text(TextContent::new("line 2")),
        ],
        details: None,
        is_error: false,
    };
    let json = serde_json::to_value(&output).expect("serialize");
    let content = json["content"].as_array().expect("content is array");
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "line 1");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["text"], "line 2");

    harness
        .log()
        .info_ctx("json_parity", "tool output content structure ok", |_| {});
}

// ---------------------------------------------------------------------------
// 25. Tool event args preserve arbitrary/complex JSON values
// ---------------------------------------------------------------------------

#[test]
fn json_parity_tool_args_preserve_arbitrary_json() {
    let harness = TestHarness::new("json_parity_tool_args_preserve_arbitrary_json");
    let complex_args = json!({
        "path": "/home/user/src/main.rs",
        "offset": 100,
        "limit": 50,
        "nested": {"a": [1, 2, 3]},
        "unicode": "\u{65E5}\u{672C}\u{8A9E}"
    });
    let event = AgentEvent::ToolExecutionStart {
        tool_call_id: "tc-cplx".to_string(),
        tool_name: "read".to_string(),
        args: complex_args.clone(),
    };
    let json = event_to_json(&event);
    assert_eq!(json["args"], complex_args);

    harness.log().info_ctx(
        "json_parity",
        "tool args preserve arbitrary JSON ok",
        |_| {},
    );
}

// ---------------------------------------------------------------------------
// 26. Extension event round-trip: direct serialize vs mapping
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_event_round_trip() {
    let harness = TestHarness::new("json_parity_extension_event_round_trip");
    let event = AgentEvent::ToolExecutionStart {
        tool_call_id: "tc-rt".to_string(),
        tool_name: "edit".to_string(),
        args: json!({"path": "/tmp/f.rs", "old_string": "foo", "new_string": "bar"}),
    };

    let direct = event_to_json(&event);
    let (_, payload) = extension_event_from_agent(&event).unwrap();
    let mapped = payload.unwrap();

    assert_eq!(direct["type"], mapped["type"]);
    assert_eq!(direct["toolCallId"], mapped["toolCallId"]);
    assert_eq!(direct["toolName"], mapped["toolName"]);
    assert_eq!(direct["args"], mapped["args"]);

    harness
        .log()
        .info_ctx("json_parity", "extension event round-trip ok", |_| {});
}

// ============================================================================
// DROPIN-124: Extension UI Response Parity Tests (bd-359pl)
// ============================================================================

// ---------------------------------------------------------------------------
// 27. ExtensionUiResponse: value variant (select)
// ---------------------------------------------------------------------------

#[test]
fn json_parity_ui_response_value_variant() {
    let harness = TestHarness::new("json_parity_ui_response_value_variant");
    let resp = pi::extensions::ExtensionUiResponse {
        id: "sel-1".to_string(),
        value: Some(json!("option_b")),
        cancelled: false,
    };
    assert_eq!(resp.id, "sel-1");
    assert_eq!(resp.value, Some(json!("option_b")));
    assert!(!resp.cancelled);

    harness
        .log()
        .info_ctx("json_parity", "ui response value variant ok", |_| {});
}

// ---------------------------------------------------------------------------
// 28. ExtensionUiResponse: confirmed variant
// ---------------------------------------------------------------------------

#[test]
fn json_parity_ui_response_confirmed_variant() {
    let harness = TestHarness::new("json_parity_ui_response_confirmed_variant");

    let confirmed = pi::extensions::ExtensionUiResponse {
        id: "cfm-1".to_string(),
        value: Some(json!(true)),
        cancelled: false,
    };
    assert_eq!(confirmed.value, Some(json!(true)));
    assert!(!confirmed.cancelled);

    let denied = pi::extensions::ExtensionUiResponse {
        id: "cfm-2".to_string(),
        value: Some(json!(false)),
        cancelled: false,
    };
    assert_eq!(denied.value, Some(json!(false)));

    harness
        .log()
        .info_ctx("json_parity", "ui response confirmed variant ok", |_| {});
}

// ---------------------------------------------------------------------------
// 29. ExtensionUiResponse: cancelled variant
// ---------------------------------------------------------------------------

#[test]
fn json_parity_ui_response_cancelled_variant() {
    let harness = TestHarness::new("json_parity_ui_response_cancelled_variant");
    let resp = pi::extensions::ExtensionUiResponse {
        id: "inp-1".to_string(),
        value: None,
        cancelled: true,
    };
    assert!(resp.cancelled);
    assert!(resp.value.is_none());

    harness
        .log()
        .info_ctx("json_parity", "ui response cancelled variant ok", |_| {});
}

// ---------------------------------------------------------------------------
// 30. ExtensionUiResponse: text input value
// ---------------------------------------------------------------------------

#[test]
fn json_parity_ui_response_text_value() {
    let harness = TestHarness::new("json_parity_ui_response_text_value");
    let input_resp = pi::extensions::ExtensionUiResponse {
        id: "inp-1".to_string(),
        value: Some(json!("user-typed-text")),
        cancelled: false,
    };
    assert_eq!(input_resp.value, Some(json!("user-typed-text")));

    let editor_resp = pi::extensions::ExtensionUiResponse {
        id: "edt-1".to_string(),
        value: Some(json!("edited prompt content")),
        cancelled: false,
    };
    assert_eq!(editor_resp.value, Some(json!("edited prompt content")));

    harness
        .log()
        .info_ctx("json_parity", "ui response text value ok", |_| {});
}

// ---------------------------------------------------------------------------
// 31. Extension event name Display matches pi-mono event hook strings
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_event_name_display() {
    let harness = TestHarness::new("json_parity_extension_event_name_display");
    let cases: Vec<(ExtensionEventName, &str)> = vec![
        (ExtensionEventName::AgentStart, "agent_start"),
        (ExtensionEventName::AgentEnd, "agent_end"),
        (ExtensionEventName::TurnStart, "turn_start"),
        (ExtensionEventName::TurnEnd, "turn_end"),
        (ExtensionEventName::MessageStart, "message_start"),
        (ExtensionEventName::MessageUpdate, "message_update"),
        (ExtensionEventName::MessageEnd, "message_end"),
        (
            ExtensionEventName::ToolExecutionStart,
            "tool_execution_start",
        ),
        (
            ExtensionEventName::ToolExecutionUpdate,
            "tool_execution_update",
        ),
        (ExtensionEventName::ToolExecutionEnd, "tool_execution_end"),
        (ExtensionEventName::ToolCall, "tool_call"),
        (ExtensionEventName::ToolResult, "tool_result"),
    ];
    for (name, expected) in &cases {
        assert_eq!(name.to_string(), *expected, "Display for {name:?}");
    }

    harness
        .log()
        .info_ctx("json_parity", "extension event name Display ok", |ctx| {
            ctx.push(("count".to_string(), cases.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 32. Tool lifecycle with tool details containing rich data
// ---------------------------------------------------------------------------

#[test]
fn json_parity_tool_details_rich_data() {
    let harness = TestHarness::new("json_parity_tool_details_rich_data");

    // Bash tool details include exitCode, timing, etc.
    let event = AgentEvent::ToolExecutionEnd {
        tool_call_id: "tc-bash-det".to_string(),
        tool_name: "bash".to_string(),
        result: ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("output"))],
            details: Some(json!({
                "exitCode": 0,
                "stdout": "hello\n",
                "stderr": "",
                "executionTimeMs": 42
            })),
            is_error: false,
        },
        is_error: false,
    };
    let json = event_to_json(&event);
    let details = &json["result"]["details"];
    assert_eq!(details["exitCode"], 0);
    assert_eq!(details["stdout"], "hello\n");
    assert_eq!(details["executionTimeMs"], 42);

    // Read tool details include size, lineCount.
    let event_read = AgentEvent::ToolExecutionEnd {
        tool_call_id: "tc-read-det".to_string(),
        tool_name: "read".to_string(),
        result: ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("file contents"))],
            details: Some(json!({
                "size": 1024,
                "lineCount": 50
            })),
            is_error: false,
        },
        is_error: false,
    };
    let json_read = event_to_json(&event_read);
    assert_eq!(json_read["result"]["details"]["size"], 1024);
    assert_eq!(json_read["result"]["details"]["lineCount"], 50);

    harness
        .log()
        .info_ctx("json_parity", "tool details rich data ok", |_| {});
}

// ---------------------------------------------------------------------------
// 33. Complete tool lifecycle ordering matches pi-mono
// ---------------------------------------------------------------------------

#[test]
fn json_parity_tool_lifecycle_ordering() {
    let harness = TestHarness::new("json_parity_tool_lifecycle_ordering");

    // Pi-mono ordering: start → (optional updates) → end
    let events = [
        AgentEvent::ToolExecutionStart {
            tool_call_id: "tc-life".to_string(),
            tool_name: "bash".to_string(),
            args: json!({"command": "sleep 1 && echo done"}),
        },
        AgentEvent::ToolExecutionUpdate {
            tool_call_id: "tc-life".to_string(),
            tool_name: "bash".to_string(),
            args: json!({"command": "sleep 1 && echo done"}),
            partial_result: ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(""))],
                details: None,
                is_error: false,
            },
        },
        AgentEvent::ToolExecutionEnd {
            tool_call_id: "tc-life".to_string(),
            tool_name: "bash".to_string(),
            result: ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("done"))],
                details: Some(json!({"exitCode": 0})),
                is_error: false,
            },
            is_error: false,
        },
    ];

    let jsons: Vec<Value> = events.iter().map(event_to_json).collect();
    assert_eq!(jsons[0]["type"], "tool_execution_start");
    assert_eq!(jsons[1]["type"], "tool_execution_update");
    assert_eq!(jsons[2]["type"], "tool_execution_end");

    // All share the same toolCallId
    for j in &jsons {
        assert_eq!(j["toolCallId"], "tc-life");
        assert_eq!(j["toolName"], "bash");
    }

    // All map to extension events in the same order
    for (event, expected) in events.iter().zip(
        [
            ExtensionEventName::ToolExecutionStart,
            ExtensionEventName::ToolExecutionUpdate,
            ExtensionEventName::ToolExecutionEnd,
        ]
        .iter(),
    ) {
        let (name, _) = extension_event_from_agent(event).unwrap();
        assert_eq!(name, *expected);
    }

    harness
        .log()
        .info_ctx("json_parity", "tool lifecycle ordering ok", |_| {});
}

// ============================================================================
// DROPIN-172: Comprehensive JSON Mode Unit Tests (bd-3p29k)
//
// Exhaustive tests for event framing, schema edge cases, ordering invariants,
// auto-compaction/retry lifecycle, and extension/tool event edge cases.
// ============================================================================

// ---------------------------------------------------------------------------
// 34. Empty text content in tool output — valid edge case
// ---------------------------------------------------------------------------

#[test]
fn json_parity_empty_text_content() {
    let harness = TestHarness::new("json_parity_empty_text_content");

    let event = AgentEvent::ToolExecutionEnd {
        tool_call_id: "tc-empty".to_string(),
        tool_name: "bash".to_string(),
        result: ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(""))],
            details: None,
            is_error: false,
        },
        is_error: false,
    };
    let json = event_to_json(&event);

    // Empty text is valid — pi-mono emits it for commands with no stdout.
    let content = json["result"]["content"].as_array().expect("content array");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["text"], "");
    assert_eq!(content[0]["type"], "text");

    harness
        .log()
        .info_ctx("json_parity", "empty text content ok", |_| {});
}

// ---------------------------------------------------------------------------
// 35. Unicode content in events — emoji, CJK, RTL
// ---------------------------------------------------------------------------

#[test]
fn json_parity_unicode_content() {
    let harness = TestHarness::new("json_parity_unicode_content");

    let unicode_cases = [
        ("emoji", "Hello \u{1F600}\u{1F680}\u{1F4A5}"),
        ("cjk", "\u{4F60}\u{597D}\u{4E16}\u{754C}"),
        ("rtl", "\u{0645}\u{0631}\u{062D}\u{0628}\u{0627}"),
        ("mixed", "Code: \u{2713} \u{2717} \u{26A0}\u{FE0F}"),
        ("zero_width", "a\u{200B}b\u{200D}c\u{FEFF}d"),
    ];

    for (label, text) in &unicode_cases {
        let event = AgentEvent::ToolExecutionEnd {
            tool_call_id: format!("tc-{label}"),
            tool_name: "bash".to_string(),
            result: ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(*text))],
                details: None,
                is_error: false,
            },
            is_error: false,
        };
        let json = event_to_json(&event);
        let content = &json["result"]["content"][0]["text"];
        assert_eq!(
            content.as_str().unwrap(),
            *text,
            "unicode mismatch: {label}"
        );

        // Round-trip through JSON string should preserve content.
        let json_str = serde_json::to_string(&json).expect("serialize");
        let parsed: Value = serde_json::from_str(&json_str).expect("deserialize");
        assert_eq!(
            parsed["result"]["content"][0]["text"].as_str().unwrap(),
            *text,
            "round-trip mismatch: {label}"
        );
    }

    harness
        .log()
        .info_ctx("json_parity", "unicode content ok", |ctx| {
            ctx.push(("cases".to_string(), unicode_cases.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 36. Large payload in tool args — 1MB+ JSON
// ---------------------------------------------------------------------------

#[test]
fn json_parity_large_payload_tool_args() {
    let harness = TestHarness::new("json_parity_large_payload_tool_args");

    // Build a ~1MB argument payload.
    let large_text: String = "x".repeat(1_000_000);
    let event = AgentEvent::ToolExecutionStart {
        tool_call_id: "tc-large".to_string(),
        tool_name: "write".to_string(),
        args: json!({"path": "/tmp/big.txt", "content": large_text}),
    };
    let json = event_to_json(&event);

    assert_eq!(json["type"], "tool_execution_start");
    assert_eq!(
        json["args"]["content"].as_str().unwrap().len(),
        1_000_000,
        "large payload must be preserved exactly"
    );

    harness
        .log()
        .info_ctx("json_parity", "large payload ok", |ctx| {
            ctx.push(("size_bytes".to_string(), "1000000".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 37. Tool output with multiple content blocks
// ---------------------------------------------------------------------------

#[test]
fn json_parity_tool_output_multiple_content_blocks() {
    let harness = TestHarness::new("json_parity_tool_output_multiple_content_blocks");

    let event = AgentEvent::ToolExecutionEnd {
        tool_call_id: "tc-multi".to_string(),
        tool_name: "bash".to_string(),
        result: ToolOutput {
            content: vec![
                ContentBlock::Text(TextContent::new("stdout: hello")),
                ContentBlock::Text(TextContent::new("stderr: warning")),
                ContentBlock::Text(TextContent::new("exit code: 0")),
            ],
            details: Some(json!({"exitCode": 0})),
            is_error: false,
        },
        is_error: false,
    };
    let json = event_to_json(&event);

    let content = json["result"]["content"].as_array().expect("content array");
    assert_eq!(content.len(), 3, "should have 3 content blocks");
    assert_eq!(content[0]["text"], "stdout: hello");
    assert_eq!(content[1]["text"], "stderr: warning");
    assert_eq!(content[2]["text"], "exit code: 0");

    // All blocks should have type "text".
    for block in content {
        assert_eq!(block["type"], "text");
    }

    harness
        .log()
        .info_ctx("json_parity", "multiple content blocks ok", |ctx| {
            ctx.push(("blocks".to_string(), "3".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 38. Tool output with zero content blocks — edge case
// ---------------------------------------------------------------------------

#[test]
fn json_parity_tool_output_empty_content() {
    let harness = TestHarness::new("json_parity_tool_output_empty_content");

    let event = AgentEvent::ToolExecutionEnd {
        tool_call_id: "tc-empty-content".to_string(),
        tool_name: "bash".to_string(),
        result: ToolOutput {
            content: vec![],
            details: None,
            is_error: false,
        },
        is_error: false,
    };
    let json = event_to_json(&event);

    let content = json["result"]["content"].as_array().expect("content array");
    assert!(content.is_empty(), "empty content array is valid");

    harness
        .log()
        .info_ctx("json_parity", "empty content array ok", |_| {});
}

// ---------------------------------------------------------------------------
// 39. Auto-compaction lifecycle — all field combinations
// ---------------------------------------------------------------------------

#[test]
fn json_parity_auto_compaction_lifecycle_matrix() {
    let harness = TestHarness::new("json_parity_auto_compaction_lifecycle_matrix");

    // Matrix: (result, aborted, will_retry, error_message)
    #[allow(clippy::type_complexity)]
    let cases: Vec<(Option<Value>, bool, bool, Option<&str>, &str)> = vec![
        (
            Some(json!({"summary": "ok"})),
            false,
            false,
            None,
            "success",
        ),
        (None, true, false, None, "aborted_no_error"),
        (
            None,
            false,
            true,
            Some("provider timeout"),
            "retry_with_error",
        ),
        (
            None,
            true,
            true,
            Some("aborted mid-stream"),
            "aborted_retry",
        ),
        (
            Some(json!({"summary": "partial"})),
            false,
            true,
            Some("token limit"),
            "partial_retry",
        ),
        (None, false, false, Some("fatal error"), "failed_no_retry"),
    ];

    for (result, aborted, will_retry, error_msg, label) in &cases {
        let start = AgentEvent::AutoCompactionStart {
            reason: format!("test_{label}"),
        };
        let end = AgentEvent::AutoCompactionEnd {
            result: result.clone(),
            aborted: *aborted,
            will_retry: *will_retry,
            error_message: error_msg.map(String::from),
        };

        let json_start = event_to_json(&start);
        let json_end = event_to_json(&end);

        assert_eq!(json_start["type"], "auto_compaction_start");
        assert_eq!(json_end["type"], "auto_compaction_end");
        assert_eq!(json_end["aborted"], *aborted, "aborted mismatch: {label}");
        assert_eq!(
            json_end["willRetry"], *will_retry,
            "willRetry mismatch: {label}"
        );

        // Optional fields: absent when None (skip_serializing_if).
        if result.is_some() {
            assert!(json_end.get("result").is_some(), "result missing: {label}");
        } else {
            assert!(
                json_end.get("result").is_none(),
                "result should be absent: {label}"
            );
        }
        if error_msg.is_some() {
            assert_eq!(
                json_end["errorMessage"],
                error_msg.unwrap(),
                "errorMessage mismatch: {label}"
            );
        } else {
            assert!(
                json_end.get("errorMessage").is_none(),
                "errorMessage should be absent: {label}"
            );
        }
    }

    harness.log().info_ctx(
        "json_parity",
        "auto-compaction lifecycle matrix ok",
        |ctx| ctx.push(("cases".to_string(), cases.len().to_string())),
    );
}

// ---------------------------------------------------------------------------
// 40. Auto-retry lifecycle — exponential backoff pattern
// ---------------------------------------------------------------------------

#[test]
fn json_parity_auto_retry_exponential_backoff() {
    let harness = TestHarness::new("json_parity_auto_retry_exponential_backoff");

    // Simulate exponential backoff: delays should follow 2^(n-1) * base_ms.
    let base_ms: u64 = 1000;
    let max_attempts: u32 = 5;
    let mut events: Vec<Value> = Vec::new();

    for attempt in 1..=max_attempts {
        let delay = base_ms * 2u64.pow(attempt - 1);
        let start = AgentEvent::AutoRetryStart {
            attempt,
            max_attempts,
            delay_ms: delay,
            error_message: format!("rate limited (attempt {attempt})"),
        };
        events.push(event_to_json(&start));
    }

    // Verify delay doubling pattern.
    for i in 1..events.len() {
        let prev_delay = events[i - 1]["delayMs"].as_u64().unwrap();
        let curr_delay = events[i]["delayMs"].as_u64().unwrap();
        assert_eq!(
            curr_delay,
            prev_delay * 2,
            "delay should double: attempt {}",
            i + 1
        );
    }

    // Verify attempt counter.
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event["attempt"], i + 1);
        assert_eq!(event["maxAttempts"], u64::from(max_attempts));
    }

    // Final retry end — success after retries.
    let end_success = event_to_json(&AgentEvent::AutoRetryEnd {
        success: true,
        attempt: 3,
        final_error: None,
    });
    assert_eq!(end_success["success"], true);
    assert!(end_success.get("finalError").is_none());

    // Final retry end — failure after max retries.
    let end_fail = event_to_json(&AgentEvent::AutoRetryEnd {
        success: false,
        attempt: max_attempts,
        final_error: Some("max retries exceeded".to_string()),
    });
    assert_eq!(end_fail["success"], false);
    assert_eq!(end_fail["finalError"], "max retries exceeded");

    harness
        .log()
        .info_ctx("json_parity", "auto-retry exponential backoff ok", |ctx| {
            ctx.push(("max_attempts".to_string(), max_attempts.to_string()));
        });
}

// ---------------------------------------------------------------------------
// 41. Extension error — all event hook names
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_error_all_hook_names() {
    let harness = TestHarness::new("json_parity_extension_error_all_hook_names");

    let hook_names = [
        "on_agent_start",
        "on_agent_end",
        "on_turn_start",
        "on_turn_end",
        "on_message_start",
        "on_message_update",
        "on_message_end",
        "on_tool_start",
        "on_tool_update",
        "on_tool_end",
        "lifecycle",
        "register",
    ];

    for hook in &hook_names {
        let event = AgentEvent::ExtensionError {
            extension_id: Some("test-ext".to_string()),
            event: hook.to_string(),
            error: format!("error in {hook}"),
        };
        let json = event_to_json(&event);

        assert_eq!(json["type"], "extension_error");
        assert_eq!(json["event"], *hook);
        assert_eq!(json["error"], format!("error in {hook}"));
        assert_eq!(json["extensionId"], "test-ext");
    }

    // Extension error without extension_id (global error).
    let global_err = event_to_json(&AgentEvent::ExtensionError {
        extension_id: None,
        event: "lifecycle".to_string(),
        error: "extension manager shutdown".to_string(),
    });
    assert!(
        global_err.get("extensionId").is_none(),
        "extensionId should be absent for global errors"
    );

    harness
        .log()
        .info_ctx("json_parity", "extension error hook names ok", |ctx| {
            ctx.push(("hooks".to_string(), hook_names.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 42. AME streaming sub-lifecycle: text_start → text_delta → text_end
// ---------------------------------------------------------------------------

#[test]
fn json_parity_ame_text_streaming_lifecycle() {
    let harness = TestHarness::new("json_parity_ame_text_streaming_lifecycle");
    let partial = Arc::new(test_assistant_message());

    let ame_sequence: Vec<AssistantMessageEvent> = vec![
        AssistantMessageEvent::Start {
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "Hello".to_string(),
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: " world".to_string(),
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: "Hello world".to_string(),
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::Done {
            reason: StopReason::Stop,
            message: Arc::clone(&partial),
        },
    ];

    let expected_types = [
        "start",
        "text_start",
        "text_delta",
        "text_delta",
        "text_end",
        "done",
    ];

    for (ame, expected_type) in ame_sequence.iter().zip(expected_types.iter()) {
        let event = AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: ame.clone(),
        };
        let json = event_to_json(&event);
        let ame_type = json["assistantMessageEvent"]["type"]
            .as_str()
            .unwrap_or("<missing>");
        assert_eq!(ame_type, *expected_type, "AME type mismatch in sequence");
    }

    // Verify text_end content matches accumulated deltas.
    let text_end_event = AgentEvent::MessageUpdate {
        message: Message::Assistant(Arc::clone(&partial)),
        assistant_message_event: AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: "Hello world".to_string(),
            partial: Arc::clone(&partial),
        },
    };
    let json = event_to_json(&text_end_event);
    assert_eq!(
        json["assistantMessageEvent"]["content"], "Hello world",
        "text_end should contain accumulated content"
    );

    harness
        .log()
        .info_ctx("json_parity", "AME text streaming lifecycle ok", |ctx| {
            ctx.push(("events".to_string(), expected_types.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 43. AME thinking sub-lifecycle
// ---------------------------------------------------------------------------

#[test]
fn json_parity_ame_thinking_lifecycle() {
    let harness = TestHarness::new("json_parity_ame_thinking_lifecycle");
    let partial = Arc::new(test_assistant_message());

    let events: Vec<(&str, AssistantMessageEvent)> = vec![
        (
            "thinking_start",
            AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                partial: Arc::clone(&partial),
            },
        ),
        (
            "thinking_delta",
            AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "Let me think...".to_string(),
                partial: Arc::clone(&partial),
            },
        ),
        (
            "thinking_end",
            AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: "Let me think about this carefully".to_string(),
                partial: Arc::clone(&partial),
            },
        ),
    ];

    for (expected_type, ame) in &events {
        let update = AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: ame.clone(),
        };
        let json = event_to_json(&update);
        let ame_json = &json["assistantMessageEvent"];

        assert_eq!(ame_json["type"], *expected_type);
        assert_eq!(ame_json["contentIndex"], 0);
    }

    harness
        .log()
        .info_ctx("json_parity", "AME thinking lifecycle ok", |ctx| {
            ctx.push(("events".to_string(), events.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 44. AME toolcall sub-lifecycle with ToolCall details
// ---------------------------------------------------------------------------

#[test]
fn json_parity_ame_toolcall_lifecycle() {
    let harness = TestHarness::new("json_parity_ame_toolcall_lifecycle");
    let partial = Arc::new(test_assistant_message());

    // toolcall_start → toolcall_delta(s) → toolcall_end
    let toolcall_start = AssistantMessageEvent::ToolCallStart {
        content_index: 1,
        partial: Arc::clone(&partial),
    };
    let toolcall_delta = AssistantMessageEvent::ToolCallDelta {
        content_index: 1,
        delta: "{\"path\": \"/tmp/test.txt\"}".to_string(),
        partial: Arc::clone(&partial),
    };
    let toolcall_end = AssistantMessageEvent::ToolCallEnd {
        content_index: 1,
        tool_call: ToolCall {
            id: "toolu_01xyz".to_string(),
            name: "read".to_string(),
            arguments: json!({"path": "/tmp/test.txt"}),
            thought_signature: Some("sig_abc".to_string()),
        },
        partial: Arc::clone(&partial),
    };

    // Verify toolcall_end includes full ToolCall details.
    let end_event = AgentEvent::MessageUpdate {
        message: Message::Assistant(Arc::clone(&partial)),
        assistant_message_event: toolcall_end,
    };
    let json = event_to_json(&end_event);
    let ame = &json["assistantMessageEvent"];

    assert_eq!(ame["type"], "toolcall_end");
    assert_eq!(ame["contentIndex"], 1);
    assert_eq!(ame["toolCall"]["id"], "toolu_01xyz");
    assert_eq!(ame["toolCall"]["name"], "read");
    assert_eq!(ame["toolCall"]["arguments"]["path"], "/tmp/test.txt");
    assert_eq!(ame["toolCall"]["thoughtSignature"], "sig_abc");

    // Verify camelCase in nested toolCall.
    assert!(ame["toolCall"].get("thought_signature").is_none());

    // Verify start and delta.
    let start_json = event_to_json(&AgentEvent::MessageUpdate {
        message: Message::Assistant(Arc::clone(&partial)),
        assistant_message_event: toolcall_start,
    });
    assert_eq!(
        start_json["assistantMessageEvent"]["type"],
        "toolcall_start"
    );
    assert_eq!(start_json["assistantMessageEvent"]["contentIndex"], 1);

    let delta_json = event_to_json(&AgentEvent::MessageUpdate {
        message: Message::Assistant(Arc::clone(&partial)),
        assistant_message_event: toolcall_delta,
    });
    assert_eq!(
        delta_json["assistantMessageEvent"]["type"],
        "toolcall_delta"
    );
    assert_eq!(
        delta_json["assistantMessageEvent"]["delta"],
        "{\"path\": \"/tmp/test.txt\"}"
    );

    harness
        .log()
        .info_ctx("json_parity", "AME toolcall lifecycle ok", |_| {});
}

// ---------------------------------------------------------------------------
// 45. AME error variant — StopReason values
// ---------------------------------------------------------------------------

#[test]
fn json_parity_ame_error_stop_reasons() {
    let harness = TestHarness::new("json_parity_ame_error_stop_reasons");
    let partial = Arc::new(test_assistant_message());

    let stop_reasons = [
        (StopReason::Stop, "stop"),
        (StopReason::Length, "length"),
        (StopReason::ToolUse, "toolUse"),
        (StopReason::Error, "error"),
        (StopReason::Aborted, "aborted"),
    ];

    for (reason, expected_str) in &stop_reasons {
        // Done variant.
        let done = AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::Done {
                reason: *reason,
                message: Arc::clone(&partial),
            },
        };
        let json = event_to_json(&done);
        assert_eq!(
            json["assistantMessageEvent"]["reason"], *expected_str,
            "done reason mismatch for {expected_str}"
        );

        // Error variant.
        let err = AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::Error {
                reason: *reason,
                error: Arc::clone(&partial),
            },
        };
        let json_err = event_to_json(&err);
        assert_eq!(
            json_err["assistantMessageEvent"]["reason"], *expected_str,
            "error reason mismatch for {expected_str}"
        );
    }

    harness
        .log()
        .info_ctx("json_parity", "AME error stop reasons ok", |ctx| {
            ctx.push(("reasons".to_string(), stop_reasons.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 46. Skip-serializing optional fields — absent when None
// ---------------------------------------------------------------------------

#[test]
fn json_parity_optional_fields_absent_when_none() {
    let harness = TestHarness::new("json_parity_optional_fields_absent_when_none");

    // AgentEnd.error: skip_serializing_if = "Option::is_none"
    let end_no_err = event_to_json(&AgentEvent::AgentEnd {
        session_id: "s".to_string().into(),
        messages: vec![],
        error: None,
    });
    assert!(
        end_no_err.get("error").is_none(),
        "error should be absent when None"
    );

    // AutoCompactionEnd.result: skip_serializing_if
    let comp_no_result = event_to_json(&AgentEvent::AutoCompactionEnd {
        result: None,
        aborted: false,
        will_retry: false,
        error_message: None,
    });
    assert!(
        comp_no_result.get("result").is_none(),
        "result should be absent when None"
    );
    assert!(
        comp_no_result.get("errorMessage").is_none(),
        "errorMessage should be absent when None"
    );

    // AutoRetryEnd.final_error: skip_serializing_if
    let retry_no_err = event_to_json(&AgentEvent::AutoRetryEnd {
        success: true,
        attempt: 1,
        final_error: None,
    });
    assert!(
        retry_no_err.get("finalError").is_none(),
        "finalError should be absent when None"
    );

    // ExtensionError.extension_id: skip_serializing_if
    let ext_no_id = event_to_json(&AgentEvent::ExtensionError {
        extension_id: None,
        event: "e".to_string(),
        error: "err".to_string(),
    });
    assert!(
        ext_no_id.get("extensionId").is_none(),
        "extensionId should be absent when None"
    );

    // TextContent.text_signature: skip_serializing_if
    let tc = TextContent::new("hello");
    let tc_json = serde_json::to_value(&tc).expect("serialize TextContent");
    assert!(
        tc_json.get("textSignature").is_none(),
        "textSignature should be absent when None"
    );

    // Verify presence when Some.
    let end_with_err = event_to_json(&AgentEvent::AgentEnd {
        session_id: "s".to_string().into(),
        messages: vec![],
        error: Some("oops".to_string()),
    });
    assert_eq!(end_with_err["error"], "oops");

    harness.log().info_ctx(
        "json_parity",
        "optional fields absent when None ok",
        |ctx| ctx.push(("fields_checked".to_string(), "5".to_string())),
    );
}

// ---------------------------------------------------------------------------
// 47. ContentBlock type tags — text, thinking, image, toolCall
// ---------------------------------------------------------------------------

#[test]
fn json_parity_content_block_type_tags() {
    let harness = TestHarness::new("json_parity_content_block_type_tags");

    let blocks: Vec<(ContentBlock, &str)> = vec![
        (ContentBlock::Text(TextContent::new("hi")), "text"),
        (
            ContentBlock::Thinking(ThinkingContent {
                thinking: "hmm".to_string(),
                thinking_signature: None,
            }),
            "thinking",
        ),
        (
            ContentBlock::Image(ImageContent {
                data: "base64data".to_string(),
                mime_type: "image/png".to_string(),
            }),
            "image",
        ),
        (
            ContentBlock::ToolCall(ToolCall {
                id: "tc-1".to_string(),
                name: "read".to_string(),
                arguments: json!({}),
                thought_signature: None,
            }),
            "toolCall",
        ),
    ];

    for (block, expected_type) in &blocks {
        let json = serde_json::to_value(block).expect("serialize");
        assert_eq!(
            json["type"], *expected_type,
            "content block type mismatch for {expected_type}"
        );
    }

    harness
        .log()
        .info_ctx("json_parity", "content block type tags ok", |ctx| {
            ctx.push(("types".to_string(), blocks.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 48. AssistantMessage with mixed content blocks
// ---------------------------------------------------------------------------

#[test]
fn json_parity_assistant_message_mixed_content() {
    let harness = TestHarness::new("json_parity_assistant_message_mixed_content");

    let msg = AssistantMessage {
        content: vec![
            ContentBlock::Thinking(ThinkingContent {
                thinking: "Let me read the file".to_string(),
                thinking_signature: Some("sig_think".to_string()),
            }),
            ContentBlock::Text(TextContent {
                text: "I'll read the file for you.".to_string(),
                text_signature: Some("sig_text".to_string()),
            }),
            ContentBlock::ToolCall(ToolCall {
                id: "toolu_01abc".to_string(),
                name: "read".to_string(),
                arguments: json!({"path": "/tmp/test.txt"}),
                thought_signature: None,
            }),
        ],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-20250514".to_string(),
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        stop_details: None,
        error_message: None,
        timestamp: 1_700_000_000,
    };

    let event = AgentEvent::MessageEnd {
        message: Message::Assistant(Arc::new(msg)),
    };
    let json = event_to_json(&event);
    let msg_json = &json["message"];

    // Verify content block ordering and types.
    let content = msg_json["content"].as_array().expect("content array");
    assert_eq!(content.len(), 3);
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["thinkingSignature"], "sig_think");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["textSignature"], "sig_text");
    assert_eq!(content[2]["type"], "toolCall");
    assert_eq!(content[2]["name"], "read");

    // Verify camelCase in nested fields.
    assert!(content[0].get("thinking_signature").is_none());
    assert!(content[1].get("text_signature").is_none());
    assert!(content[2].get("thought_signature").is_none());

    harness
        .log()
        .info_ctx("json_parity", "mixed content blocks ok", |ctx| {
            ctx.push(("blocks".to_string(), "3".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 49. Full lifecycle with tool execution turn
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn json_parity_full_lifecycle_with_tool_turn() {
    let harness = TestHarness::new("json_parity_full_lifecycle_with_tool_turn");
    let sid = "session-full-lifecycle";
    let partial = Arc::new(test_assistant_message());

    // Complete lifecycle: agent_start → user msg → turn with tool → agent_end.
    let events: Vec<AgentEvent> = vec![
        // Agent start.
        AgentEvent::AgentStart {
            session_id: sid.to_string().into(),
        },
        // User message lifecycle.
        AgentEvent::MessageStart {
            message: test_user_message(),
        },
        AgentEvent::MessageEnd {
            message: test_user_message(),
        },
        // Turn 0: assistant with tool call.
        AgentEvent::TurnStart {
            session_id: sid.to_string().into(),
            turn_index: 0,
            timestamp: 1_700_000_000,
        },
        AgentEvent::MessageStart {
            message: Message::Assistant(Arc::clone(&partial)),
        },
        AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::Start {
                partial: Arc::clone(&partial),
            },
        },
        AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: Arc::clone(&partial),
            },
        },
        AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call: ToolCall {
                    id: "tc-1".to_string(),
                    name: "bash".to_string(),
                    arguments: json!({"command": "echo hello"}),
                    thought_signature: None,
                },
                partial: Arc::clone(&partial),
            },
        },
        AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::Done {
                reason: StopReason::ToolUse,
                message: Arc::clone(&partial),
            },
        },
        AgentEvent::MessageEnd {
            message: Message::Assistant(Arc::clone(&partial)),
        },
        // Tool execution.
        AgentEvent::ToolExecutionStart {
            tool_call_id: "tc-1".to_string(),
            tool_name: "bash".to_string(),
            args: json!({"command": "echo hello"}),
        },
        AgentEvent::ToolExecutionEnd {
            tool_call_id: "tc-1".to_string(),
            tool_name: "bash".to_string(),
            result: ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("hello\n"))],
                details: Some(json!({"exitCode": 0})),
                is_error: false,
            },
            is_error: false,
        },
        // Turn end with tool results.
        AgentEvent::TurnEnd {
            session_id: sid.to_string().into(),
            turn_index: 0,
            message: Message::Assistant(Arc::clone(&partial)),
            tool_results: vec![test_user_message()],
            latency_breakdown: None,
        },
        // Agent end.
        AgentEvent::AgentEnd {
            session_id: sid.to_string().into(),
            messages: vec![test_user_message(), Message::Assistant(partial)],
            error: None,
        },
    ];

    let jsons: Vec<Value> = events.iter().map(event_to_json).collect();

    // Verify structural ordering invariant.
    let type_seq: Vec<&str> = jsons
        .iter()
        .map(|j| j["type"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(type_seq[0], "agent_start");
    assert_eq!(*type_seq.last().unwrap(), "agent_end");

    // agent_start must be first, agent_end must be last.
    for t in &type_seq[1..type_seq.len() - 1] {
        assert_ne!(*t, "agent_start", "only one agent_start allowed");
        assert_ne!(*t, "agent_end", "agent_end must be last");
    }

    // turn_start before turn_end.
    let turn_start_idx = type_seq.iter().position(|t| *t == "turn_start").unwrap();
    let turn_end_idx = type_seq.iter().position(|t| *t == "turn_end").unwrap();
    assert!(
        turn_start_idx < turn_end_idx,
        "turn_start must precede turn_end"
    );

    // tool_execution_start before tool_execution_end.
    let tool_start_idx = type_seq
        .iter()
        .position(|t| *t == "tool_execution_start")
        .unwrap();
    let tool_end_idx = type_seq
        .iter()
        .position(|t| *t == "tool_execution_end")
        .unwrap();
    assert!(
        tool_start_idx < tool_end_idx,
        "tool_start must precede tool_end"
    );

    // Tool execution is within turn boundaries.
    assert!(tool_start_idx > turn_start_idx);
    assert!(tool_end_idx < turn_end_idx);

    // Verify all NDJSON-serializable (single-line).
    for json in &jsons {
        let line = serde_json::to_string(json).expect("serialize");
        assert!(
            !line.contains('\n'),
            "NDJSON line must not contain newlines"
        );
    }

    harness
        .log()
        .info_ctx("json_parity", "full lifecycle with tool turn ok", |ctx| {
            ctx.push(("events".to_string(), jsons.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 50. Lifecycle with auto-retry mid-stream
// ---------------------------------------------------------------------------

#[test]
fn json_parity_lifecycle_with_retry() {
    let harness = TestHarness::new("json_parity_lifecycle_with_retry");
    let sid = "session-retry";

    let events: Vec<AgentEvent> = vec![
        AgentEvent::AgentStart {
            session_id: sid.to_string().into(),
        },
        AgentEvent::MessageStart {
            message: test_user_message(),
        },
        AgentEvent::MessageEnd {
            message: test_user_message(),
        },
        // First attempt fails → retry.
        AgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 1000,
            error_message: "rate_limit_error".to_string(),
        },
        // Second attempt succeeds.
        AgentEvent::AutoRetryEnd {
            success: true,
            attempt: 2,
            final_error: None,
        },
        AgentEvent::TurnStart {
            session_id: sid.to_string().into(),
            turn_index: 0,
            timestamp: 1_700_001_000,
        },
        AgentEvent::MessageStart {
            message: Message::Assistant(Arc::new(test_assistant_message())),
        },
        AgentEvent::MessageEnd {
            message: Message::Assistant(Arc::new(test_assistant_message())),
        },
        AgentEvent::TurnEnd {
            session_id: sid.to_string().into(),
            turn_index: 0,
            message: Message::Assistant(Arc::new(test_assistant_message())),
            tool_results: vec![],
            latency_breakdown: None,
        },
        AgentEvent::AgentEnd {
            session_id: sid.to_string().into(),
            messages: vec![],
            error: None,
        },
    ];

    let jsons: Vec<Value> = events.iter().map(event_to_json).collect();

    // Retry events appear between user message and turn.
    let retry_start_idx = jsons
        .iter()
        .position(|j| j["type"] == "auto_retry_start")
        .unwrap();
    let retry_end_idx = jsons
        .iter()
        .position(|j| j["type"] == "auto_retry_end")
        .unwrap();
    let turn_start_idx = jsons
        .iter()
        .position(|j| j["type"] == "turn_start")
        .unwrap();

    assert!(retry_start_idx < retry_end_idx);
    assert!(retry_end_idx < turn_start_idx);

    harness
        .log()
        .info_ctx("json_parity", "lifecycle with retry ok", |ctx| {
            ctx.push(("events".to_string(), jsons.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 51. Lifecycle with auto-compaction
// ---------------------------------------------------------------------------

#[test]
fn json_parity_lifecycle_with_compaction() {
    let harness = TestHarness::new("json_parity_lifecycle_with_compaction");
    let sid = "session-compact";

    let events: Vec<AgentEvent> = vec![
        AgentEvent::AgentStart {
            session_id: sid.to_string().into(),
        },
        // Turn 0 completes normally.
        AgentEvent::TurnStart {
            session_id: sid.to_string().into(),
            turn_index: 0,
            timestamp: 1_700_000_000,
        },
        AgentEvent::TurnEnd {
            session_id: sid.to_string().into(),
            turn_index: 0,
            message: Message::Assistant(Arc::new(test_assistant_message())),
            tool_results: vec![],
            latency_breakdown: None,
        },
        // Compaction triggered between turns.
        AgentEvent::AutoCompactionStart {
            reason: "context window 90% full".to_string(),
        },
        AgentEvent::AutoCompactionEnd {
            result: Some(json!({
                "summary": "Compacted 15 messages into 3",
                "tokensBefore": 95000,
                "tokensAfter": 25000,
            })),
            aborted: false,
            will_retry: false,
            error_message: None,
        },
        // Turn 1 after compaction.
        AgentEvent::TurnStart {
            session_id: sid.to_string().into(),
            turn_index: 1,
            timestamp: 1_700_001_000,
        },
        AgentEvent::TurnEnd {
            session_id: sid.to_string().into(),
            turn_index: 1,
            message: Message::Assistant(Arc::new(test_assistant_message())),
            tool_results: vec![],
            latency_breakdown: None,
        },
        AgentEvent::AgentEnd {
            session_id: sid.to_string().into(),
            messages: vec![],
            error: None,
        },
    ];

    let jsons: Vec<Value> = events.iter().map(event_to_json).collect();

    // Compaction between turn 0 end and turn 1 start.
    let turn0_end_idx = jsons
        .iter()
        .position(|j| j["type"] == "turn_end" && j["turnIndex"] == 0)
        .unwrap();
    let comp_start_idx = jsons
        .iter()
        .position(|j| j["type"] == "auto_compaction_start")
        .unwrap();
    let comp_end_idx = jsons
        .iter()
        .position(|j| j["type"] == "auto_compaction_end")
        .unwrap();
    let turn1_start_idx = jsons
        .iter()
        .position(|j| j["type"] == "turn_start" && j["turnIndex"] == 1)
        .unwrap();

    assert!(turn0_end_idx < comp_start_idx);
    assert!(comp_start_idx < comp_end_idx);
    assert!(comp_end_idx < turn1_start_idx);

    // Compaction result has expected fields.
    let comp_end = &jsons[comp_end_idx];
    assert_eq!(comp_end["result"]["tokensBefore"], 95000);
    assert_eq!(comp_end["result"]["tokensAfter"], 25000);

    harness
        .log()
        .info_ctx("json_parity", "lifecycle with compaction ok", |ctx| {
            ctx.push(("events".to_string(), jsons.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 52. Turn index monotonically increases
// ---------------------------------------------------------------------------

#[test]
fn json_parity_turn_index_monotonic() {
    let harness = TestHarness::new("json_parity_turn_index_monotonic");
    let sid = "session-turns";

    let mut events = Vec::new();
    events.push(AgentEvent::AgentStart {
        session_id: sid.to_string().into(),
    });

    for turn in 0..5_usize {
        events.push(AgentEvent::TurnStart {
            session_id: sid.to_string().into(),
            turn_index: turn,
            #[allow(clippy::cast_possible_wrap)]
            timestamp: 1_700_000_000 + (turn as i64),
        });
        events.push(AgentEvent::TurnEnd {
            session_id: sid.to_string().into(),
            turn_index: turn,
            message: Message::Assistant(Arc::new(test_assistant_message())),
            tool_results: vec![],
            latency_breakdown: None,
        });
    }

    events.push(AgentEvent::AgentEnd {
        session_id: sid.to_string().into(),
        messages: vec![],
        error: None,
    });

    let jsons: Vec<Value> = events.iter().map(event_to_json).collect();

    // Collect turn indices from turn_start events.
    let turn_indices: Vec<u64> = jsons
        .iter()
        .filter(|j| j["type"] == "turn_start")
        .map(|j| j["turnIndex"].as_u64().unwrap())
        .collect();

    assert_eq!(turn_indices, vec![0, 1, 2, 3, 4]);

    // Each turn_start/turn_end pair has matching index.
    let start_indices: Vec<u64> = jsons
        .iter()
        .filter(|j| j["type"] == "turn_start")
        .map(|j| j["turnIndex"].as_u64().unwrap())
        .collect();
    let end_indices: Vec<u64> = jsons
        .iter()
        .filter(|j| j["type"] == "turn_end")
        .map(|j| j["turnIndex"].as_u64().unwrap())
        .collect();
    assert_eq!(start_indices, end_indices, "turn indices must match");

    harness
        .log()
        .info_ctx("json_parity", "turn index monotonic ok", |ctx| {
            ctx.push(("turns".to_string(), "5".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 53. NDJSON serialization — each event fits in one line
// ---------------------------------------------------------------------------

#[test]
fn json_parity_ndjson_single_line() {
    let harness = TestHarness::new("json_parity_ndjson_single_line");
    let partial = Arc::new(test_assistant_message());

    // Build events with content that could break NDJSON framing.
    let tricky_events: Vec<AgentEvent> = vec![
        // Multiline text in tool output.
        AgentEvent::ToolExecutionEnd {
            tool_call_id: "tc-nl".to_string(),
            tool_name: "bash".to_string(),
            result: ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(
                    "line1\nline2\nline3\n",
                ))],
                details: None,
                is_error: false,
            },
            is_error: false,
        },
        // Tab and carriage return in delta.
        AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "col1\tcol2\r\ncol3".to_string(),
                partial: Arc::clone(&partial),
            },
        },
        // Error message with newlines.
        AgentEvent::ExtensionError {
            extension_id: Some("ext-1".to_string()),
            event: "on_tool_start".to_string(),
            error: "Error: something went wrong\n    at line 42\n    at line 84".to_string(),
        },
    ];

    for event in &tricky_events {
        let json = event_to_json(event);
        let line = serde_json::to_string(&json).expect("serialize");
        assert!(
            !line.contains('\n'),
            "NDJSON line must not contain literal newlines: {}",
            &line[..line.len().min(100)]
        );
    }

    harness
        .log()
        .info_ctx("json_parity", "ndjson single line ok", |ctx| {
            ctx.push(("events".to_string(), tricky_events.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 54. TurnEnd.tool_results can contain multiple messages
// ---------------------------------------------------------------------------

#[test]
fn json_parity_turn_end_multiple_tool_results() {
    let harness = TestHarness::new("json_parity_turn_end_multiple_tool_results");

    let event = AgentEvent::TurnEnd {
        session_id: "s".to_string().into(),
        turn_index: 0,
        message: Message::Assistant(Arc::new(test_assistant_message())),
        tool_results: vec![
            test_user_message(),
            test_user_message(),
            test_user_message(),
        ],
        latency_breakdown: None,
    };
    let json = event_to_json(&event);

    let results = json["toolResults"].as_array().expect("toolResults array");
    assert_eq!(results.len(), 3, "should have 3 tool results");

    harness
        .log()
        .info_ctx("json_parity", "turn_end multiple tool results ok", |ctx| {
            ctx.push(("count".to_string(), "3".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 55. AgentEnd.messages preserves message ordering
// ---------------------------------------------------------------------------

#[test]
fn json_parity_agent_end_message_ordering() {
    let harness = TestHarness::new("json_parity_agent_end_message_ordering");

    let msgs: Vec<Message> = (0..5)
        .map(|i| {
            Message::User(pi::model::UserMessage {
                content: pi::model::UserContent::Text(format!("message {i}")),
                timestamp: 1_700_000_000 + i64::from(i),
            })
        })
        .collect();

    let event = AgentEvent::AgentEnd {
        session_id: "s".to_string().into(),
        messages: msgs,
        error: None,
    };
    let json = event_to_json(&event);

    let msg_array = json["messages"].as_array().expect("messages array");
    assert_eq!(msg_array.len(), 5);

    // Verify ordering is preserved.
    for (i, msg) in msg_array.iter().enumerate() {
        let text = msg["content"].as_str().unwrap();
        assert_eq!(text, format!("message {i}"));
    }

    harness
        .log()
        .info_ctx("json_parity", "agent_end message ordering ok", |ctx| {
            ctx.push(("count".to_string(), "5".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 56. Extension event payload round-trip — all 10 forwarded events
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn json_parity_extension_event_payload_all_forwarded() {
    let harness = TestHarness::new("json_parity_extension_event_payload_all_forwarded");
    let partial = Arc::new(test_assistant_message());

    let events: Vec<(AgentEvent, &str)> = vec![
        (
            AgentEvent::AgentStart {
                session_id: "s123".to_string().into(),
            },
            "sessionId",
        ),
        (
            AgentEvent::AgentEnd {
                session_id: "s123".to_string().into(),
                messages: vec![test_user_message()],
                error: Some("timeout".to_string()),
            },
            "sessionId",
        ),
        (
            AgentEvent::TurnStart {
                session_id: "s123".to_string().into(),
                turn_index: 3,
                timestamp: 1_700_000_000,
            },
            "turnIndex",
        ),
        (
            AgentEvent::TurnEnd {
                session_id: "s123".to_string().into(),
                turn_index: 3,
                message: test_user_message(),
                tool_results: vec![],
                latency_breakdown: None,
            },
            "turnIndex",
        ),
        (
            AgentEvent::MessageStart {
                message: test_user_message(),
            },
            "message",
        ),
        (
            AgentEvent::MessageUpdate {
                message: Message::Assistant(Arc::clone(&partial)),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "x".to_string(),
                    partial: Arc::clone(&partial),
                },
            },
            "assistantMessageEvent",
        ),
        (
            AgentEvent::MessageEnd {
                message: test_user_message(),
            },
            "message",
        ),
        (
            AgentEvent::ToolExecutionStart {
                tool_call_id: "tc-1".to_string(),
                tool_name: "read".to_string(),
                args: json!({"path": "/tmp/file"}),
            },
            "toolCallId",
        ),
        (
            AgentEvent::ToolExecutionUpdate {
                tool_call_id: "tc-1".to_string(),
                tool_name: "bash".to_string(),
                args: json!({}),
                partial_result: test_tool_output(),
            },
            "toolCallId",
        ),
        (
            AgentEvent::ToolExecutionEnd {
                tool_call_id: "tc-1".to_string(),
                tool_name: "read".to_string(),
                result: test_tool_output(),
                is_error: false,
            },
            "toolCallId",
        ),
    ];

    for (event, expected_key) in &events {
        let result = extension_event_from_agent(event);
        assert!(result.is_some(), "should be forwarded");
        let (_, payload) = result.unwrap();
        let payload = payload.expect("should have payload");

        // Each payload should contain the key-specific field.
        assert!(
            payload.get(*expected_key).is_some(),
            "payload missing {expected_key} for event {:?}",
            event_to_json(event)["type"]
        );

        // Payload should be a JSON object.
        assert!(
            payload.is_object(),
            "payload should be object for {:?}",
            event_to_json(event)["type"]
        );
    }

    harness.log().info_ctx(
        "json_parity",
        "extension event payload all forwarded ok",
        |ctx| ctx.push(("events".to_string(), events.len().to_string())),
    );
}

// ---------------------------------------------------------------------------
// 57. SessionHeader required fields and defaults
// ---------------------------------------------------------------------------

#[test]
fn json_parity_session_header_defaults() {
    let harness = TestHarness::new("json_parity_session_header_defaults");

    let header = pi::session::SessionHeader::new();
    let json = serde_json::to_value(&header).expect("serialize");

    // Required fields must be present.
    assert_eq!(json["type"], "session");
    assert!(json["id"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(json["timestamp"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(json["cwd"].as_str().is_some_and(|s| !s.is_empty()));

    // Optional fields should be absent (skip_serializing_if).
    let optional_fields = ["provider", "modelId", "thinkingLevel", "branchedFrom"];
    for field in &optional_fields {
        assert!(
            json.get(field).is_none() || json[field].is_null(),
            "optional field {field} should be absent/null in default header"
        );
    }

    // Version field should be present (not optional in struct, has value).
    // If version is 1 it should be present.
    if let Some(ver) = json.get("version") {
        assert!(ver.is_number());
    }

    harness
        .log()
        .info_ctx("json_parity", "session header defaults ok", |ctx| {
            ctx.push(("fields".to_string(), "checked".to_string()));
        });
}

// ---------------------------------------------------------------------------
// 58. Tool details edge cases — various detail shapes
// ---------------------------------------------------------------------------

#[test]
fn json_parity_tool_details_edge_cases() {
    let harness = TestHarness::new("json_parity_tool_details_edge_cases");

    let detail_cases: Vec<(Option<Value>, &str)> = vec![
        (None, "null"),
        (Some(json!({})), "empty_object"),
        (Some(json!({"exitCode": 0, "stdout": "ok"})), "bash_success"),
        (
            Some(json!({"exitCode": 1, "stderr": "not found"})),
            "bash_failure",
        ),
        (Some(json!({"size": 0, "lineCount": 0})), "empty_file"),
        (
            Some(json!({"size": 1_048_576, "lineCount": 50000})),
            "large_file",
        ),
        (
            Some(json!({"matchCount": 42, "fileCount": 7})),
            "grep_result",
        ),
        (Some(json!({"linesChanged": 5, "hunks": 2})), "edit_result"),
    ];

    for (details, label) in &detail_cases {
        let event = AgentEvent::ToolExecutionEnd {
            tool_call_id: format!("tc-{label}"),
            tool_name: "bash".to_string(),
            result: ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("output"))],
                details: details.clone(),
                is_error: false,
            },
            is_error: false,
        };
        let json = event_to_json(&event);
        let result = &json["result"];

        match details {
            None => {
                assert!(
                    result["details"].is_null(),
                    "details should be null: {label}"
                );
            }
            Some(expected) => {
                assert_eq!(&result["details"], expected, "details mismatch: {label}");
            }
        }
    }

    harness
        .log()
        .info_ctx("json_parity", "tool details edge cases ok", |ctx| {
            ctx.push(("cases".to_string(), detail_cases.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 59. Usage camelCase and field completeness
// ---------------------------------------------------------------------------

#[test]
fn json_parity_usage_fields_camel_case() {
    let harness = TestHarness::new("json_parity_usage_fields_camel_case");

    let usage = Usage {
        total_tokens: 500,
        input: 200,
        output: 300,
        ..Usage::default()
    };
    let json = serde_json::to_value(&usage).expect("serialize");

    // Key fields must use camelCase.
    assert_eq!(json["totalTokens"], 500);
    assert_eq!(json["input"], 200);
    assert_eq!(json["output"], 300);

    // No snake_case leaks.
    let json_str = serde_json::to_string(&json).expect("to_string");
    assert!(
        !json_str.contains("\"total_tokens\""),
        "should use totalTokens not total_tokens"
    );

    harness
        .log()
        .info_ctx("json_parity", "usage fields camelCase ok", |_| {});
}

// ---------------------------------------------------------------------------
// 60. Multi-tool turn — consistent toolCallIds
// ---------------------------------------------------------------------------

#[test]
fn json_parity_multi_tool_turn() {
    let harness = TestHarness::new("json_parity_multi_tool_turn");

    let tools = [
        ("tc-a", "read", json!({"path": "/a"})),
        ("tc-b", "write", json!({"path": "/b", "content": "x"})),
        ("tc-c", "bash", json!({"command": "ls"})),
    ];

    let mut all_events: Vec<Value> = Vec::new();

    for (id, name, args) in &tools {
        all_events.push(event_to_json(&AgentEvent::ToolExecutionStart {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            args: args.clone(),
        }));
        all_events.push(event_to_json(&AgentEvent::ToolExecutionEnd {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            result: test_tool_output(),
            is_error: false,
        }));
    }

    // Verify each start/end pair shares the same toolCallId.
    for chunk in all_events.chunks(2) {
        let start_id = chunk[0]["toolCallId"].as_str().unwrap();
        let end_id = chunk[1]["toolCallId"].as_str().unwrap();
        assert_eq!(start_id, end_id, "toolCallId must match in start/end pair");
    }

    // Verify toolCallIds are unique across tools.
    let ids: Vec<&str> = all_events
        .iter()
        .filter(|e| e["type"] == "tool_execution_start")
        .map(|e| e["toolCallId"].as_str().unwrap())
        .collect();
    let unique: std::collections::HashSet<&&str> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len(), "toolCallIds must be unique");

    harness
        .log()
        .info_ctx("json_parity", "multi-tool turn ok", |ctx| {
            ctx.push(("tools".to_string(), tools.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 61. Content block camelCase — ThinkingContent fields
// ---------------------------------------------------------------------------

#[test]
fn json_parity_thinking_content_camel_case() {
    let harness = TestHarness::new("json_parity_thinking_content_camel_case");

    let tc = ThinkingContent {
        thinking: "deep thought".to_string(),
        thinking_signature: Some("sig_think_123".to_string()),
    };
    let json = serde_json::to_value(&tc).expect("serialize");

    assert_eq!(json["thinking"], "deep thought");
    assert_eq!(json["thinkingSignature"], "sig_think_123");
    assert!(json.get("thinking_signature").is_none());

    // Without signature.
    let tc_no_sig = ThinkingContent {
        thinking: "thought".to_string(),
        thinking_signature: None,
    };
    let json_no_sig = serde_json::to_value(&tc_no_sig).expect("serialize");
    assert!(json_no_sig.get("thinkingSignature").is_none());

    harness
        .log()
        .info_ctx("json_parity", "thinking content camelCase ok", |_| {});
}

// ---------------------------------------------------------------------------
// 62. ImageContent camelCase — mimeType field
// ---------------------------------------------------------------------------

#[test]
fn json_parity_image_content_camel_case() {
    let harness = TestHarness::new("json_parity_image_content_camel_case");

    let img = ImageContent {
        data: "iVBORw0KGgoAAAANS".to_string(),
        mime_type: "image/png".to_string(),
    };
    let json = serde_json::to_value(&img).expect("serialize");

    assert_eq!(json["data"], "iVBORw0KGgoAAAANS");
    assert_eq!(json["mimeType"], "image/png");
    assert!(
        json.get("mime_type").is_none(),
        "should use mimeType not mime_type"
    );

    harness
        .log()
        .info_ctx("json_parity", "image content camelCase ok", |_| {});
}

// ---------------------------------------------------------------------------
// 63. ToolCall camelCase — thoughtSignature
// ---------------------------------------------------------------------------

#[test]
fn json_parity_tool_call_camel_case() {
    let harness = TestHarness::new("json_parity_tool_call_camel_case");

    let tc = ToolCall {
        id: "toolu_01abc".to_string(),
        name: "read".to_string(),
        arguments: json!({"path": "/tmp/x"}),
        thought_signature: Some("sig_thought_xyz".to_string()),
    };
    let json = serde_json::to_value(&tc).expect("serialize");

    assert_eq!(json["id"], "toolu_01abc");
    assert_eq!(json["name"], "read");
    assert_eq!(json["thoughtSignature"], "sig_thought_xyz");
    assert!(json.get("thought_signature").is_none());

    // Without signature.
    let tc_no_sig = ToolCall {
        id: "toolu_02".to_string(),
        name: "bash".to_string(),
        arguments: json!({}),
        thought_signature: None,
    };
    let json_no_sig = serde_json::to_value(&tc_no_sig).expect("serialize");
    assert!(json_no_sig.get("thoughtSignature").is_none());

    harness
        .log()
        .info_ctx("json_parity", "tool call camelCase ok", |_| {});
}

// ---------------------------------------------------------------------------
// 64. Special characters in tool args don't break JSON
// ---------------------------------------------------------------------------

#[test]
fn json_parity_special_chars_in_tool_args() {
    let harness = TestHarness::new("json_parity_special_chars_in_tool_args");

    let special_cases = [
        ("quotes", json!({"text": "He said \"hello\""})),
        ("backslash", json!({"path": "C:\\Users\\test"})),
        ("null_byte", json!({"data": "before\x00after"})),
        ("tab", json!({"tsv": "col1\tcol2\tcol3"})),
        ("newline", json!({"multiline": "line1\nline2\nline3"})),
        ("control_chars", json!({"raw": "\x01\x02\x03\x1b[31m"})),
    ];

    for (label, args) in &special_cases {
        let event = AgentEvent::ToolExecutionStart {
            tool_call_id: format!("tc-{label}"),
            tool_name: "bash".to_string(),
            args: args.clone(),
        };
        let json = event_to_json(&event);

        // Must serialize to valid single-line JSON.
        let line = serde_json::to_string(&json).expect("serialize");
        assert!(!line.contains('\n'), "newline in NDJSON: {label}");

        // Round-trip must preserve args.
        let parsed: Value = serde_json::from_str(&line).expect("deserialize");
        assert_eq!(parsed["args"], *args, "round-trip mismatch: {label}");
    }

    harness
        .log()
        .info_ctx("json_parity", "special chars in tool args ok", |ctx| {
            ctx.push(("cases".to_string(), special_cases.len().to_string()));
        });
}

// ---------------------------------------------------------------------------
// 65. Extension UI request with null payload fields
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_null_payload_fields() {
    let harness = TestHarness::new("json_parity_extension_ui_null_payload_fields");

    // Null values in payload should be preserved when flattened.
    let req = ExtensionUiRequest::new(
        "r-null",
        "notify",
        json!({
            "message": "hello",
            "notifyType": null,
            "extra": null,
        }),
    );
    let event = req.to_rpc_event();

    assert_eq!(event["message"], "hello");
    assert!(event["notifyType"].is_null());
    assert!(event["extra"].is_null());

    harness
        .log()
        .info_ctx("json_parity", "extension UI null payload fields ok", |_| {});
}

// ---------------------------------------------------------------------------
// 66. Extension UI request with array payload (non-object)
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_array_payload() {
    let harness = TestHarness::new("json_parity_extension_ui_array_payload");

    let req = ExtensionUiRequest::new("r-arr", "notify", json!(["item1", "item2"]));
    let event = req.to_rpc_event();

    // Array payloads cannot be flattened → stored under "payload" key.
    assert_eq!(event["payload"], json!(["item1", "item2"]));

    harness
        .log()
        .info_ctx("json_parity", "extension UI array payload ok", |_| {});
}

// ---------------------------------------------------------------------------
// 67. Extension UI request with numeric payload (non-object)
// ---------------------------------------------------------------------------

#[test]
fn json_parity_extension_ui_numeric_payload() {
    let harness = TestHarness::new("json_parity_extension_ui_numeric_payload");

    let req = ExtensionUiRequest::new("r-num", "notify", json!(42));
    let event = req.to_rpc_event();

    assert_eq!(event["payload"], 42);

    let req_bool = ExtensionUiRequest::new("r-bool", "notify", json!(true));
    let event_bool = req_bool.to_rpc_event();
    assert_eq!(event_bool["payload"], true);

    harness
        .log()
        .info_ctx("json_parity", "extension UI numeric payload ok", |_| {});
}
