//! Rich display coverage through the actual persistent Python EvalTool.
mod common;

use common::TestHarness;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput};
use serde_json::{Value, json};

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

fn python_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn cell(tool: &pi::eval::EvalTool, code: &str) -> ToolOutput {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
        .block_on(tool.execute("display-cell", json!({"kernel":"python", "code":code}), None))
        .expect("Python kernel protocol")
}

fn all_text(output: &ToolOutput) -> String {
    output.content.iter().filter_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    }).collect::<Vec<_>>().join("\n")
}

fn finish(harness: &TestHarness) {
    harness.log().info("verify", "rich-display assertions passed");
    harness.write_jsonl_logs(&harness.temp_path("display.jsonl")).unwrap();
}

#[test]
fn python_displays_reach_native_content_and_structured_details() {
    if !python_available() { return; }
    let harness = TestHarness::new("python_displays_reach_native_content_and_structured_details");
    let tool = pi::eval::EvalTool::new(&harness.temp_path("."));
    let code = format!(
        "import base64\nvalue={{'answer':[42,True,None]}}\ndisplay(value)\nvalue['answer'].append(99)\ndisplay('## Answer',mime='text/markdown')\ndisplay(base64.b64decode('{PNG}'),mime='image/png')\nprint('captured stdout')\nNone"
    );
    let out = cell(&tool, &code);
    assert!(!out.is_error, "{}", all_text(&out));
    assert_eq!(out.details.as_ref().unwrap()["jsonOutputs"], json!([{"answer":[42,true,null]}]));
    assert!(all_text(&out).contains("## Answer"));
    assert!(all_text(&out).contains("captured stdout"));
    let images: Vec<_> = out.content.iter().filter_map(|block| match block {
        ContentBlock::Image(image) => Some(image), _ => None,
    }).collect();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].data, PNG);
    assert_eq!(images[0].mime_type, "image/png");
    assert_eq!(out.details.as_ref().unwrap()["displays"][2]["width"], 1);
    assert_eq!(out.details.as_ref().unwrap()["displays"][2]["height"], 1);
    let next = cell(&tool, "value['answer']");
    assert!(!next.is_error);
    assert!(all_text(&next).contains("99"));
    assert!(next.details.unwrap().get("displays").is_none());
    finish(&harness);
}

#[test]
fn python_display_failure_preserves_prior_output_and_kernel_state() {
    if !python_available() { return; }
    let harness = TestHarness::new("python_display_failure_preserves_prior_output_and_kernel_state");
    let tool = pi::eval::EvalTool::new(&harness.temp_path("."));
    let out = cell(&tool, "keep=42\ndisplay({'before':1})\ndisplay('unsupported',mime='text/html')");
    assert!(out.is_error);
    assert!(all_text(&out).contains("EVAL_DISPLAY_MIME"));
    assert_eq!(out.details.unwrap()["jsonOutputs"], json!([{"before":1}]));
    let next = cell(&tool, "display(None)\nkeep");
    assert!(!next.is_error);
    assert!(all_text(&next).ends_with("42\n"));
    assert_eq!(next.details.unwrap()["jsonOutputs"], json!([Value::Null]));
    finish(&harness);
}

#[test]
fn file_backed_cells_display_and_late_tasks_cannot_pollute_a_later_cell() {
    if !python_available() { return; }
    let harness = TestHarness::new("file_backed_cells_display_and_late_tasks_cannot_pollute_a_later_cell");
    let root = harness.temp_path(".");
    std::fs::write(root.join("view.py"), "display({'script':True})\n").unwrap();
    let tool = pi::eval::EvalTool::new(&root);
    let loaded = cell(&tool, "%load view.py");
    assert!(!loaded.is_error);
    assert_eq!(loaded.details.unwrap()["jsonOutputs"], json!([{"script":true}]));
    let started = cell(&tool, "import asyncio\nevent=asyncio.Event()\nasync def late():\n    await event.wait()\n    display('stale')\ntask=asyncio.create_task(late())\nNone");
    assert!(!started.is_error);
    let next = cell(&tool, "event.set()\ntry:\n    await task\nexcept RuntimeError as e:\n    error=str(e)\ndisplay({'current':True})\nerror");
    assert!(!next.is_error, "{}", all_text(&next));
    assert!(all_text(&next).contains("EVAL_DISPLAY_CLOSED"));
    assert_eq!(next.details.unwrap()["jsonOutputs"], json!([{"current":true}]));
    finish(&harness);
}
