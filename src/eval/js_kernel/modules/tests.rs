use super::*;
use crate::eval::js_kernel::{JsCellResponse, JsKernel};
use std::time::Duration;

fn cell(kernel: &JsKernel, code: &str) -> JsCellResponse {
    kernel
        .submit(code.into(), Instant::now() + Duration::from_secs(5))
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .expect("module cell must terminate")
}

fn succeeds(kernel: &JsKernel, code: &str) -> JsCellResponse {
    let result = cell(kernel, code);
    assert!(result.ok, "{code}: {:?}", result.error);
    result
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
fn transitive_modules_and_live_bindings_use_the_kernel_workspace() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("lib")).unwrap();
    std::fs::write(
        dir.path().join("lib/count.mjs"),
        "export let value = 40; export function step() { return ++value; }",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("facade.js"),
        "export {value, step} from './lib/count.mjs';",
    )
    .unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    succeeds(
        &kernel,
        "globalThis.counter = await import('./facade.js'); counter.step()",
    );
    assert_eq!(
        succeeds(&kernel, "counter.step(); counter.value")
            .result
            .as_deref(),
        Some("42")
    );
    succeeds(
        &kernel,
        "globalThis.same = await import('./lib/../lib/count.mjs')",
    );
    assert_eq!(
        succeeds(&kernel, "same.value").result.as_deref(),
        Some("42")
    );
}

#[test]
fn cached_module_side_effects_run_once_and_namespaces_remain_private() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("once.mjs"),
        "globalThis.loads = (globalThis.loads || 0) + 1; const privateValue = 7; export const answer = 42;").unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    succeeds(&kernel, "globalThis.a = await import('./once.mjs')");
    std::fs::write(
        dir.path().join("once.mjs"),
        "throw new Error('must not re-evaluate cached source')",
    )
    .unwrap();
    succeeds(&kernel, "globalThis.b = await import('././once.mjs')");
    assert_eq!(
        succeeds(&kernel, "[a === b, a.answer, loads, typeof privateValue]")
            .result
            .as_deref(),
        Some("[true,42,1,\"undefined\"]")
    );
}

#[test]
fn module_top_level_await_uses_existing_timers_and_keeps_its_origin() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("awaited.mjs"),
        "await sleep(1); export const answer = 42; export const location = import.meta.url;",
    )
    .unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    succeeds(&kernel, "globalThis.mod = await import('./awaited.mjs')");
    assert_eq!(
        succeeds(&kernel, "mod.answer").result.as_deref(),
        Some("42")
    );
    let expected =
        url::Url::from_file_path(std::fs::canonicalize(dir.path().join("awaited.mjs")).unwrap())
            .unwrap();
    let location = succeeds(&kernel, "mod.location").result.unwrap();
    assert_eq!(
        serde_json::from_str::<String>(&location).unwrap(),
        expected.as_str()
    );
}

#[test]
fn static_cycles_are_linked_by_quickjs_not_recursive_host_evaluation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.mjs"),
        "import {b} from './b.mjs'; export function a() { return 40; } export const answer = b();",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("b.mjs"),
        "import {a} from './a.mjs'; export function b() { return a() + 2; }",
    )
    .unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    succeeds(&kernel, "globalThis.cycle = await import('./a.mjs')");
    assert_eq!(
        succeeds(&kernel, "cycle.answer").result.as_deref(),
        Some("42")
    );
}

#[test]
fn module_compilation_failure_executes_no_prefix_and_preserves_ordinary_state() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("bad.mjs"),
        "globalThis.executed = true; export const = 1;",
    )
    .unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    succeeds(&kernel, "globalThis.keep = 42");
    assert!(!cell(&kernel, "await import('./bad.mjs')").ok);
    assert_eq!(
        succeeds(&kernel, "[keep, typeof executed]")
            .result
            .as_deref(),
        Some("[42,\"undefined\"]")
    );
}

#[test]
fn module_authority_is_refreshed_for_each_kernel_request() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("value.mjs"), "export const value = 42;").unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    succeeds(&kernel, "globalThis.previous = await import('./value.mjs')");
    let receiver = {
        let owner = owner_without_io();
        let _guard = owner.cx().clone().set_current_restricted();
        kernel
            .submit(
                "await import('./value.mjs')".into(),
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap()
    };
    let denied = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(!denied.ok);
    assert!(denied.error.unwrap().contains("EVAL_MODULE_DENIED"));
    succeeds(&kernel, "globalThis.current = await import('./value.mjs')");
    assert_eq!(
        succeeds(&kernel, "current === previous").result.as_deref(),
        Some("true")
    );
}

#[test]
fn unfinished_module_evaluation_obeys_the_cell_deadline_and_retires_its_realm() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("slow.mjs"),
        "globalThis.moduleMarker = 7; await sleep(60000); export const done = true;",
    )
    .unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    let stopped = kernel
        .submit(
            "await import('./slow.mjs')".into(),
            Instant::now() + Duration::from_millis(50),
        )
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert!(!stopped.ok);
    assert!(stopped.error.as_ref().unwrap().contains("EVAL_STATE_RESET"));
    assert_eq!(
        succeeds(&kernel, "typeof moduleMarker").result.as_deref(),
        Some("\"undefined\"")
    );
}

#[test]
fn imports_refuse_urls_packages_attributes_and_workspace_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(dir.path().join("outside.js"), "globalThis.escaped = true;").unwrap();
    std::fs::write(root.join("ok.js"), "export const value = 1;").unwrap();
    let kernel = JsKernel::spawn_in(&root).unwrap();
    for spec in [
        "../outside.js",
        "https://host/test.js",
        "file:///outside.js",
        "node:fs",
        "package",
        "./ok.js?fresh=1",
        "./ok.js#x",
    ] {
        let quoted = serde_json::to_string(spec).unwrap();
        let out = cell(&kernel, &format!("await import({quoted})"));
        assert!(!out.ok, "accepted {spec}");
        assert!(out.error.unwrap().contains("EVAL_MODULE"));
    }
    assert!(!cell(&kernel, "await import('./ok.js', {with:{type:'json'}})").ok);
    assert_eq!(
        succeeds(&kernel, "typeof escaped").result.as_deref(),
        Some("\"undefined\"")
    );
}

#[test]
fn admission_checks_the_active_owner_before_any_file_work() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("module.js"), "export default 42;").unwrap();
    let files = files(dir.path());
    let name = files.resolve_path("eval_script", "./module.js").unwrap();
    files.finish();
    assert!(files.read_module(&name).err().unwrap().contains("CLOSED"));
    files.begin(owner_without_io(), Instant::now() + Duration::from_secs(5));
    assert!(
        files
            .resolve_path("eval_script", "./missing.js")
            .err()
            .unwrap()
            .contains("DENIED")
    );
    let owner = AgentCx::for_testing_with_io();
    owner.set_cancel_requested(true);
    files.begin(owner, Instant::now() + Duration::from_secs(5));
    assert!(
        files
            .read_module(&name)
            .err()
            .unwrap()
            .contains("CANCELLED")
    );
    files.begin(AgentCx::for_testing_with_io(), Instant::now());
    assert!(files.read_module(&name).err().unwrap().contains("TIMEOUT"));
    assert_eq!(files.0.borrow().declarations, 0);
}

#[test]
fn module_file_and_realm_budgets_reject_before_compilation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("module.js");
    std::fs::write(&path, "export default 42;").unwrap();
    let files = files(dir.path());
    let name = files.resolve_path("eval_script", "./module.js").unwrap();
    let bytes = usize::try_from(std::fs::metadata(&path).unwrap().len()).unwrap();
    files.0.borrow_mut().source_bytes = MAX_SOURCE_BYTES - bytes;
    assert_eq!(files.read_module(&name).unwrap().code, "export default 42;");
    assert!(files.read_module(&name).err().unwrap().contains("LIMIT"));
    files.0.borrow_mut().source_bytes = 0;
    files.0.borrow_mut().declarations = MAX_MODULES - 1;
    assert!(files.read_module(&name).is_ok());
    assert!(files.read_module(&name).err().unwrap().contains("LIMIT"));
    files.0.borrow_mut().declarations = 0;
    std::fs::File::create(&path)
        .unwrap()
        .set_len(2 * 1024 * 1024 + 1)
        .unwrap();
    assert!(files.read_module(&name).err().unwrap().contains("LIMIT"));
}

#[cfg(unix)]
#[test]
fn in_workspace_aliases_share_identity_but_escaping_symlinks_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("actual.mjs"), "export const value = 42;").unwrap();
    std::os::unix::fs::symlink("actual.mjs", root.join("alias.mjs")).unwrap();
    std::fs::write(dir.path().join("outside.mjs"), "export default 0;").unwrap();
    std::os::unix::fs::symlink("../outside.mjs", root.join("escape.mjs")).unwrap();
    let files = files(&root);
    assert_eq!(
        files.resolve_path("eval_script", "./actual.mjs").unwrap(),
        files.resolve_path("eval_script", "./alias.mjs").unwrap()
    );
    assert!(files.resolve_path("eval_script", "./escape.mjs").is_err());
}
