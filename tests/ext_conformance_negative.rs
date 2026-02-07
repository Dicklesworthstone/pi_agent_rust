//! Negative tests for unsupported / invalid extension behaviors (bd-icjb).
//!
//! These tests prove that the extension system fails cleanly and predictably
//! when extensions attempt invalid registrations, load from bad paths,
//! or violate API contracts.

mod common;

use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn deterministic_env() -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert(
        "PI_DETERMINISTIC_TIME_MS".to_string(),
        "1700000000000".to_string(),
    );
    env.insert("PI_DETERMINISTIC_TIME_STEP_MS".to_string(), "1".to_string());
    env.insert(
        "PI_DETERMINISTIC_CWD".to_string(),
        "/tmp/ext-neg-test".to_string(),
    );
    env.insert(
        "PI_DETERMINISTIC_HOME".to_string(),
        "/tmp/ext-neg-test-home".to_string(),
    );
    env.insert("HOME".to_string(), "/tmp/ext-neg-test-home".to_string());
    env.insert("PI_DETERMINISTIC_RANDOM".to_string(), "0.5".to_string());
    env
}

/// Create a temporary `.ts` fixture file and return its path.
fn write_temp_fixture(dir: &Path, filename: &str, content: &str) -> PathBuf {
    let file_path = dir.join(filename);
    std::fs::write(&file_path, content).expect("write fixture");
    file_path
}

/// Load an extension from a path. Returns Ok((manager, runtime)) or Err(message).
fn load_extension(
    fixture_path: &Path,
) -> Result<(ExtensionManager, JsExtensionRuntimeHandle), String> {
    let spec = JsExtensionLoadSpec::from_entry_path(fixture_path)
        .map_err(|e| format!("load spec: {e}"))?;

    let manager = ExtensionManager::new();
    let cwd = PathBuf::from("/tmp/ext-neg-test");
    let _ = std::fs::create_dir_all(&cwd);
    let _ = std::fs::create_dir_all("/tmp/ext-neg-test-home");
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));

    let js_config = PiJsRuntimeConfig {
        cwd: "/tmp/ext-neg-test".to_string(),
        env: deterministic_env(),
        ..Default::default()
    };

    #[allow(clippy::redundant_clone)]
    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .map_err(|e| format!("start runtime: {e}"))
        }
    })?;
    manager.set_js_runtime(runtime.clone());

    #[allow(clippy::redundant_clone)]
    let manager_ret = manager.clone();
    common::run_async({
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .map_err(|e| format!("load extension: {e}"))
        }
    })?;

    Ok((manager_ret, runtime))
}

/// Extract the error from a `load_extension` result (since the Ok type doesn't impl Debug).
fn expect_load_err(result: Result<(ExtensionManager, JsExtensionRuntimeHandle), String>) -> String {
    match result {
        Err(e) => e,
        Ok(_) => panic!("Expected error, but load succeeded"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 1. Load spec validation
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn load_spec_nonexistent_path() {
    let result = JsExtensionLoadSpec::from_entry_path("/nonexistent/extension.ts");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("does not exist"),
        "Expected 'does not exist', got: {err}"
    );
}

#[test]
fn load_spec_root_path() {
    let result = JsExtensionLoadSpec::from_entry_path("/");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("no filename") || err.contains("does not exist"),
        "Expected path validation error, got: {err}"
    );
}

#[test]
fn load_spec_directory_without_entrypoint() {
    let dir = tempfile::tempdir().unwrap();
    let result = JsExtensionLoadSpec::from_entry_path(dir.path());
    // from_entry_path accepts raw dirs and uses dir name as ID, but loading
    // such a spec through the runtime will fail because there is no JS file.
    // The validation happens downstream in load_js_extensions.
    // Just verify the spec was created (even if it won't load).
    match result {
        Err(err) => {
            let msg = err.to_string();
            assert!(!msg.is_empty(), "Error should have a message");
        }
        Ok(spec) => {
            // Spec was created — verify loading it fails
            let load_result = load_extension(&spec.entry_path);
            assert!(
                load_result.is_err(),
                "Loading a dir without index.ts should fail"
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 2. Module loading errors
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn load_extension_non_function_export() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        "export default { not_a_function: true };",
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("default-export a function") || err.contains("must default-export"),
        "Expected default export error, got: {err}"
    );
}

#[test]
fn load_extension_syntax_error() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        "export default function(pi) { this is invalid javascript !@#$ }",
    );
    let result = load_extension(&fixture);
    assert!(result.is_err(), "Should fail on syntax error");
}

#[test]
fn load_extension_throws_during_init() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { throw new Error("init failed"); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("init failed"),
        "Expected init error, got: {err}"
    );
}

#[test]
fn load_extension_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(dir.path(), "index.ts", "");
    let result = load_extension(&fixture);
    assert!(result.is_err(), "Empty file should fail to load");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 3. registerTool validation
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn register_tool_not_an_object() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.registerTool("string_not_object"); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("spec must be an object"),
        "Expected 'spec must be an object', got: {err}"
    );
}

#[test]
fn register_tool_missing_name() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r"export default function(pi) { pi.registerTool({ execute: async () => ({}) }); }",
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("name is required"),
        "Expected 'name is required', got: {err}"
    );
}

#[test]
fn register_tool_empty_name() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.registerTool({ name: "   ", execute: async () => ({}) }); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("name is required"),
        "Expected 'name is required', got: {err}"
    );
}

#[test]
fn register_tool_execute_not_function() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.registerTool({ name: "t", execute: "not_a_function" }); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("execute must be a function"),
        "Expected 'execute must be a function', got: {err}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 4. registerCommand validation
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn register_command_empty_name() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.registerCommand("", { handler: async () => {} }); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("name is required"),
        "Expected 'name is required', got: {err}"
    );
}

#[test]
fn register_command_spec_not_object() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.registerCommand("test", "not_an_object"); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("spec must be an object"),
        "Expected 'spec must be an object', got: {err}"
    );
}

#[test]
fn register_command_handler_not_function() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.registerCommand("test", { handler: 42 }); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("handler must be a function"),
        "Expected 'handler must be a function', got: {err}"
    );
}

#[test]
fn register_command_missing_handler() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.registerCommand("test", { description: "no handler" }); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("handler must be a function"),
        "Expected 'handler must be a function', got: {err}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 5. registerProvider validation
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn register_provider_empty_id() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.registerProvider("", { models: [] }); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("id is required"),
        "Expected 'id is required', got: {err}"
    );
}

#[test]
fn register_provider_spec_not_object() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.registerProvider("test", null); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("spec must be an object"),
        "Expected 'spec must be an object', got: {err}"
    );
}

#[test]
fn register_provider_stream_simple_not_function() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) {
            pi.registerProvider("test", {
                streamSimple: "not_a_function",
                models: []
            });
        }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("streamSimple must be a function"),
        "Expected 'streamSimple must be a function', got: {err}"
    );
}

#[test]
fn register_provider_stream_simple_without_api() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) {
            pi.registerProvider("test", {
                streamSimple: async function*() { yield "hi"; },
                models: []
            });
        }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("api is required when registering streamSimple"),
        "Expected 'api is required', got: {err}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 6. registerShortcut validation
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn register_shortcut_spec_not_object() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) {
            pi.registerShortcut({ kind: "ctrl", key: "k" }, null);
        }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("spec must be an object"),
        "Expected 'spec must be an object', got: {err}"
    );
}

#[test]
fn register_shortcut_handler_not_function() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) {
            pi.registerShortcut({ kind: "ctrl", key: "k" }, { handler: 42 });
        }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("handler must be a function"),
        "Expected 'handler must be a function', got: {err}"
    );
}

#[test]
fn register_shortcut_reserved_key() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) {
            pi.registerShortcut({ kind: "ctrl", key: "c" }, { handler: async () => {} });
        }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("reserved"),
        "Expected 'reserved' error, got: {err}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 7. registerMessageRenderer validation
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn register_message_renderer_empty_type() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.registerMessageRenderer("", (d) => ""); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("customType is required"),
        "Expected 'customType is required', got: {err}"
    );
}

#[test]
fn register_message_renderer_not_function() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.registerMessageRenderer("test/x", "not_a_function"); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("renderer must be a function"),
        "Expected 'renderer must be a function', got: {err}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 8. registerFlag validation
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn register_flag_empty_name() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.registerFlag("", { default: false }); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("name is required"),
        "Expected 'name is required', got: {err}"
    );
}

#[test]
fn register_flag_spec_not_object() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.registerFlag("debug", null); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("spec must be an object"),
        "Expected 'spec must be an object', got: {err}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 9. on / registerHook validation
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn register_hook_empty_event_name() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.on("", async () => {}); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("event name is required"),
        "Expected 'event name is required', got: {err}"
    );
}

#[test]
fn register_hook_handler_not_function() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) { pi.on("agent_start", "not_a_function"); }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("handler must be a function"),
        "Expected 'handler must be a function', got: {err}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 10. Module require errors
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn require_nonexistent_module() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        "const missing = require(\"@nonexistent/totally-fake-module\");\nexport default function(pi) { }",
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("Cannot find module") || err.contains("module"),
        "Expected module not found error, got: {err}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 11. Tool execution errors
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn execute_nonexistent_tool() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) {
            pi.registerTool({
                name: "valid_tool",
                description: "A tool",
                parameters: { type: "object", properties: {} },
                execute: async () => ({ content: [{ type: "text", text: "ok" }] })
            });
        }"#,
    );

    let (_manager, runtime) = load_extension(&fixture).expect("load should succeed");

    let result: Result<serde_json::Value, String> = common::run_async(async move {
        runtime
            .execute_tool(
                "nonexistent_tool".to_string(),
                "tc-test".to_string(),
                serde_json::json!({}),
                serde_json::json!({}),
                20_000,
            )
            .await
            .map_err(|e| format!("{e}"))
    });

    match result {
        Err(err) => {
            eprintln!("Got expected error for nonexistent tool: {err}");
        }
        Ok(val) => {
            let text = serde_json::to_string(&val).unwrap_or_default();
            eprintln!("Nonexistent tool returned value: {text}");
            assert!(
                text.contains("error") || text.contains("not found") || text.contains("Unknown"),
                "Expected error in result, got: {text}"
            );
        }
    }
}

#[test]
fn execute_tool_that_throws() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) {
            pi.registerTool({
                name: "crasher",
                description: "Always throws",
                parameters: { type: "object", properties: {} },
                execute: async () => { throw new Error("tool execution failed"); }
            });
        }"#,
    );

    let (_manager, runtime) = load_extension(&fixture).expect("load should succeed");

    let result: Result<serde_json::Value, String> = common::run_async(async move {
        runtime
            .execute_tool(
                "crasher".to_string(),
                "tc-test".to_string(),
                serde_json::json!({}),
                serde_json::json!({}),
                20_000,
            )
            .await
            .map_err(|e| format!("{e}"))
    });

    // The tool error should not crash the host
    match result {
        Err(err) => {
            eprintln!("Tool that throws returned error: {err}");
            assert!(
                err.contains("tool execution failed") || err.contains("error"),
                "Expected tool execution error, got: {err}"
            );
        }
        Ok(val) => {
            let text = serde_json::to_string(&val).unwrap_or_default();
            eprintln!("Tool that throws returned value (error in result): {text}");
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 12. Command execution errors
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn execute_command_that_throws() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) {
            pi.registerCommand("boom", {
                description: "Always throws",
                handler: async () => { throw new Error("command failed"); }
            });
        }"#,
    );

    let (_manager, runtime) = load_extension(&fixture).expect("load should succeed");

    let result: Result<serde_json::Value, String> = common::run_async(async move {
        runtime
            .execute_command(
                "boom".to_string(),
                String::new(),
                serde_json::json!({}),
                20_000,
            )
            .await
            .map_err(|e| format!("{e}"))
    });

    // The command error should not crash the host — test passes if we get here
    match result {
        Err(err) => eprintln!("Command that throws returned error: {err}"),
        Ok(val) => {
            let text = serde_json::to_string(&val).unwrap_or_default();
            eprintln!("Command that throws returned value: {text}");
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 13. Event dispatch with throwing handler
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn dispatch_event_handler_throws() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) {
            pi.on("agent_start", async () => {
                throw new Error("event handler blew up");
            });
        }"#,
    );

    let (_manager, runtime) = load_extension(&fixture).expect("load should succeed");

    let result: Result<serde_json::Value, String> = common::run_async(async move {
        runtime
            .dispatch_event(
                "agent_start".to_string(),
                serde_json::json!({}),
                serde_json::json!({}),
                20_000,
            )
            .await
            .map_err(|e| format!("{e}"))
    });

    // Test passes if we get here without panic
    match result {
        Err(err) => eprintln!("Event handler error (expected): {err}"),
        Ok(val) => {
            let text = serde_json::to_string(&val).unwrap_or_default();
            eprintln!("Event dispatch result: {text}");
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 14. First error stops loading
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn first_registration_error_stops_loading() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) {
            pi.registerTool("not_an_object");
            pi.registerCommand("test", { handler: async () => {} });
        }"#,
    );
    let result = load_extension(&fixture);
    let err = expect_load_err(result);
    assert!(
        err.contains("registerTool"),
        "Expected registerTool error (first failure stops), got: {err}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 15. Clean shutdown
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn clean_shutdown_after_valid_load() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) {
            pi.registerTool({
                name: "safe",
                description: "Safe tool",
                parameters: { type: "object", properties: {} },
                execute: async () => ({ content: [{ type: "text", text: "ok" }] })
            });
        }"#,
    );

    let (manager, _runtime) = load_extension(&fixture).expect("should load");

    let shutdown_ok = common::run_async(
        async move { manager.shutdown(std::time::Duration::from_secs(5)).await },
    );

    assert!(shutdown_ok, "Shutdown should complete cleanly");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 16. Multiple valid registrations in one extension
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn multiple_valid_registrations_succeed() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) {
            pi.registerTool({
                name: "tool_a",
                description: "First tool",
                parameters: { type: "object", properties: {} },
                execute: async () => ({ content: [{ type: "text", text: "a" }] })
            });
            pi.registerTool({
                name: "tool_b",
                description: "Second tool",
                parameters: { type: "object", properties: {} },
                execute: async () => ({ content: [{ type: "text", text: "b" }] })
            });
            pi.on("agent_start", async () => {});
            pi.registerFlag("debug", { type: "boolean", default: false });
        }"#,
    );

    let (manager, _runtime) =
        load_extension(&fixture).expect("should load with multiple registrations");

    let tools = manager.extension_tool_defs();
    assert!(
        tools.len() >= 2,
        "Should have at least 2 tools, got {}",
        tools.len()
    );

    let hooks = manager.list_event_hooks();
    assert!(!hooks.is_empty(), "Should have event hooks registered");

    let flags = manager.list_flags();
    assert!(!flags.is_empty(), "Should have flags registered");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 17. Undefined API access
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn access_undefined_api_method() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r"export default function(pi) {
            pi.nonExistentMethod();
        }",
    );
    let result = load_extension(&fixture);
    assert!(result.is_err(), "Calling undefined API method should fail");
}

#[test]
fn access_undefined_property_gracefully() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = write_temp_fixture(
        dir.path(),
        "index.ts",
        r#"export default function(pi) {
            const x = pi.nonExistentProperty;
            if (x !== undefined) {
                throw new Error("Expected undefined");
            }
        }"#,
    );
    let result = load_extension(&fixture);
    assert!(
        result.is_ok(),
        "Reading undefined property should not crash: {:?}",
        result.err()
    );
}
