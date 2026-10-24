//! Public native lifecycle coverage for disposable eval interpreters.
#![recursion_limit = "256"]

use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput};
use serde_json::{Value, json};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
        .block_on(future)
}

fn python(workspace: &Path) -> Option<EvalTool> {
    let probe = Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .stdin(Stdio::null())
        .output();
    match probe {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("SKIP native Python per-call tests: python3 unavailable");
            None
        }
        Err(error) => panic!("Python probe failed: {error}"),
        Ok(probe) => {
            assert!(probe.status.success(), "Python startup failed");
            Some(
                EvalTool::new_with_python(
                    workspace,
                    String::from_utf8(probe.stdout).unwrap().trim(),
                )
                .unwrap(),
            )
        }
    }
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

fn gone(pid: u32, descendant: bool) {
    let pid = sysinfo::Pid::from_u32(pid);
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let mut processes = sysinfo::System::new();
        processes.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        if processes.process(pid).is_none_or(|process| {
            descendant && cfg!(unix) && process.status() == sysinfo::ProcessStatus::Zombie
        }) {
            return;
        }
        assert!(
            Instant::now() < end,
            "private process {pid} outlived its call"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn private_python_process_and_descendants_retire_without_touching_retained_state() {
    let dir = tempfile::tempdir().unwrap();
    let Some(tool) = python(dir.path()) else {
        return;
    };
    run(async {
        let seed = tool
            .execute(
                "seed",
                json!({"code":
                    "import os\nkeep = 42\ndisplay({'pid': os.getpid()})"
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!seed.is_error, "{}", text(&seed));
        let retained_pid = seed.details.unwrap()["jsonOutputs"][0]["pid"].clone();
        // The kernel pops `_PI_EVAL_PACKAGE_ROOT` from the environment at
        // import (576173a35), so read its owned storage root from the module.
        let private = tool.execute("private", json!({
            "kernel_mode":"per_call", "reset":true,
            "code":"import os, sys, subprocess\nassert 'keep' not in globals()\nkeep = 99\nchild = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])\ndisplay({'pid': os.getpid(), 'child': child.pid, 'packages': sys.modules['_pi_eval_kernel']._PACKAGE_ROOT})"
        }), None).await.unwrap();
        assert!(!private.is_error, "{}", text(&private));
        let details = private.details.unwrap();
        assert_eq!(details["cell"], 1);
        assert_eq!(details["kernelMode"], "per_call");
        assert_eq!(details["stateRetained"], false);
        let value = &details["jsonOutputs"][0];
        assert_ne!(value["pid"], retained_pid);
        gone(
            u32::try_from(value["pid"].as_u64().unwrap()).unwrap(),
            false,
        );
        gone(
            u32::try_from(value["child"].as_u64().unwrap()).unwrap(),
            true,
        );
        assert!(!Path::new(value["packages"].as_str().unwrap()).exists());
        let kept = tool
            .execute(
                "kept",
                json!({"code":
                    "assert keep == 42\ndisplay({'pid': os.getpid()})"
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!kept.is_error, "{}", text(&kept));
        assert_eq!(
            kept.details.as_ref().unwrap()["jsonOutputs"][0]["pid"],
            retained_pid
        );
        assert_eq!(kept.details.as_ref().unwrap()["cell"], 2);
    });
}

#[test]
fn private_async_bridge_live_displays_and_console_archive_use_the_native_path() {
    let dir = tempfile::tempdir().unwrap();
    let Some(tool) = python(dir.path()) else {
        return;
    };
    std::fs::write(dir.path().join("input.txt"), "bridge-value-42\n").unwrap();
    std::fs::write(dir.path().join("cell.py"),
        "import asyncio\nvalue = await tool.call('read', {'path':'input.txt'})\nassert 'bridge-value-42' in value\nprint('private-output')\ndisplay({'answer':42})\n").unwrap();
    let updates = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&updates);
    let output = run(tool.execute(
        "loaded",
        json!({
            "kernel_mode":"per_call", "code":"%load cell.py", "output_path":"output.jsonl"
        }),
        Some(Box::new(move |update| {
            observed.lock().unwrap().push(update.details.unwrap());
        })),
    ))
    .unwrap();
    assert!(!output.is_error, "{}", text(&output));
    let details = output.details.unwrap();
    assert_eq!(details["jsonOutputs"], json!([{"answer":42}]));
    assert!(details["pythonRuntime"]["executable"].is_string());
    assert!(!updates.lock().unwrap().is_empty());
    assert!(updates.lock().unwrap().iter().all(|details| {
        details["kernelMode"] == "per_call" && details["stateRetained"] == false
    }));
    let archive = std::fs::read_to_string(dir.path().join("output.jsonl")).unwrap();
    assert!(archive.contains("private-output"));
    for line in archive.lines() {
        serde_json::from_str::<Value>(line).unwrap();
    }
    let retained = run(tool.execute(
        "new-session-kernel",
        json!({"code":"'value' in globals()"}),
        None,
    ))
    .unwrap();
    assert!(!retained.is_error);
    assert!(text(&retained).contains("False"));
    assert_eq!(retained.details.unwrap()["cell"], 1);
}

#[test]
fn private_errors_and_source_refusals_never_reset_the_retained_kernel() {
    let dir = tempfile::tempdir().unwrap();
    let Some(tool) = python(dir.path()) else {
        return;
    };
    run(async {
        assert!(
            !tool
                .execute("seed", json!({"code":"keep = 42"}), None)
                .await
                .unwrap()
                .is_error
        );
        for code in [
            "raise ValueError('private failure')",
            "%load missing.py",
            "await tool.call('bash', {'command':'true'})",
        ] {
            let output = tool
                .execute(
                    "failure",
                    json!({
                        "kernel_mode":"per_call", "reset":true, "code":code
                    }),
                    None,
                )
                .await
                .unwrap();
            assert!(output.is_error);
            assert_eq!(output.details.unwrap()["stateRetained"], false);
        }
        let kept = tool
            .execute("kept", json!({"code":"keep"}), None)
            .await
            .unwrap();
        assert!(text(&kept).trim().ends_with("42"));
        assert_eq!(kept.details.unwrap()["cell"], 2);
    });
}

#[cfg(unix)]
#[test]
fn private_soft_stop_reports_retirement_not_reusable_state() {
    let dir = tempfile::tempdir().unwrap();
    let Some(tool) = python(dir.path()) else {
        return;
    };
    run(async {
        tool.execute("seed", json!({"code":"keep = 42"}), None)
            .await
            .unwrap();
        let output = tool
            .execute(
                "soft",
                json!({
                    "kernel_mode":"per_call", "soft_timeout_secs":1, "timeout_secs":10,
                    "code":"keep = 99\nwhile True:\n    keep += 1"
                }),
                None,
            )
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(text(&output).contains("EVAL_PER_CALL"));
        assert_eq!(output.details.as_ref().unwrap()["softTimeout"], true);
        assert_eq!(output.details.as_ref().unwrap()["stateReset"], true);
        assert_eq!(output.details.as_ref().unwrap()["stateRetained"], false);
        let kept = tool
            .execute("kept", json!({"code":"keep"}), None)
            .await
            .unwrap();
        assert!(text(&kept).trim().ends_with("42"));
    });
}
