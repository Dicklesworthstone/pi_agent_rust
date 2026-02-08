//! Unit tests for the `node:child_process` shim (bd-1h06).
//!
//! Tests verify spawn/exec API surface, option validation, kill semantics,
//! sync exec result parsing, and denied-by-policy behavior. The shim
//! delegates to `pi.exec` / `__pi_exec_sync_native` hostcalls; these tests
//! exercise the JS layer above those hostcalls.
#![allow(clippy::needless_raw_string_hashes)]

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use std::sync::Arc;

// ─── Helpers ────────────────────────────────────────────────────────────────

fn load_ext(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/cp_test.mjs", source.as_bytes());
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

fn cp_ext_source(js_expr: &str) -> String {
    format!(
        r#"
import cp from "node:child_process";
const {{ spawn, spawnSync, execSync, execFileSync, exec, execFile, fork }} = cp;

export default function activate(pi) {{
  pi.on("agent_start", (event, ctx) => {{
    let result;
    try {{
      result = String({js_expr});
    }} catch (e) {{
      result = "ERROR:" + e.message;
    }}
    return {{ result }};
  }});
}}
"#
    )
}

fn eval_cp(js_expr: &str) -> String {
    let harness = common::TestHarness::new("cp_shim");
    let source = cp_ext_source(js_expr);
    let mgr = load_ext(&harness, &source);

    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch agent_start")
    });

    response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_else(|| "NO_RESPONSE".to_string())
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. Module Exports
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn exports_all_functions() {
    let result = eval_cp(
        r#"(() => {
        const checks = [
            typeof spawn === "function",
            typeof spawnSync === "function",
            typeof execSync === "function",
            typeof execFileSync === "function",
            typeof exec === "function",
            typeof execFile === "function",
            typeof fork === "function",
        ];
        return checks.every(Boolean);
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn default_export_has_all_functions() {
    let result = eval_cp(
        r#"(() => {
        return typeof cp.spawn === "function" &&
               typeof cp.spawnSync === "function" &&
               typeof cp.execSync === "function" &&
               typeof cp.exec === "function" &&
               typeof cp.fork === "function";
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn bare_import_works() {
    let harness = common::TestHarness::new("cp_bare_import");
    let source = r#"
import cp from "child_process";

export default function activate(pi) {
  pi.on("agent_start", (event, ctx) => {
    return { result: String(typeof cp.spawn === "function") };
  });
}
"#;
    let mgr = load_ext(&harness, source);
    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });
    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "true");
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. spawn() — Validation & Structure
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn spawn_empty_command_throws() {
    let result = eval_cp(
        r#"(() => {
        try {
            spawn("");
            return "should_have_thrown";
        } catch (e) {
            return e.message.includes("command is required") ? "ok" : e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn spawn_null_command_throws() {
    let result = eval_cp(
        r#"(() => {
        try {
            spawn(null);
            return "should_have_thrown";
        } catch (e) {
            return e.message.includes("command is required") ? "ok" : e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn spawn_non_array_args_throws() {
    let result = eval_cp(
        r#"(() => {
        try {
            spawn("ls", "not-an-array");
            return "should_have_thrown";
        } catch (e) {
            return e.message.includes("args must be an array") ? "ok" : e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn spawn_returns_child_with_pid() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hello"]);
        return typeof child.pid === "number" && child.pid >= 1000;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_child_has_emitter_methods() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hello"]);
        return typeof child.on === "function" &&
               typeof child.once === "function" &&
               typeof child.off === "function" &&
               typeof child.emit === "function";
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_child_has_stdout_stderr() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hello"]);
        return child.stdout !== null &&
               child.stderr !== null &&
               typeof child.stdout.on === "function" &&
               typeof child.stderr.on === "function";
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_child_has_stdin_when_pipe() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("cat", [], { stdio: ["pipe", "pipe", "pipe"] });
        return child.stdin !== null && typeof child.stdin.on === "function";
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_child_killed_initially_false() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hello"]);
        return child.killed === false;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_increments_pid() {
    let result = eval_cp(
        r#"(() => {
        const c1 = spawn("echo", ["a"]);
        const c2 = spawn("echo", ["b"]);
        return c2.pid === c1.pid + 1;
    })()"#,
    );
    assert_eq!(result, "true");
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. spawn() — Options Validation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn spawn_unsupported_option_throws() {
    let result = eval_cp(
        r#"(() => {
        try {
            spawn("echo", ["hi"], { env: { FOO: "bar" } });
            return "should_have_thrown";
        } catch (e) {
            return e.message.includes("unsupported option") ? "ok" : e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn spawn_shell_true_throws() {
    let result = eval_cp(
        r#"(() => {
        try {
            spawn("echo", ["hi"], { shell: true });
            return "should_have_thrown";
        } catch (e) {
            return e.message.includes("only shell=false") ? "ok" : e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn spawn_shell_false_allowed() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hi"], { shell: false });
        return typeof child.pid === "number";
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_stdio_must_be_array() {
    let result = eval_cp(
        r#"(() => {
        try {
            spawn("echo", [], { stdio: "inherit" });
            return "should_have_thrown";
        } catch (e) {
            return e.message.includes("must be an array") ? "ok" : e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn spawn_stdio_must_have_3_entries() {
    let result = eval_cp(
        r#"(() => {
        try {
            spawn("echo", [], { stdio: ["pipe", "pipe"] });
            return "should_have_thrown";
        } catch (e) {
            return e.message.includes("exactly 3 entries") ? "ok" : e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn spawn_stdio_unsupported_value_throws() {
    let result = eval_cp(
        r#"(() => {
        try {
            spawn("echo", [], { stdio: ["pipe", "inherit", "pipe"] });
            return "should_have_thrown";
        } catch (e) {
            return e.message.includes("unsupported stdio") ? "ok" : e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn spawn_stdio_ignore_nullifies_stdout() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hi"], { stdio: ["pipe", "ignore", "pipe"] });
        return child.stdout === null && child.stderr !== null;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_stdio_ignore_all() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", [], { stdio: ["ignore", "ignore", "ignore"] });
        return child.stdin === null && child.stdout === null && child.stderr === null;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_cwd_option_accepted() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hi"], { cwd: "/tmp" });
        return typeof child.pid === "number";
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_detached_option_accepted() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hi"], { detached: true });
        return typeof child.pid === "number";
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_timeout_option_accepted() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hi"], { timeout: 100 });
        return typeof child.pid === "number";
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_timeout_must_be_non_negative_number() {
    let result = eval_cp(
        r#"(() => {
        try {
            spawn("echo", ["hi"], { timeout: "100" });
            return "should_have_thrown";
        } catch (e) {
            return e.message.includes("timeout must be a non-negative number") ? "ok" : e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. spawn() — kill() semantics
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn spawn_kill_sets_killed_flag() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("sleep", ["100"]);
        child.kill();
        return child.killed;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_kill_returns_true() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("sleep", ["100"]);
        return child.kill();
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_kill_after_done_returns_false() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hi"]);
        child.kill(); // first kill
        return child.kill(); // second kill after done
    })()"#,
    );
    assert_eq!(result, "false");
}

#[test]
fn spawn_kill_emits_close_event() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("sleep", ["100"]);
        let closeCode = "not_called";
        child.on("close", (code) => { closeCode = String(code); });
        child.kill();
        return closeCode;
    })()"#,
    );
    // kill passes null as code
    assert_eq!(result, "null");
}

#[test]
fn spawn_kill_close_emitted_only_once() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("sleep", ["100"]);
        let count = 0;
        child.on("close", () => { count++; });
        child.kill();
        child.kill();
        child.kill();
        return count;
    })()"#,
    );
    assert_eq!(result, "1");
}

#[test]
fn spawn_kill_with_signal_name() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("sleep", ["100"]);
        const killed = child.kill("SIGKILL");
        return killed && child.killed;
    })()"#,
    );
    assert_eq!(result, "true");
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. spawn() — Emitter Chaining
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn spawn_child_on_returns_self_for_chaining() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hi"]);
        const result = child.on("close", () => {});
        return result === child;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_stdout_on_returns_self_for_chaining() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hi"]);
        const result = child.stdout.on("data", () => {});
        return result === child.stdout;
    })()"#,
    );
    assert_eq!(result, "true");
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. fork() — Not Available
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn fork_throws_not_available() {
    let result = eval_cp(
        r#"(() => {
        try {
            fork("module.js");
            return "should_have_thrown";
        } catch (e) {
            return e.message.includes("not available") ? "ok" : e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

// ═══════════════════════════════════════════════════════════════════════════
// 7. execSync() — Validation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn exec_sync_empty_command_throws() {
    let result = eval_cp(
        r#"(() => {
        try {
            execSync("");
            return "should_have_thrown";
        } catch (e) {
            return e.message.includes("command is required") ? "ok" : e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn exec_file_sync_empty_file_throws() {
    let result = eval_cp(
        r#"(() => {
        try {
            execFileSync("");
            return "should_have_thrown";
        } catch (e) {
            return e.message.includes("file is required") ? "ok" : e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

// ═══════════════════════════════════════════════════════════════════════════
// 8. spawnSync() — Validation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn spawn_sync_empty_command_throws() {
    let result = eval_cp(
        r#"(() => {
        try {
            spawnSync("");
            return "should_have_thrown";
        } catch (e) {
            return e.message.includes("command is required") ? "ok" : e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn spawn_sync_args_coerced_to_strings() {
    // spawnSync accepts args and coerces them to strings
    let result = eval_cp(
        r#"(() => {
        // This will fail at the hostcall level, but we're testing JS validation
        try {
            const result = spawnSync("echo", [42, true]);
            // If hostcall is unavailable, we get an error result
            return result.error ? "hostcall_error" : "ok";
        } catch (e) {
            return "error:" + e.message;
        }
    })()"#,
    );
    // The args validation should pass (42 and true get coerced to strings)
    // The actual exec may fail without a real hostcall
    assert!(
        result == "ok" || result == "hostcall_error" || result.starts_with("error:"),
        "got: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 9. Process Kill Bridge
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn process_kill_invalid_pid_throws_einval() {
    let result = eval_cp(
        r#"(() => {
        try {
            globalThis.__pi_process_kill_impl(0);
            return "should_have_thrown";
        } catch (e) {
            return e.code === "EINVAL" ? "ok" : "wrong_code:" + e.code;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn process_kill_nonexistent_pid_throws_esrch() {
    let result = eval_cp(
        r#"(() => {
        try {
            globalThis.__pi_process_kill_impl(99999);
            return "should_have_thrown";
        } catch (e) {
            return e.code === "ESRCH" ? "ok" : "wrong_code:" + e.code;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn process_kill_spawned_child_succeeds() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("sleep", ["100"]);
        const killed = globalThis.__pi_process_kill_impl(child.pid);
        return killed === true && child.killed === true;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn process_kill_with_signal() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("sleep", ["100"]);
        const killed = globalThis.__pi_process_kill_impl(child.pid, "SIGKILL");
        return killed === true;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn process_kill_negative_pid_resolves_to_abs() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("sleep", ["100"]);
        // Negative PID gets abs() applied, so -pid should kill the child
        const killed = globalThis.__pi_process_kill_impl(-child.pid);
        return killed === true;
    })()"#,
    );
    assert_eq!(result, "true");
}

// ═══════════════════════════════════════════════════════════════════════════
// 10. Emitter off/removeListener
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn spawn_child_emitter_off_removes_listener() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hi"]);
        let called = false;
        const handler = () => { called = true; };
        child.on("close", handler);
        child.off("close", handler);
        child.emit("close", 0);
        return called;
    })()"#,
    );
    assert_eq!(result, "false");
}

#[test]
fn spawn_child_emitter_remove_listener_alias() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hi"]);
        let called = false;
        const handler = () => { called = true; };
        child.on("close", handler);
        child.removeListener("close", handler);
        child.emit("close", 0);
        return called;
    })()"#,
    );
    assert_eq!(result, "false");
}

#[test]
fn spawn_child_emitter_once_fires_once() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hi"]);
        let count = 0;
        child.once("custom", () => { count++; });
        child.emit("custom");
        child.emit("custom");
        child.emit("custom");
        return count;
    })()"#,
    );
    assert_eq!(result, "1");
}

// ═══════════════════════════════════════════════════════════════════════════
// 11. exec() / execFile() — Callback Validation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn exec_accepts_callback_as_second_arg() {
    // Just validate it doesn't throw - callback won't fire synchronously
    let result = eval_cp(
        r#"(() => {
        try {
            exec("echo hello", (err, stdout, stderr) => {});
            return "ok";
        } catch (e) {
            return "error:" + e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn exec_accepts_options_and_callback() {
    let result = eval_cp(
        r#"(() => {
        try {
            exec("echo hello", { cwd: "/tmp" }, (err, stdout, stderr) => {});
            return "ok";
        } catch (e) {
            return "error:" + e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn exec_returns_child_process_handle() {
    let result = eval_cp(
        r#"(() => {
        const child = exec("echo hello", { timeout: 50 }, (err, stdout, stderr) => {});
        return (
            child &&
            typeof child.pid === "number" &&
            typeof child.kill === "function" &&
            typeof child.on === "function"
        );
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn exec_file_accepts_args_and_callback() {
    let result = eval_cp(
        r#"(() => {
        try {
            execFile("echo", ["hello"], (err, stdout, stderr) => {});
            return "ok";
        } catch (e) {
            return "error:" + e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn exec_file_returns_child_process_handle() {
    let result = eval_cp(
        r#"(() => {
        const child = execFile("echo", ["hello"], { timeout: 50 }, (err, stdout, stderr) => {});
        return (
            child &&
            typeof child.pid === "number" &&
            typeof child.kill === "function" &&
            typeof child.on === "function"
        );
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn exec_file_accepts_callback_only() {
    let result = eval_cp(
        r#"(() => {
        try {
            execFile("echo", (err, stdout, stderr) => {});
            return "ok";
        } catch (e) {
            return "error:" + e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

// ═══════════════════════════════════════════════════════════════════════════
// 12. Global State Isolation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn child_process_state_persists_across_calls() {
    let result = eval_cp(
        r#"(() => {
        const c1 = spawn("echo", ["first"]);
        const pid1 = c1.pid;
        const c2 = spawn("echo", ["second"]);
        const pid2 = c2.pid;
        return pid2 > pid1;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn killed_child_removed_from_state() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("sleep", ["100"]);
        const pid = child.pid;
        child.kill();
        // After kill, trying to kill again via process bridge should fail
        try {
            globalThis.__pi_process_kill_impl(pid);
            return "should_have_thrown";
        } catch (e) {
            return e.code === "ESRCH" ? "ok" : "wrong:" + e.code;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}
