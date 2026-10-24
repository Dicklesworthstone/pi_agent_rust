//! Rich Python objects through the public Rust EvalTool and native updates.
//! Optional scientific-library cases are separate from the stdlib-only lane.
#![recursion_limit = "512"]

use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("runtime")
        .block_on(future)
}

fn available() -> bool {
    let python = std::env::var("PI_EVAL_PYTHON").unwrap_or_else(|_| "python3".into());
    let found = std::process::Command::new(python)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !found {
        eprintln!("Skipping rich Python workflow: configured interpreter unavailable");
    }
    found
}

fn matplotlib_available() -> bool {
    if !available() {
        return false;
    }
    let python = std::env::var("PI_EVAL_PYTHON").unwrap_or_else(|_| "python3".into());
    let found = std::process::Command::new(python)
        .args(["-c", "import matplotlib.pyplot"])
        .env("MPLBACKEND", "Agg")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !found {
        eprintln!("Skipping automatic pyplot workflow: optional Matplotlib unavailable");
    }
    found
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

async fn cell(tool: &EvalTool, code: &str) -> ToolOutput {
    tool.execute(
        "rich-cell",
        json!({"kernel":"python", "code":code, "timeout_secs":60}),
        None,
    )
    .await
    .expect("cell transport")
}

#[test]
fn rich_bundle_reaches_native_update_before_cell_can_complete() {
    if !available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let gate = dir.path().join("rich-gate");
    let tool = EvalTool::new(dir.path());
    let observed = Arc::new(AtomicBool::new(false));
    let callback_observed = Arc::clone(&observed);
    let output = run(tool.execute(
        "rich-progress",
        json!({
            "kernel":"python", "timeout_secs":60, "code":r#"
import asyncio
from pathlib import Path
class Report:
    def _repr_mimebundle_(self, include, exclude):
        return {'application/json': payload, 'text/plain': 'unused fallback'}
payload = {'native-rich-ready': 42}
display(Report())
payload['native-rich-ready'] = 99
for attempt in range(500):
    if Path('rich-gate').exists():
        break
    await asyncio.sleep(0.01)
else:
    raise AssertionError('display did not reach native update before completion')
42
"#
        }),
        Some(Box::new(move |update| {
            if update.content.iter().any(|block|
            matches!(block, ContentBlock::Text(text) if text.text.contains("native-rich-ready"))
        ) && std::fs::write(&gate, "observed").is_ok() {
            callback_observed.store(true, Ordering::SeqCst);
        }
        })),
    ))
    .expect("cell transport");
    assert!(!output.is_error, "{}", text(&output));
    assert!(observed.load(Ordering::SeqCst));
    assert_eq!(
        output.details.as_ref().unwrap()["jsonOutputs"],
        json!([{"native-rich-ready":42}])
    );
    assert_eq!(
        output.details.as_ref().unwrap()["displays"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(text(&output).contains("42"));
}

#[test]
fn rich_final_expression_preserves_kernel_and_plain_repr_semantics() {
    if !available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let first = cell(
            &tool,
            r#"
class Report:
    def _repr_markdown_(self):
        return '## Rich answer'
report = Report()
report
"#,
        )
        .await;
        assert!(!first.is_error, "{}", text(&first));
        assert!(text(&first).contains("## Rich answer"));
        assert!(!text(&first).contains("Report object"));
        let retained = cell(&tool, "report").await;
        assert!(!retained.is_error);
        assert_eq!(retained.details.as_ref().unwrap()["restarted"], false);
        assert_eq!(retained.details.as_ref().unwrap()["cell"], 2);
        assert_eq!(text(&retained).trim(), "## Rich answer");
        let ordinary = cell(&tool, "{'answer': 42}").await;
        assert_eq!(text(&ordinary).trim(), "{'answer': 42}");
        assert!(ordinary.details.as_ref().unwrap().get("displays").is_none());
    });
}

#[test]
fn invalid_rich_data_keeps_prior_output_without_success_or_state_loss() {
    if !available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let failed = cell(
            &tool,
            r#"
keep = 42
class Report:
    def _repr_mimebundle_(self, **kwargs):
        return {'image/png': 'not a PNG', 'text/plain': 'must not be used'}
display({'accepted': True})
display(Report())
"#,
        )
        .await;
        assert!(failed.is_error);
        assert_eq!(
            failed.details.as_ref().unwrap()["jsonOutputs"],
            json!([{"accepted":true}])
        );
        assert!(!text(&failed).contains("\nmust not be used\n"));
        let retained = cell(&tool, "keep").await;
        assert!(!retained.is_error);
        assert_eq!(text(&retained).trim(), "42");
        assert_eq!(retained.details.as_ref().unwrap()["restarted"], false);
    });
}

#[test]
fn retained_task_cannot_invoke_rich_formatters_in_a_later_cell() {
    if !available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let seeded = cell(
            &tool,
            r#"
import asyncio
ready = asyncio.Event()
called = []
class Report:
    def _repr_json_(self):
        called.append(True)
        return {'stale': True}
async def old():
    await ready.wait()
    try:
        display(Report())
    except RuntimeError as error:
        return str(error)
    return 'unexpected success'
pending = asyncio.create_task(old())
await asyncio.sleep(0)
"#,
        )
        .await;
        assert!(!seeded.is_error, "{}", text(&seeded));
        let later = cell(
            &tool,
            r#"
ready.set()
assert 'EVAL_DISPLAY_CLOSED' in await pending
assert called == []
display({'current': True})
"#,
        )
        .await;
        assert!(!later.is_error, "{}", text(&later));
        assert_eq!(
            later.details.as_ref().unwrap()["jsonOutputs"],
            json!([{"current":true}])
        );
    });
}

#[test]
fn png_representation_becomes_native_image_content_and_live_progress() {
    if !available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    let observed = Arc::new(AtomicBool::new(false));
    let callback_observed = Arc::clone(&observed);
    let output = run(tool.execute(
        "rich-image",
        json!({
            "kernel":"python", "timeout_secs":60, "code":r#"
import struct, zlib

def png_chunk(kind, data):
    return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))

raster = (b'\x89PNG\r\n\x1a\n'
          + png_chunk(b'IHDR', struct.pack('>IIBBBBB', 2, 1, 8, 6, 0, 0, 0))
          + png_chunk(b'IDAT', zlib.compress(b'\x00\x10\x20\x30\xff\x40\x50\x60\xff'))
          + png_chunk(b'IEND', b''))
class Picture:
    def _repr_png_(self):
        return raster
Picture()
"#
        }),
        Some(Box::new(move |update| {
            if update.content.iter().any(|block|
            matches!(block, ContentBlock::Image(image) if image.mime_type == "image/png")
        ) {
            callback_observed.store(true, Ordering::SeqCst);
        }
        })),
    ))
    .expect("cell transport");
    assert!(!output.is_error, "{}", text(&output));
    assert!(
        observed.load(Ordering::SeqCst),
        "PNG must reach the live native content path"
    );
    let images: Vec<_> = output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image(image) => Some(image),
            _ => None,
        })
        .collect();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].mime_type, "image/png");
    let details = output.details.as_ref().unwrap();
    assert_eq!(details["displays"][0]["width"], 2);
    assert_eq!(details["displays"][0]["height"], 1);
    assert!(!details.to_string().contains(&images[0].data));
}

#[test]
fn automatic_pyplot_output_reaches_native_images_and_retains_closed_figure() {
    if !matplotlib_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let first = cell(
            &tool,
            r#"
import matplotlib
matplotlib.use('agg')
import matplotlib.pyplot as plt
figure, axes = plt.subplots(figsize=(2, 1), dpi=40)
axes.plot([0, 1], [1, 0])
plt.show()
"#,
        )
        .await;
        assert!(!first.is_error, "{}", text(&first));
        assert_eq!(
            first
                .content
                .iter()
                .filter(|block| matches!(block, ContentBlock::Image(_)))
                .count(),
            1
        );
        assert_eq!(first.details.as_ref().unwrap()["displays"][0]["width"], 80);
        let retained = cell(
            &tool,
            "assert plt.get_fignums() == []; axes.set_title('edited'); figure",
        )
        .await;
        assert!(!retained.is_error, "{}", text(&retained));
        assert_eq!(retained.details.as_ref().unwrap()["restarted"], false);
        assert_eq!(
            retained
                .content
                .iter()
                .filter(|block| matches!(block, ContentBlock::Image(_)))
                .count(),
            1
        );
    });
}

#[test]
fn failed_python_cell_keeps_automatic_plot_and_cleans_its_manager() {
    if !matplotlib_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let failed = cell(
            &tool,
            r#"
import matplotlib
matplotlib.use('agg')
import matplotlib.pyplot as plt
figure = plt.figure(figsize=(2, 1), dpi=40)
raise RuntimeError('original plotting cell failed')
"#,
        )
        .await;
        assert!(failed.is_error);
        assert!(text(&failed).contains("original plotting cell failed"));
        assert!(
            failed
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image(_)))
        );
        let later = cell(&tool, "assert plt.get_fignums() == []; 42").await;
        assert!(!later.is_error, "{}", text(&later));
        assert_eq!(text(&later).trim(), "42");
        assert!(
            !later
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image(_)))
        );
    });
}
