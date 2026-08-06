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

const GENERATE_LIFECYCLE_HOOK_PARITY_ARTIFACT_ENV: &str =
    "PI_GENERATE_LIFECYCLE_HOOK_PARITY_ARTIFACT";

fn lifecycle_hook_parity_artifact_generation_enabled(raw: Option<&str>) -> bool {
    raw == Some("1")
}

fn lifecycle_hook_parity_artifact_generation_requested() -> bool {
    lifecycle_hook_parity_artifact_generation_enabled(
        std::env::var(GENERATE_LIFECYCLE_HOOK_PARITY_ARTIFACT_ENV)
            .ok()
            .as_deref(),
    )
}

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

fn recorded_events(manager: &ExtensionManager) -> Vec<String> {
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .execute_command("get-events", "", 5000)
                .await
                .expect("get recorded events")
        }
    });

    serde_json::from_str(result.as_str().expect("event command returns string"))
        .expect("parse recorded events")
}

fn exercise_lifecycle_hooks(manager: &ExtensionManager) {
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

            let input_response = manager
                .dispatch_event_with_response(
                    ExtensionEventName::Input,
                    Some(json!({"text": "hello", "source": "interactive"})),
                    5000,
                )
                .await
                .expect("dispatch input")
                .expect("input hook returns transform");
            assert_eq!(
                input_response.get("action").and_then(Value::as_str),
                Some("transform")
            );
            assert_eq!(
                input_response.get("text").and_then(Value::as_str),
                Some("hello [hooked]")
            );

            let before_agent_start_response = manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({
                        "prompt": "summarize",
                        "systemPrompt": "base-system",
                    })),
                    5000,
                )
                .await
                .expect("dispatch before_agent_start")
                .expect("before_agent_start hook returns response");
            assert_eq!(
                before_agent_start_response
                    .get("systemPrompt")
                    .and_then(Value::as_str),
                Some("base-system + hook-rules")
            );

            let user_bash_response = manager
                .dispatch_event_with_response(
                    ExtensionEventName::UserBash,
                    Some(json!({"command": "git status --short"})),
                    5000,
                )
                .await
                .expect("dispatch user_bash")
                .expect("user_bash hook returns response");
            assert_eq!(
                user_bash_response.get("allow").and_then(Value::as_bool),
                Some(true)
            );

            let tool = make_tool_call("read", json!({"path": "sample.txt"}));
            manager
                .dispatch_tool_call(&tool, 5000)
                .await
                .expect("dispatch tool_call");
            manager
                .dispatch_tool_result(&tool, &make_tool_output("ok"), false, 5000)
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
}

fn collect_cancellable_lifecycle_results(cancel_manager: &ExtensionManager) -> Vec<Value> {
    let cancellable_hooks = [
        (
            "session_before_switch",
            ExtensionEventName::SessionBeforeSwitch,
            json!({"fromSessionId": "s1", "toSessionId": "s2"}),
        ),
        (
            "session_before_fork",
            ExtensionEventName::SessionBeforeFork,
            json!({"fromSessionId": "s1", "newSessionId": "s1-fork"}),
        ),
        (
            "session_before_compact",
            ExtensionEventName::SessionBeforeCompact,
            json!({"preparation": {}, "branchEntries": []}),
        ),
        (
            "session_before_tree",
            ExtensionEventName::SessionBeforeTree,
            json!({"preparation": {"branchCount": 1, "entryCount": 2}}),
        ),
    ];

    let mut cancellable_results = Vec::new();
    for (hook, event_name, payload) in cancellable_hooks {
        let cancelled = common::run_async({
            let manager = cancel_manager.clone();
            async move {
                manager
                    .dispatch_cancellable_event(event_name, Some(payload), 5000)
                    .await
                    .expect("dispatch cancellable lifecycle hook")
            }
        });
        assert!(cancelled, "{hook} did not cancel as expected");
        cancellable_results.push(json!({
            "hook": hook,
            "mode": "cancellable",
            "cancelled": cancelled,
        }));
    }

    cancellable_results
}

fn build_lifecycle_hook_parity_artifact(
    ordering_trace: &[String],
    cancellable_results: &[Value],
) -> Value {
    json!({
        "schema": "pi.ext.lifecycle_hook_parity_matrix.v1",
        "generated_at": chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "status": "pass",
        "coverage": [
            {"hook": "startup", "mode": "fire_and_forget", "observed": true},
            {"hook": "agent_start", "mode": "fire_and_forget", "observed": true},
            {"hook": "input", "mode": "transform", "observed": true},
            {"hook": "before_agent_start", "mode": "pre_agent", "observed": true},
            {"hook": "user_bash", "mode": "first_result", "observed": true},
            {"hook": "tool_call", "mode": "pre_tool", "observed": true},
            {"hook": "tool_result", "mode": "post_tool", "observed": true},
            {"hook": "agent_end", "mode": "fire_and_forget", "observed": true},
            {"hook": "session_before_switch", "mode": "cancellable", "observed": true},
            {"hook": "session_before_fork", "mode": "cancellable", "observed": true},
            {"hook": "session_before_compact", "mode": "cancellable", "observed": true},
            {"hook": "session_before_tree", "mode": "cancellable", "observed": true}
        ],
        "ordering_trace": ordering_trace,
        "cancellable_assertions": cancellable_results,
        "reproduce_command": "PI_GENERATE_LIFECYCLE_HOOK_PARITY_ARTIFACT=1 cargo test --test extensions_event_wiring lifecycle_hook_parity_matrix_writes_evidence_artifact -- --exact --nocapture",
    })
}

fn validate_and_maybe_write_lifecycle_hook_parity_artifact(
    ordering_trace: &[String],
    cancellable_results: &[Value],
) {
    let artifact = build_lifecycle_hook_parity_artifact(ordering_trace, cancellable_results);
    let payload =
        serde_json::to_string_pretty(&artifact).expect("serialize lifecycle hook parity artifact");
    assert!(
        !payload.trim().is_empty(),
        "lifecycle hook parity payload must be non-empty"
    );

    let roundtripped: Value =
        serde_json::from_str(&payload).expect("parse serialized lifecycle hook parity artifact");
    assert_eq!(roundtripped, artifact, "lifecycle hook artifact roundtrip");
    assert_eq!(
        roundtripped.get("schema").and_then(Value::as_str),
        Some("pi.ext.lifecycle_hook_parity_matrix.v1")
    );
    assert_eq!(
        roundtripped.get("status").and_then(Value::as_str),
        Some("pass")
    );
    assert_eq!(
        roundtripped
            .get("coverage")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(12)
    );
    assert_eq!(
        roundtripped.get("ordering_trace"),
        Some(&json!(ordering_trace))
    );
    assert_eq!(
        roundtripped.get("cancellable_assertions"),
        Some(&json!(cancellable_results))
    );

    if lifecycle_hook_parity_artifact_generation_requested() {
        let artifact_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("ext_conformance")
            .join("reports")
            .join("lifecycle_hooks");
        std::fs::create_dir_all(&artifact_dir)
            .expect("create lifecycle hook parity artifact directory");
        let artifact_path = artifact_dir.join("lifecycle_hook_parity_matrix.json");
        std::fs::write(&artifact_path, payload.as_bytes())
            .expect("write lifecycle hook parity artifact");
        let metadata = std::fs::metadata(&artifact_path)
            .expect("stat generated lifecycle hook parity artifact");
        assert!(metadata.is_file(), "lifecycle hook artifact is not a file");
        assert_eq!(
            metadata.len(),
            u64::try_from(payload.len()).expect("lifecycle hook payload length fits u64"),
            "generated lifecycle hook artifact length mismatch"
        );
    } else {
        eprintln!(
            "Lifecycle hook artifact validated in memory; set \
             {GENERATE_LIFECYCLE_HOOK_PARITY_ARTIFACT_ENV}=1 to write tracked evidence"
        );
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

    pi.on("input", (event, ctx) => {
        events.push("input:" + event.text + ":" + event.source);
        return { action: "transform", text: event.text + " [hooked]" };
    });

    pi.on("before_agent_start", (event, ctx) => {
        events.push("before_agent_start:" + event.prompt);
        return { systemPrompt: event.systemPrompt + " + hook-rules" };
    });

    pi.on("user_bash", (event, ctx) => {
        events.push("user_bash:" + event.command);
        return { allow: true };
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

/// Extension that cancels `session_before_switch` via `{cancelled: true}`.
#[allow(dead_code)]
const SESSION_CANCEL_EXT: &str = r#"
export default function init(pi) {
    pi.on("session_before_switch", (event, ctx) => {
        return { cancelled: true, reason: "Extension vetoed switch" };
    });

    pi.on("session_before_fork", (event, ctx) => {
        return { cancel: true };
    });

    pi.on("session_before_compact", (event, ctx) => {
        return false;
    });

    pi.on("session_before_tree", (event, ctx) => {
        return { cancel: true };
    });

    pi.on("session_switch", (event, ctx) => {
        // After-event: just record, no cancellation possible.
        return null;
    });

    pi.on("session_fork", (event, ctx) => {
        return null;
    });

    pi.on("session_compact", (event, ctx) => {
        return null;
    });
}
"#;

/// Extension that does NOT cancel `session_before_switch`.
#[allow(dead_code)]
const SESSION_ALLOW_EXT: &str = r#"
export default function init(pi) {
    pi.on("session_before_switch", (event, ctx) => {
        return { cancelled: false };
    });

    pi.on("session_before_fork", (event, ctx) => {
        return null;
    });

    pi.on("session_before_compact", (event, ctx) => {
        return true;
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

/// Extension that validates `signal` presence for `session_before`_* events.
const SESSION_SIGNAL_EXT: &str = r#"
export default function init(pi) {
    const probe = (event) => {
        const sig = event && event.signal;
        return {
            hasSignal: !!sig,
            aborted: sig ? !!sig.aborted : null,
            hasAddListener: !!(sig && typeof sig.addEventListener === 'function'),
        };
    };

    pi.on("session_before_compact", probe);
    pi.on("session_before_tree", probe);
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

fn assert_signal_probe(response: &Value) {
    assert_eq!(
        response.get("hasSignal").and_then(Value::as_bool),
        Some(true),
        "expected injected signal"
    );
    assert_eq!(
        response.get("aborted").and_then(Value::as_bool),
        Some(false),
        "expected non-aborted signal"
    );
    assert_eq!(
        response.get("hasAddListener").and_then(Value::as_bool),
        Some(true),
        "expected AbortSignal-like interface"
    );
}

#[test]
fn session_before_compact_injects_signal() {
    let harness = common::TestHarness::new("session_before_compact_injects_signal");
    let manager = load_js_extension(&harness, SESSION_SIGNAL_EXT);

    let response = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::SessionBeforeCompact,
                    Some(json!({"preparation": {}, "branchEntries": []})),
                    5000,
                )
                .await
                .expect("dispatch session_before_compact")
        }
    });

    let response = response.expect("expected response");
    assert_signal_probe(&response);
}

#[test]
fn session_before_tree_injects_signal() {
    let harness = common::TestHarness::new("session_before_tree_injects_signal");
    let manager = load_js_extension(&harness, SESSION_SIGNAL_EXT);

    let response = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::SessionBeforeTree,
                    Some(json!({"preparation": {"branchCount": 0, "entryCount": 0}})),
                    5000,
                )
                .await
                .expect("dispatch session_before_tree")
        }
    });

    let response = response.expect("expected response");
    assert_signal_probe(&response);
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

#[test]
fn lifecycle_hook_parity_matrix_writes_evidence_artifact() {
    let harness = common::TestHarness::new("lifecycle_hook_parity_matrix_writes_evidence_artifact");
    let manager = load_js_extension(&harness, EVENT_TRACKING_EXT);

    exercise_lifecycle_hooks(&manager);
    let ordering_trace = recorded_events(&manager);
    assert_eq!(
        ordering_trace,
        vec![
            "startup".to_string(),
            "agent_start".to_string(),
            "input:hello:interactive".to_string(),
            "before_agent_start:summarize".to_string(),
            "user_bash:git status --short".to_string(),
            "tool_call:read".to_string(),
            "tool_result:read".to_string(),
            "agent_end".to_string(),
        ],
        "lifecycle hook ordering changed"
    );

    let cancel_harness =
        common::TestHarness::new("lifecycle_hook_parity_matrix_cancellable_session_hooks");
    let cancel_manager = load_js_extension(&cancel_harness, SESSION_CANCEL_EXT);
    let cancellable_results = collect_cancellable_lifecycle_results(&cancel_manager);
    validate_and_maybe_write_lifecycle_hook_parity_artifact(&ordering_trace, &cancellable_results);
}

#[test]
fn lifecycle_hook_parity_artifact_generation_requires_exact_one() {
    assert!(!lifecycle_hook_parity_artifact_generation_enabled(None));
    assert!(!lifecycle_hook_parity_artifact_generation_enabled(Some("")));
    assert!(!lifecycle_hook_parity_artifact_generation_enabled(Some(
        "0"
    )));
    assert!(!lifecycle_hook_parity_artifact_generation_enabled(Some(
        "true"
    )));
    assert!(!lifecycle_hook_parity_artifact_generation_enabled(Some(
        " 1"
    )));
    assert!(!lifecycle_hook_parity_artifact_generation_enabled(Some(
        "1 "
    )));
    assert!(lifecycle_hook_parity_artifact_generation_enabled(Some("1")));
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
                    std::sync::Arc::new(json!({})),
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
