//! Reachable EvalTool coverage for JavaScript timers and async host calls.
mod common;

use common::TestHarness;
use pi::tools::{Tool, ToolOutput};
use serde_json::json;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
        .block_on(future)
}

fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|block| match block {
            pi::model::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn async_js_can_read_workspace_files_and_reuse_globals() {
    let harness = TestHarness::new("async_js_can_read_workspace_files_and_reuse_globals");
    let root = harness.temp_path(".");
    std::fs::write(root.join("input.txt"), "async-bridge-42").unwrap();
    let tool = pi::eval::EvalTool::new(&root);
    let result=block_on(tool.execute("load", json!({"kernel":"js","code":"await sleep(0); globalThis.loaded = await tool.call('read',{path:'input.txt'}); console.log(loaded)"}), None)).unwrap();
    assert!(!result.is_error, "{}", text(&result));
    assert!(text(&result).contains("async-bridge-42"));
    let result = block_on(tool.execute(
        "reuse",
        json!({"kernel":"js","code":"loaded.includes('async-bridge-42')"}),
        None,
    ))
    .unwrap();
    assert!(!result.is_error);
    assert!(text(&result).contains("true"));
}

#[test]
fn async_js_host_bridge_does_not_gain_write_permission() {
    let harness = TestHarness::new("async_js_host_bridge_does_not_gain_write_permission");
    let root = harness.temp_path(".");
    let tool = pi::eval::EvalTool::new(&root);
    let result=block_on(tool.execute("denied",json!({"kernel":"js","code":"await tool.call('write',{path:'forbidden.txt',content:'must not exist'})"}),None)).unwrap();
    assert!(result.is_error, "{}", text(&result));
    assert!(!root.join("forbidden.txt").exists());
    let result = block_on(tool.execute(
        "after",
        json!({"kernel":"js","code":"new Promise(resolve=>setTimeout(resolve,0,42))"}),
        None,
    ))
    .unwrap();
    assert!(!result.is_error, "{}", text(&result));
    assert!(text(&result).contains("42"));
}

#[test]
fn async_js_timeout_is_disclosed_and_next_cell_is_usable() {
    let harness = TestHarness::new("async_js_timeout_is_disclosed_and_next_cell_is_usable");
    let tool = pi::eval::EvalTool::new(&harness.temp_path("."));
    let result=block_on(tool.execute("timeout",json!({"kernel":"js","timeout_secs":1,"code":"globalThis.old=42; await sleep(60000); old=99"}),None)).unwrap();
    assert!(result.is_error);
    assert!(text(&result).contains("EVAL_STATE_RESET"));
    let result =
        block_on(tool.execute("fresh", json!({"kernel":"js","code":"typeof old"}), None)).unwrap();
    assert!(!result.is_error, "{}", text(&result));
    assert!(text(&result).contains("undefined"));
}
