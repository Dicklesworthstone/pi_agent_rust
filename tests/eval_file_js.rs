//! File-backed JavaScript through the public EvalTool, not a separate evaluator.
#![recursion_limit = "512"]

use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput};
use serde_json::json;

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8).build().unwrap().block_on(future)
}

fn text(output: &ToolOutput) -> String {
    output.content.iter().filter_map(|part| match part {
        ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    }).collect::<Vec<_>>().join("\n")
}

#[test]
fn scripts_reuse_state_and_reload_current_source() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("setup data.js");
    std::fs::write(&script, "globalThis.answer = 41; function inc(x) { return x + 1; } answer").unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let first = tool.execute("load", json!({"kernel":"js", "code":"%load 'setup data.js'"}), None).await.unwrap();
        assert!(!first.is_error, "{}", text(&first));
        assert!(text(&first).trim().ends_with("41"));
        assert_eq!(first.details.as_ref().unwrap()["sourceCommand"], "%load 'setup data.js'");
        let next = tool.execute("next", json!({"kernel":"js", "code":"inc(answer)"}), None).await.unwrap();
        assert_eq!(text(&next).trim(), "42");
        std::fs::write(&script, "globalThis.answer = 99; answer").unwrap();
        let loaded = tool.execute("reload", json!({"kernel":"js", "code":"%load 'setup data.js'"}), None).await.unwrap();
        assert_eq!(text(&loaded).trim(), "99");
        assert_eq!(loaded.details.as_ref().unwrap()["cell"], 3);
    });
}

#[test]
fn scripts_can_await_the_existing_tool_bridge() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "file-backed bridge").unwrap();
    std::fs::write(dir.path().join("async.js"), "await sleep(1); globalThis.payload = await tool.call('read', {path:'data.txt'}); console.log(payload)").unwrap();
    let tool = EvalTool::new(dir.path());
    let result = run(tool.execute("load", json!({"kernel":"js", "code":"%load async.js"}), None)).unwrap();
    assert!(!result.is_error, "{}", text(&result));
    assert!(text(&result).contains("file-backed bridge"));
    let next = run(tool.execute("next", json!({"kernel":"js", "code":"payload.includes('file-backed bridge')"}), None)).unwrap();
    assert_eq!(text(&next).trim(), "true");
}

#[test]
fn load_admission_failure_does_not_apply_requested_reset() {
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(tool.execute("seed", json!({"kernel":"js", "code":"globalThis.keep = 7"}), None)).unwrap();
    let error = run(tool.execute("bad", json!({"kernel":"js", "code":"%load missing.js", "reset":true}), None)).unwrap_err();
    assert!(error.to_string().contains("EVAL_LOAD_PATH"));
    let kept = run(tool.execute("keep", json!({"kernel":"js", "code":"keep"}), None)).unwrap();
    assert_eq!(text(&kept).trim(), "7");
    assert_eq!(kept.details.as_ref().unwrap()["cell"], 2);
}

#[test]
fn syntax_and_runtime_failures_keep_filename_and_do_not_retry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.js");
    let tool = EvalTool::new(dir.path());
    run(tool.execute("seed", json!({"kernel":"js", "code":"globalThis.count = 0"}), None)).unwrap();
    std::fs::write(&path, "globalThis.count += 1; if (").unwrap();
    let syntax = run(tool.execute("syntax", json!({"kernel":"js", "code":"%load broken.js"}), None)).unwrap();
    assert!(syntax.is_error);
    assert!(text(&syntax).contains("broken.js"));
    let count = run(tool.execute("read", json!({"kernel":"js", "code":"count"}), None)).unwrap();
    assert_eq!(text(&count).trim(), "0");
    std::fs::write(&path, "globalThis.count += 1; JSON.parse('{')").unwrap();
    let runtime = run(tool.execute("runtime", json!({"kernel":"js", "code":"%load broken.js"}), None)).unwrap();
    assert!(runtime.is_error);
    let count = run(tool.execute("read", json!({"kernel":"js", "code":"count"}), None)).unwrap();
    assert_eq!(text(&count).trim(), "1");
}
