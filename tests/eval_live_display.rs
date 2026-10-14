//! Live rich progress through the real retained kernels and public Tool API.
#![recursion_limit = "256"]
mod common;

use common::TestHarness;
use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput, ToolUpdate};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

fn python_available() -> bool {
    let available = std::process::Command::new("python3").arg("--version")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .status().is_ok_and(|status| status.success());
    if !available { eprintln!("Skipping live Python display test: python3 unavailable"); }
    available
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread().blocking_threads(1, 8)
        .build().unwrap().block_on(future)
}

fn text(output: &ToolOutput) -> String {
    output.content.iter().filter_map(|block| match block {
        ContentBlock::Text(value) => Some(value.text.as_str()), _ => None,
    }).collect::<Vec<_>>().join("\n")
}

fn finish(harness: &TestHarness) {
    harness.log().info("verify", "live-display assertions passed");
    harness.write_jsonl_logs(&harness.temp_path("live-display.jsonl")).unwrap();
}

#[test]
fn python_json_and_png_callbacks_unlock_their_own_blocked_cell() {
    if !python_available() { return; }
    let harness = TestHarness::new("python_json_and_png_callbacks_unlock_their_own_blocked_cell");
    let root = harness.temp_path(".");
    let tool = EvalTool::new(&root);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let gate_root = root.clone();
    let callback = Box::new(move |event: ToolUpdate| {
        let details = event.details.as_ref().unwrap();
        assert_eq!(details["status"], "running");
        assert_eq!(details["displayStream"]["complete"], false);
        if details["jsonOutputs"] == json!([{"answer":42}]) {
            std::fs::write(gate_root.join("json-gate"), "ready").unwrap();
        }
        if event.content.iter().any(|block| matches!(block, ContentBlock::Image(image) if image.data == PNG)) {
            std::fs::write(gate_root.join("png-gate"), "ready").unwrap();
        }
        sink.lock().unwrap().push(event);
    });
    let code = format!(
        "value={{'answer':42}}\ndisplay(value)\nassert 'ready' in tool.read('json-gate')\nvalue['answer']=0\ndisplay('{PNG}',mime='image/png')\nassert 'ready' in tool.read('png-gate')\ndisplay('# live markdown',mime='text/markdown')\n42"
    );
    let out = run(tool.execute("live", json!({"code":code}), Some(callback))).unwrap();
    assert!(!out.is_error, "{}", text(&out));
    let details = out.details.as_ref().unwrap();
    assert_eq!(details["jsonOutputs"], json!([{"answer":42}]));
    assert_eq!(details["displayStream"]["sequence"], 3);
    assert_eq!(details["displayStream"]["complete"], true);
    assert_eq!(out.content.iter().filter(|block| matches!(block, ContentBlock::Image(_))).count(), 1);
    assert_eq!(text(&out).matches("# live markdown").count(), 1);
    assert!(events.lock().unwrap().len() >= 2);
    finish(&harness);
}

#[test]
fn python_rich_progress_can_cancel_before_native_bridge_continuation() {
    if !python_available() { return; }
    let harness = TestHarness::new("python_rich_progress_can_cancel_before_native_bridge_continuation");
    let root = harness.temp_path(".");
    std::fs::write(root.join("source"), "ready").unwrap();
    let tool = EvalTool::new(&root);
    let owner = pi::agent_cx::AgentCx::for_testing();
    let cancel = owner.clone();
    let callback = Box::new(move |event: ToolUpdate| {
        if event.details.as_ref().unwrap()["jsonOutputs"] == json!([{"cancel":true}]) {
            cancel.set_cancel_requested(true);
        }
    });
    let error = run(owner.with_current(tool.execute("cancel", json!({"code":concat!(
        "keep=42\ndisplay({'cancel':True})\ntool.read('source')\n",
        "from pathlib import Path\nPath('must-not-exist').write_text('escaped')"
    )}), Some(callback)))).unwrap_err();
    assert!(error.to_string().contains("EVAL_CANCELLED"), "{error}");
    assert!(error.to_string().contains("EVAL_DISPLAY_INCOMPLETE"));
    assert!(!root.join("must-not-exist").exists());
    let next = run(tool.execute("fresh", json!({"code":"'keep' in globals()"}), None)).unwrap();
    assert!(text(&next).contains("False"));
    assert_eq!(next.details.as_ref().unwrap()["cell"], 1);
    assert!(next.details.as_ref().unwrap().get("displayStream").is_none());
    finish(&harness);
}

#[test]
fn python_rewritten_final_display_discards_kernel_instead_of_replacing_live_data() {
    if !python_available() { return; }
    let harness = TestHarness::new("python_rewritten_final_display_discards_kernel_instead_of_replacing_live_data");
    let tool = EvalTool::new(&harness.temp_path("."));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let error = run(tool.execute("forged-final", json!({"code":concat!(
        "keep=7\ndisplay({'value':'original'})\nimport __main__ as server\n",
        "server._DISPLAY_CONTEXT.get().frames[0]['data']['value']='rewritten'"
    )}), Some(Box::new(move |event: ToolUpdate| sink.lock().unwrap().push(event))))).unwrap_err();
    assert!(error.to_string().contains("final rich displays differ"), "{error}");
    assert_eq!(seen.lock().unwrap()[0].details.as_ref().unwrap()["jsonOutputs"], json!([{"value":"original"}]));
    let next = run(tool.execute("fresh", json!({"code":"'keep' in globals()"}), None)).unwrap();
    assert!(text(&next).contains("False"));
    assert_eq!(next.details.as_ref().unwrap()["cell"], 1);
    finish(&harness);
}

#[test]
fn python_large_live_json_is_omitted_honestly_but_final_data_is_complete() {
    if !python_available() { return; }
    let harness = TestHarness::new("python_large_live_json_is_omitted_honestly_but_final_data_is_complete");
    let tool = EvalTool::new(&harness.temp_path("."));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let out = run(tool.execute("large", json!({"code":"display({'large':'x'*300000})"}),
        Some(Box::new(move |event: ToolUpdate| sink.lock().unwrap().push(event))))).unwrap();
    assert!(!out.is_error, "{}", text(&out));
    let events = seen.lock().unwrap();
    assert!(!events.is_empty());
    for event in events.iter() {
        let details = event.details.as_ref().unwrap();
        assert_eq!(details["jsonOutputs"], json!([]));
        assert_eq!(details["displays"][0]["previewOmitted"], true);
        assert_eq!(details["displayStream"]["omittedDisplays"], 1);
    }
    assert_eq!(out.details.as_ref().unwrap()["jsonOutputs"][0]["large"].as_str().unwrap().len(), 300_000);
    assert_eq!(out.details.as_ref().unwrap()["displayStream"]["complete"], true);
    finish(&harness);
}

#[test]
fn python_rich_stream_leaves_console_archive_schema_and_sequence_unchanged() {
    if !python_available() { return; }
    let harness = TestHarness::new("python_rich_stream_leaves_console_archive_schema_and_sequence_unchanged");
    let root = harness.temp_path(".");
    let tool = EvalTool::new(&root);
    let out = run(tool.execute("archive", json!({
        "code":"print('before')\ndisplay({'answer':42})\nprint('after')\nraise ValueError('expected')",
        "output_path":"console.jsonl"
    }), None)).unwrap();
    assert!(out.is_error);
    assert_eq!(out.details.as_ref().unwrap()["outputArtifact"]["status"], "written");
    assert_eq!(out.details.as_ref().unwrap()["displayStream"]["complete"], true);
    let rows: Vec<Value> = std::fs::read_to_string(root.join("console.jsonl")).unwrap()
        .lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(rows[0]["schema"], "pi.eval.console_artifact.v1");
    let frames = &rows[1..rows.len()-1];
    assert_eq!(frames.len(), 4);
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(frame["name"], "stdout");
        assert_eq!(frame["sequence"], index + 1);
        assert!(frame.get("display").is_none());
    }
    assert_eq!(rows.last().unwrap()["status"], "error");
    assert_eq!(rows.last().unwrap()["sequence"], 4);
    finish(&harness);
}
