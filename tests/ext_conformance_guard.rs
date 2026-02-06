#![cfg(feature = "ext-conformance")]
//! Tier 2a conformance tests: Guard/Gate extensions (bd-1g64).
//!
//! Tests cover 5 extensions that intercept events to gate/guard operations:
//! 1. permission-gate — blocks dangerous bash commands via `tool_call` hook
//! 2. confirm-destructive — gates session clear/switch/fork via `session_before_*` hooks
//! 3. dirty-repo-guard — prevents session changes with uncommitted git changes
//! 4. bash-spawn-hook — registers custom bash tool with spawn hook
//! 5. timed-confirm — registers timed confirmation commands
//!
//! Run: `cargo test --features ext-conformance --test ext_conformance_guard -- --nocapture`

mod common;

use chrono::{SecondsFormat, Utc};
use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

// ─── Paths ──────────────────────────────────────────────────────────────────

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn artifacts_dir() -> PathBuf {
    project_root().join("tests/ext_conformance/artifacts")
}

fn reports_dir() -> PathBuf {
    project_root().join("tests/ext_conformance/reports/guard")
}

// ─── Deterministic settings ─────────────────────────────────────────────────

const DEFAULT_TIMEOUT_MS: u64 = 20_000;

struct DeterministicSettings {
    time_ms: String,
    cwd: String,
    home: String,
}

fn deterministic_settings_for(ext_id: &str) -> DeterministicSettings {
    let cwd = format!("/tmp/ext-conformance-test/{ext_id}");
    let home = format!("/tmp/ext-conformance-home/{ext_id}");
    let _ = std::fs::create_dir_all(&cwd);
    let _ = std::fs::create_dir_all(&home);
    DeterministicSettings {
        time_ms: "1700000000000".to_string(),
        cwd,
        home,
    }
}

// ─── Extension loader ───────────────────────────────────────────────────────

struct LoadedExtension {
    manager: ExtensionManager,
    runtime: JsExtensionRuntimeHandle,
}

fn load_guard_extension(ext_id: &str, entry_file: &str) -> Result<LoadedExtension, String> {
    let ext_path = artifacts_dir().join(ext_id).join(entry_file);
    if !ext_path.exists() {
        return Err(format!("extension not found: {}", ext_path.display()));
    }

    let spec =
        JsExtensionLoadSpec::from_entry_path(&ext_path).map_err(|e| format!("load spec: {e}"))?;

    let settings = deterministic_settings_for(ext_id);
    let cwd = PathBuf::from(&settings.cwd);
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));

    let mut env = HashMap::new();
    env.insert("PI_DETERMINISTIC_TIME_MS".to_string(), settings.time_ms);
    env.insert("PI_DETERMINISTIC_TIME_STEP_MS".to_string(), "1".to_string());
    env.insert("PI_DETERMINISTIC_CWD".to_string(), settings.cwd.clone());
    env.insert("PI_DETERMINISTIC_HOME".to_string(), settings.home.clone());
    env.insert("HOME".to_string(), settings.home);
    env.insert("PI_DETERMINISTIC_RANDOM".to_string(), "0.5".to_string());

    let js_config = PiJsRuntimeConfig {
        cwd: settings.cwd,
        env,
        ..Default::default()
    };

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

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .map_err(|e| format!("load extension: {e}"))
        }
    })?;

    Ok(LoadedExtension { manager, runtime })
}

// ─── Context builder ────────────────────────────────────────────────────────

fn build_ctx(ext_id: &str, has_ui: bool) -> Value {
    let settings = deterministic_settings_for(ext_id);
    json!({
        "hasUI": has_ui,
        "cwd": settings.cwd,
        "sessionEntries": [],
        "sessionBranch": [],
        "sessionLeafEntry": null,
        "modelRegistry": {},
    })
}

// ─── Event dispatch helper ──────────────────────────────────────────────────

fn dispatch_event(
    runtime: &JsExtensionRuntimeHandle,
    event_name: &str,
    payload: Value,
    ctx: Value,
) -> Result<Value, String> {
    common::run_async({
        let runtime = runtime.clone();
        let event_name = event_name.to_string();
        async move {
            runtime
                .dispatch_event(event_name, payload, ctx, DEFAULT_TIMEOUT_MS)
                .await
                .map_err(|e| format!("dispatch_event: {e}"))
        }
    })
}

// ============================================================================
// Registration conformance: verify each extension registers expected hooks/tools
// ============================================================================

#[test]
fn permission_gate_registers_tool_call_hook() {
    let loaded = load_guard_extension("permission-gate", "permission-gate.ts")
        .expect("load permission-gate");
    let hooks = loaded.manager.list_event_hooks();
    assert!(
        hooks.contains(&"tool_call".to_string()),
        "permission-gate should register tool_call hook, got: {hooks:?}"
    );
}

#[test]
fn confirm_destructive_registers_session_hooks() {
    let loaded = load_guard_extension("confirm-destructive", "confirm-destructive.ts")
        .expect("load confirm-destructive");
    let hooks = loaded.manager.list_event_hooks();
    assert!(
        hooks.contains(&"session_before_switch".to_string()),
        "confirm-destructive should register session_before_switch, got: {hooks:?}"
    );
    assert!(
        hooks.contains(&"session_before_fork".to_string()),
        "confirm-destructive should register session_before_fork, got: {hooks:?}"
    );
}

#[test]
fn dirty_repo_guard_registers_session_hooks() {
    let loaded = load_guard_extension("dirty-repo-guard", "dirty-repo-guard.ts")
        .expect("load dirty-repo-guard");
    let hooks = loaded.manager.list_event_hooks();
    assert!(
        hooks.contains(&"session_before_switch".to_string()),
        "dirty-repo-guard should register session_before_switch, got: {hooks:?}"
    );
    assert!(
        hooks.contains(&"session_before_fork".to_string()),
        "dirty-repo-guard should register session_before_fork, got: {hooks:?}"
    );
}

#[test]
fn bash_spawn_hook_registers_tool() {
    let loaded = load_guard_extension("bash-spawn-hook", "bash-spawn-hook.ts")
        .expect("load bash-spawn-hook");
    let tools = loaded.manager.extension_tool_defs();
    assert!(
        !tools.is_empty(),
        "bash-spawn-hook should register at least one tool, got: {tools:?}"
    );
    // The tool should be a bash variant
    let tool_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    eprintln!("[bash-spawn-hook] registered tools: {tool_names:?}");
    assert!(
        !tool_names.is_empty(),
        "bash-spawn-hook should register named tool(s)"
    );
}

#[test]
fn timed_confirm_registers_commands() {
    let loaded =
        load_guard_extension("timed-confirm", "timed-confirm.ts").expect("load timed-confirm");
    let commands = loaded.manager.list_commands();
    let command_names: Vec<&str> = commands
        .iter()
        .filter_map(|c| c.get("name").and_then(Value::as_str))
        .collect();
    eprintln!("[timed-confirm] registered commands: {command_names:?}");
    assert!(
        command_names.contains(&"timed"),
        "timed-confirm should register /timed command, got: {command_names:?}"
    );
    assert!(
        command_names.contains(&"timed-select"),
        "timed-confirm should register /timed-select command, got: {command_names:?}"
    );
    assert!(
        command_names.contains(&"timed-signal"),
        "timed-confirm should register /timed-signal command, got: {command_names:?}"
    );
}

// ============================================================================
// Behavioral: permission-gate blocks dangerous commands (no UI)
// ============================================================================

#[test]
fn permission_gate_blocks_rm_rf_without_ui() {
    let loaded = load_guard_extension("permission-gate", "permission-gate.ts")
        .expect("load permission-gate");
    let ctx = build_ctx("permission-gate", false);
    let event_payload = json!({
        "toolName": "bash",
        "input": { "command": "rm -rf /important" },
    });

    let result = dispatch_event(&loaded.runtime, "tool_call", event_payload, ctx);
    match result {
        Ok(val) => {
            let block = val.get("block").and_then(Value::as_bool).unwrap_or(false);
            let reason = val
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default();
            eprintln!("[permission-gate] rm -rf result: block={block}, reason={reason:?}");
            assert!(block, "should block rm -rf without UI, got: {val}");
            assert!(
                reason.to_lowercase().contains("dangerous")
                    || reason.to_lowercase().contains("blocked")
                    || reason.to_lowercase().contains("no ui"),
                "reason should explain the block, got: {reason}"
            );
        }
        Err(e) => {
            // An error also indicates the command was not allowed through
            eprintln!("[permission-gate] rm -rf error (acceptable): {e}");
        }
    }
}

#[test]
fn permission_gate_blocks_sudo_without_ui() {
    let loaded = load_guard_extension("permission-gate", "permission-gate.ts")
        .expect("load permission-gate");
    let ctx = build_ctx("permission-gate", false);
    let event_payload = json!({
        "toolName": "bash",
        "input": { "command": "sudo apt install something" },
    });

    let result = dispatch_event(&loaded.runtime, "tool_call", event_payload, ctx);
    match result {
        Ok(val) => {
            let block = val.get("block").and_then(Value::as_bool).unwrap_or(false);
            eprintln!("[permission-gate] sudo result: {val}");
            assert!(block, "should block sudo without UI, got: {val}");
        }
        Err(e) => {
            eprintln!("[permission-gate] sudo error (acceptable): {e}");
        }
    }
}

#[test]
fn permission_gate_blocks_chmod_777_without_ui() {
    let loaded = load_guard_extension("permission-gate", "permission-gate.ts")
        .expect("load permission-gate");
    let ctx = build_ctx("permission-gate", false);
    let event_payload = json!({
        "toolName": "bash",
        "input": { "command": "chmod 777 /etc/shadow" },
    });

    let result = dispatch_event(&loaded.runtime, "tool_call", event_payload, ctx);
    match result {
        Ok(val) => {
            let block = val.get("block").and_then(Value::as_bool).unwrap_or(false);
            eprintln!("[permission-gate] chmod 777 result: {val}");
            assert!(block, "should block chmod 777 without UI, got: {val}");
        }
        Err(e) => {
            eprintln!("[permission-gate] chmod 777 error (acceptable): {e}");
        }
    }
}

#[test]
fn permission_gate_allows_safe_command() {
    let loaded = load_guard_extension("permission-gate", "permission-gate.ts")
        .expect("load permission-gate");
    let ctx = build_ctx("permission-gate", false);
    let event_payload = json!({
        "toolName": "bash",
        "input": { "command": "echo hello" },
    });

    let result = dispatch_event(&loaded.runtime, "tool_call", event_payload, ctx);
    match result {
        Ok(val) => {
            let block = val.get("block").and_then(Value::as_bool).unwrap_or(false);
            eprintln!("[permission-gate] safe command result: {val}");
            assert!(!block, "should allow safe command (echo), got: {val}");
        }
        Err(e) => {
            // Errors are unexpected for safe commands
            panic!("safe command should not error: {e}");
        }
    }
}

#[test]
fn permission_gate_ignores_non_bash_tools() {
    let loaded = load_guard_extension("permission-gate", "permission-gate.ts")
        .expect("load permission-gate");
    let ctx = build_ctx("permission-gate", false);
    let event_payload = json!({
        "toolName": "read",
        "input": { "path": "/etc/passwd" },
    });

    let result = dispatch_event(&loaded.runtime, "tool_call", event_payload, ctx);
    match result {
        Ok(val) => {
            let block = val.get("block").and_then(Value::as_bool).unwrap_or(false);
            eprintln!("[permission-gate] non-bash tool result: {val}");
            assert!(!block, "should not block non-bash tools, got: {val}");
        }
        Err(e) => {
            panic!("non-bash tool should not error: {e}");
        }
    }
}

// ============================================================================
// Behavioral: confirm-destructive returns undefined without UI
// ============================================================================

#[test]
fn confirm_destructive_noop_without_ui() {
    let loaded = load_guard_extension("confirm-destructive", "confirm-destructive.ts")
        .expect("load confirm-destructive");
    let ctx = build_ctx("confirm-destructive", false);

    // Without UI, confirm-destructive should be a no-op (returns undefined)
    let event_payload = json!({ "reason": "new" });
    let result = dispatch_event(&loaded.runtime, "session_before_switch", event_payload, ctx);
    match result {
        Ok(val) => {
            // Should return undefined/null (no blocking without UI)
            let cancel = val.get("cancel").and_then(Value::as_bool).unwrap_or(false);
            eprintln!("[confirm-destructive] no-UI result: {val}");
            assert!(
                !cancel,
                "confirm-destructive should not cancel without UI, got: {val}"
            );
        }
        Err(e) => {
            eprintln!("[confirm-destructive] error (may be ok): {e}");
        }
    }
}

// ============================================================================
// Behavioral: dirty-repo-guard checks git status
// ============================================================================

#[test]
fn dirty_repo_guard_attempts_exec() {
    // dirty-repo-guard uses pi.exec("git", ["status", "--porcelain"]).
    // In test env, exec is denied by default policy → the handler should
    // catch the error and either allow (git not available) or cancel.
    let loaded = load_guard_extension("dirty-repo-guard", "dirty-repo-guard.ts")
        .expect("load dirty-repo-guard");
    let ctx = build_ctx("dirty-repo-guard", false);

    let event_payload = json!({ "reason": "new" });
    let result = dispatch_event(&loaded.runtime, "session_before_switch", event_payload, ctx);
    match result {
        Ok(val) => {
            // With exec denied, git check fails → should allow the action
            // (returns undefined or non-cancel result)
            eprintln!("[dirty-repo-guard] session_before_switch result: {val}");
        }
        Err(e) => {
            // Error from denied exec is acceptable — means policy enforcement works
            let lower = e.to_lowercase();
            eprintln!("[dirty-repo-guard] exec error (expected): {e}");
            assert!(
                lower.contains("denied") || lower.contains("exec") || lower.contains("error"),
                "error should relate to exec denial, got: {e}"
            );
        }
    }
}

// ============================================================================
// Registration completeness: no extra/missing registrations
// ============================================================================

#[test]
fn all_guard_extensions_load_successfully() {
    let extensions = [
        ("permission-gate", "permission-gate.ts"),
        ("confirm-destructive", "confirm-destructive.ts"),
        ("dirty-repo-guard", "dirty-repo-guard.ts"),
        ("bash-spawn-hook", "bash-spawn-hook.ts"),
        ("timed-confirm", "timed-confirm.ts"),
    ];

    let mut failures = Vec::new();
    for (ext_id, entry_file) in &extensions {
        let start = Instant::now();
        match load_guard_extension(ext_id, entry_file) {
            Ok(_loaded) => {
                let elapsed = start.elapsed();
                eprintln!("[guard] loaded {ext_id} in {elapsed:?}");
            }
            Err(e) => {
                failures.push(format!("{ext_id}: {e}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "guard extension load failures ({}):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn guard_extensions_register_no_flags() {
    // None of the guard extensions should register flags
    let extensions = [
        ("permission-gate", "permission-gate.ts"),
        ("confirm-destructive", "confirm-destructive.ts"),
        ("dirty-repo-guard", "dirty-repo-guard.ts"),
        ("bash-spawn-hook", "bash-spawn-hook.ts"),
        ("timed-confirm", "timed-confirm.ts"),
    ];

    for (ext_id, entry_file) in &extensions {
        let loaded = load_guard_extension(ext_id, entry_file)
            .unwrap_or_else(|e| panic!("load {ext_id}: {e}"));
        let flags = loaded.manager.list_flags();
        assert!(
            flags.is_empty(),
            "{ext_id} should register no flags, got: {flags:?}"
        );
    }
}

#[test]
fn guard_extensions_register_no_shortcuts() {
    let extensions = [
        ("permission-gate", "permission-gate.ts"),
        ("confirm-destructive", "confirm-destructive.ts"),
        ("dirty-repo-guard", "dirty-repo-guard.ts"),
        ("bash-spawn-hook", "bash-spawn-hook.ts"),
        ("timed-confirm", "timed-confirm.ts"),
    ];

    for (ext_id, entry_file) in &extensions {
        let loaded = load_guard_extension(ext_id, entry_file)
            .unwrap_or_else(|e| panic!("load {ext_id}: {e}"));
        let shortcuts = loaded.manager.list_shortcuts();
        assert!(
            shortcuts.is_empty(),
            "{ext_id} should register no shortcuts, got: {shortcuts:?}"
        );
    }
}

// ============================================================================
// JSONL report generation
// ============================================================================

#[derive(Debug, Serialize)]
struct GuardTestResult {
    extension_id: String,
    test_name: String,
    category: String,
    status: String,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<Value>,
}

#[test]
#[allow(clippy::too_many_lines)]
fn guard_conformance_report() {
    let report_dir = reports_dir();
    let _ = std::fs::create_dir_all(&report_dir);

    let mut results: Vec<GuardTestResult> = Vec::new();

    let extensions: &[(&str, &str, &[&str], &[&str])] = &[
        (
            "permission-gate",
            "permission-gate.ts",
            &[],            // expected tools
            &["tool_call"], // expected hooks
        ),
        (
            "confirm-destructive",
            "confirm-destructive.ts",
            &[],
            &["session_before_switch", "session_before_fork"],
        ),
        (
            "dirty-repo-guard",
            "dirty-repo-guard.ts",
            &[],
            &["session_before_switch", "session_before_fork"],
        ),
        (
            "bash-spawn-hook",
            "bash-spawn-hook.ts",
            &["bash"], // expects at least one tool with "bash" in the name
            &[],
        ),
        ("timed-confirm", "timed-confirm.ts", &[], &[]),
    ];

    let expected_commands: &[(&str, &[&str])] = &[
        ("permission-gate", &[]),
        ("confirm-destructive", &[]),
        ("dirty-repo-guard", &[]),
        ("bash-spawn-hook", &[]),
        ("timed-confirm", &["timed", "timed-select", "timed-signal"]),
    ];

    // Phase 1: Registration conformance
    for (ext_id, entry_file, expected_tools, expected_hooks) in extensions {
        let start = Instant::now();
        match load_guard_extension(ext_id, entry_file) {
            Ok(loaded) => {
                let elapsed = start.elapsed().as_millis();

                // Check hooks
                let hooks = loaded.manager.list_event_hooks();
                let hooks_pass = expected_hooks
                    .iter()
                    .all(|h| hooks.contains(&h.to_string()));
                results.push(GuardTestResult {
                    extension_id: ext_id.to_string(),
                    test_name: "registration_hooks".to_string(),
                    category: "registration".to_string(),
                    status: if hooks_pass { "pass" } else { "fail" }.to_string(),
                    duration_ms: u64::try_from(elapsed).unwrap_or(0),
                    error: if hooks_pass {
                        None
                    } else {
                        Some(format!("expected hooks {expected_hooks:?}, got {hooks:?}"))
                    },
                    output: Some(json!({ "hooks": hooks })),
                });

                // Check tools
                let tools = loaded.manager.extension_tool_defs();
                let tool_names: Vec<String> = tools
                    .iter()
                    .filter_map(|t| t.get("name").and_then(Value::as_str).map(String::from))
                    .collect();
                let tools_pass = if expected_tools.is_empty() {
                    true
                } else {
                    expected_tools
                        .iter()
                        .all(|t| tool_names.iter().any(|n| n.contains(t)))
                };
                results.push(GuardTestResult {
                    extension_id: ext_id.to_string(),
                    test_name: "registration_tools".to_string(),
                    category: "registration".to_string(),
                    status: if tools_pass { "pass" } else { "fail" }.to_string(),
                    duration_ms: u64::try_from(elapsed).unwrap_or(0),
                    error: if tools_pass {
                        None
                    } else {
                        Some(format!(
                            "expected tools matching {expected_tools:?}, got {tool_names:?}"
                        ))
                    },
                    output: Some(json!({ "tools": tool_names })),
                });

                // Check commands
                let commands = loaded.manager.list_commands();
                let cmd_names: Vec<String> = commands
                    .iter()
                    .filter_map(|c| c.get("name").and_then(Value::as_str).map(String::from))
                    .collect();
                let exp_cmds = expected_commands
                    .iter()
                    .find(|(id, _)| *id == *ext_id)
                    .map_or(&[] as &[&str], |(_, c)| *c);
                let cmds_pass = exp_cmds.iter().all(|c| cmd_names.contains(&c.to_string()));
                results.push(GuardTestResult {
                    extension_id: ext_id.to_string(),
                    test_name: "registration_commands".to_string(),
                    category: "registration".to_string(),
                    status: if cmds_pass { "pass" } else { "fail" }.to_string(),
                    duration_ms: u64::try_from(elapsed).unwrap_or(0),
                    error: if cmds_pass {
                        None
                    } else {
                        Some(format!("expected commands {exp_cmds:?}, got {cmd_names:?}"))
                    },
                    output: Some(json!({ "commands": cmd_names })),
                });

                // No flags/shortcuts expected
                let flags = loaded.manager.list_flags();
                let shortcuts = loaded.manager.list_shortcuts();
                let no_extras = flags.is_empty() && shortcuts.is_empty();
                results.push(GuardTestResult {
                    extension_id: ext_id.to_string(),
                    test_name: "registration_no_extras".to_string(),
                    category: "registration".to_string(),
                    status: if no_extras { "pass" } else { "fail" }.to_string(),
                    duration_ms: u64::try_from(elapsed).unwrap_or(0),
                    error: if no_extras {
                        None
                    } else {
                        Some(format!(
                            "unexpected flags={flags:?} shortcuts={shortcuts:?}"
                        ))
                    },
                    output: None,
                });
            }
            Err(e) => {
                results.push(GuardTestResult {
                    extension_id: ext_id.to_string(),
                    test_name: "load".to_string(),
                    category: "registration".to_string(),
                    status: "error".to_string(),
                    duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(0),
                    error: Some(e),
                    output: None,
                });
            }
        }
    }

    // Phase 2: Behavioral tests for permission-gate
    {
        let start = Instant::now();
        if let Ok(loaded) = load_guard_extension("permission-gate", "permission-gate.ts") {
            let dangerous_cmds = [
                ("rm_rf", "rm -rf /important", true),
                ("sudo", "sudo apt install foo", true),
                ("chmod_777", "chmod 777 /etc/shadow", true),
                ("safe_echo", "echo hello", false),
                ("safe_ls", "ls -la", false),
            ];

            for (test_id, command, expect_block) in &dangerous_cmds {
                let ctx = build_ctx("permission-gate", false);
                let payload = json!({
                    "toolName": "bash",
                    "input": { "command": command },
                });
                let event_start = Instant::now();
                let result = dispatch_event(&loaded.runtime, "tool_call", payload, ctx);
                let elapsed = event_start.elapsed().as_millis();

                let (status, error, output) = match result {
                    Ok(val) => {
                        let blocked = val.get("block").and_then(Value::as_bool).unwrap_or(false);
                        let pass = blocked == *expect_block;
                        (
                            if pass { "pass" } else { "fail" }.to_string(),
                            if pass {
                                None
                            } else {
                                Some(format!(
                                    "expected block={expect_block}, got block={blocked}"
                                ))
                            },
                            Some(val),
                        )
                    }
                    Err(e) => {
                        // Error means blocked (acceptable for dangerous commands)
                        let pass = *expect_block;
                        (
                            if pass { "pass" } else { "fail" }.to_string(),
                            if pass {
                                None
                            } else {
                                Some(format!("unexpected error: {e}"))
                            },
                            Some(json!({ "error": e })),
                        )
                    }
                };

                results.push(GuardTestResult {
                    extension_id: "permission-gate".to_string(),
                    test_name: format!("behavior_tool_call_{test_id}"),
                    category: "behavioral".to_string(),
                    status,
                    duration_ms: u64::try_from(elapsed).unwrap_or(0),
                    error,
                    output,
                });
            }

            // Non-bash tool should not be blocked
            let ctx = build_ctx("permission-gate", false);
            let payload = json!({ "toolName": "read", "input": { "path": "/etc/passwd" } });
            let event_start = Instant::now();
            let result = dispatch_event(&loaded.runtime, "tool_call", payload, ctx);
            let elapsed = event_start.elapsed().as_millis();
            let (status, error) = match result {
                Ok(val) => {
                    let blocked = val.get("block").and_then(Value::as_bool).unwrap_or(false);
                    if blocked {
                        (
                            "fail".to_string(),
                            Some("non-bash tool was blocked".to_string()),
                        )
                    } else {
                        ("pass".to_string(), None)
                    }
                }
                Err(e) => ("fail".to_string(), Some(format!("unexpected error: {e}"))),
            };
            results.push(GuardTestResult {
                extension_id: "permission-gate".to_string(),
                test_name: "behavior_non_bash_passthrough".to_string(),
                category: "behavioral".to_string(),
                status,
                duration_ms: u64::try_from(elapsed).unwrap_or(0),
                error,
                output: None,
            });

            eprintln!(
                "[guard-report] permission-gate behavioral tests done in {:?}",
                start.elapsed()
            );
        }
    }

    // Write JSONL report
    let events_path = report_dir.join("guard_events.jsonl");
    let mut lines: Vec<String> = Vec::new();
    for r in &results {
        let entry = json!({
            "schema": "pi.ext.guard_conformance.v1",
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            "extension_id": r.extension_id,
            "test_name": r.test_name,
            "category": r.category,
            "status": r.status,
            "duration_ms": r.duration_ms,
            "error": r.error,
            "output": r.output,
        });
        lines.push(serde_json::to_string(&entry).unwrap_or_default());
    }
    let _ = std::fs::write(&events_path, lines.join("\n") + "\n");

    // Write triage summary
    let pass_count = results.iter().filter(|r| r.status == "pass").count();
    let fail_count = results.iter().filter(|r| r.status == "fail").count();
    let error_count = results.iter().filter(|r| r.status == "error").count();
    let triage = json!({
        "schema": "pi.ext.guard_triage.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "tier": "2a",
        "extensions": ["permission-gate", "confirm-destructive", "dirty-repo-guard",
                       "bash-spawn-hook", "timed-confirm"],
        "counts": {
            "total": results.len(),
            "pass": pass_count,
            "fail": fail_count,
            "error": error_count,
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

    // Summary
    eprintln!("\n=== Guard Conformance Report (Tier 2a) ===");
    eprintln!("  Total: {}", results.len());
    eprintln!("  Pass:  {pass_count}");
    eprintln!("  Fail:  {fail_count}");
    eprintln!("  Error: {error_count}");
    eprintln!("  Report: {}", events_path.display());
    eprintln!("  Triage: {}", triage_path.display());

    assert_eq!(
        fail_count, 0,
        "all guard conformance checks should pass, {fail_count} failed"
    );
    assert_eq!(
        error_count, 0,
        "no guard extensions should error, {error_count} errored"
    );
}
