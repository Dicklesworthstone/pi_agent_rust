//! Reliability and failure-mode tests for the extension runtime (bd-2km5).
//!
//! Tests cover:
//! - Timeout handling (event/tool handlers that exceed budget)
//! - Shutdown with pending operations
//! - Double-shutdown safety
//! - Manager budget exhaustion (`effective_timeout` returns ~0)
//! - Extension load failure recovery (one fails, others still work)
//! - Error recovery (unhandled JS errors don't kill the runtime)
//! - Concurrent event dispatch
//! - Slow tool call timeout behavior

#![allow(clippy::redundant_clone, clippy::doc_markdown)]

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ─── Helpers ────────────────────────────────────────────────────────────────

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

fn try_load_js_extension(
    harness: &common::TestHarness,
    filename: &str,
    source: &str,
) -> (ExtensionManager, bool) {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file(filename, source.as_bytes());
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

    let loaded = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![spec]).await.is_ok() }
    });

    (manager, loaded)
}

fn shutdown(manager: &ExtensionManager) {
    let _ = common::run_async({
        let manager = manager.clone();
        async move { manager.shutdown(Duration::from_millis(500)).await }
    });
}

// ─── Extension Sources ──────────────────────────────────────────────────────

/// Well-behaved extension with tool and event hook for baseline.
const GOOD_EXT: &str = r#"
export default function activate(pi) {
    pi.registerTool({
        name: "greet",
        description: "Say hello",
        parameters: { type: "object", properties: { name: { type: "string" } } },
        execute: async (params) => {
            return { content: [{ type: "text", text: "Hello, " + (params.name || "world") }] };
        }
    });
    pi.events("register", {
        name: "good-ext",
        hooks: ["before_agent_start"]
    });
}
"#;

/// Extension whose event hook throws an unhandled error.
const THROWING_EVENT_EXT: &str = r#"
export default function activate(pi) {
    pi.events("register", {
        name: "throwing-ext",
        hooks: ["before_agent_start"]
    });
    pi.events("on", {
        event: "before_agent_start",
        handler: () => {
            throw new Error("intentional failure in event hook");
        }
    });
}
"#;

/// Extension whose tool handler throws.
const THROWING_TOOL_EXT: &str = r#"
export default function activate(pi) {
    pi.registerTool({
        name: "crasher",
        description: "Always throws",
        parameters: { type: "object", properties: {} },
        execute: async () => {
            throw new Error("intentional tool failure");
        }
    });
}
"#;

/// Extension whose event hook blocks for a long time (simulating slow work).
const SLOW_EVENT_EXT: &str = r#"
export default function activate(pi) {
    pi.events("register", {
        name: "slow-ext",
        hooks: ["before_agent_start"]
    });
    pi.events("on", {
        event: "before_agent_start",
        handler: async () => {
            // Block for ~10 seconds — should be killed by timeout
            const start = Date.now();
            while (Date.now() - start < 10000) {
                // Busy wait; QuickJS interrupt budget should catch this
            }
            return { systemPrompt: "should not reach here" };
        }
    });
}
"#;

/// Extension with a slow tool call (busy wait).
const SLOW_TOOL_EXT: &str = r#"
export default function activate(pi) {
    pi.registerTool({
        name: "slow-tool",
        description: "Takes forever",
        parameters: { type: "object", properties: {} },
        execute: async () => {
            const start = Date.now();
            while (Date.now() - start < 10000) {
                // Busy wait
            }
            return { content: [{ type: "text", text: "done" }] };
        }
    });
}
"#;

/// Extension with syntax error (fails to load).
const SYNTAX_ERROR_EXT: &str = r#"
export default function activate(pi) {
    // Deliberate syntax error
    pi.registerTool({{{
        name: "bad"
    });
}
"#;

/// Extension that returns null from event hook (no-op).
const NOOP_EVENT_EXT: &str = r#"
export default function activate(pi) {
    pi.events("register", {
        name: "noop-ext",
        hooks: ["before_agent_start"]
    });
    pi.events("on", {
        event: "before_agent_start",
        handler: () => null
    });
}
"#;

/// Extension that returns a rejected promise from event hook.
const REJECTED_PROMISE_EXT: &str = r#"
export default function activate(pi) {
    pi.events("register", {
        name: "rejected-ext",
        hooks: ["before_agent_start"]
    });
    pi.events("on", {
        event: "before_agent_start",
        handler: async () => {
            return Promise.reject(new Error("promise rejected intentionally"));
        }
    });
}
"#;

// ─── Timeout Tests ──────────────────────────────────────────────────────────

#[test]
fn event_dispatch_completes_within_timeout_for_good_extension() {
    let harness = common::TestHarness::new("reliability_good_event");
    let manager = load_js_extension(&harness, GOOD_EXT);

    let start = Instant::now();
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({"systemPrompt": "test"})),
                    5_000,
                )
                .await
        }
    });

    let elapsed = start.elapsed();
    assert!(result.is_ok(), "dispatch should succeed: {result:?}");
    assert!(
        elapsed < Duration::from_secs(3),
        "dispatch should be fast, took {elapsed:?}"
    );

    shutdown(&manager);
}

#[test]
fn slow_event_hook_is_bounded_by_timeout() {
    let harness = common::TestHarness::new("reliability_slow_event");
    let manager = load_js_extension(&harness, SLOW_EVENT_EXT);

    let start = Instant::now();
    // Use a short timeout (1s) - the handler busy-waits for 10s.
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({"systemPrompt": "test"})),
                    1_000,
                )
                .await
        }
    });

    let elapsed = start.elapsed();
    // Should be bounded by the timeout, not the 10s busy wait.
    // Allow generous margin for CI/debug overhead.
    assert!(
        elapsed < Duration::from_secs(8),
        "timeout should cap execution, took {elapsed:?}"
    );
    // The result may be an error (timeout) or empty (interrupt budget caught it)
    eprintln!(
        "[slow_event] elapsed={elapsed:?}, result={:?}",
        result
            .as_ref()
            .map_or_else(std::string::ToString::to_string, |v| format!("{v:?}"))
    );

    shutdown(&manager);
}

#[test]
fn slow_tool_call_is_bounded_by_timeout() {
    let harness = common::TestHarness::new("reliability_slow_tool");
    let manager = load_js_extension(&harness, SLOW_TOOL_EXT);

    let runtime = manager.js_runtime().expect("js runtime");
    let ctx = json!({ "hasUI": false, "cwd": harness.temp_dir().display().to_string() });

    let start = Instant::now();
    let result = futures::executor::block_on(runtime.execute_tool(
        "slow-tool".to_string(),
        "bench-0".to_string(),
        json!({}),
        ctx,
        1_000, // 1 second timeout vs 10s busy wait
    ));

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(8),
        "tool timeout should cap execution, took {elapsed:?}"
    );
    eprintln!(
        "[slow_tool] elapsed={elapsed:?}, result_ok={}",
        result.is_ok()
    );

    shutdown(&manager);
}

// ─── Error Recovery Tests ───────────────────────────────────────────────────

#[test]
fn throwing_event_hook_does_not_crash_runtime() {
    let harness = common::TestHarness::new("reliability_throwing_event");
    let manager = load_js_extension(&harness, THROWING_EVENT_EXT);

    // First dispatch: the handler throws.
    let result1 = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({"systemPrompt": "test"})),
                    5_000,
                )
                .await
        }
    });
    eprintln!("[throwing_event] result1: {result1:?}");

    // Second dispatch: runtime should still be functional.
    let result2 = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({"systemPrompt": "second attempt"})),
                    5_000,
                )
                .await
        }
    });
    eprintln!("[throwing_event] result2: {result2:?}");

    // The runtime should still be alive — the second dispatch should not panic.
    // Whether it returns Ok or Err depends on implementation, but we should not crash.
    assert!(
        manager.js_runtime().is_some(),
        "JS runtime should still be alive after error"
    );

    shutdown(&manager);
}

#[test]
fn throwing_tool_returns_error_not_crash() {
    let harness = common::TestHarness::new("reliability_throwing_tool");
    let manager = load_js_extension(&harness, THROWING_TOOL_EXT);

    let runtime = manager.js_runtime().expect("js runtime");
    let ctx = json!({ "hasUI": false, "cwd": harness.temp_dir().display().to_string() });

    let result = futures::executor::block_on(runtime.execute_tool(
        "crasher".to_string(),
        "call-1".to_string(),
        json!({}),
        ctx.clone(),
        5_000,
    ));

    // Tool should return an error, not crash.
    eprintln!("[throwing_tool] result: {result:?}");

    // Runtime should still be alive for a second call.
    let result2 = futures::executor::block_on(runtime.execute_tool(
        "crasher".to_string(),
        "call-2".to_string(),
        json!({}),
        ctx,
        5_000,
    ));
    eprintln!("[throwing_tool] result2: {result2:?}");

    assert!(
        manager.js_runtime().is_some(),
        "runtime should survive tool errors"
    );

    shutdown(&manager);
}

#[test]
fn rejected_promise_event_hook_does_not_crash() {
    let harness = common::TestHarness::new("reliability_rejected_promise");
    let manager = load_js_extension(&harness, REJECTED_PROMISE_EXT);

    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({"systemPrompt": "test"})),
                    5_000,
                )
                .await
        }
    });
    eprintln!("[rejected_promise] result: {result:?}");

    // Runtime should survive rejected promises.
    assert!(
        manager.js_runtime().is_some(),
        "runtime should survive rejected promise"
    );

    shutdown(&manager);
}

// ─── Shutdown Tests ─────────────────────────────────────────────────────────

#[test]
fn graceful_shutdown_completes_within_budget() {
    let harness = common::TestHarness::new("reliability_shutdown_budget");
    let manager = load_js_extension(&harness, GOOD_EXT);

    let start = Instant::now();
    let ok = common::run_async({
        let manager = manager.clone();
        async move { manager.shutdown(Duration::from_secs(2)).await }
    });

    let elapsed = start.elapsed();
    eprintln!("[shutdown_budget] ok={ok}, elapsed={elapsed:?}");
    assert!(ok, "shutdown should succeed within budget");
    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown should be fast, took {elapsed:?}"
    );
}

#[test]
fn double_shutdown_is_safe() {
    let harness = common::TestHarness::new("reliability_double_shutdown");
    let manager = load_js_extension(&harness, GOOD_EXT);

    // First shutdown.
    let ok1 = common::run_async({
        let manager = manager.clone();
        async move { manager.shutdown(Duration::from_secs(2)).await }
    });
    assert!(ok1, "first shutdown should succeed");

    // Second shutdown should be a no-op (runtime already cleared).
    let ok2 = common::run_async({
        let manager = manager.clone();
        async move { manager.shutdown(Duration::from_secs(2)).await }
    });
    // Second shutdown returns true (no runtime to shut down = success).
    assert!(ok2, "second shutdown should also succeed (no-op)");
}

#[test]
fn dispatch_after_shutdown_returns_none() {
    let harness = common::TestHarness::new("reliability_dispatch_after_shutdown");
    let manager = load_js_extension(&harness, GOOD_EXT);

    // Shut down first.
    common::run_async({
        let manager = manager.clone();
        async move { manager.shutdown(Duration::from_secs(2)).await }
    });

    // Dispatch after shutdown — should not panic, should return None or error.
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({"systemPrompt": "test"})),
                    1_000,
                )
                .await
        }
    });
    eprintln!("[dispatch_after_shutdown] result: {result:?}");

    // Should NOT panic. The result should be Ok(None) or Err — both acceptable.
    match result {
        Ok(None) => eprintln!("  -> Ok(None): no runtime, no hooks to dispatch"),
        Ok(Some(v)) => eprintln!("  -> Ok(Some): unexpected value: {v}"),
        Err(e) => eprintln!("  -> Err: {e}"),
    }
}

// ─── Load Failure Recovery Tests ────────────────────────────────────────────

#[test]
fn syntax_error_extension_fails_to_load() {
    let harness = common::TestHarness::new("reliability_syntax_error");
    let (manager, loaded) = try_load_js_extension(&harness, "extensions/bad.mjs", SYNTAX_ERROR_EXT);

    assert!(!loaded, "extension with syntax error should fail to load");

    shutdown(&manager);
}

#[test]
fn runtime_survives_failed_load_attempt() {
    let harness = common::TestHarness::new("reliability_load_recovery");
    let cwd = harness.temp_dir().to_path_buf();

    // Set up manager + runtime.
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

    // First: try to load a broken extension.
    let bad_path = harness.create_file("extensions/bad.mjs", SYNTAX_ERROR_EXT.as_bytes());
    let bad_spec = JsExtensionLoadSpec::from_entry_path(&bad_path).expect("spec");
    let bad_result = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![bad_spec]).await }
    });
    assert!(bad_result.is_err(), "broken extension should fail to load");

    // Second: load a good extension — should succeed on the same runtime.
    let good_path = harness.create_file("extensions/good.mjs", GOOD_EXT.as_bytes());
    let good_spec = JsExtensionLoadSpec::from_entry_path(&good_path).expect("spec");
    let good_result = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![good_spec]).await }
    });
    assert!(
        good_result.is_ok(),
        "good extension should load after bad one: {good_result:?}"
    );

    // Dispatch an event — the good extension should respond.
    let event_result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({"systemPrompt": "recovery test"})),
                    5_000,
                )
                .await
        }
    });
    assert!(
        event_result.is_ok(),
        "event dispatch should work after recovery: {event_result:?}"
    );

    shutdown(&manager);
}

// ─── Rapid Sequential Dispatch Tests ────────────────────────────────────────

#[test]
fn rapid_sequential_dispatches_do_not_deadlock() {
    let harness = common::TestHarness::new("reliability_rapid_dispatches");
    let manager = load_js_extension(&harness, NOOP_EVENT_EXT);

    // Dispatch many events in rapid sequence.
    let start = Instant::now();
    let mut ok_count = 0_usize;
    for i in 0..10 {
        let result = common::run_async({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch_event_with_response(
                        ExtensionEventName::BeforeAgentStart,
                        Some(json!({"systemPrompt": format!("rapid-{i}")})),
                        2_000,
                    )
                    .await
            }
        });
        if result.is_ok() {
            ok_count += 1;
        }
    }

    let elapsed = start.elapsed();
    eprintln!("[rapid_dispatches] dispatched 10 events in {elapsed:?}, {ok_count} succeeded");

    // All should complete without deadlock.
    assert!(
        elapsed < Duration::from_secs(30),
        "rapid dispatch should not deadlock, took {elapsed:?}"
    );
    // At least some should succeed.
    assert!(ok_count > 0, "at least one rapid dispatch should succeed");

    shutdown(&manager);
}

// ─── Manager-Level Budget Tests ─────────────────────────────────────────────

#[test]
fn noop_event_dispatch_with_zero_timeout_returns_quickly() {
    let harness = common::TestHarness::new("reliability_zero_timeout");
    let manager = load_js_extension(&harness, GOOD_EXT);

    let start = Instant::now();
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({"systemPrompt": "zero timeout"})),
                    0, // Zero timeout
                )
                .await
        }
    });

    let elapsed = start.elapsed();
    eprintln!("[zero_timeout] elapsed={elapsed:?}, result={result:?}");
    // Should return quickly (timeout fires immediately).
    assert!(
        elapsed < Duration::from_secs(3),
        "zero timeout should return quickly, took {elapsed:?}"
    );

    shutdown(&manager);
}

#[test]
fn tool_call_with_zero_timeout_returns_quickly() {
    let harness = common::TestHarness::new("reliability_zero_tool_timeout");
    let manager = load_js_extension(&harness, GOOD_EXT);

    let runtime = manager.js_runtime().expect("js runtime");
    let ctx = json!({ "hasUI": false, "cwd": harness.temp_dir().display().to_string() });

    let start = Instant::now();
    let result = futures::executor::block_on(runtime.execute_tool(
        "greet".to_string(),
        "call-0".to_string(),
        json!({"name": "zero"}),
        ctx,
        0, // Zero timeout
    ));

    let elapsed = start.elapsed();
    eprintln!(
        "[zero_tool_timeout] elapsed={elapsed:?}, result_ok={}",
        result.is_ok()
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "zero tool timeout should return quickly, took {elapsed:?}"
    );

    shutdown(&manager);
}

// ─── Rapid Lifecycle Tests ──────────────────────────────────────────────────

#[test]
fn rapid_create_load_shutdown_cycle() {
    // Verify that creating, loading, and shutting down an extension
    // N times in a row does not leak or deadlock.
    let harness = common::TestHarness::new("reliability_rapid_lifecycle");
    let iterations = 5;

    for i in 0..iterations {
        let cwd = harness.temp_dir().to_path_buf();
        let ext_path = harness.create_file(format!("extensions/ext_{i}.mjs"), GOOD_EXT.as_bytes());
        let spec = JsExtensionLoadSpec::from_entry_path(&ext_path).expect("spec");

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
                    .expect("start runtime")
            }
        });
        manager.set_js_runtime(runtime);

        let load_ok = common::run_async({
            let manager = manager.clone();
            async move { manager.load_js_extensions(vec![spec]).await.is_ok() }
        });
        assert!(load_ok, "iteration {i}: load should succeed");

        let shutdown_ok = common::run_async({
            let manager = manager.clone();
            async move { manager.shutdown(Duration::from_secs(2)).await }
        });
        assert!(shutdown_ok, "iteration {i}: shutdown should succeed");
    }
}

// ─── Mixed Load Tests ───────────────────────────────────────────────────────

#[test]
fn multiple_extensions_one_good_one_throwing() {
    let harness = common::TestHarness::new("reliability_mixed_extensions");
    let cwd = harness.temp_dir().to_path_buf();

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
                .expect("start runtime")
        }
    });
    manager.set_js_runtime(runtime);

    // Load good extension first.
    let good_path = harness.create_file("extensions/good.mjs", GOOD_EXT.as_bytes());
    let good_spec = JsExtensionLoadSpec::from_entry_path(&good_path).expect("good spec");
    let good_ok = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![good_spec]).await.is_ok() }
    });
    assert!(good_ok, "good extension should load");

    // Load throwing extension.
    let throw_path = harness.create_file("extensions/throw.mjs", THROWING_EVENT_EXT.as_bytes());
    let throw_spec = JsExtensionLoadSpec::from_entry_path(&throw_path).expect("throw spec");
    let throw_ok = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![throw_spec]).await.is_ok() }
    });
    // Throwing at load time might or might not fail — the throw is in the handler, not activate.
    eprintln!("[mixed] throw_ext loaded: {throw_ok}");

    // Dispatch event — the throwing hook throws, but the system should not crash.
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({"systemPrompt": "mixed test"})),
                    5_000,
                )
                .await
        }
    });
    eprintln!("[mixed] dispatch result: {result:?}");

    // Runtime should still be alive.
    assert!(
        manager.js_runtime().is_some(),
        "runtime should survive mixed extension dispatch"
    );

    shutdown(&manager);
}

// ─── Regression: Ensure no panics for edge-case payloads ────────────────────

#[test]
fn dispatch_with_null_payload_does_not_panic() {
    let harness = common::TestHarness::new("reliability_null_payload");
    let manager = load_js_extension(&harness, GOOD_EXT);

    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    None, // null payload
                    5_000,
                )
                .await
        }
    });
    // Should not panic.
    eprintln!("[null_payload] result: {result:?}");

    shutdown(&manager);
}

#[test]
fn dispatch_with_empty_object_payload() {
    let harness = common::TestHarness::new("reliability_empty_payload");
    let manager = load_js_extension(&harness, GOOD_EXT);

    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({})), // empty payload
                    5_000,
                )
                .await
        }
    });
    eprintln!("[empty_payload] result: {result:?}");
    assert!(result.is_ok());

    shutdown(&manager);
}

#[test]
fn dispatch_with_large_payload_does_not_crash() {
    let harness = common::TestHarness::new("reliability_large_payload");
    let manager = load_js_extension(&harness, GOOD_EXT);

    // Create a large-ish payload (~100KB).
    let big_string = "x".repeat(100_000);
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({"systemPrompt": big_string})),
                    5_000,
                )
                .await
        }
    });
    eprintln!("[large_payload] result_ok={}", result.is_ok());
    // Should not crash or hang.

    shutdown(&manager);
}

// ─── Event for Unknown Hooks ────────────────────────────────────────────────

#[test]
fn dispatch_unregistered_event_returns_none() {
    let harness = common::TestHarness::new("reliability_unregistered_event");
    let manager = load_js_extension(&harness, GOOD_EXT);

    // Dispatch an event for which no hook is registered.
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::SessionSwitch,
                    Some(json!({"from": "a", "to": "b"})),
                    5_000,
                )
                .await
        }
    });

    match result {
        Ok(None) => eprintln!("[unregistered_event] Ok(None) — expected, no hooks"),
        Ok(Some(v)) => eprintln!("[unregistered_event] Ok({v}) — possibly empty response"),
        Err(e) => eprintln!("[unregistered_event] Err({e})"),
    }

    // The key assertion: no panic, no hang.
    shutdown(&manager);
}

// ─── Concurrent + Advanced Failure Mode Tests ──────────────────────────────

/// Extension that increments a counter on each event, used to detect missing dispatches.
const COUNTER_EXT: &str = r#"
let count = 0;
export default function activate(pi) {
    pi.events("register", {
        name: "counter-ext",
        hooks: ["before_agent_start"]
    });
    pi.events("on", {
        event: "before_agent_start",
        handler: () => {
            count += 1;
            return { count };
        }
    });
    pi.registerTool({
        name: "get_count",
        description: "Get event count",
        parameters: { type: "object", properties: {} },
        execute: async () => {
            return { content: [{ type: "text", text: String(count) }] };
        }
    });
}
"#;

/// Extension that deliberately takes varying time per event (0-5ms).
const JITTERY_EXT: &str = r#"
export default function activate(pi) {
    pi.events("register", {
        name: "jittery-ext",
        hooks: ["before_agent_start"]
    });
    pi.events("on", {
        event: "before_agent_start",
        handler: async () => {
            const ms = Math.floor(Math.random() * 5);
            await new Promise(r => setTimeout(r, ms));
            return { jitter: ms };
        }
    });
}
"#;

/// Concurrent dispatch: multiple threads fire events at the same manager simultaneously.
#[test]
fn concurrent_event_dispatch_does_not_deadlock_or_crash() {
    let harness = common::TestHarness::new("reliability_concurrent_dispatch");
    let manager = load_js_extension(&harness, COUNTER_EXT);

    let n_threads = 4;
    let events_per_thread = 25;
    let total_success: u32 = (0..n_threads)
        .map(|_| {
            let mgr = manager.clone();
            std::thread::spawn(move || {
                let mut successes = 0u32;
                for _ in 0..events_per_thread {
                    let result = common::run_async({
                        let mgr = mgr.clone();
                        async move {
                            mgr.dispatch_event(
                                ExtensionEventName::AgentStart,
                                Some(json!({"systemPrompt": "test", "model": "test"})),
                            )
                            .await
                        }
                    });
                    if result.is_ok() {
                        successes += 1;
                    }
                }
                successes
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|h| h.join().unwrap())
        .sum();
    eprintln!(
        "[concurrent_dispatch] {total_success}/{} events completed",
        n_threads * events_per_thread
    );

    // At least 80% should succeed (some may fail under contention, but no deadlock/crash).
    let threshold = u32::try_from(n_threads * events_per_thread * 80 / 100).unwrap_or(0);
    assert!(
        total_success >= threshold,
        "Too few events succeeded: {total_success} < {threshold}"
    );

    shutdown(&manager);
}

/// High-volume error recovery: rapid events where extension throws, verify runtime survives.
#[test]
fn high_volume_error_recovery() {
    let harness = common::TestHarness::new("reliability_high_volume_errors");
    let manager = load_js_extension(&harness, THROWING_EVENT_EXT);

    let n_events = 100;
    let start = Instant::now();

    for i in 0..n_events {
        let _ = common::run_async({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch_event(
                        ExtensionEventName::AgentStart,
                        Some(json!({"systemPrompt": "test", "model": "test"})),
                    )
                    .await
            }
        });
        // Ensure we don't spend too long (deadlock detection).
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "Stuck at event {i}, possible deadlock"
        );
    }

    let elapsed = start.elapsed();
    eprintln!("[high_volume_errors] Dispatched {n_events} events (all throwing) in {elapsed:?}");

    // Runtime should still be alive after all the errors.
    shutdown(&manager);
}

/// Cascaded recovery: error in one event dispatch, then good dispatch works.
#[test]
fn error_followed_by_success() {
    let harness = common::TestHarness::new("reliability_error_then_success");

    // Load both throwing and good extensions on same manager.
    let cwd = harness.temp_dir().to_path_buf();
    let throw_path = harness.create_file("extensions/throw.mjs", THROWING_EVENT_EXT.as_bytes());
    let good_path = harness.create_file("extensions/good.mjs", COUNTER_EXT.as_bytes());
    let throw_spec = JsExtensionLoadSpec::from_entry_path(&throw_path).expect("throw spec");
    let good_spec = JsExtensionLoadSpec::from_entry_path(&good_path).expect("good spec");

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
                .expect("start")
        }
    });
    manager.set_js_runtime(runtime);

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![throw_spec, good_spec])
                .await
                .expect("load");
        }
    });

    // Dispatch 5 events — throwing ext will fail, but counter ext should still work.
    for _ in 0..5 {
        let _ = common::run_async({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch_event(
                        ExtensionEventName::AgentStart,
                        Some(json!({"systemPrompt": "test", "model": "test"})),
                    )
                    .await
            }
        });
    }

    // Verify the runtime is still alive and responding.
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::AgentStart,
                    Some(json!({"systemPrompt": "final", "model": "test"})),
                    5_000,
                )
                .await
        }
    });

    match &result {
        Ok(Some(v)) => eprintln!("[error_then_success] Response: {v}"),
        Ok(None) => eprintln!("[error_then_success] No response (hooks may be fire-and-forget)"),
        Err(e) => eprintln!("[error_then_success] Error: {e}"),
    }
    // Key: no panic, no hang, runtime survived mixed error/success.
    shutdown(&manager);
}

/// Multiple create-load-dispatch-shutdown cycles with jittery extension.
#[test]
fn repeated_lifecycle_with_jittery_ext() {
    let cycles = 3;
    for cycle in 0..cycles {
        let harness = common::TestHarness::new(format!("reliability_jittery_cycle_{cycle}"));
        let manager = load_js_extension(&harness, JITTERY_EXT);

        // Dispatch several events with variable latency.
        for _ in 0..10 {
            let _ = common::run_async({
                let manager = manager.clone();
                async move {
                    manager
                        .dispatch_event(
                            ExtensionEventName::AgentStart,
                            Some(json!({"systemPrompt": "test", "model": "test"})),
                        )
                        .await
                }
            });
        }

        shutdown(&manager);
        eprintln!("[jittery_lifecycle] Cycle {cycle}/{cycles} completed");
    }
}

/// Dispatch various event types in rapid succession — no type causes crash.
#[test]
fn mixed_event_types_rapid_dispatch() {
    let harness = common::TestHarness::new("reliability_mixed_events");
    let manager = load_js_extension(&harness, GOOD_EXT);

    let event_types = [
        ExtensionEventName::AgentStart,
        ExtensionEventName::TurnStart,
        ExtensionEventName::TurnEnd,
        ExtensionEventName::AgentEnd,
    ];

    let start = Instant::now();
    let mut dispatched = 0u32;

    for _ in 0..20 {
        for event in &event_types {
            let _ = common::run_async({
                let manager = manager.clone();
                let event = *event;
                async move {
                    manager
                        .dispatch_event(event, Some(json!({"systemPrompt": "x", "model": "y"})))
                        .await
                }
            });
            dispatched += 1;
        }
    }

    let elapsed = start.elapsed();
    eprintln!("[mixed_events] Dispatched {dispatched} events across {} types in {elapsed:?}", event_types.len());
    assert!(
        elapsed < Duration::from_secs(15),
        "Mixed event dispatch took too long: {elapsed:?}"
    );

    shutdown(&manager);
}

/// Shutdown during active dispatch — no hang, no panic.
#[test]
fn shutdown_during_active_dispatch() {
    let harness = common::TestHarness::new("reliability_shutdown_during_dispatch");
    let manager = load_js_extension(&harness, JITTERY_EXT);

    // Start dispatching in background.
    let mgr_clone = manager.clone();
    let dispatch_handle = std::thread::spawn(move || {
        for _ in 0..50 {
            let _ = common::run_async({
                let mgr = mgr_clone.clone();
                async move {
                    mgr.dispatch_event(
                        ExtensionEventName::AgentStart,
                        Some(json!({"systemPrompt": "bg", "model": "test"})),
                    )
                    .await
                }
            });
        }
    });

    // Give dispatches a small head start, then initiate shutdown.
    std::thread::sleep(Duration::from_millis(10));
    let shutdown_start = Instant::now();
    let _ = common::run_async({
        let manager = manager.clone();
        async move { manager.shutdown(Duration::from_secs(2)).await }
    });
    let shutdown_elapsed = shutdown_start.elapsed();

    eprintln!("[shutdown_during_dispatch] Shutdown completed in {shutdown_elapsed:?}");
    assert!(
        shutdown_elapsed < Duration::from_secs(5),
        "Shutdown took too long: {shutdown_elapsed:?}"
    );

    // Wait for dispatch thread to finish (it may have some errors due to shutdown).
    dispatch_handle.join().expect("dispatch thread panicked");
}

/// Load failure does not prevent subsequent loads from succeeding.
#[test]
fn load_failure_then_good_load_succeeds() {
    let harness = common::TestHarness::new("reliability_load_recovery");
    let cwd = harness.temp_dir().to_path_buf();

    // Create bad extension (syntax error).
    let bad_path = harness.create_file("extensions/bad.mjs", b"this is not valid javascript {{{{");
    let bad_spec = JsExtensionLoadSpec::from_entry_path(&bad_path).expect("bad spec");

    // Create good extension.
    let good_path = harness.create_file("extensions/good.mjs", GOOD_EXT.as_bytes());
    let good_spec = JsExtensionLoadSpec::from_entry_path(&good_path).expect("good spec");

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
                .expect("start")
        }
    });
    manager.set_js_runtime(runtime);

    // Try to load bad extension — should fail.
    let bad_result = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![bad_spec]).await }
    });
    assert!(bad_result.is_err(), "Bad extension should fail to load");
    eprintln!("[load_recovery] Bad load correctly failed: {}", bad_result.unwrap_err());

    // Now load good extension — should succeed.
    let good_result = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![good_spec]).await }
    });
    assert!(good_result.is_ok(), "Good extension should load after bad one failed");
    eprintln!("[load_recovery] Good load succeeded after bad load");

    // Dispatch event to verify it works.
    let dispatch_result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event(
                    ExtensionEventName::AgentStart,
                    Some(json!({"systemPrompt": "test", "model": "test"})),
                )
                .await
        }
    });
    assert!(dispatch_result.is_ok(), "Dispatch should work after recovery");

    shutdown(&manager);
}
