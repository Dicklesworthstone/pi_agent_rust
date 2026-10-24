//! Reload workflows through the public tool and real `QuickJS` module loader.
#![recursion_limit = "512"]

use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput};
use serde_json::{Value, json};

fn run(tool: &EvalTool, input: Value) -> ToolOutput {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
        .block_on(tool.execute("reload-test", input, None))
        .unwrap()
}

fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn cell(tool: &EvalTool, code: &str) -> ToolOutput {
    let output = run(tool, json!({"kernel":"js", "code":code}));
    assert!(!output.is_error, "{}", text(&output));
    output
}

#[test]
fn typed_transitive_graph_data_and_canonical_metadata_refresh_without_reset() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("analysis")).unwrap();
    std::fs::write(
        dir.path().join("analysis/value.ts"),
        "export const n: number = 40;",
    )
    .unwrap();
    std::fs::write(dir.path().join("analysis/options.json"), "{\"extra\":2}").unwrap();
    std::fs::write(dir.path().join("analysis/title.md"), "Before\n").unwrap();
    std::fs::write(
        dir.path().join("analysis/entry.mts"),
        concat!(
            "import {n} from './value.ts';\n",
            "import options from './options.json' with {type:'json'};\n",
            "import title from './title.md' with {type:'text'};\n",
            "export const value: number = n + options.extra;\n",
            "export {title}; export const url = import.meta.url;\n",
        ),
    )
    .unwrap();
    let tool = EvalTool::new(dir.path());
    cell(
        &tool,
        "globalThis.scratch = 7; globalThis.old = await import('./analysis/entry.mts')",
    );
    std::fs::write(
        dir.path().join("analysis/value.ts"),
        "export const n: number = 90;",
    )
    .unwrap();
    std::fs::write(dir.path().join("analysis/options.json"), "{\"extra\":9}").unwrap();
    std::fs::write(dir.path().join("analysis/title.md"), "After\n").unwrap();
    let output = cell(
        &tool,
        concat!(
            "reloadModules(); globalThis.fresh = await import('./analysis/entry.mts');",
            "display([scratch,old.value,fresh.value,old.title,fresh.title,old.url===fresh.url]);"
        ),
    );
    assert_eq!(
        output.details.as_ref().unwrap()["jsonOutputs"][0],
        json!([7, 42, 99, "Before\n", "After\n", true])
    );
    assert_eq!(output.details.as_ref().unwrap()["reset"], false);
    assert_eq!(output.details.as_ref().unwrap()["cell"], 2);
    assert_eq!(output.details.as_ref().unwrap()["restarted"], false);
    let native = cell(&tool, "fresh.url");
    let expected = url::Url::from_file_path(
        std::fs::canonicalize(dir.path().join("analysis/entry.mts")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        serde_json::from_str::<String>(text(&native).trim()).unwrap(),
        expected.as_str()
    );
}

#[test]
fn file_backed_scripts_keep_current_import_origin_even_after_same_file_was_a_module() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("analysis")).unwrap();
    std::fs::write(dir.path().join("analysis/leaf.mjs"), "export const n = 42;").unwrap();
    let script = dir.path().join("analysis/setup.js");
    std::fs::write(&script, "globalThis.selected = await import('./leaf.mjs');").unwrap();
    let tool = EvalTool::new(dir.path());
    cell(&tool, "await import('./analysis/setup.js')");
    std::fs::write(dir.path().join("analysis/leaf.mjs"), "export const n = 99;").unwrap();
    cell(&tool, "reloadModules()");
    cell(&tool, "%load analysis/setup.js");
    assert_eq!(text(&cell(&tool, "selected.n")).trim(), "99");
    // The next inline cell must still resolve against the workspace, not
    // the directory of that loaded script or any previous imported module.
    std::fs::write(dir.path().join("root.mjs"), "export const n = 7;").unwrap();
    assert_eq!(
        text(&cell(&tool, "(await import('./root.mjs')).n")).trim(),
        "7"
    );
}

#[test]
fn wrong_import_attributes_and_post_import_reload_are_errors_not_silent_cache_hits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.json"), "{\"value\":42}").unwrap();
    std::fs::write(dir.path().join("module.mjs"), "export const value = 42;").unwrap();
    let tool = EvalTool::new(dir.path());
    cell(
        &tool,
        "reloadModules(); globalThis.data = await import('./data.json', {with:{type:'json'}})",
    );
    let bad = run(
        &tool,
        json!({"kernel":"js", "code":"await import('./data.json')"}),
    );
    assert!(bad.is_error);
    assert!(text(&bad).contains("EVAL_MODULE_ATTRIBUTES"));
    let bad = run(
        &tool,
        json!({"kernel":"js", "code":"await import('./module.mjs'); reloadModules()"}),
    );
    assert!(bad.is_error);
    assert!(text(&bad).contains("EVAL_MODULE_RELOAD_ORDER"));
    assert_eq!(text(&cell(&tool, "data.default.value")).trim(), "42");
    let bad = run(
        &tool,
        json!({"kernel":"js", "code":"reloadModules(); reloadModules()"}),
    );
    assert!(bad.is_error);
    assert!(text(&bad).contains("EVAL_MODULE_RELOAD_ORDER"));
}

#[test]
fn full_reset_remains_explicit_and_separate_tools_do_not_share_reload_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("value.mjs");
    std::fs::write(&path, "export const n = 42;").unwrap();
    let first = EvalTool::new(dir.path());
    let second = EvalTool::new(dir.path());
    cell(
        &first,
        "globalThis.keep = 7; globalThis.value = await import('./value.mjs')",
    );
    cell(&second, "globalThis.value = await import('./value.mjs')");
    std::fs::write(&path, "export const n = 99;").unwrap();
    assert_eq!(
        text(&cell(
            &first,
            "reloadModules(); (await import('./value.mjs')).n"
        ))
        .trim(),
        "99"
    );
    assert_eq!(
        text(&cell(&second, "(await import('./value.mjs')).n")).trim(),
        "42"
    );
    let reset = run(
        &first,
        json!({"kernel":"js", "reset":true, "code":"display(typeof keep)"}),
    );
    assert!(!reset.is_error, "{}", text(&reset));
    assert_eq!(
        reset.details.as_ref().unwrap()["jsonOutputs"][0],
        "undefined"
    );
    assert_eq!(reset.details.as_ref().unwrap()["cell"], 1);
    assert_eq!(text(&cell(&first, "reloadModules()")).trim(), "1");
}

#[test]
fn changed_package_exports_and_transitive_main_refresh_as_one_new_graph() {
    let dir = tempfile::tempdir().unwrap();
    let entry = dir.path().join("node_modules/entry");
    let dependency = dir.path().join("node_modules/dep");
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::create_dir_all(&dependency).unwrap();
    std::fs::write(entry.join("package.json"), r#"{"exports":"./old.mjs"}"#).unwrap();
    std::fs::write(
        entry.join("old.mjs"),
        "export const revision = 1; export async function later(){return (await import('dep')).n;}",
    )
    .unwrap();
    std::fs::write(entry.join("new.mts"),
        "export const revision: number = 2; export async function later(){return (await import('dep')).n;}").unwrap();
    std::fs::write(dependency.join("package.json"), r#"{"main":"./old.mjs"}"#).unwrap();
    std::fs::write(dependency.join("old.mjs"), "export const n = 42;").unwrap();
    std::fs::write(dependency.join("new.mjs"), "export const n = 99;").unwrap();
    let tool = EvalTool::new(dir.path());
    cell(
        &tool,
        "globalThis.keep = 7; globalThis.old = await import('entry'); await old.later()",
    );
    std::fs::write(entry.join("package.json"), r#"{"exports":"./new.mts"}"#).unwrap();
    std::fs::write(dependency.join("package.json"), r#"{"main":"./new.mjs"}"#).unwrap();
    assert_eq!(
        text(&cell(&tool, "(await import('entry')) === old")).trim(),
        "true"
    );
    let output = cell(
        &tool,
        concat!(
            "reloadModules(); globalThis.fresh = await import('entry');",
            "display([keep,old.revision,fresh.revision,await old.later(),await fresh.later()]);"
        ),
    );
    assert_eq!(
        output.details.as_ref().unwrap()["jsonOutputs"][0],
        json!([7, 1, 2, 42, 99])
    );
    assert_eq!(output.details.as_ref().unwrap()["reset"], false);
    assert_eq!(output.details.as_ref().unwrap()["restarted"], false);
}

#[test]
fn newly_created_manifest_requires_reload_and_does_not_erase_working_values() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("node_modules/new-package");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("index.mjs"), "export const n = 42;").unwrap();
    let tool = EvalTool::new(dir.path());
    cell(&tool, "globalThis.keep = 7");
    let missing = run(
        &tool,
        json!({"kernel":"js", "code":"await import('new-package')"}),
    );
    assert!(missing.is_error);
    assert!(text(&missing).contains("EVAL_PACKAGE_MANIFEST"));
    std::fs::write(package.join("package.json"), r#"{"exports":"./index.mjs"}"#).unwrap();
    let cached = run(
        &tool,
        json!({"kernel":"js", "code":"await import('new-package')"}),
    );
    assert!(cached.is_error);
    assert!(text(&cached).contains("EVAL_PACKAGE_MANIFEST"));
    let output = cell(
        &tool,
        "reloadModules(); display([keep,(await import('new-package')).n]);",
    );
    assert_eq!(
        output.details.as_ref().unwrap()["jsonOutputs"][0],
        json!([7, 42])
    );
    assert_eq!(output.details.as_ref().unwrap()["restarted"], false);
}

#[test]
fn changed_package_type_or_sealed_exports_are_not_bypassed_by_old_cache_entries() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("node_modules/pkg");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{"type":"module","exports":"./index.js"}"#,
    )
    .unwrap();
    std::fs::write(package.join("index.js"), "export const n = 42;").unwrap();
    let tool = EvalTool::new(dir.path());
    cell(&tool, "globalThis.old = await import('pkg')");
    std::fs::write(
        package.join("package.json"),
        r#"{"type":"commonjs","exports":"./index.js"}"#,
    )
    .unwrap();
    std::fs::write(
        package.join("index.js"),
        "globalThis.unadmittedPrefix = true; module.exports = 99;",
    )
    .unwrap();
    assert_eq!(
        text(&cell(&tool, "(await import('pkg')) === old")).trim(),
        "true"
    );
    let wrong_type = run(
        &tool,
        json!({"kernel":"js", "code":"reloadModules(); await import('pkg')"}),
    );
    assert!(wrong_type.is_error);
    assert!(text(&wrong_type).contains("EVAL_PACKAGE_COMMONJS"));
    std::fs::write(
        package.join("package.json"),
        r#"{"type":"module","exports":null,"main":"index.js"}"#,
    )
    .unwrap();
    let sealed = run(
        &tool,
        json!({"kernel":"js", "code":"reloadModules(); await import('pkg')"}),
    );
    assert!(sealed.is_error);
    assert!(text(&sealed).contains("EVAL_PACKAGE_NOT_EXPORTED"));
    let kept = cell(&tool, "display([old.n,typeof unadmittedPrefix]);");
    assert_eq!(
        kept.details.as_ref().unwrap()["jsonOutputs"][0],
        json!([42, "undefined"])
    );
}
