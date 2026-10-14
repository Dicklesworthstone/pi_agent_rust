//! Workspace ES modules through the public eval tool, including retained state
//! and file-backed importer origins. These tests require the DSR Rust lane.

mod common;

use common::TestHarness;
use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput};
use serde_json::{Value, json};

fn run(tool: &EvalTool, input: Value) -> ToolOutput {
    asupersync::runtime::RuntimeBuilder::current_thread().blocking_threads(1, 8)
        .build().unwrap().block_on(tool.execute("module-cell", input, None)).unwrap()
}

fn json_output(output: &ToolOutput) -> &Value {
    assert!(!output.is_error, "{:?}", output.content);
    &output.details.as_ref().unwrap()["jsonOutputs"][0]
}

fn finish(harness: &TestHarness) {
    harness.log().info("verify", "module assertions passed");
    let path = harness.temp_path("module-events.jsonl");
    harness.write_jsonl_logs(&path).unwrap();
    let errors = common::logging::validate_jsonl_v2_only(&std::fs::read_to_string(path).unwrap());
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn loaded_scripts_import_relative_to_their_source_even_in_later_cells() {
    let harness = TestHarness::new("eval_modules_source_origin");
    let root = harness.temp_path(".");
    std::fs::create_dir(root.join("analysis")).unwrap();
    std::fs::write(root.join("values.mjs"), "export const answer = -1;").unwrap();
    std::fs::write(root.join("analysis/values.mjs"), "export const answer = 42;").unwrap();
    std::fs::write(root.join("analysis/setup.js"),
        "async function answerLater() { return (await import('./values.mjs')).answer; }").unwrap();
    let tool = EvalTool::new(&root);
    let loaded = run(&tool, json!({"kernel":"js", "code":"%load analysis/setup.js"}));
    assert!(!loaded.is_error, "{:?}", loaded.content);
    assert!(loaded.details.as_ref().unwrap()["sourcePath"].as_str().unwrap().ends_with("setup.js"));
    let result = run(&tool, json!({"kernel":"js", "code":"display(await answerLater())"}));
    assert_eq!(json_output(&result), 42);
    // Inline imports still use the workspace, not the last loaded directory.
    let result = run(&tool, json!({"kernel":"js", "code":"display((await import('./values.mjs')).answer)"}));
    assert_eq!(json_output(&result), -1);
    finish(&harness);
}

#[test]
fn reset_clears_module_cache_without_affecting_another_tool_instance() {
    let harness = TestHarness::new("eval_modules_reset");
    let root = harness.temp_path(".");
    let path = root.join("value.mjs");
    std::fs::write(&path, "export const value = 7;").unwrap();
    let first = EvalTool::new(&root);
    let second = EvalTool::new(&root);
    let input = json!({"kernel":"js", "code":"display((await import('./value.mjs')).value)"});
    assert_eq!(json_output(&run(&first, input.clone())), 7);
    assert_eq!(json_output(&run(&second, input.clone())), 7);
    std::fs::write(&path, "export const value = 42;").unwrap();
    assert_eq!(json_output(&run(&first, input.clone())), 7);
    let mut reset = input.clone();
    reset["reset"] = json!(true);
    let reset = run(&first, reset);
    assert_eq!(json_output(&reset), 42);
    assert_eq!(reset.details.as_ref().unwrap()["reset"], true);
    assert_eq!(json_output(&run(&second, input)), 7);
    finish(&harness);
}

#[test]
fn imported_async_code_uses_real_bridge_displays_and_native_error_contract() {
    let harness = TestHarness::new("eval_modules_native_helpers");
    let root = harness.temp_path(".");
    std::fs::write(root.join("input.txt"), "module-bridge-payload").unwrap();
    std::fs::write(root.join("worker.mjs"),
        "await sleep(1); const text = await tool.call('read', {path:'input.txt'}); display({matched:text.includes('module-bridge-payload')}); export const done = true;").unwrap();
    let tool = EvalTool::new(&root);
    let result = run(&tool, json!({"kernel":"js", "code":"await import('./worker.mjs')"}));
    assert_eq!(json_output(&result), &json!({"matched":true}));
    std::fs::write(root.join("failure.mjs"), "throw new Error('module failure');").unwrap();
    let result = run(&tool, json!({"kernel":"js", "code":"await import('./failure.mjs')"}));
    assert!(result.is_error);
    assert!(result.content.iter().any(|block| matches!(block, ContentBlock::Text(text) if text.text.contains("module failure"))));
    let result = run(&tool, json!({"kernel":"js", "code":"display((await import('./worker.mjs')).done)"}));
    assert_eq!(json_output(&result), true);
    finish(&harness);
}
