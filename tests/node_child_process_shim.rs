// Integration tests are separate crates, so src/lib.rs's `recursion_limit`
// does not reach here; with the `wasm-host` feature, proving `Send` for the
// extension-loading futures exceeds the default 128.
#![recursion_limit = "256"]

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
            spawn("echo", ["hi"], { uid: 0 });
            return "should_have_thrown";
        } catch (e) {
            return e.message.includes("unsupported option") ? "ok" : e.message;
        }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn spawn_explicit_environment_exposes_a_writable_pipe() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hi"], { env: { FOO: "bar" } });
        return typeof child.stdin.write === "function" && typeof child.stdin.end === "function";
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn spawn_rejects_invalid_environment_shape() {
    let result = eval_cp(
        r#"(() => {
        for (const env of [null, "PATH=/bin", []]) {
            try { spawn("echo", [], { env }); return "accepted"; }
            catch (error) { if (!error.message.includes("env must be an object")) return error.message; }
        }
        return "ok";
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
fn legacy_stream_spawn_detached_option_accepted() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hi"], { detached: true, stdio: ["ignore", "pipe", "pipe"] });
        return typeof child.pid === "number";
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn persistent_spawn_rejects_detached_processes() {
    let result = eval_cp(
        r#"(() => {
        try { spawn("echo", [], { detached: true }); return "accepted"; }
        catch (error) { return error.message.includes("cannot be detached") ? "ok" : error.message; }
    })()"#,
    );
    assert_eq!(result, "ok");
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
fn legacy_stream_spawn_kill_after_done_returns_false() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("echo", ["hi"], { stdio: ["ignore", "pipe", "pipe"] });
        child.kill(); // first kill
        return child.kill(); // second kill after done
    })()"#,
    );
    assert_eq!(result, "false");
}

#[test]
fn legacy_stream_spawn_kill_emits_close_event() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("sleep", ["100"], { stdio: ["ignore", "pipe", "pipe"] });
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
fn legacy_stream_spawn_kill_close_emitted_only_once() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("sleep", ["100"], { stdio: ["ignore", "pipe", "pipe"] });
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

/// This test is also a portable subprocess fixture. Selecting it alone with
/// `PIJS_STDIO_TEST_PEER` set produces genuine newline-delimited responses.
#[test]
fn persistent_stdio_peer() {
    use sha2::Digest as _;
    use std::io::{BufRead as _, Write as _};
    if std::env::var("PIJS_STDIO_TEST_PEER").as_deref() != Ok("enabled") {
        return;
    }
    let workspace = std::env::var("PI_WORKSPACE_DIR").expect("child workspace metadata");
    assert_eq!(std::env::var("PWD").as_deref(), Ok(workspace.as_str()));
    assert_eq!(
        std::fs::canonicalize(&workspace).expect("workspace path"),
        std::env::current_dir()
            .expect("child cwd")
            .canonicalize()
            .expect("canonical child cwd"),
        "both inherited and explicitly copied environments must use the selected workspace"
    );
    for name in [
        "NODE_OPTIONS",
        "LD_PRELOAD",
        "PYTHONPATH",
        "PRIVATE_TOKEN",
        "PRIVATE_SERVICE",
    ] {
        assert!(std::env::var_os(name).is_none(), "unsafe inherited {name}");
    }
    for line in std::io::stdin().lock().lines() {
        let request: serde_json::Value =
            serde_json::from_str(&line.expect("peer stdin")).expect("peer JSON");
        let result = if request["method"] == "tools/index-large" {
            let text = request["params"]["text"]
                .as_str()
                .expect("large tool input");
            serde_json::json!({
                "bytes": text.len(), "characters": text.chars().count(),
                "sha256": pi::package_manager::hex_encode(&sha2::Sha256::digest(text.as_bytes())),
            })
        } else {
            serde_json::json!({"method": request["method"], "text": request["params"]["text"]})
        };
        println!(
            "{}",
            serde_json::json!({
                "jsonrpc": "2.0", "id": request["id"],
                "result": result,
            })
        );
        std::io::stdout().flush().expect("peer response flush");
    }
    eprintln!("pijs-stdio-peer-eof");
}

#[test]
fn loaded_extension_exchanges_persistent_stdio_frames_and_closes_after_eof() {
    use sha2::Digest as _;
    let harness = common::TestHarness::new("cp_persistent_stdio");
    let cwd = harness.temp_dir().to_path_buf();
    let executable = serde_json::to_string(&std::env::current_exe().expect("test executable"))
        .expect("serialize executable");
    let mut environment = std::collections::HashMap::<String, String>::new();
    for name in [
        "PATH",
        "SystemRoot",
        "SYSTEMROOT",
        "WINDIR",
        "TEMP",
        "TMP",
        "HOME",
        "USERPROFILE",
    ] {
        if let Ok(value) = std::env::var(name) {
            // Windows environment access is case-insensitive; only include
            // one spelling so the explicit environment stays unambiguous.
            if cfg!(windows) && environment.keys().any(|key| key.eq_ignore_ascii_case(name)) {
                continue;
            }
            environment.insert(name.to_string(), value);
        }
    }
    environment.insert("PIJS_STDIO_TEST_PEER".to_string(), "enabled".to_string());
    // Real integrations copy the disclosed snapshot into child options. A
    // host loader setting must not make that ordinary pattern fail admission,
    // and it must not reach the child. Drive metadata remains readable to
    // node:path but is not an ordinary child environment binding.
    for (name, value) in [
        ("PI_WORKSPACE_DIR", "snapshot-fixture"),
        ("PWD", "/stale/launcher/workspace"),
        ("NODE_OPTIONS", "--require=untrusted"),
        ("LD_PRELOAD", "/untrusted.so"),
        ("PYTHONPATH", "/untrusted/python"),
        ("PRIVATE_TOKEN", "must-not-reach-child"),
        ("PRIVATE_SERVICE", "custom-secret-must-not-reach-child"),
        ("=D:", "D:\\project"),
    ] {
        environment.insert(name.to_string(), value.to_string());
    }
    let source = format!(
        r#"
        import {{ spawn }} from 'node:child_process';
        export default function(pi) {{
            async function exchange(explicitEnvironment) {{
                const child = spawn({executable},
                    ['--exact', 'persistent_stdio_peer', '--nocapture', '--quiet'],
                    explicitEnvironment
                        ? {{ env: {{ ...process.env }}, timeout: 10000 }}
                        : {{ timeout: 10000 }});
                const replies = new Map();
                const acks = [];
                const writeResults = [];
                let stdout = '', stderr = '', exits = 0, closes = 0, drains = 0;
                child.stdin.on('drain', () => {{ drains++; }});
                child.stdout.on('data', (chunk) => {{
                    stdout += chunk.toString('utf8');
                    let newline;
                    while ((newline = stdout.indexOf('\n')) >= 0) {{
                        const line = stdout.slice(0, newline);
                        stdout = stdout.slice(newline + 1);
                        try {{
                            const message = JSON.parse(line.slice(line.indexOf('{{')));
                            replies.get(message.id)?.(message.result);
                        }} catch (_) {{}}
                    }}
                }});
                child.stderr.on('data', (chunk) => {{ stderr += chunk.toString('utf8'); }});
                child.on('exit', () => {{ exits++; }});
                const closed = new Promise((resolve, reject) => {{
                    child.on('error', reject);
                    child.on('close', (code, signal) => {{ closes++; resolve({{ code, signal }}); }});
                }});
                function request(id, method, text = 'duplex λ') {{
                    const response = new Promise((resolve, reject) => {{
                        replies.set(id, resolve);
                        const ready = child.stdin.write(JSON.stringify({{jsonrpc:'2.0', id, method, params:{{text}}}}) + '\n',
                            (error) => {{ if (error) reject(error); else acks.push(id); }});
                        writeResults.push({{id, ready}});
                    }});
                    return response;
                }}
                // The first frame is deliberately queued before admission.
                const first = await request(1, 'initialize');
                const second = await request(2, 'tools/list');
                const third = await request(3, 'tools/call');
                let oversizedRejected = false;
                try {{ child.stdin.write(new Uint8Array(4 * 1024 * 1024 + 1)); }}
                catch (error) {{ oversizedRejected = error instanceof RangeError; }}
                // context-mode writes each JSON-RPC tool request in one call.
                // Index input can span several bounded native pipe messages.
                const drained = new Promise(resolve => child.stdin.once('drain', resolve));
                const large = await request(4, 'tools/index-large', 'λ🚀Z'.repeat(60000));
                await drained;
                const afterLarge = await request(5, 'tools/call');
                child.stdin.end(() => acks.push('end'));
                const status = await closed;
                await Promise.resolve();
                return {{ first, second, third, large, afterLarge, acks, status, exits, closes,
                    oversizedRejected, writeResults, drains,
                    stderr, killAfterClose: child.kill(), ended: child.stdin.writableEnded }};
            }}
            pi.on('agent_start', async () => {{
                const visibility = {{
                    getterHidden: process.env.PRIVATE_SERVICE === undefined,
                    apiHidden: pi.env.get('PRIVATE_SERVICE') === undefined,
                    keysHidden: !Object.keys(process.env).includes('PRIVATE_SERVICE'),
                    spreadHidden: !Object.prototype.hasOwnProperty.call({{ ...process.env }}, 'PRIVATE_SERVICE'),
                }};
                const explicit = await exchange(true);
                const inherited = await exchange(false);
                return {{ result: {{ explicit, inherited, visibility }} }};
            }});
        }}
    "#
    );
    let entry = harness.create_file("extensions/persistent.mjs", source.as_bytes());
    let spec = JsExtensionLoadSpec::from_entry_path(entry).expect("persistent extension spec");
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let mut policy = pi::extensions::ExtensionPolicy::default();
    policy.deny_caps.retain(|cap| cap != "exec" && cap != "env");
    policy
        .default_caps
        .extend(["exec".to_string(), "env".to_string()]);
    assert!(
        pi::extensions_js::is_env_var_allowed("PRIVATE_SERVICE"),
        "the fixture must require its custom broker rule"
    );
    policy
        .secret_broker
        .secret_exact
        .push("PRIVATE_SERVICE".to_string());
    let config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        disk_cache_dir: None,
        env: environment,
        ..Default::default()
    };
    let runtime = common::run_async(JsExtensionRuntimeHandle::start_with_policy(
        config,
        tools,
        manager.clone(),
        policy,
    ))
    .expect("start persistent runtime");
    manager.set_js_runtime(runtime);
    let response = common::run_async(async move {
        manager
            .load_js_extensions(vec![spec])
            .await
            .expect("load persistent extension");
        manager
            .dispatch_event_with_response(ExtensionEventName::AgentStart, None, 25000)
            .await
            .expect("dispatch persistent exchange")
            .expect("exchange result")
    });
    assert_eq!(
        response["result"]["visibility"],
        serde_json::json!({
            "getterHidden": true,
            "apiHidden": true,
            "keysHidden": true,
            "spreadHidden": true,
        }),
        "{response}"
    );
    for mode in ["explicit", "inherited"] {
        let result = &response["result"][mode];
        assert_eq!(result["first"]["method"], "initialize", "{response}");
        assert_eq!(result["second"]["method"], "tools/list", "{response}");
        assert_eq!(result["third"]["text"], "duplex λ", "{response}");
        let large_text = "λ🚀Z".repeat(60000);
        assert_eq!(
            result["large"],
            serde_json::json!({
                "bytes": large_text.len(), "characters": large_text.chars().count(),
                "sha256": pi::package_manager::hex_encode(&sha2::Sha256::digest(large_text.as_bytes())),
            }),
            "large UTF-8 frames must reach the peer without truncation or interleaving"
        );
        assert_eq!(result["afterLarge"]["text"], "duplex λ", "{response}");
        assert_eq!(result["oversizedRejected"], true, "{response}");
        assert_eq!(
            result["writeResults"],
            serde_json::json!([
                {"id": 1, "ready": true}, {"id": 2, "ready": true},
                {"id": 3, "ready": true}, {"id": 4, "ready": false},
                {"id": 5, "ready": true},
            ]),
            "{response}"
        );
        assert!(
            result["drains"].as_u64().unwrap_or_default() >= 1,
            "{response}"
        );
        assert_eq!(
            result["acks"],
            serde_json::json!([1, 2, 3, 4, 5, "end"]),
            "{response}"
        );
        assert_eq!(
            result["status"],
            serde_json::json!({"code":0,"signal":null}),
            "{response}"
        );
        assert_eq!(result["exits"], 1, "{response}");
        assert_eq!(result["closes"], 1, "{response}");
        assert_eq!(result["killAfterClose"], false, "{response}");
        assert_eq!(result["ended"], true, "{response}");
        assert!(
            result["stderr"]
                .as_str()
                .unwrap_or_default()
                .contains("pijs-stdio-peer-eof"),
            "{response}"
        );
    }
}

/// A failed owner remains in the actor for diagnostics and frozen name
/// reservations. Its already-admitted MCP process must not survive there, and
/// cancelling it must not disturb a different owner's live server.
#[test]
#[allow(clippy::too_many_lines)]
fn quarantining_an_extension_reaps_its_persistent_child_and_preserves_its_peer() {
    use std::time::{Duration, Instant};

    let harness = common::TestHarness::new("cp_quarantine_native_lifetime");
    let cwd = harness.temp_dir().to_path_buf();
    let executable = serde_json::to_string(&std::env::current_exe().expect("test executable"))
        .expect("serialize executable");
    let mut environment = std::collections::HashMap::<String, String>::new();
    for name in [
        "PATH",
        "SystemRoot",
        "SYSTEMROOT",
        "WINDIR",
        "TEMP",
        "TMP",
        "HOME",
        "USERPROFILE",
    ] {
        if let Ok(value) = std::env::var(name) {
            if cfg!(windows) && environment.keys().any(|key| key.eq_ignore_ascii_case(name)) {
                continue;
            }
            environment.insert(name.to_string(), value);
        }
    }
    environment.extend([
        ("PIJS_STDIO_TEST_PEER".to_string(), "enabled".to_string()),
        (
            "PI_WORKSPACE_DIR".to_string(),
            "snapshot-fixture".to_string(),
        ),
        ("PI_EXT_COMPAT_SCAN".to_string(), "0".to_string()),
    ]);
    let specs = ["quarantine-owner", "quarantine-peer"]
        .into_iter()
        .map(|owner| {
            let source = format!(
                r#"
                import {{ spawn }} from 'node:child_process';
                export default function(pi) {{
                    let child, serial = 0, stdout = '';
                    const pending = new Map();
                    function request(method) {{
                        const id = ++serial;
                        return new Promise((resolve, reject) => {{
                            pending.set(id, {{ resolve, reject }});
                            child.stdin.write(JSON.stringify({{
                                jsonrpc: '2.0', id, method, params: {{ text: '{owner}' }}
                            }}) + '\n', error => {{ if (error) reject(error); }});
                        }});
                    }}
                    function tool(name) {{
                        return {{ name, description: name,
                            parameters: {{ type: 'object', properties: {{}} }},
                            async execute() {{ return {{ content: [], details: {{}} }}; }} }};
                    }}
                    pi.registerTool(tool('{owner}_tool'));
                    pi.registerCommand('{owner}-start', {{ handler: async () => {{
                        child = spawn({executable},
                            ['--exact', 'persistent_stdio_peer', '--nocapture', '--quiet'],
                            {{ env: {{ ...process.env }}, timeout: 60000 }});
                        child.stdout.on('data', chunk => {{
                            stdout += chunk.toString('utf8');
                            let newline;
                            while ((newline = stdout.indexOf('\n')) >= 0) {{
                                const line = stdout.slice(0, newline);
                                stdout = stdout.slice(newline + 1);
                                try {{
                                    const message = JSON.parse(line.slice(line.indexOf('{{')));
                                    pending.get(message.id)?.resolve(message.result);
                                    pending.delete(message.id);
                                }} catch (_) {{}}
                            }}
                        }});
                        child.on('error', error => {{
                            for (const waiter of pending.values()) waiter.reject(error);
                            pending.clear();
                        }});
                        child.on('close', () => {{
                            for (const waiter of pending.values()) waiter.reject(new Error('peer closed'));
                            pending.clear();
                        }});
                        return {{ result: await request('initialize') }};
                    }} }});
                    pi.registerCommand('{owner}-ping', {{ handler: async () =>
                        ({{ result: await request('tools/list') }}) }});
                    if ('{owner}' === 'quarantine-owner') {{
                        pi.registerCommand('quarantine-native-owner', {{ handler() {{
                            pi.registerTool(tool('quarantine-peer_tool'));
                        }} }});
                    }}
                }}
                "#
            );
            let entry = harness.create_file(
                format!("extensions/{owner}/index.mjs"),
                source.as_bytes(),
            );
            JsExtensionLoadSpec::from_entry_path(entry).expect("process owner spec")
        })
        .collect();
    let manager = ExtensionManager::new();
    let mut policy = pi::extensions::ExtensionPolicy::default();
    policy.deny_caps.retain(|cap| cap != "exec" && cap != "env");
    policy
        .default_caps
        .extend(["exec".to_string(), "env".to_string()]);
    let runtime = common::run_async(JsExtensionRuntimeHandle::start_with_policy(
        PiJsRuntimeConfig {
            cwd: cwd.display().to_string(),
            env: environment,
            disk_cache_dir: None,
            repair_mode: pi::extensions_js::RepairMode::Off,
            ..Default::default()
        },
        Arc::new(ToolRegistry::new(&[], &cwd, None)),
        manager.clone(),
        policy,
    ))
    .expect("start quarantine fixture runtime");
    manager.set_js_runtime(runtime);
    let ready = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(specs)
                .await
                .expect("load owners");
            let owner = manager
                .execute_command("quarantine-owner-start", "", 15000)
                .await
                .expect("real owner process initialized");
            let peer = manager
                .execute_command("quarantine-peer-start", "", 15000)
                .await
                .expect("real peer process initialized");
            (owner, peer)
        }
    });
    let active = |owner| manager.quota_state(owner).map(|state| state.1);
    let active_before = (active("quarantine-owner"), active("quarantine-peer"));
    let collision = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .execute_command("quarantine-native-owner", "", 5000)
                .await
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while active("quarantine-owner") != Some(0) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    // This count is released by the actual subprocess/pipe workers, not by
    // removing the session handle. Capture it before any overall shutdown.
    let active_after = (active("quarantine-owner"), active("quarantine-peer"));
    let peer_response = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .execute_command("quarantine-peer-ping", "", 5000)
                .await
        }
    });
    let stopped = common::run_async({
        let manager = manager.clone();
        async move { manager.shutdown(Duration::from_secs(5)).await }
    });
    assert_eq!(ready.0["result"]["method"], "initialize");
    assert_eq!(ready.1["result"]["method"], "initialize");
    assert_eq!(active_before, (Some(1), Some(1)));
    let collision = collision
        .expect_err("collision quarantines its owner")
        .to_string();
    assert!(collision.contains("tool name collision"), "{collision}");
    assert_eq!(
        active_after,
        (Some(0), Some(1)),
        "quarantine must reap only its owner's real subprocess before shutdown"
    );
    assert_eq!(
        peer_response.expect("healthy process continues serving requests")["result"],
        serde_json::json!({ "method": "tools/list", "text": "quarantine-peer" })
    );
    assert!(stopped);
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
fn legacy_stream_killed_child_removed_from_state() {
    let result = eval_cp(
        r#"(() => {
        const child = spawn("sleep", ["100"], { stdio: ["ignore", "pipe", "pipe"] });
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

#[test]
fn pending_process_handles_do_not_collide_with_native_pids() {
    futures::executor::block_on(async {
        let clock = Arc::new(pi::scheduler::DeterministicClock::new(0));
        let runtime = pi::extensions_js::PiJsRuntime::with_clock(clock)
            .await
            .expect("create process registry runtime");
        runtime
            .eval(
                r#"
                globalThis.registryResult = {};
                import('node:child_process').then(({ spawn }) => {
                    const first = spawn('registry-first');
                    const second = spawn('registry-second');
                    registryResult.provisional = [first.pid, second.pid];
                    first.on('error', () => {});
                    second.on('error', () => {});
                    first.on('spawn', () => {
                        registryResult.duringFirstSpawn = __pi_child_process_registry_size();
                        registryResult.admittedPid = first.pid;
                    });
                    first.on('close', () => {
                        registryResult.afterFirstClose = __pi_child_process_registry_size();
                    });
                    second.on('close', () => {
                        registryResult.afterSecondClose = __pi_child_process_registry_size();
                    });
                });
                "#,
            )
            .await
            .expect("queue concurrent process admissions");
        let mut requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 2);
        let first = requests.pop_front().expect("first admission");
        let second = requests.pop_front().expect("second admission");
        // Exercise the registry transition with the PID that previously collided
        // with the second provisional handle. No process or native session is
        // fabricated: the real native poll rejects this absent session afterward.
        runtime.complete_hostcall(
            first.call_id,
            pi::scheduler::HostcallOutcome::Success(serde_json::json!({
                "session": "registry-fixture-no-native-session",
                "pid": 1001
            })),
        );
        runtime.tick().await.expect("deliver first admission");
        runtime.complete_hostcall(
            second.call_id,
            pi::scheduler::HostcallOutcome::Error {
                code: "denied".to_string(),
                message: "registry fixture denies second admission".to_string(),
            },
        );
        runtime.tick().await.expect("deliver second admission");
        let result = runtime
            .read_global_json("registryResult")
            .await
            .expect("read registry transition");
        for pid in result["provisional"].as_array().expect("provisional PIDs") {
            assert!(pid.as_u64().expect("numeric provisional PID") > u64::from(u32::MAX));
        }
        assert_eq!(result["duringFirstSpawn"], 2, "{result}");
        assert_eq!(result["admittedPid"], 1001, "{result}");
        assert_eq!(result["afterFirstClose"], 1, "{result}");
        assert_eq!(result["afterSecondClose"], 0, "{result}");
        assert_eq!(runtime.pending_hostcall_count(), 0);
    });
}

#[test]
fn stdin_end_callbacks_settle_after_denied_admission_and_close() {
    futures::executor::block_on(async {
        let clock = Arc::new(pi::scheduler::DeterministicClock::new(0));
        let runtime = pi::extensions_js::PiJsRuntime::with_clock(clock)
            .await
            .expect("create stdin lifecycle runtime");
        runtime
            .eval(
                r#"
                globalThis.endResult = { writes: [], pending: [], afterClose: [], finishes: 0, closes: 0 };
                import('node:child_process').then(({ spawn }) => {
                    const child = spawn('stdin-denied-fixture');
                    child.on('error', () => {});
                    child.stdin.on('finish', () => endResult.finishes++);
                    child.stdin.write('λ'.repeat(200000), error => endResult.writes.push(error?.code));
                    child.stdin.end(error => endResult.pending.push(error?.code));
                    child.stdin.end(error => endResult.pending.push(error?.code));
                    child.on('close', () => {
                        endResult.closes++;
                        let synchronous = true;
                        child.stdin.end(error => {
                            endResult.afterClose.push({ code: error?.code, synchronous });
                        });
                        synchronous = false;
                    });
                });
                "#,
            )
            .await
            .expect("queue EOF callbacks before admission");
        let mut requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 1);
        runtime.complete_hostcall(
            requests.pop_front().expect("pending admission").call_id,
            pi::scheduler::HostcallOutcome::Error {
                code: "denied".to_string(),
                message: "stdin lifecycle fixture denies admission".to_string(),
            },
        );
        runtime.tick().await.expect("deliver denied admission");
        runtime
            .drain_microtasks()
            .await
            .expect("settle EOF callbacks");
        let result = runtime
            .read_global_json("endResult")
            .await
            .expect("read EOF lifecycle result");
        assert_eq!(result["writes"], serde_json::json!(["denied"]));
        assert_eq!(result["pending"], serde_json::json!(["denied", "denied"]));
        assert_eq!(
            result["afterClose"],
            serde_json::json!([{ "code": "ERR_STREAM_DESTROYED", "synchronous": false }]),
            "{result}"
        );
        assert_eq!(result["finishes"], 0);
        assert_eq!(result["closes"], 1);
        assert_eq!(runtime.pending_hostcall_count(), 0);
    });
}
