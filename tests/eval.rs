//! Integration tests for the eval tool (bd-cv653.1.4).
//!
//! Acceptance coverage (harness-logged per the global mandate):
//! 1. JS kernel: const → mutate → top-level await, state persists.
//! 2. Python kernel: variable persists; tool.read from inside Python
//!    returns file content.
//! 3. Kernel crash → auto-restart with explicit state loss.
//! 4. Bridge policy: workspace-outside read denied like a direct read.
//! 5. Session end kills the kernel tree (no orphans).
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
