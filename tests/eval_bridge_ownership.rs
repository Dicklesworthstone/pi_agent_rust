//! Public EvalTool coverage for cell-owned Python bridge calls.
#![recursion_limit = "512"]

use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput};
use serde_json::json;

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("runtime")
        .block_on(future)
}

fn available() -> bool {
    let python = std::env::var("PI_EVAL_PYTHON").unwrap_or_else(|_| "python3".into());
    let available = std::process::Command::new(python)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !available {
        eprintln!("Skipping: configured Python unavailable");
    }
    available
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
    tool.execute("bridge", json!({"code":code, "timeout_secs":10}), None)
        .await
        .expect("cell transport")
}

#[test]
fn retained_task_cannot_read_with_a_later_cells_authority() {
    if !available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("secret.txt"), "private-native-payload").unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let seed = cell(
            &tool,
            r#"
import asyncio
ready = asyncio.Event()
async def late():
    await ready.wait()
    try:
        return await tool.call('read', {'path':'secret.txt'})
    except Exception as error:
        return str(error)
old_task = asyncio.create_task(late())
await asyncio.sleep(0)
"#,
        )
        .await;
        assert!(!seed.is_error, "{}", text(&seed));
        let later = cell(&tool, "ready.set()\nawait old_task").await;
        assert!(!later.is_error, "{}", text(&later));
        assert!(text(&later).contains("EVAL_BRIDGE_CLOSED"));
        assert!(!text(&later).contains("private-native-payload"));
        let current = cell(&tool, "tool.read('secret.txt')").await;
        assert!(!current.is_error, "{}", text(&current));
        assert!(text(&current).contains("private-native-payload"));
        assert_eq!(current.details.as_ref().unwrap()["cell"], 3);
    });
}

#[test]
fn copied_worker_context_cannot_race_the_control_reader() {
    if !available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "native-payload").unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let worker = cell(
            &tool,
            r#"
import asyncio
try:
    await asyncio.to_thread(tool.read, 'data.txt')
except Exception as error:
    verdict = str(error)
verdict
"#,
        )
        .await;
        assert!(!worker.is_error, "{}", text(&worker));
        assert!(text(&worker).contains("EVAL_BRIDGE_THREAD"));
        let current = cell(&tool, "tool.read('data.txt')").await;
        assert!(!current.is_error, "{}", text(&current));
        assert!(text(&current).contains("native-payload"));
    });
}

#[test]
fn ordinary_native_tool_denial_preserves_state_and_later_bridge_calls() {
    if !available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "native-payload").unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let denied = cell(
            &tool,
            r#"
keep = 42
try:
    await tool.call('bash', {'command':'echo should-not-run'})
except Exception as error:
    verdict = str(error)
verdict
"#,
        )
        .await;
        assert!(!denied.is_error, "{}", text(&denied));
        assert!(text(&denied).contains("EVAL_BRIDGE_DENIED"));
        let next = cell(&tool, "[keep, tool.read('data.txt')]").await;
        assert!(!next.is_error, "{}", text(&next));
        assert!(text(&next).contains("42"));
        assert!(text(&next).contains("native-payload"));
        assert_eq!(next.details.as_ref().unwrap()["cell"], 2);
    });
}

#[test]
fn caught_bridge_corruption_cannot_return_a_reusable_kernel() {
    if !available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "native-payload").unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        // Deliberate kernel-private access supplies a corrupt reply locally;
        // the host's actual reply remains unread. This exercises the Rust
        // retirement path, not an exception that happens to end the cell.
        let broken = tool
            .execute(
                "broken",
                json!({"code":r#"
import io
keep = 42
private = tool.read.__globals__
original = private['_REAL_STDIN']
private['_REAL_STDIN'] = io.StringIO('{"bridge_result":{"call":999,"ok":true,"content":"wrong"}}\n')
try:
    try:
        tool.read('data.txt')
    except Exception:
        pass
finally:
    private['_REAL_STDIN'] = original
42
"#}),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            broken.to_string().contains("EVAL_BRIDGE_PROTOCOL"),
            "{broken}"
        );
        assert!(broken.to_string().contains("EVAL_STATE_RESET"), "{broken}");
        let next = cell(&tool, "['keep' in globals(), tool.read('data.txt')]").await;
        assert!(!next.is_error, "{}", text(&next));
        assert!(text(&next).contains("False"));
        assert!(text(&next).contains("native-payload"));
        assert_eq!(next.details.as_ref().unwrap()["cell"], 1);
    });
}
