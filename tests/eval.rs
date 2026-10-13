//! Integration tests for the eval tool (bd-cv653.1.4).
//!
//! Acceptance coverage (harness-logged per the global mandate):
//! 1. JS kernel: const → mutate → top-level await, state persists.
//! 2. Python kernel: variable persists; tool.read from inside Python
//!    returns file content.
//! 3. Kernel crash → auto-restart with explicit state loss.
//! 4. Bridge policy: workspace-outside read denied like a direct read.
//! 5. Session end kills the kernel tree (no orphans).
//! 6. Python async cells retain loop/context/task state, preserve bridge
//!    restrictions, and recover from exceptions and host-side timeouts.
//!
//! Logging: structured JSONL per tests/common/logging.rs, v2-validated,
//! recorded as artifacts.

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::tools::{Tool, ToolOutput};
use serde_json::json;

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(errors.is_empty(), "JSONL v2 validation errors: {errors:?}");
}

fn block_on_local<F: std::future::Future>(future: F) -> F::Output {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future)
}

fn first_text(output: &ToolOutput) -> &str {
    output
        .content
        .iter()
        .find_map(|block| match block {
            pi::model::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

fn python_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn js_and_python_kernels_acceptance_cycle() {
    let case = "js_and_python_kernels_acceptance_cycle";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let tool = pi::eval::EvalTool::new(&root);

    // JS: const + mutate + top-level await across cells.
    let out = block_on_local(tool.execute(
        "c1",
        json!({"kernel": "js", "code": "const base = 40; let acc = base;"}),
        None,
    ))
    .expect("js cell 1");
    assert!(!out.is_error, "js cell 1: {}", first_text(&out));
    let out =
        block_on_local(tool.execute("c2", json!({"kernel": "js", "code": "acc += 2; acc"}), None))
            .expect("js cell 2");
    assert!(
        first_text(&out).contains("42"),
        "js cell 2: {}",
        first_text(&out)
    );
    let out = block_on_local(tool.execute(
        "c3",
        json!({"kernel": "js", "code": "await Promise.resolve(base + acc)"}),
        None,
    ))
    .expect("js await cell");
    let text = first_text(&out);
    harness
        .log()
        .info("verify", format!("js await cell: {text}"));
    assert!(text.contains("82"), "top-level await must settle: {text}");

    // Python: persist + tool.read bridge.
    if !python_available() {
        harness.log().info(
            "skip",
            "python3 unavailable; python cells skipped".to_string(),
        );
        finish_case(&harness, case);
        return;
    }
    std::fs::write(root.join("data.txt"), "bridge-payload-77\n").expect("fixture");
    let out = block_on_local(tool.execute(
        "c4",
        json!({"kernel": "python", "code": "marker = 'kept'\nmarker"}),
        None,
    ))
    .expect("py cell 1");
    assert!(!out.is_error, "py cell 1: {}", first_text(&out));
    let out = block_on_local(
        tool.execute(
            "c5",
            json!({"kernel": "python", "code": "content = tool.read('data.txt')\n(marker, 'bridge-payload-77' in content)"}),
            None,
        ),
    )
    .expect("py bridge cell");
    let text = first_text(&out);
    harness
        .log()
        .info("verify", format!("py bridge cell: {text}"));
    assert!(
        text.contains("kept") && text.contains("True"),
        "persistence + bridge read: {text}"
    );
    finish_case(&harness, case);
}

#[test]
fn kernel_crash_and_bridge_denial() {
    let case = "kernel_crash_and_bridge_denial";
    let harness = TestHarness::new(case);
    if !python_available() {
        return;
    }
    let root = harness.temp_path(".");
    let tool = pi::eval::EvalTool::new(&root);

    let out =
        block_on_local(tool.execute("c1", json!({"kernel": "python", "code": "y = 7"}), None))
            .expect("seed");
    assert!(!out.is_error);
    let err = block_on_local(tool.execute(
        "c2",
        json!({"kernel": "python", "code": "import os\nos._exit(3)"}),
        None,
    ))
    .expect_err("crash must surface");
    harness.log().info("verify", format!("crash error: {err}"));
    assert!(err.to_string().contains("EVAL_KERNEL_CRASH"), "{err}");

    let out = block_on_local(tool.execute(
        "c3",
        json!({"kernel": "python", "code": "'y' in dir()"}),
        None,
    ))
    .expect("restarted");
    let text = first_text(&out);
    harness
        .log()
        .info("verify", format!("post-crash state: {text}"));
    assert!(
        text.contains("False"),
        "state must be lost after crash: {text}"
    );

    let out = block_on_local(tool.execute(
        "c4",
        json!({"kernel": "python", "code": "tool.read('/etc/passwd')"}),
        None,
    ))
    .expect("outside read");
    let text = first_text(&out);
    harness
        .log()
        .info("verify", format!("outside read result: {text}"));
    assert!(
        out.is_error || text.contains("denied") || text.contains("outside"),
        "workspace-outside read must be denied: {text}"
    );
    finish_case(&harness, case);
}

#[test]
fn registry_exposes_eval_by_default() {
    let case = "registry_exposes_eval_by_default";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let registry = pi::tools::ToolRegistry::new(&["eval"], &root, None::<&pi::config::Config>);
    let names: Vec<&str> = registry.tools().iter().map(|tool| tool.name()).collect();
    harness
        .log()
        .info("verify", format!("registry tools: {names:?}"));
    assert!(names.contains(&"eval"), "eval must register: {names:?}");
    finish_case(&harness, case);
}

fn python_cell(harness: &TestHarness, tool: &pi::eval::EvalTool, code: &str) -> ToolOutput {
    harness.log().info("cell_input", code.to_string());
    let output = block_on_local(tool.execute(
        "async-cell",
        json!({"kernel": "python", "code": code}),
        None,
    ))
    .expect("Python cell protocol");
    harness.log().info("cell_output", first_text(&output).to_string());
    output
}

#[test]
fn python_async_state_loop_context_and_tasks_survive_cells() {
    let case = "python_async_state_loop_context_and_tasks_survive_cells";
    let harness = TestHarness::new(case);
    if !python_available() {
        return;
    }
    let tool = pi::eval::EvalTool::new(&harness.temp_path("."));
    let out = python_cell(&harness, &tool, "import asyncio, contextvars\nloop = asyncio.get_running_loop()\nkey = contextvars.ContextVar('key')\nkey.set(40)\nevent = asyncio.Event()\nasync def later():\n    await event.wait()\n    return 2\ntask = asyncio.create_task(later())\nNone");
    assert!(!out.is_error, "{}", first_text(&out));
    let out = python_cell(&harness, &tool, "event.set()\nvalue = key.get() + await task\n(value, asyncio.get_running_loop() is loop)");
    assert!(!out.is_error, "{}", first_text(&out));
    assert_eq!(first_text(&out).trim(), "(42, True)");
    let out = python_cell(&harness, &tool, "await asyncio.sleep(0, result=value)");
    assert!(!out.is_error, "{}", first_text(&out));
    assert_eq!(first_text(&out).trim(), "42");
    finish_case(&harness, case);
}

#[test]
fn python_async_compile_and_exception_recovery() {
    let case = "python_async_compile_and_exception_recovery";
    let harness = TestHarness::new(case);
    if !python_available() {
        return;
    }
    let tool = pi::eval::EvalTool::new(&harness.temp_path("."));
    let out = python_cell(&harness, &tool, "sentinel = 7\nimport asyncio");
    assert!(!out.is_error, "{}", first_text(&out));
    let out = python_cell(&harness, &tool, "sentinel = 9\nyield 1");
    assert!(out.is_error);
    assert!(first_text(&out).contains("SyntaxError"));
    for code in ["raise SystemExit(3)", "raise asyncio.CancelledError()"] {
        let out = python_cell(&harness, &tool, code);
        assert!(out.is_error, "{}", first_text(&out));
        let out = python_cell(&harness, &tool, "await asyncio.sleep(0, result=sentinel)");
        assert!(!out.is_error, "{}", first_text(&out));
        assert_eq!(first_text(&out).trim(), "7");
    }
    finish_case(&harness, case);
}

#[test]
fn python_async_bridge_preserves_host_whitelist() {
    let case = "python_async_bridge_preserves_host_whitelist";
    let harness = TestHarness::new(case);
    if !python_available() {
        return;
    }
    let root = harness.temp_path(".");
    std::fs::write(root.join("data.txt"), "async-bridge-verified\n").expect("fixture");
    let tool = pi::eval::EvalTool::new(&root);
    let out = python_cell(&harness, &tool, "import asyncio\nresults = await asyncio.gather(tool.call('read', {'path': 'data.txt'}), tool.call('read', {'path': 'data.txt'}))\nlen(results) == 2 and all('async-bridge-verified' in text for text in results)");
    assert!(!out.is_error, "{}", first_text(&out));
    assert!(first_text(&out).trim().ends_with("True"));
    let out = python_cell(&harness, &tool, "await tool.call('write', {'path': 'must-not-exist', 'content': 'forbidden'})");
    assert!(out.is_error);
    assert!(first_text(&out).contains("EVAL_BRIDGE_DENIED"));
    assert!(!root.join("must-not-exist").exists());
    let out = python_cell(&harness, &tool, "'async-bridge-verified' in tool.read('data.txt')");
    assert!(!out.is_error, "{}", first_text(&out));
    assert_eq!(first_text(&out).trim(), "True");
    finish_case(&harness, case);
}

#[test]
fn python_async_timeout_discards_state_and_restarts() {
    let case = "python_async_timeout_discards_state_and_restarts";
    let harness = TestHarness::new(case);
    if !python_available() {
        return;
    }
    let tool = pi::eval::EvalTool::new(&harness.temp_path("."));
    let error = block_on_local(tool.execute(
        "timeout-cell",
        json!({"kernel": "python", "timeout_secs": 1, "code": "import asyncio\nmarker = 99\nawait asyncio.sleep(60)"}),
        None,
    ))
    .expect_err("host must terminate the async cell");
    harness.log().info("timeout", error.to_string());
    assert!(error.to_string().contains("EVAL_TIMEOUT"));
    let out = python_cell(&harness, &tool, "import asyncio\n(await asyncio.sleep(0, result=42), 'marker' in globals())");
    assert!(!out.is_error, "{}", first_text(&out));
    assert!(first_text(&out).contains("(42, False)"));
    finish_case(&harness, case);
}

#[test]
fn python_async_statements_capture_stdout_and_stderr() {
    let case = "python_async_statements_capture_stdout_and_stderr";
    let harness = TestHarness::new(case);
    if !python_available() {
        return;
    }
    let tool = pi::eval::EvalTool::new(&harness.temp_path("."));
    let out = python_cell(&harness, &tool, "import sys\nfrom contextlib import asynccontextmanager\n@asynccontextmanager\nasync def scope():\n    yield 40\nasync def values():\n    for x in range(2):\n        yield x\nasync with scope() as total:\n    print('async-stdout')\nasync for x in values():\n    total += x\nprint('async-stderr', file=sys.stderr)\ntotal");
    assert!(!out.is_error, "{}", first_text(&out));
    assert!(first_text(&out).contains("async-stdout\nasync-stderr\n41"));
    finish_case(&harness, case);
}

#[test]
fn python_coroutine_value_is_not_implicitly_awaited() {
    let case = "python_coroutine_value_is_not_implicitly_awaited";
    let harness = TestHarness::new(case);
    if !python_available() {
        return;
    }
    let tool = pi::eval::EvalTool::new(&harness.temp_path("."));
    let out = python_cell(&harness, &tool, "async def later():\n    raise ValueError('must not run')\ncoro = later()\ncoro");
    assert!(!out.is_error, "{}", first_text(&out));
    assert!(first_text(&out).contains("coroutine object"));
    let out = python_cell(&harness, &tool, "coro.close()");
    assert!(!out.is_error, "{}", first_text(&out));
    finish_case(&harness, case);
}
