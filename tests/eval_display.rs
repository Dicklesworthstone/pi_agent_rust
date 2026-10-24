//! Rich display coverage through the actual persistent Python `EvalTool`.
mod common;

use common::TestHarness;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput};
use serde_json::{Value, json};

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

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
        .block_on(tool.execute(
            "display-cell",
            json!({"kernel":"python", "code":code}),
            None,
        ))
        .expect("Python kernel protocol")
}

fn all_text(output: &ToolOutput) -> String {
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

fn finish(harness: &TestHarness) {
    harness
        .log()
        .info("verify", "rich-display assertions passed");
    harness
        .write_jsonl_logs(harness.temp_path("display.jsonl"))
        .unwrap();
}

#[test]
fn python_displays_reach_native_content_and_structured_details() {
    if !python_available() {
        return;
    }
    let harness = TestHarness::new("python_displays_reach_native_content_and_structured_details");
    let tool = pi::eval::EvalTool::new(&harness.temp_path("."));
    let code = format!(
        "import base64\nvalue={{'answer':[42,True,None]}}\ndisplay(value)\nvalue['answer'].append(99)\ndisplay('## Answer',mime='text/markdown')\ndisplay(base64.b64decode('{PNG}'),mime='image/png')\nprint('captured stdout')\nNone"
    );
    let out = cell(&tool, &code);
    assert!(!out.is_error, "{}", all_text(&out));
    assert_eq!(
        out.details.as_ref().unwrap()["jsonOutputs"],
        json!([{"answer":[42,true,null]}])
    );
    assert!(all_text(&out).contains("## Answer"));
    assert!(all_text(&out).contains("captured stdout"));
    let images: Vec<_> = out
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image(image) => Some(image),
            _ => None,
        })
        .collect();
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
    if !python_available() {
        return;
    }
    let harness =
        TestHarness::new("python_display_failure_preserves_prior_output_and_kernel_state");
    let tool = pi::eval::EvalTool::new(&harness.temp_path("."));
    let out = cell(
        &tool,
        "keep=42\ndisplay({'before':1})\ndisplay('unsupported',mime='text/html')",
    );
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
    if !python_available() {
        return;
    }
    let harness =
        TestHarness::new("file_backed_cells_display_and_late_tasks_cannot_pollute_a_later_cell");
    let root = harness.temp_path(".");
    std::fs::write(root.join("view.py"), "display({'script':True})\n").unwrap();
    let tool = pi::eval::EvalTool::new(&root);
    let loaded = cell(&tool, "%load view.py");
    assert!(!loaded.is_error);
    assert_eq!(
        loaded.details.unwrap()["jsonOutputs"],
        json!([{"script":true}])
    );
    let started = cell(
        &tool,
        "import asyncio\nevent=asyncio.Event()\nasync def late():\n    await event.wait()\n    display('stale')\ntask=asyncio.create_task(late())\nNone",
    );
    assert!(!started.is_error);
    let next = cell(
        &tool,
        "event.set()\ntry:\n    await task\nexcept RuntimeError as e:\n    error=str(e)\ndisplay({'current':True})\nerror",
    );
    assert!(!next.is_error, "{}", all_text(&next));
    assert!(all_text(&next).contains("EVAL_DISPLAY_CLOSED"));
    assert_eq!(
        next.details.unwrap()["jsonOutputs"],
        json!([{"current":true}])
    );
    finish(&harness);
}

fn js_cell(tool: &pi::eval::EvalTool, code: &str) -> pi::error::Result<ToolOutput> {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
        .block_on(tool.execute("js-display-cell", json!({"kernel":"js", "code":code}), None))
}

#[test]
fn javascript_rich_output_reaches_native_images_and_json_without_losing_state() {
    use base64::Engine as _;
    let harness = TestHarness::new(
        "javascript_rich_output_reaches_native_images_and_json_without_losing_state",
    );
    let tool = pi::eval::EvalTool::new(&harness.temp_path("."));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(PNG)
        .unwrap();
    let code = format!(
        "const value={{answer:[42,true,null]}}; display(value); value.answer.push(99); display('## Answer','text/markdown'); display(new Uint8Array({}),'image/png'); console.log('captured console'); 17",
        serde_json::to_string(&bytes).unwrap()
    );
    let out = js_cell(&tool, &code).unwrap();
    assert!(!out.is_error, "{}", all_text(&out));
    assert_eq!(out.details.as_ref().unwrap()["kernel"], "js");
    assert_eq!(
        out.details.as_ref().unwrap()["jsonOutputs"],
        json!([{"answer":[42,true,null]}])
    );
    assert!(all_text(&out).contains("captured console"));
    assert!(all_text(&out).ends_with("17\n"));
    assert!(matches!(&out.content[2], ContentBlock::Text(text) if text.text == "## Answer"));
    assert!(
        matches!(&out.content[3], ContentBlock::Image(image) if image.mime_type == "image/png" && image.data == PNG)
    );
    assert_eq!(out.details.as_ref().unwrap()["displays"][2]["width"], 1);
    assert_eq!(out.details.as_ref().unwrap()["displays"][2]["height"], 1);
    let next = js_cell(&tool, "value.answer").unwrap();
    assert!(!next.is_error);
    assert_eq!(all_text(&next), "[42,true,null,99]\n");
    assert!(next.details.unwrap().get("displays").is_none());
    finish(&harness);
}

#[test]
fn javascript_async_display_and_exceptions_keep_the_correct_cell_output() {
    let harness =
        TestHarness::new("javascript_async_display_and_exceptions_keep_the_correct_cell_output");
    let tool = pi::eval::EvalTool::new(&harness.temp_path("."));
    let out = js_cell(&tool, "globalThis.keep=42; display(1); await sleep(1); display(2); throw new Error('after display')").unwrap();
    assert!(out.is_error);
    assert!(all_text(&out).contains("after display"));
    assert_eq!(out.details.unwrap()["jsonOutputs"], json!([1, 2]));
    let out = js_cell(&tool, "keep").unwrap();
    assert!(!out.is_error);
    assert_eq!(all_text(&out), "42\n");
    assert!(out.details.unwrap().get("displays").is_none());
    finish(&harness);
}

#[test]
fn javascript_bad_png_refuses_the_whole_collection_and_discards_the_kernel() {
    let harness =
        TestHarness::new("javascript_bad_png_refuses_the_whole_collection_and_discards_the_kernel");
    let root = harness.temp_path(".");
    std::fs::write(root.join("not-a-display.png"), PNG).unwrap();
    let tool = pi::eval::EvalTool::new(&root);
    for value in [
        "not-a-display.png",
        "https://example.invalid/image.png",
        "aGVsbG8=",
    ] {
        let code =
            format!("globalThis.keep=42; display('valid prefix'); display({value:?},'image/png')");
        let error = js_cell(&tool, &code).expect_err("host must refuse invalid image output");
        assert!(error.to_string().contains("EVAL_DISPLAY_INVALID"));
        let next = js_cell(&tool, "typeof keep").unwrap();
        assert!(!next.is_error);
        assert!(all_text(&next).contains("undefined"));
        assert_eq!(next.details.as_ref().unwrap()["restarted"], true);
        assert!(next.details.unwrap().get("displays").is_none());
    }
    finish(&harness);
}

#[test]
fn javascript_display_limit_is_an_error_not_a_silent_success_or_later_leak() {
    let harness =
        TestHarness::new("javascript_display_limit_is_an_error_not_a_silent_success_or_later_leak");
    let tool = pi::eval::EvalTool::new(&harness.temp_path("."));
    let out = js_cell(&tool, "for(let i=0;i<33;i++)display(i)").unwrap();
    assert!(out.is_error);
    assert!(all_text(&out).contains("EVAL_DISPLAY_LIMIT"));
    assert_eq!(
        out.details.unwrap()["jsonOutputs"],
        json!((0..32).collect::<Vec<_>>())
    );
    let next = js_cell(&tool, "display(null)").unwrap();
    assert!(!next.is_error);
    assert_eq!(next.details.unwrap()["jsonOutputs"], json!([null]));
    finish(&harness);
}
