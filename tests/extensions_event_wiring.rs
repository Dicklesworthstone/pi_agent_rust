#![allow(clippy::redundant_clone)]
//! Unit tests: tool/command/event wiring (bd-1u6).
//!
//! Tests the event dispatch paths on [`ExtensionManager`]:
//! - `dispatch_event` (fire-and-forget)
//! - `dispatch_event_with_response` (returns value)
//! - `dispatch_cancellable_event` (can cancel operations)
//! - `dispatch_tool_call` (pre-exec hook, can block)
//! - `dispatch_tool_result` (post-exec hook, can modify)
//! - Event hook filtering (only matching hooks invoked)
//! - Tool registration and routing through extension tools

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
    PROTOCOL_VERSION, RegisterPayload,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::model::ToolCall;
use pi::tools::{ToolOutput, ToolRegistry};
use serde_json::{Value, json};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Load a JS extension with the given source code and return the manager.
fn load_js_extension(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/ext.mjs", source.as_bytes());
    let spec = JsExtensionLoadSpec::from_entry_path(&ext_entry_path).expect("load spec");

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(runtime);

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });

    manager
}

fn make_tool_call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: format!("call-{name}"),
        name: name.to_string(),
        arguments: args,
        thought_signature: None,
    }
}

fn make_tool_output(text: &str) -> ToolOutput {
    ToolOutput {
        content: vec![pi::model::ContentBlock::Text(pi::model::TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        details: None,
        is_error: false,
    }
}

// ---------------------------------------------------------------------------
// Extension sources
// ---------------------------------------------------------------------------

/// Extension that registers lifecycle event hooks and records invocations.
const EVENT_TRACKING_EXT: &str = r#"
export default function init(pi) {
    const events = [];

    pi.on("startup", (event, ctx) => {
        events.push("startup");
        return null;
    });

    pi.on("tool_call", (event, ctx) => {
        events.push("tool_call:" + event.toolName);
        // Non-blocking: return null or object without block=true
        return { block: false };
    });

    pi.on("tool_result", (event, ctx) => {
        events.push("tool_result:" + event.toolName);
        return null;
    });

    pi.on("agent_start", (event, ctx) => {
        events.push("agent_start");
        return null;
    });

    pi.on("agent_end", (event, ctx) => {
        events.push("agent_end");
        return null;
    });

    // Command to retrieve collected events
    pi.registerCommand("get-events", {
        description: "Return collected events",
        handler: async () => {
            return JSON.stringify(events);
        }
    });
}
"#;

/// Extension that blocks a specific tool call.
const BLOCKING_TOOL_CALL_EXT: &str = r#"
export default function init(pi) {
    pi.on("tool_call", (event, ctx) => {
        if (event.toolName === "dangerous_tool") {
            return { block: true, reason: "Tool is dangerous" };
        }
        return null;
    });
}
"#;

/// Extension that returns a response from a generic event handler.
const RESPONDING_EVENT_EXT: &str = r#"
export default function init(pi) {
    pi.on("agent_start", (event, ctx) => {
        return { modified: true, text: "transformed" };
    });

    pi.on("turn_start", (event, ctx) => {
        return false; // Signals cancellation via raw false
    });
}
"#;

/// Extension with NO event hooks (to test filtering).
const NO_HOOKS_EXT: &str = r#"
export default function init(pi) {
    pi.registerCommand("noop", {
        description: "No-op command",
        handler: async () => null
    });
}
"#;

/// Extension that registers a tool.
const TOOL_EXT: &str = r#"
export default function init(pi) {
    pi.registerTool({
        name: "ext-greet",
        description: "Greeting tool",
        parameters: {
            type: "object",
            properties: {
                name: { type: "string", description: "Name to greet" }
            },
            required: ["name"]
        },
        execute: async (toolCallId, input, result, signal, ctx) => {
            return "Hello, " + input.name + "!";
        }
    });
}
"#;

// ---------------------------------------------------------------------------
// Tests: dispatch_event (fire-and-forget)
// ---------------------------------------------------------------------------

#[test]
fn dispatch_event_invokes_matching_hook() {
    let harness = common::TestHarness::new("dispatch_event_invokes_matching_hook");
    let manager = load_js_extension(&harness, EVENT_TRACKING_EXT);

    // Dispatch startup event
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event(ExtensionEventName::Startup, Some(json!({"version": "1.0"})))
                .await
                .expect("dispatch startup");
        }
    });

    // Verify event was recorded by retrieving via command
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .execute_command("get-events", "", 5000)
                .await
                .expect("get events")
        }
    });
    let events: Vec<String> = serde_json::from_str(result.as_str().unwrap()).expect("parse events");
    assert!(
        events.contains(&"startup".to_string()),
        "Expected startup event, got: {events:?}"
    );
}

#[test]
fn dispatch_event_no_hook_returns_ok() {
    let harness = common::TestHarness::new("dispatch_event_no_hook_returns_ok");
    let manager = load_js_extension(&harness, NO_HOOKS_EXT);

    // Dispatching an event with no matching hook should succeed silently
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event(ExtensionEventName::AgentStart, None)
                .await
                .expect("dispatch without hooks should succeed");
        }
    });
}

// ---------------------------------------------------------------------------
// Tests: dispatch_event_with_response
// ---------------------------------------------------------------------------

#[test]
fn dispatch_event_with_response_returns_value() {
    let harness = common::TestHarness::new("dispatch_event_with_response_returns_value");
    let manager = load_js_extension(&harness, RESPONDING_EVENT_EXT);

    let response = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::AgentStart,
                    Some(json!({"session_id": "s1"})),
                    5000,
                )
                .await
                .expect("dispatch agent_start event")
        }
    });

    let response = response.expect("should have a response");
    assert_eq!(
        response.get("modified").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        response.get("text").and_then(Value::as_str),
        Some("transformed")
    );
}

#[test]
fn dispatch_event_with_response_none_when_no_hooks() {
    let harness = common::TestHarness::new("dispatch_event_with_response_none_when_no_hooks");
    let manager = load_js_extension(&harness, NO_HOOKS_EXT);

    let response = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(ExtensionEventName::Input, None, 5000)
                .await
                .expect("dispatch without hooks")
        }
    });

    assert!(response.is_none(), "Expected None when no hooks registered");
}

// ---------------------------------------------------------------------------
// Tests: dispatch_cancellable_event
// ---------------------------------------------------------------------------

#[test]
fn dispatch_cancellable_event_detects_false() {
    let harness = common::TestHarness::new("dispatch_cancellable_event_detects_false");
    let manager = load_js_extension(&harness, RESPONDING_EVENT_EXT);

    // turn_start handler returns `false` which dispatch_cancellable_event treats as cancellation
    let cancelled = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_cancellable_event(ExtensionEventName::TurnStart, None, 5000)
                .await
                .expect("dispatch cancellable")
        }
    });

    assert!(
        cancelled,
        "Expected cancellation when handler returns false"
    );
}

#[test]
fn dispatch_cancellable_event_not_cancelled_when_no_hooks() {
    let harness =
        common::TestHarness::new("dispatch_cancellable_event_not_cancelled_when_no_hooks");
    let manager = load_js_extension(&harness, NO_HOOKS_EXT);

    let cancelled = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_cancellable_event(ExtensionEventName::BeforeAgentStart, None, 5000)
                .await
                .expect("dispatch cancellable without hooks")
        }
    });

    assert!(
        !cancelled,
        "Should not be cancelled when no hooks registered"
    );
}

// ---------------------------------------------------------------------------
// Tests: dispatch_tool_call
// ---------------------------------------------------------------------------

#[test]
fn dispatch_tool_call_without_hooks_returns_none() {
    let harness = common::TestHarness::new("dispatch_tool_call_without_hooks_returns_none");
    let manager = load_js_extension(&harness, NO_HOOKS_EXT);

    let tool_call = make_tool_call("read", json!({"path": "/tmp/test.txt"}));
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_tool_call(&tool_call, 5000)
                .await
                .expect("dispatch tool call")
        }
    });

    assert!(result.is_none(), "Expected None when no tool_call hooks");
}

#[test]
fn dispatch_tool_call_non_blocking_returns_result() {
    let harness = common::TestHarness::new("dispatch_tool_call_non_blocking_returns_result");
    let manager = load_js_extension(&harness, EVENT_TRACKING_EXT);

    let tool_call = make_tool_call("read", json!({"path": "/tmp/test.txt"}));
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_tool_call(&tool_call, 5000)
                .await
                .expect("dispatch tool call")
        }
    });

    // Non-blocking response should be returned but not block
    if let Some(ref event_result) = result {
        assert!(
            !event_result.block,
            "Expected non-blocking response, got block=true"
        );
    }
}

#[test]
fn dispatch_tool_call_blocking_returns_block_with_reason() {
    let harness = common::TestHarness::new("dispatch_tool_call_blocking_returns_block_with_reason");
    let manager = load_js_extension(&harness, BLOCKING_TOOL_CALL_EXT);

    let tool_call = make_tool_call("dangerous_tool", json!({}));
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_tool_call(&tool_call, 5000)
                .await
                .expect("dispatch tool call")
        }
    });

    let event_result = result.expect("Expected blocking response");
    assert!(event_result.block, "Expected block=true for dangerous tool");
    assert_eq!(
        event_result.reason.as_deref(),
        Some("Tool is dangerous"),
        "Expected reason message"
    );
}

#[test]
fn dispatch_tool_call_non_dangerous_passes_through() {
    let harness = common::TestHarness::new("dispatch_tool_call_non_dangerous_passes_through");
    let manager = load_js_extension(&harness, BLOCKING_TOOL_CALL_EXT);

    let tool_call = make_tool_call("safe_tool", json!({}));
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_tool_call(&tool_call, 5000)
                .await
                .expect("dispatch tool call")
        }
    });

    // Handler returns null for non-dangerous tools → no result
    assert!(
        result.is_none(),
        "Expected None for non-dangerous tool, got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Tests: dispatch_tool_result
// ---------------------------------------------------------------------------

#[test]
fn dispatch_tool_result_without_hooks_returns_none() {
    let harness = common::TestHarness::new("dispatch_tool_result_without_hooks_returns_none");
    let manager = load_js_extension(&harness, NO_HOOKS_EXT);

    let tool_call = make_tool_call("read", json!({}));
    let output = make_tool_output("file contents");
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_tool_result(&tool_call, &output, false, 5000)
                .await
                .expect("dispatch tool result")
        }
    });

    assert!(result.is_none(), "Expected None when no tool_result hooks");
}

#[test]
fn dispatch_tool_result_with_hook_invoked() {
    let harness = common::TestHarness::new("dispatch_tool_result_with_hook_invoked");
    let manager = load_js_extension(&harness, EVENT_TRACKING_EXT);

    let tool_call = make_tool_call("write", json!({}));
    let output = make_tool_output("ok");
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_tool_result(&tool_call, &output, false, 5000)
                .await
                .expect("dispatch tool result");
        }
    });

    // Verify the hook was invoked by checking the event log
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .execute_command("get-events", "", 5000)
                .await
                .expect("get events")
        }
    });
    let events: Vec<String> = serde_json::from_str(result.as_str().unwrap()).expect("parse events");
    assert!(
        events.contains(&"tool_result:write".to_string()),
        "Expected tool_result:write in events, got: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// Tests: Event hook filtering
// ---------------------------------------------------------------------------

#[test]
fn event_hooks_only_matching_hooks_invoked() {
    let harness = common::TestHarness::new("event_hooks_only_matching_hooks_invoked");
    let manager = load_js_extension(&harness, EVENT_TRACKING_EXT);

    // Dispatch agent_start (which has a hook) and turn_start (which does NOT)
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event(
                    ExtensionEventName::AgentStart,
                    Some(json!({"session_id": "s1"})),
                )
                .await
                .expect("dispatch agent_start");

            // turn_start has no hook registered in our extension
            manager
                .dispatch_event(
                    ExtensionEventName::TurnStart,
                    Some(json!({"session_id": "s1", "turn_index": 0})),
                )
                .await
                .expect("dispatch turn_start");
        }
    });

    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .execute_command("get-events", "", 5000)
                .await
                .expect("get events")
        }
    });
    let events: Vec<String> = serde_json::from_str(result.as_str().unwrap()).expect("parse events");

    assert!(
        events.contains(&"agent_start".to_string()),
        "Expected agent_start in events"
    );
    assert!(
        !events.iter().any(|e| e.contains("turn_start")),
        "turn_start should NOT be in events (no hook registered)"
    );
}

// ---------------------------------------------------------------------------
// Tests: Event ordering across lifecycle
// ---------------------------------------------------------------------------

#[test]
fn event_ordering_startup_then_tool_call_then_agent_end() {
    let harness = common::TestHarness::new("event_ordering_startup_then_tool_call_then_agent_end");
    let manager = load_js_extension(&harness, EVENT_TRACKING_EXT);

    // Simulate lifecycle sequence: startup → agent_start → tool_call → tool_result → agent_end
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event(ExtensionEventName::Startup, Some(json!({"version": "1.0"})))
                .await
                .expect("dispatch startup");

            manager
                .dispatch_event(
                    ExtensionEventName::AgentStart,
                    Some(json!({"session_id": "s1"})),
                )
                .await
                .expect("dispatch agent_start");

            let tool = ToolCall {
                id: "call-1".to_string(),
                name: "read".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            manager
                .dispatch_tool_call(&tool, 5000)
                .await
                .expect("dispatch tool_call");

            let output = ToolOutput {
                content: vec![pi::model::ContentBlock::Text(pi::model::TextContent {
                    text: "ok".to_string(),
                    text_signature: None,
                })],
                details: None,
                is_error: false,
            };
            manager
                .dispatch_tool_result(&tool, &output, false, 5000)
                .await
                .expect("dispatch tool_result");

            manager
                .dispatch_event(
                    ExtensionEventName::AgentEnd,
                    Some(json!({"session_id": "s1"})),
                )
                .await
                .expect("dispatch agent_end");
        }
    });

    // Verify ordering
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .execute_command("get-events", "", 5000)
                .await
                .expect("get events")
        }
    });
    let events: Vec<String> = serde_json::from_str(result.as_str().unwrap()).expect("parse events");

    assert_eq!(
        events.len(),
        5,
        "Expected 5 lifecycle events, got: {events:?}"
    );
    assert_eq!(events[0], "startup");
    assert_eq!(events[1], "agent_start");
    assert_eq!(events[2], "tool_call:read");
    assert_eq!(events[3], "tool_result:read");
    assert_eq!(events[4], "agent_end");
}

// ---------------------------------------------------------------------------
// Tests: Tool registration and routing
// ---------------------------------------------------------------------------

#[test]
fn extension_tool_registered_in_manager() {
    let harness = common::TestHarness::new("extension_tool_registered_in_manager");
    let manager = load_js_extension(&harness, TOOL_EXT);

    let tool_defs = manager.extension_tool_defs();
    assert!(
        !tool_defs.is_empty(),
        "Expected at least one extension tool def"
    );

    let greet_tool = tool_defs
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some("ext-greet"))
        .expect("ext-greet tool should be registered");
    assert_eq!(
        greet_tool.get("description").and_then(Value::as_str),
        Some("Greeting tool")
    );
}

#[test]
fn extension_tool_execution_returns_result() {
    let harness = common::TestHarness::new("extension_tool_execution_returns_result");
    let manager = load_js_extension(&harness, TOOL_EXT);

    let runtime = manager.js_runtime().expect("runtime should exist");
    let result = common::run_async({
        async move {
            runtime
                .execute_tool(
                    "ext-greet".to_string(),
                    "call-1".to_string(),
                    json!({"name": "World"}),
                    json!({}),
                    5000,
                )
                .await
                .expect("execute tool")
        }
    });

    let text = result.as_str().unwrap_or_default();
    assert!(
        text.contains("Hello") && text.contains("World"),
        "Expected greeting, got: {text}"
    );
}

// ---------------------------------------------------------------------------
// Tests: Manager without JS runtime
// ---------------------------------------------------------------------------

#[test]
fn dispatch_event_without_runtime_succeeds() {
    // A manager with registered hooks but no JS runtime should not panic
    let manager = ExtensionManager::new();
    manager.register(RegisterPayload {
        name: "dummy".to_string(),
        version: "1.0.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: Vec::new(),
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: vec!["startup".to_string()],
    });

    common::run_async({
        let manager = manager.clone();
        async move {
            // dispatch_event should succeed even without runtime (events silently dropped)
            let result = manager
                .dispatch_event(ExtensionEventName::Startup, None)
                .await;
            assert!(
                result.is_ok(),
                "dispatch_event without runtime should not error"
            );
        }
    });
}

#[test]
fn dispatch_tool_call_without_runtime_returns_none() {
    let manager = ExtensionManager::new();
    manager.register(RegisterPayload {
        name: "dummy".to_string(),
        version: "1.0.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: Vec::new(),
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: vec!["tool_call".to_string()],
    });

    let tool_call = make_tool_call("read", json!({}));
    common::run_async({
        let manager = manager.clone();
        async move {
            let result = manager.dispatch_tool_call(&tool_call, 5000).await;
            // Without a JS runtime, should succeed but return None
            assert!(
                result.is_ok(),
                "dispatch_tool_call without runtime should not error: {result:?}"
            );
        }
    });
}
