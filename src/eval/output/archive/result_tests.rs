//! Full cell artifacts exercise the same capture/finalization as the tool.

use super::*;
use crate::eval::EvalTool;
use crate::model::ImageContent;
use crate::tools::Tool;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
        .block_on(future)
}

fn records(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn completed(rows: &[Value]) -> &Value {
    let results: Vec<_> = rows.iter().filter(|row| row["kind"] == "result").collect();
    assert_eq!(
        results.len(),
        1,
        "one authoritative result, not a callback preview"
    );
    assert_eq!(rows[rows.len() - 2]["kind"], "result");
    &results[0]["output"]
}

fn native_output() -> ToolOutput {
    ToolOutput {
        content: vec![
            ContentBlock::Text(TextContent::new("# Final report\n42\n")),
            ContentBlock::Image(ImageContent {
                data: PNG.into(),
                mime_type: "image/png".into(),
            }),
        ],
        details: Some(json!({"kernel":"js", "cell":7, "reset":false,
            "jsonOutputs":[{"answer":42, "observations":[true,null,"界"]}],
            "displays":[{"mimeType":"image/png","width":1,"height":1}]})),
        is_error: false,
    }
}

#[test]
fn native_final_text_structured_data_and_png_survive_without_receipt_recursion() {
    let directory = tempfile::tempdir().unwrap();
    let archive =
        Archive::prepare_sync(directory.path(), "rich.jsonl", "js", AgentCx::for_testing())
            .unwrap();
    let mut capture = super::super::Capture::new("js", None);
    capture.attach_archive(Some(archive));
    let mut output = native_output();
    run(capture.finish_archive(&mut output, false));
    assert!(!output.is_error);
    let rows = records(&directory.path().join("rich.jsonl"));
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["schema"], "pi.eval.output_artifact.v1");
    let saved = completed(&rows);
    assert_eq!(
        saved["details"]["jsonOutputs"],
        json!([{"answer":42,"observations":[true,null,"界"]}])
    );
    assert_eq!(saved["details"]["cell"], 7);
    assert!(saved["details"].get("outputArtifact").is_none());
    let restored: ToolOutput = serde_json::from_value(saved.clone()).unwrap();
    assert!(!restored.is_error);
    assert_eq!(restored.content.len(), 2);
    assert!(
        matches!(&restored.content[0], ContentBlock::Text(text) if text.text == "# Final report\n42\n")
    );
    assert!(
        matches!(&restored.content[1], ContentBlock::Image(image) if image.data == PNG && image.mime_type == "image/png")
    );
    assert_eq!(
        output.content.len(),
        3,
        "only the returned result gets an archive notice"
    );
    assert_eq!(rows[2]["resultIncluded"], true);
    assert_eq!(rows[2]["sequence"], 0);
    assert_eq!(
        output.details.as_ref().unwrap()["outputArtifact"]["resultIncluded"],
        true
    );
}

#[test]
fn failed_final_serialization_preserves_native_result_and_refuses_complete_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let mut archive = Archive::prepare_sync(
        directory.path(),
        "too-big.jsonl",
        "js",
        AgentCx::for_testing(),
    )
    .unwrap();
    archive.limit = archive.bytes + END_RESERVE + 64;
    let mut capture = super::super::Capture::new("js", None);
    capture.attach_archive(Some(archive));
    let mut output = native_output();
    run(capture.finish_archive(&mut output, false));
    assert!(output.is_error);
    assert!(matches!(&output.content[1], ContentBlock::Image(image) if image.data == PNG));
    let details = output.details.as_ref().unwrap();
    assert_eq!(details["jsonOutputs"][0]["answer"], 42);
    assert_eq!(details["outputArtifact"]["status"], "failed");
    assert_eq!(details["outputArtifact"]["executionFailed"], false);
    assert!(
        details["outputArtifact"]["error"]
            .as_str()
            .unwrap()
            .contains("EVAL_ARTIFACT_LIMIT")
    );
    assert!(!directory.path().join("too-big.jsonl").exists());
}

#[test]
fn escaping_is_charged_during_serialization_and_failure_remains_sticky() {
    let directory = tempfile::tempdir().unwrap();
    let mut archive = Archive::prepare_sync(
        directory.path(),
        "escaped.jsonl",
        "js",
        AgentCx::for_testing(),
    )
    .unwrap();
    archive.limit = archive.bytes + END_RESERVE + 400;
    let output = ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new("\0".repeat(128)))],
        details: None,
        is_error: false,
    };
    assert!(
        archive
            .record_result(&output)
            .unwrap_err()
            .to_string()
            .contains("EVAL_ARTIFACT_LIMIT")
    );
    assert!(archive.bytes <= archive.limit - END_RESERVE);
    assert!(
        archive
            .record_result(&native_output())
            .unwrap_err()
            .to_string()
            .contains("EVAL_ARTIFACT_INCOMPLETE")
    );
    assert!(archive.publish_sync(false).is_err());
    assert!(!directory.path().join("escaped.jsonl").exists());
}

#[test]
fn duplicate_results_and_late_console_frames_poison_the_archive() {
    for duplicate in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let mut archive = Archive::prepare_sync(
            directory.path(),
            "closed.jsonl",
            "js",
            AgentCx::for_testing(),
        )
        .unwrap();
        archive.record_result(&native_output()).unwrap();
        let late = if duplicate {
            archive.record_result(&native_output())
        } else {
            archive.record(&json!({"name":"stdout","sequence":1,"text":"late"}))
        };
        assert!(
            late.unwrap_err()
                .to_string()
                .contains("EVAL_ARTIFACT_INCOMPLETE")
        );
        assert!(archive.publish_sync(false).is_err());
        assert!(!directory.path().join("closed.jsonl").exists());
    }
}

#[test]
fn interrupted_result_keeps_live_content_but_never_gets_a_written_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let archive = Archive::prepare_sync(
        directory.path(),
        "partial.jsonl",
        "js",
        AgentCx::for_testing(),
    )
    .unwrap();
    let mut capture = super::super::Capture::new("js", None);
    capture.attach_archive(Some(archive));
    let mut output = native_output();
    output.is_error = true;
    run(capture.finish_archive(&mut output, true));
    assert!(output.is_error);
    assert!(matches!(&output.content[1], ContentBlock::Image(image) if image.data == PNG));
    assert_eq!(
        output.details.as_ref().unwrap()["outputArtifact"]["status"],
        "not_published"
    );
    assert!(!directory.path().join("partial.jsonl").exists());
}

#[test]
fn public_javascript_quiet_cell_saves_its_final_expression_and_no_console_frames() {
    let directory = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(directory.path());
    let output = run(tool.execute(
        "quiet-result",
        json!({
            "kernel":"js","code":"6 * 7","output_path":"answer.jsonl"
        }),
        None,
    ))
    .unwrap();
    assert!(!output.is_error);
    let rows = records(&directory.path().join("answer.jsonl"));
    assert_eq!(rows.len(), 3);
    let saved: ToolOutput = serde_json::from_value(completed(&rows).clone()).unwrap();
    let ContentBlock::Text(saved_text) = &saved.content[0] else {
        panic!("saved result is not text: {:?}", saved.content);
    };
    // The archive keeps the visible result, including the first cell's banner.
    assert_eq!(saved_text.text.trim(), "(js kernel started)\n42");
    assert_eq!(rows.last().unwrap()["stdoutBytes"], 0);
    assert_eq!(rows.last().unwrap()["status"], "success");
}

#[test]
fn public_javascript_large_json_markdown_and_png_persist_even_on_ordinary_exception() {
    let directory = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(directory.path());
    let code = format!(
        "globalThis.keep=42; display({{large:'x'.repeat(300000)}}); display('# report','text/markdown'); display('{PNG}','image/png'); throw new Error('expected-failure');"
    );
    let output = run(tool.execute(
        "rich-error",
        json!({
            "kernel":"js","code":code,"output_path":"report.jsonl"
        }),
        None,
    ))
    .unwrap();
    assert!(output.is_error);
    let rows = records(&directory.path().join("report.jsonl"));
    let saved = completed(&rows);
    assert_eq!(
        saved["details"]["jsonOutputs"][0]["large"]
            .as_str()
            .unwrap()
            .len(),
        300_000
    );
    assert_eq!(saved["isError"], true);
    let saved: ToolOutput = serde_json::from_value(saved.clone()).unwrap();
    assert!(
        saved
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image(image) if image.data == PNG))
    );
    assert!(
        saved.content.iter().any(
            |block| matches!(block, ContentBlock::Text(text) if text.text.contains("# report"))
        )
    );
    assert!(saved.content.iter().any(
        |block| matches!(block, ContentBlock::Text(text) if text.text.contains("expected-failure"))
    ));
    assert_eq!(rows.last().unwrap()["status"], "error");
    let next = run(tool.execute("retained", json!({"kernel":"js","code":"keep"}), None)).unwrap();
    assert!(!next.is_error);
    assert!(matches!(&next.content[0], ContentBlock::Text(text) if text.text.trim() == "42"));
}

#[test]
fn public_python_rich_results_use_the_same_durable_format() {
    if !std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!("Skipping: python3 unavailable");
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(directory.path());
    let code = format!("display({{'answer':42}})\ndisplay('{PNG}',mime='image/png')\n42");
    let output = run(tool.execute(
        "python-rich",
        json!({"code":code,"output_path":"python.jsonl"}),
        None,
    ))
    .unwrap();
    assert!(!output.is_error);
    let rows = records(&directory.path().join("python.jsonl"));
    let saved = completed(&rows);
    assert_eq!(saved["details"]["jsonOutputs"], json!([{"answer":42}]));
    let saved: ToolOutput = serde_json::from_value(saved.clone()).unwrap();
    assert!(
        saved
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image(image) if image.data == PNG))
    );
    assert!(
        matches!(saved.content.last().unwrap(), ContentBlock::Text(text) if text.text.trim() == "42")
    );
    assert_eq!(rows.last().unwrap()["status"], "success");
}
