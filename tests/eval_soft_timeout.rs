//! Opt-in recoverable Python deadlines through the public EvalTool.
#![recursion_limit = "512"]

use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput};
use serde_json::{Value, json};

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("runtime")
        .block_on(future)
}

fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(value) => Some(value.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(unix)]
fn python() -> bool {
    let executable = std::env::var("PI_EVAL_PYTHON").unwrap_or_else(|_| "python3".into());
    let available = std::process::Command::new(executable)
        .args(["-c", "import signal; assert hasattr(signal, 'setitimer')"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !available {
        eprintln!("Skipping: POSIX Python timer unavailable");
    }
    available
}

#[cfg(unix)]
async fn cell(tool: &EvalTool, code: &str) -> ToolOutput {
    tool.execute("cell", json!({"code":code}), None)
        .await
        .expect("cell transport")
}

#[test]
#[cfg(unix)]
fn soft_cpu_deadline_retains_variables_process_and_native_tool_channel() {
    if !python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "native-soft-deadline-payload").unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let seed = cell(&tool, "import os\nkeep = 42\npid = os.getpid()").await;
        assert!(!seed.is_error, "{}", text(&seed));
        let stopped = tool
            .execute(
                "soft",
                json!({
                    "code":"print('before-soft-stop')\nsteps = 0\nwhile True:\n    steps += 1",
                    "soft_timeout_secs":1, "timeout_secs":10,
                }),
                None,
            )
            .await
            .unwrap();
        assert!(stopped.is_error);
        assert!(text(&stopped).contains("before-soft-stop"));
        assert!(text(&stopped).contains("EVAL_SOFT_TIMEOUT"));
        let details = stopped.details.as_ref().unwrap();
        assert_eq!(details["softTimeout"], true);
        assert_eq!(details["stateReset"], false);
        assert_eq!(details["errorKind"], "soft_timeout");
        let next = cell(
            &tool,
            "[keep, steps > 0, os.getpid() == pid, tool.read('data.txt')]",
        )
        .await;
        assert!(!next.is_error, "{}", text(&next));
        assert!(text(&next).contains("42, True, True"));
        assert!(text(&next).contains("native-soft-deadline-payload"));
        assert_eq!(next.details.as_ref().unwrap()["cell"], 3);
    });
}

#[test]
#[cfg(unix)]
fn soft_await_deadline_settles_the_root_and_runs_finally() {
    if !python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let stopped = tool.execute("soft", json!({
            "code":"import asyncio\nroot = asyncio.current_task()\ncleaned = False\ntry:\n    await asyncio.sleep(60)\nfinally:\n    cleaned = True",
            "soft_timeout_secs":1, "timeout_secs":10,
        }), None).await.unwrap();
        assert!(stopped.is_error);
        assert_eq!(stopped.details.as_ref().unwrap()["stateReset"], false);
        let next = cell(&tool, "[root.done(), cleaned]").await;
        assert!(!next.is_error, "{}", text(&next));
        assert!(text(&next).contains("[True, True]"));
    });
}

#[test]
fn invalid_soft_budgets_are_rejected_before_reset_or_kernel_execution() {
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let seeded = tool
            .execute(
                "seed",
                json!({"kernel":"js", "code":"globalThis.keep = 42"}),
                None,
            )
            .await
            .unwrap();
        assert!(!seeded.is_error);
        for budget in [
            json!(0),
            json!(-1),
            json!(true),
            json!("1"),
            json!(1.5),
            json!(600),
        ] {
            let error = tool.execute("bad", json!({
                "code":"unexpected = True", "soft_timeout_secs":budget, "timeout_secs":30, "reset":true,
            }), None).await.unwrap_err();
            assert!(error.to_string().contains("EVAL_INPUT"), "{error}");
        }
        for (kernel, soft, hard) in [("js", 1, 10), ("python", 2, 2), ("python", 3, 2)] {
            let error = tool.execute("bad", json!({
                "kernel":kernel, "code":"", "soft_timeout_secs":soft, "timeout_secs":hard, "reset":true,
            }), None).await.unwrap_err();
            assert!(error.to_string().contains("EVAL_INPUT"), "{error}");
        }
        let kept = tool
            .execute("kept", json!({"kernel":"js", "code":"keep"}), None)
            .await
            .unwrap();
        assert_eq!(text(&kept).trim(), "42");
        assert_eq!(kept.details.as_ref().unwrap()["cell"], 2);
        let schema = tool.parameters();
        assert_eq!(
            schema["properties"]["soft_timeout_secs"]["type"],
            Value::String("integer".into())
        );
    });
}

#[test]
#[cfg(unix)]
fn ignoring_soft_interruption_does_not_extend_the_hard_deadline() {
    if !python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let started = std::time::Instant::now();
        let error = tool.execute("wedged", json!({
            "code":"keep = 42\nwhile True:\n    try:\n        while True:\n            pass\n    except BaseException:\n        pass",
            "soft_timeout_secs":1, "timeout_secs":2,
        }), None).await.unwrap_err();
        assert!(error.to_string().contains("EVAL_TIMEOUT"), "{error}");
        assert!(error.to_string().contains("EVAL_STATE_RESET"), "{error}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let fresh = cell(&tool, "'keep' in globals()").await;
        assert!(!fresh.is_error, "{}", text(&fresh));
        assert_eq!(text(&fresh).trim(), "False");
        assert_eq!(fresh.details.as_ref().unwrap()["cell"], 1);
    });
}

#[test]
#[cfg(unix)]
fn unsettled_root_retires_instead_of_resuming_in_another_cell() {
    if !python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let error = tool.execute("unsettled", json!({
            "code":"import asyncio\nkeep = 42\nasyncio.get_running_loop().stop()\nawait asyncio.sleep(60)",
            "soft_timeout_secs":1, "timeout_secs":10,
        }), None).await.unwrap_err();
        assert!(
            error.to_string().contains("EVAL_KERNEL_INCOMPLETE"),
            "{error}"
        );
        assert!(error.to_string().contains("EVAL_STATE_RESET"), "{error}");
        let fresh = cell(&tool, "'keep' in globals()").await;
        assert!(!fresh.is_error, "{}", text(&fresh));
        assert_eq!(text(&fresh).trim(), "False");
        assert_eq!(fresh.details.as_ref().unwrap()["cell"], 1);
    });
}

#[test]
#[cfg(not(unix))]
fn unsupported_platform_refuses_soft_deadlines_explicitly() {
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    let error = run(tool.execute(
        "unsupported",
        json!({
            "code":"", "soft_timeout_secs":1, "timeout_secs":10,
        }),
        None,
    ))
    .unwrap_err();
    assert!(error.to_string().contains("POSIX Python"));
}
