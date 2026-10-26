//! Native QuickJS and filesystem admission regressions, not a JS mock host.

use super::*;
use crate::agent_cx::AgentCx;
use crate::eval::js_kernel::{JsCellResponse, JsKernel};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

fn cell(kernel: &JsKernel, code: &str) -> JsCellResponse {
    let owner = AgentCx::for_testing_with_io();
    let _guard = owner.cx().clone().set_current_restricted();
    kernel
        .submit(code.into(), Instant::now() + Duration::from_secs(5))
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .expect("CommonJS cell must settle")
}

fn succeeds(kernel: &JsKernel, code: &str) -> String {
    let result = cell(kernel, code);
    assert!(result.ok, "{code}: {:?}", result.error);
    result.result.unwrap_or_default()
}

fn files(root: &Path) -> ModuleFiles {
    let files = ModuleFiles::new(root.to_path_buf(), Arc::new(AtomicBool::new(false)));
    files.begin(
        AgentCx::for_testing_with_io(),
        Instant::now() + Duration::from_secs(5),
    );
    files
}

#[test]
fn esm_default_import_executes_commonjs_once_with_private_wrapper_state() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("counter.cjs"),
        "var hidden = 40; globalThis.loads = (globalThis.loads || 0) + 1; module.exports = () => ++hidden;",
    )
    .unwrap();
    let kernel = JsKernel::spawn_in(root.path()).unwrap();
    succeeds(&kernel, "globalThis.a = await import('./counter.cjs'); a.default()");
    assert_eq!(
        succeeds(&kernel, "globalThis.b = await import('././counter.cjs'); [a === b, b.default(), loads, typeof hidden, typeof require]"),
        "[true,42,1,\"undefined\",\"undefined\"]"
    );
}

#[test]
fn native_commonjs_cycles_and_json_dependencies_keep_export_identity() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.cjs"), "exports.ready = false; const b = require('./b.cjs'); exports.seen = b.seen; exports.data = require('./data.json'); exports.ready = true; exports.b = b;").unwrap();
    std::fs::write(root.path().join("b.cjs"), "const a = require('./a.cjs'); exports.seen = a.ready; exports.a = a;").unwrap();
    std::fs::write(root.path().join("data.json"), "\u{feff}{\"answer\":42}").unwrap();
    let kernel = JsKernel::spawn_in(root.path()).unwrap();
    assert_eq!(
        succeeds(&kernel, "globalThis.a = (await import('./a.cjs')).default; [a.seen, a.ready, a.b.a === a, a.data.answer]"),
        "[false,true,true,42]"
    );
    assert_eq!(
        succeeds(&kernel, "(await import('./b.cjs')).default === a.b"),
        "true"
    );
}

#[test]
fn linking_a_missing_named_export_does_not_run_commonjs_body() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("value.cjs"), "globalThis.bodyRan = true; exports.value = 42;").unwrap();
    std::fs::write(root.path().join("invalid.mjs"), "import {missing} from './value.cjs'; export default missing;").unwrap();
    let kernel = JsKernel::spawn_in(root.path()).unwrap();
    assert!(!cell(&kernel, "await import('./invalid.mjs')").ok);
    assert_eq!(succeeds(&kernel, "typeof bodyRan"), "\"undefined\"");
}

#[test]
fn cached_requires_check_the_current_cell_owner_without_discarding_exports() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.cjs"), "module.exports = () => require('./value.cjs');").unwrap();
    std::fs::write(root.path().join("value.cjs"), "module.exports = {answer:42};").unwrap();
    let kernel = JsKernel::spawn_in(root.path()).unwrap();
    succeeds(&kernel, "globalThis.get = (await import('./main.cjs')).default; globalThis.first = get()");
    let receiver = {
        let owner = super::super::owner_without_io();
        let _guard = owner.cx().clone().set_current_restricted();
        kernel.submit("get()".into(), Instant::now() + Duration::from_secs(5)).unwrap()
    };
    let denied = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(!denied.ok);
    assert!(denied.error.unwrap().contains("DENIED"));
    assert_eq!(succeeds(&kernel, "get() === first"), "true");
}

#[test]
fn requires_refuse_esm_builtins_remote_paths_and_workspace_escapes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("main.cjs"), "module.exports = require;").unwrap();
    std::fs::write(root.join("esm.mjs"), "globalThis.escaped = true; export default 1;").unwrap();
    std::fs::write(temp.path().join("outside.cjs"), "globalThis.escaped = true;").unwrap();
    let kernel = JsKernel::spawn_in(&root).unwrap();
    succeeds(&kernel, "globalThis.req = (await import('./main.cjs')).default");
    for specifier in ["./esm.mjs", "node:fs", "fs", "https://host/a.cjs", "../outside.cjs", "./main.cjs?x"] {
        let argument = serde_json::to_string(specifier).unwrap();
        let result = cell(&kernel, &format!("req({argument})"));
        assert!(!result.ok, "accepted {specifier}");
    }
    assert_eq!(succeeds(&kernel, "typeof escaped"), "\"undefined\"");
}

#[test]
fn reload_refreshes_cjs_graph_without_rebinding_old_require_closures() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.cjs"), "module.exports = () => require('./value.cjs').answer;").unwrap();
    std::fs::write(root.path().join("value.cjs"), "exports.answer = 1;").unwrap();
    let kernel = JsKernel::spawn_in(root.path()).unwrap();
    succeeds(&kernel, "globalThis.old = (await import('./main.cjs')).default; old()");
    std::fs::write(root.path().join("value.cjs"), "exports.answer = 2;").unwrap();
    succeeds(&kernel, "reloadModules(); globalThis.fresh = (await import('./main.cjs')).default");
    assert_eq!(succeeds(&kernel, "[old(),fresh(),old === fresh]"), "[1,2,false]");
}

#[test]
fn explicit_commonjs_package_exports_and_top_level_return_work() {
    let root = tempfile::tempdir().unwrap();
    let package = root.path().join("node_modules/legacy");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("package.json"), r#"{"exports":"./main.cjs"}"#).unwrap();
    std::fs::write(package.join("main.cjs"), "exports.answer = require('./data.json').n; return; throw new Error('unreachable');").unwrap();
    std::fs::write(package.join("data.json"), r#"{"n":42}"#).unwrap();
    let kernel = JsKernel::spawn_in(root.path()).unwrap();
    assert_eq!(succeeds(&kernel, "(await import('legacy')).default.answer"), "42");
}

#[test]
fn failed_source_reads_and_json_depth_share_native_realm_budgets() {
    let root = tempfile::tempdir().unwrap();
    let input = "module.exports = 42;";
    std::fs::write(root.path().join("a.cjs"), input).unwrap();
    std::fs::write(root.path().join("nested.json"), format!("{}0{}", "[".repeat(65), "]".repeat(65))).unwrap();
    let files = files(root.path());
    let identity = files.resolve_path("eval_script", "./a.cjs").unwrap();
    let json = files.resolve_commonjs(&identity, "./nested.json").unwrap();
    assert!(files.commonjs_request("read", &json, "").unwrap_err().contains("JSON_LIMIT"));
    files.0.borrow_mut().source_bytes = super::super::MAX_SOURCE_BYTES - input.len();
    assert!(files.commonjs_request("read", &identity, "").is_ok());
    assert!(files.commonjs_request("read", &identity, "").unwrap_err().contains("LIMIT"));
    files.finish();
    assert!(files.commonjs_request("check", &identity, "").unwrap_err().contains("CLOSED"));
}

#[test]
fn commonjs_loops_obey_native_cell_deadline_and_retire_state() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("loop.cjs"), "globalThis.beforeLoop = true; while (true) {};").unwrap();
    let kernel = JsKernel::spawn_in(root.path()).unwrap();
    let owner = AgentCx::for_testing_with_io();
    let _guard = owner.cx().clone().set_current_restricted();
    let result = kernel
        .submit("await import('./loop.cjs')".into(), Instant::now() + Duration::from_millis(50))
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert!(!result.ok);
    assert!(result.error.unwrap().contains("EVAL_STATE_RESET"));
    assert_eq!(succeeds(&kernel, "typeof beforeLoop"), "\"undefined\"");
}

#[cfg(unix)]
#[test]
fn symlink_aliases_cannot_change_commonjs_into_esm_or_escape_workspace() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("value.cjs"), "module.exports = 42;").unwrap();
    std::fs::write(root.path().join("esm.mjs"), "export default 1;").unwrap();
    std::os::unix::fs::symlink("value.cjs", root.path().join("alias.cjs")).unwrap();
    std::os::unix::fs::symlink("value.cjs", root.path().join("wrong.mjs")).unwrap();
    std::os::unix::fs::symlink("esm.mjs", root.path().join("wrong.cjs")).unwrap();
    let files = files(root.path());
    assert_eq!(
        files.resolve_path("eval_script", "./value.cjs").unwrap(),
        files.resolve_path("eval_script", "./alias.cjs").unwrap()
    );
    for name in ["./wrong.mjs", "./wrong.cjs"] {
        assert!(files.resolve_path("eval_script", name).unwrap_err().contains("TYPE"));
    }
}
