use super::*;
use crate::agent_cx::AgentCx;
use crate::eval::js_kernel::{JsCellResponse, JsKernel};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

fn files(root: &Path) -> ModuleFiles {
    let files = ModuleFiles::new(root.to_path_buf(), Arc::new(AtomicBool::new(false)));
    files.begin(
        AgentCx::for_testing_with_io(),
        Instant::now() + Duration::from_secs(10),
    );
    files
}

fn package(root: &Path, name: &str, manifest: &str, sources: &[(&str, &str)]) -> PathBuf {
    let directory = root.join("node_modules").join(name);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("package.json"), manifest).unwrap();
    for (name, text) in sources {
        let path = directory.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    directory
}

fn cell(kernel: &JsKernel, code: &str) -> JsCellResponse {
    kernel
        .submit(code.into(), Instant::now() + Duration::from_secs(5))
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .expect("package cell must terminate")
}

fn success(kernel: &JsKernel, code: &str) -> JsCellResponse {
    let result = cell(kernel, code);
    assert!(result.ok, "{code}: {:?}", result.error);
    result
}

#[test]
fn package_names_are_not_paths_urls_or_encoded_traversal() {
    assert_eq!(split_package("data-kit").unwrap(), ("data-kit", ""));
    assert_eq!(
        split_package("@scope/data-kit/features/sum").unwrap(),
        ("@scope/data-kit", "/features/sum")
    );
    assert_eq!(
        split_package("@scope/a/@literal").unwrap(),
        ("@scope/a", "/@literal")
    );
    for name in [
        "",
        "/absolute",
        "../outside",
        "@scope",
        "@/name",
        "@scope/",
        "a//b",
        "a/../b",
        "a/node_modules/b",
        "a/%2e%2e/b",
        "a/x?y",
        "a/x#y",
        "a\\b",
        "https://host/x",
        "node:fs",
        "a/b.",
        "a/b ",
        "a/x\0y",
        "a/*",
        "name with spaces",
    ] {
        assert!(split_package(name).is_err(), "{name:?}");
    }
}

#[test]
fn conditions_keep_author_order_and_null_is_not_absence() {
    let parse = |text| serde_json::from_str::<Manifest>(text).unwrap();
    let a = parse(r#"{"exports":{"default":"./first.mjs","import":"./second.mjs"}}"#);
    assert_eq!(
        exported_target(a.exports.as_ref().unwrap(), ".").unwrap(),
        "./first.mjs"
    );
    let b = parse(
        r#"{"exports":{"node":"./node.mjs","import":{"unused":"./x.mjs","default":"./esm.mjs"}}}"#,
    );
    assert_eq!(
        exported_target(b.exports.as_ref().unwrap(), ".").unwrap(),
        "./esm.mjs"
    );
    let sealed = parse(r#"{"exports":null,"main":"./fallback.mjs"}"#);
    assert!(sealed.exports.is_some());
    assert!(exported_target(sealed.exports.as_ref().unwrap(), ".").is_err());
    let blocked = parse(r#"{"exports":{"import":null,"default":"./fallback.mjs"}}"#);
    assert!(exported_target(blocked.exports.as_ref().unwrap(), ".").is_err());
    assert!(parse("{}").exports.is_none());
    for text in [
        r#"{"exports":{"import":"./a.mjs","import":"./b.mjs"}}"#,
        r#"{"exports":"./a.mjs","exports":"./b.mjs"}"#,
    ] {
        assert!(serde_json::from_str::<Manifest>(text).is_err());
    }
}

#[test]
fn exact_exports_seal_private_paths_and_reject_target_escapes() {
    let value: Export =
        serde_json::from_str(r#"{".":"./index.mjs","./data":"./data.json","./private":null}"#)
            .unwrap();
    assert_eq!(exported_target(&value, "./data").unwrap(), "./data.json");
    assert!(exported_target(&value, "./private").is_err());
    assert!(exported_target(&value, "./internal.mjs").is_err());
    let mixed: Export =
        serde_json::from_str(r#"{".":"./index.mjs","default":"./other.mjs"}"#).unwrap();
    assert!(exported_target(&mixed, ".").is_err());
    for target in [
        "../outside.mjs",
        "/absolute.mjs",
        "./../outside.mjs",
        "./node_modules/x.mjs",
        "./%2e%2e/out.mjs",
        "./index",
        "./directory/",
        "file:///x.mjs",
        "./x.node",
    ] {
        assert!(
            target_path(Path::new("/workspace"), target).is_err(),
            "{target}"
        );
    }
    assert_eq!(
        target_path(Path::new("/workspace"), "./x.cjs").unwrap(),
        Path::new("/workspace").join("x.cjs")
    );
}

#[test]
fn nested_dependencies_resolve_nearest_without_searching_above_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    package(
        dir.path(),
        "outside-only",
        r#"{"exports":"./index.mjs"}"#,
        &[("index.mjs", "export default 0;")],
    );
    package(
        &root,
        "dep",
        r#"{"exports":"./root.mjs"}"#,
        &[("root.mjs", "export default 1;")],
    );
    let outer = package(
        &root,
        "parent",
        r#"{"exports":"./index.mjs"}"#,
        &[("index.mjs", "export default 0;")],
    );
    let inner = package(
        &outer,
        "dep",
        r#"{"exports":"./nested.mjs"}"#,
        &[("nested.mjs", "export default 2;")],
    );
    let files = files(&root);
    let importer = std::fs::canonicalize(outer.join("index.mjs")).unwrap();
    let identity = files
        .resolve_path(importer.to_str().unwrap(), "dep")
        .unwrap();
    assert_eq!(
        files.module_path(&identity).unwrap(),
        std::fs::canonicalize(inner.join("nested.mjs"))
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert!(
        files
            .resolve_path("eval_script", "outside-only")
            .unwrap_err()
            .contains("NOT_FOUND")
    );
    std::fs::write(inner.join("package.json"), r#"{"exports":null}"#).unwrap();
    let fresh = self::files(&root);
    assert!(
        fresh
            .resolve_path(importer.to_str().unwrap(), "dep")
            .unwrap_err()
            .contains("NOT_EXPORTED")
    );
}

#[test]
fn metadata_snapshots_permissions_and_budgets_are_realm_owned() {
    let dir = tempfile::tempdir().unwrap();
    let directory = package(
        dir.path(),
        "pkg",
        r#"{"exports":"./old.mjs"}"#,
        &[
            ("old.mjs", "export default 1;"),
            ("new.mjs", "export default 2;"),
        ],
    );
    let files = files(dir.path());
    let first = files.resolve_path("eval_script", "pkg").unwrap();
    std::fs::write(directory.join("package.json"), r#"{"exports":"./new.mjs"}"#).unwrap();
    assert_eq!(files.resolve_path("eval_script", "pkg").unwrap(), first);
    assert_ne!(
        self::files(dir.path())
            .resolve_path("eval_script", "pkg")
            .unwrap(),
        first
    );
    files.begin(
        super::super::owner_without_io(),
        Instant::now() + Duration::from_secs(2),
    );
    assert!(
        files
            .resolve_path("eval_script", "pkg")
            .unwrap_err()
            .contains("DENIED")
    );
    let fresh = self::files(dir.path());
    fresh.0.borrow_mut().packages.bytes = MAX_METADATA_BYTES;
    assert!(
        fresh
            .resolve_path("eval_script", "pkg")
            .unwrap_err()
            .contains("LIMIT")
    );
    let fresh = self::files(dir.path());
    fresh.0.borrow_mut().packages.lookups = MAX_MANIFEST_LOOKUPS;
    assert!(
        fresh
            .resolve_path("eval_script", "pkg")
            .unwrap_err()
            .contains("LIMIT")
    );
    std::fs::write(
        directory.join("package.json"),
        "x".repeat(MAX_MANIFEST_BYTES + 1),
    )
    .unwrap();
    assert!(
        self::files(dir.path())
            .resolve_path("eval_script", "pkg")
            .unwrap_err()
            .contains("LIMIT")
    );
}

#[test]
fn package_javascript_requires_esm_and_honors_nearer_scope() {
    let dir = tempfile::tempdir().unwrap();
    let directory = package(
        dir.path(),
        "pkg",
        r#"{"type":"module","main":"index.js"}"#,
        &[
            ("index.js", "export default 42;"),
            ("legacy/value.js", "globalThis.shouldNotRun = true;"),
        ],
    );
    std::fs::write(
        directory.join("legacy/package.json"),
        r#"{"type":"commonjs"}"#,
    )
    .unwrap();
    let files = files(dir.path());
    let entry = files.resolve_path("eval_script", "pkg").unwrap();
    assert!(
        files
            .resolve_path(&entry, "./legacy/value.js")
            .unwrap_err()
            .contains("COMMONJS")
    );
    package(
        dir.path(),
        "unmarked",
        r#"{"main":"index.js"}"#,
        &[("index.js", "module.exports = 1;")],
    );
    assert!(
        files
            .resolve_path("eval_script", "unmarked")
            .unwrap_err()
            .contains("COMMONJS")
    );
}

#[test]
fn real_kernel_composes_scoped_typed_packages_and_native_data_modules() {
    let dir = tempfile::tempdir().unwrap();
    package(
        dir.path(),
        "@demo/report",
        r#"{"exports":{".":{"import":"./report.mts"},"./data":"./data.json","./template":"./template.md"}}"#,
        &[
            (
                "report.mts",
                "import {base} from 'base-value'; export const answer: number = base + 2;",
            ),
            ("data.json", "{\"answer\":42}"),
            ("template.md", "Answer: {{value}}\r\n"),
        ],
    );
    package(
        dir.path(),
        "base-value",
        r#"{"type":"module","exports":"./index.js"}"#,
        &[("index.js", "export const base = 40;")],
    );
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    let response = success(
        &kernel,
        "import * as report from '@demo/report'; import data from '@demo/report/data' with {type:'json'}; import template from '@demo/report/template' with {type:'text'}; [report.answer, data.answer, template]",
    );
    assert_eq!(
        response.result.as_deref(),
        Some("[42,42,\"Answer: {{value}}\\r\\n\"]")
    );
    assert_eq!(
        success(&kernel, "report.answer + data.answer")
            .result
            .as_deref(),
        Some("84")
    );
    assert!(!cell(&kernel, "await import('@demo/report/data')").ok);
    assert!(
        !cell(
            &kernel,
            "await import('@demo/report', {with:{type:'json'}})"
        )
        .ok
    );
}

#[test]
fn package_and_relative_imports_share_native_cache_and_live_bindings() {
    let dir = tempfile::tempdir().unwrap();
    package(
        dir.path(),
        "counter",
        r#"{"type":"module","exports":"./index.js"}"#,
        &[(
            "index.js",
            "globalThis.loads = (globalThis.loads || 0) + 1; export let n = 40; export function step() { return ++n; }",
        )],
    );
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    success(
        &kernel,
        "globalThis.a = await import('counter'); globalThis.b = await import('./node_modules/counter/index.js'); a.step(); b.step()",
    );
    assert_eq!(
        success(&kernel, "[a === b, a.n, loads]").result.as_deref(),
        Some("[true,42,1]")
    );
}

#[cfg(unix)]
#[test]
fn in_workspace_package_links_work_but_escaping_links_and_special_manifests_fail() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir_all(root.join("packages/linked")).unwrap();
    std::fs::create_dir(root.join("node_modules")).unwrap();
    std::fs::write(
        root.join("packages/linked/package.json"),
        r#"{"type":"module","exports":"./index.js"}"#,
    )
    .unwrap();
    std::fs::write(root.join("packages/linked/index.js"), "export default 42;").unwrap();
    std::os::unix::fs::symlink("../packages/linked", root.join("node_modules/linked")).unwrap();
    let files = files(&root);
    assert!(
        files
            .resolve_path("eval_script", "linked")
            .unwrap()
            .ends_with("packages/linked/index.js")
    );
    let outside = package(
        dir.path(),
        "outside",
        r#"{"exports":"./index.mjs"}"#,
        &[("index.mjs", "export default 0;")],
    );
    std::os::unix::fs::symlink(&outside, root.join("node_modules/escape")).unwrap();
    assert!(
        files
            .resolve_path("eval_script", "escape")
            .unwrap_err()
            .contains("PATH")
    );
    std::fs::create_dir(root.join("node_modules/pipe")).unwrap();
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        root.join("node_modules/pipe/package.json"),
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap();
    assert!(
        files
            .resolve_path("eval_script", "pipe")
            .unwrap_err()
            .contains("LIMIT")
    );
}

#[test]
fn reloaded_package_exports_keep_older_importers_on_their_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let original = r#"{"exports":"./old.mjs"}"#;
    let replacement = r#"{"exports":"./new.mjs"}"#;
    let directory = package(
        dir.path(),
        "pkg",
        original,
        &[
            ("old.mjs", "export default 1;"),
            ("new.mjs", "export default 2;"),
        ],
    );
    let files = files(dir.path());
    let old = files.resolve_path("eval_script", "pkg").unwrap();
    std::fs::write(directory.join("package.json"), replacement).unwrap();
    // Ordinary imports retain both the module identity and metadata snapshot.
    assert_eq!(files.resolve_path("eval_script", "pkg").unwrap(), old);
    files.begin(
        AgentCx::for_testing_with_io(),
        Instant::now() + Duration::from_secs(5),
    );
    assert_eq!(files.reload().unwrap(), 1);
    let fresh = files.resolve_path("eval_script", "pkg").unwrap();
    assert_ne!(old, fresh);
    assert_eq!(
        files.module_path(&fresh).unwrap(),
        std::fs::canonicalize(directory.join("new.mjs"))
            .unwrap()
            .to_str()
            .unwrap()
    );
    // Resolution inside an already-retained module uses its graph's metadata,
    // not the current generation selected for inline cells and loaded scripts.
    assert_eq!(files.resolve_path(&old, "pkg").unwrap(), old);
    assert_eq!(files.resolve_path(&fresh, "pkg").unwrap(), fresh);
    // Each generation also looks up the importer's own (absent) workspace
    // package.json for self-references; absent manifests add no bytes.
    let state = files.0.borrow();
    assert_eq!(state.packages.lookups, 4);
    assert_eq!(state.packages.manifests.len(), 4);
    assert_eq!(state.packages.bytes, original.len() + replacement.len());
}

#[test]
fn missing_metadata_and_lookup_limits_are_versioned_without_resetting_budgets() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let path = root.join("package.json");
    let files = files(&root);
    assert!(manifest(&files, &root, &path, 0).unwrap().is_none());
    std::fs::write(&path, "{}").unwrap();
    assert!(manifest(&files, &root, &path, 0).unwrap().is_none());
    for generation in 1..MAX_MANIFEST_LOOKUPS {
        files.begin(
            AgentCx::for_testing_with_io(),
            Instant::now() + Duration::from_secs(5),
        );
        assert_eq!(files.reload().unwrap() as usize, generation);
        assert!(
            manifest(&files, &root, &path, u32::try_from(generation).unwrap())
                .unwrap()
                .is_some()
        );
    }
    files.begin(
        AgentCx::for_testing_with_io(),
        Instant::now() + Duration::from_secs(5),
    );
    let next = files.reload().unwrap();
    assert!(
        manifest(&files, &root, &path, next)
            .unwrap_err()
            .contains("LIMIT")
    );
    // Existing positive and negative snapshots are still usable at capacity.
    assert!(manifest(&files, &root, &path, 0).unwrap().is_none());
    assert!(manifest(&files, &root, &path, 1).unwrap().is_some());
    let state = files.0.borrow();
    assert_eq!(state.packages.lookups, MAX_MANIFEST_LOOKUPS);
    assert_eq!(state.packages.manifests.len(), MAX_MANIFEST_LOOKUPS);
    assert_eq!(state.packages.bytes, 2 * (MAX_MANIFEST_LOOKUPS - 1));
    drop(state);
    files.begin(
        super::super::owner_without_io(),
        Instant::now() + Duration::from_secs(5),
    );
    assert!(
        manifest(&files, &root, &path, 1)
            .unwrap_err()
            .contains("DENIED")
    );
}

#[test]
fn reloading_package_metadata_cannot_recover_the_realm_byte_allowance() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let path = root.join("package.json");
    let content = r#"{"type":"module"}"#;
    std::fs::write(&path, content).unwrap();
    let files = files(&root);
    files.0.borrow_mut().packages.bytes = MAX_METADATA_BYTES - content.len();
    let original = manifest(&files, &root, &path, 0).unwrap().unwrap();
    assert_eq!(files.0.borrow().packages.bytes, MAX_METADATA_BYTES);
    let generation = files.reload().unwrap();
    assert!(
        manifest(&files, &root, &path, generation)
            .unwrap_err()
            .contains("LIMIT")
    );
    let retained = manifest(&files, &root, &path, 0).unwrap().unwrap();
    assert!(Rc::ptr_eq(&original, &retained));
    let state = files.0.borrow();
    assert_eq!(state.packages.bytes, MAX_METADATA_BYTES);
    assert_eq!(state.packages.lookups, 2);
    assert_eq!(state.packages.manifests.len(), 1);
}
