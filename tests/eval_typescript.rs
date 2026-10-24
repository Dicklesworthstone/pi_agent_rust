//! TypeScript ES modules through the public eval tool.
//!
//! These tests require the repository's DSR Rust lane.

mod common;

use common::TestHarness;
use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput};
use serde_json::{Value, json};

fn run(tool: &EvalTool, input: Value) -> ToolOutput {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
        .block_on(tool.execute("typescript-cell", input, None))
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
        .collect()
}

fn json_output(output: &ToolOutput) -> &Value {
    assert!(!output.is_error, "{}", text(output));
    &output.details.as_ref().unwrap()["jsonOutputs"][0]
}

fn finish(harness: &TestHarness) {
    harness
        .log()
        .info("verify", "TypeScript eval assertions passed");
    let path = harness.temp_path("typescript-events.jsonl");
    harness.write_jsonl_logs(&path).unwrap();
    let errors = common::logging::validate_jsonl_v2_only(&std::fs::read_to_string(path).unwrap());
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn typescript_modules_strip_types_and_keep_runtime_imports() {
    let harness = TestHarness::new("eval_typescript_modules");
    let root = harness.temp_path(".");
    std::fs::write(
        root.join("base.ts"),
        "export type Shape = { answer: number }; export const base: number = 40;",
    )
    .unwrap();
    std::fs::write(
        root.join("answer.mts"),
        "import type {Shape} from './base.ts'; import {base} from './base.ts'; const value: Shape = {answer: base + 2}; export const answer: number = value.answer;",
    )
    .unwrap();

    let tool = EvalTool::new(&root);
    let output = run(
        &tool,
        json!({"kernel":"js","code":"display((await import('./answer.mts')).answer)"}),
    );
    assert_eq!(json_output(&output), 42);
    finish(&harness);
}

#[test]
fn typescript_enum_modules_share_the_native_quickjs_cache_and_reset() {
    let harness = TestHarness::new("eval_typescript_cache");
    let root = harness.temp_path(".");
    let path = root.join("level.ts");
    std::fs::write(
        &path,
        "globalThis.tsLoads = (globalThis.tsLoads || 0) + 1; enum Level { Low = 40, High = 42 } export const answer: number = Level.High;",
    )
    .unwrap();

    let tool = EvalTool::new(&root);
    let input = json!({"kernel":"js","code":"globalThis.level = await import('./level.ts'); display([level.answer, tsLoads])"});
    assert_eq!(json_output(&run(&tool, input.clone())), &json!([42, 1]));

    std::fs::write(
        &path,
        "globalThis.tsLoads = (globalThis.tsLoads || 0) + 1; export const answer: number = 99;",
    )
    .unwrap();
    assert_eq!(json_output(&run(&tool, input.clone())), &json!([42, 1]));

    let mut reset = input;
    reset["reset"] = json!(true);
    let reset = run(&tool, reset);
    assert_eq!(json_output(&reset), &json!([99, 1]));
    assert_eq!(reset.details.as_ref().unwrap()["reset"], true);
    finish(&harness);
}

#[test]
fn malformed_typescript_executes_no_prefix_and_does_not_poison_the_realm() {
    let harness = TestHarness::new("eval_typescript_parse_failure");
    let root = harness.temp_path(".");
    std::fs::write(
        root.join("bad.ts"),
        "globalThis.tsExecuted = true; export const broken: = 1;",
    )
    .unwrap();

    let tool = EvalTool::new(&root);
    let failed = run(
        &tool,
        json!({"kernel":"js","code":"await import('./bad.ts')"}),
    );
    assert!(failed.is_error);
    assert!(text(&failed).contains("EVAL_TYPESCRIPT_PARSE"));

    let state = run(
        &tool,
        json!({"kernel":"js","code":"display(typeof tsExecuted)"}),
    );
    assert_eq!(json_output(&state), "undefined");
    finish(&harness);
}
