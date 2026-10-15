//! Python bridge ownership through the public EvalTool.
#![recursion_limit = "256"]

use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput};
use serde_json::json;
use std::process::Command;

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
        .block_on(future)
}

fn text(output: &ToolOutput) -> String {
    output.content.iter().filter_map(|block| match block {
        ContentBlock::Text(value) => Some(value.text.as_str()),
        _ => None,
    }).collect::<Vec<_>>().join("\n")
}

fn python() -> Option<String> {
    let executable = std::env::var("PI_EVAL_PYTHON").unwrap_or_else(|_| "python3".into());
    match Command::new(&executable).arg("--version").output() {
        Ok(output) => {
            assert!(output.status.success(), "Python version probe failed");
            Some(executable)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("SKIP Python bridge tests: interpreter {executable:?} is unavailable");
            None
        }
        Err(error) => panic!("Cannot probe Python interpreter: {error}"),
    }
}

#[test]
fn retained_task_cannot_issue_a_host_read_under_the_next_cell() {
    if python().is_none() { return; }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("private.txt"), "STALE-READ-CANARY").unwrap();
    std::fs::write(dir.path().join("current.txt"), "current result").unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let setup = tool.execute("setup", json!({"code":
            "import asyncio\ngate = asyncio.Event()\nasync def old_work():\n    await gate.wait()\n    return await tool.call('read', {'path':'private.txt'})\nold_task = asyncio.create_task(old_work())"
        }), None).await.unwrap();
        assert!(!setup.is_error, "{}", text(&setup));
        let stale = tool.execute("next", json!({"code":"gate.set(); await old_task"}), None).await.unwrap();
        assert!(stale.is_error);
        assert!(text(&stale).contains("EVAL_BRIDGE_CLOSED"));
        assert!(!text(&stale).contains("STALE-READ-CANARY"));
        let current = tool.execute("recover", json!({"code":"tool.read('current.txt')"}), None).await.unwrap();
        assert!(!current.is_error, "{}", text(&current));
        assert!(text(&current).contains("current result"));
    });
}

#[test]
fn copied_worker_context_does_not_own_protocol_stdin() {
    if python().is_none() { return; }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("current.txt"), "current result").unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let denied = tool.execute("worker", json!({"code":
            "import asyncio; await asyncio.to_thread(tool.read, 'current.txt')"
        }), None).await.unwrap();
        assert!(denied.is_error);
        assert!(text(&denied).contains("EVAL_BRIDGE_THREAD"));
        let current = tool.execute("foreground", json!({"code":"tool.read('current.txt')"}), None).await.unwrap();
        assert!(!current.is_error, "{}", text(&current));
        assert!(text(&current).contains("current result"));
    });
}
