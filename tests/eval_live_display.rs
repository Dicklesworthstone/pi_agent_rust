//! Live rich progress through the real retained kernels and public Tool API.
#![recursion_limit = "256"]
mod common;

use common::TestHarness;
use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput, ToolUpdate};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

fn python_available() -> bool {
    let available = std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !available {
        eprintln!("Skipping live Python display test: python3 unavailable");
    }
    available
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
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

fn finish(harness: &TestHarness) {
    harness
        .log()
        .info("verify", "live-display assertions passed");
    harness
        .write_jsonl_logs(harness.temp_path("live-display.jsonl"))
        .unwrap();
}

#[test]
fn python_json_and_png_callbacks_unlock_their_own_blocked_cell() {
    if !python_available() {
        return;
    }
    let harness = TestHarness::new("python_json_and_png_callbacks_unlock_their_own_blocked_cell");
    let root = harness.temp_path(".");
    let tool = EvalTool::new(&root);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let gate_root = root;
    let callback = Box::new(move |event: ToolUpdate| {
        let details = event.details.as_ref().unwrap();
        assert_eq!(details["status"], "running");
        assert_eq!(details["displayStream"]["complete"], false);
        if details["jsonOutputs"] == json!([{"answer":42}]) {
            std::fs::write(gate_root.join("json-gate"), "ready").unwrap();
        }
        if event
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image(image) if image.data == PNG))
        {
            std::fs::write(gate_root.join("png-gate"), "ready").unwrap();
        }
        sink.lock().unwrap().push(event);
    });
    let code = format!(
        "value={{'answer':42}}\ndisplay(value)\nassert 'ready' in tool.read('json-gate')\nvalue['answer']=0\ndisplay('{PNG}',mime='image/png')\nassert 'ready' in tool.read('png-gate')\ndisplay('# live markdown',mime='text/markdown')\n42"
    );
    let out = run(tool.execute("live", json!({"code":code}), Some(callback))).unwrap();
    assert!(!out.is_error, "{}", text(&out));
    let details = out.details.as_ref().unwrap();
    assert_eq!(details["jsonOutputs"], json!([{"answer":42}]));
    assert_eq!(details["displayStream"]["sequence"], 3);
    assert_eq!(details["displayStream"]["complete"], true);
    assert_eq!(
        out.content
            .iter()
            .filter(|block| matches!(block, ContentBlock::Image(_)))
            .count(),
        1
    );
    assert_eq!(text(&out).matches("# live markdown").count(), 1);
    assert!(events.lock().unwrap().len() >= 2);
    finish(&harness);
}

#[test]
fn python_rich_progress_can_cancel_before_native_bridge_continuation() {
    if !python_available() {
        return;
    }
    let harness =
        TestHarness::new("python_rich_progress_can_cancel_before_native_bridge_continuation");
    let root = harness.temp_path(".");
    std::fs::write(root.join("source"), "ready").unwrap();
    let tool = EvalTool::new(&root);
    let owner = pi::agent_cx::AgentCx::for_testing();
    let cancel = owner.clone();
    let callback = Box::new(move |event: ToolUpdate| {
        if event.details.as_ref().unwrap()["jsonOutputs"] == json!([{"cancel":true}]) {
            cancel.set_cancel_requested(true);
        }
    });
    let error = run(owner.with_current(tool.execute(
        "cancel",
        json!({"code":concat!(
            "keep=42\ndisplay({'cancel':True})\ntool.read('source')\n",
            "from pathlib import Path\nPath('must-not-exist').write_text('escaped')"
        )}),
        Some(callback),
    )))
    .unwrap_err();
    assert!(error.to_string().contains("EVAL_CANCELLED"), "{error}");
    assert!(error.to_string().contains("EVAL_DISPLAY_INCOMPLETE"));
    assert!(!root.join("must-not-exist").exists());
    let next = run(tool.execute("fresh", json!({"code":"'keep' in globals()"}), None)).unwrap();
    assert!(text(&next).contains("False"));
    assert_eq!(next.details.as_ref().unwrap()["cell"], 1);
    assert!(
        next.details
            .as_ref()
            .unwrap()
            .get("displayStream")
            .is_none()
    );
    finish(&harness);
}

#[test]
fn python_rewritten_final_display_discards_kernel_instead_of_replacing_live_data() {
    if !python_available() {
        return;
    }
    let harness = TestHarness::new(
        "python_rewritten_final_display_discards_kernel_instead_of_replacing_live_data",
    );
    let tool = EvalTool::new(&harness.temp_path("."));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let error = run(tool.execute(
        "forged-final",
        // The browser bootstrap (7084ac850) loads the kernel as its own
        // module rather than as `__main__`.
        json!({"code":concat!(
            "keep=7\ndisplay({'value':'original'})\nimport sys\nserver = sys.modules['_pi_eval_kernel']\n",
            "server._DISPLAY_CONTEXT.get().frames[0]['data']['value']='rewritten'"
        )}),
        Some(Box::new(move |event: ToolUpdate| {
            sink.lock().unwrap().push(event);
        })),
    ))
    .unwrap_err();
    assert!(
        error.to_string().contains("final rich displays differ"),
        "{error}"
    );
    assert_eq!(
        seen.lock().unwrap()[0].details.as_ref().unwrap()["jsonOutputs"],
        json!([{"value":"original"}])
    );
    let next = run(tool.execute("fresh", json!({"code":"'keep' in globals()"}), None)).unwrap();
    assert!(text(&next).contains("False"));
    assert_eq!(next.details.as_ref().unwrap()["cell"], 1);
    finish(&harness);
}

#[test]
fn python_large_live_json_is_omitted_honestly_but_final_data_is_complete() {
    if !python_available() {
        return;
    }
    let harness =
        TestHarness::new("python_large_live_json_is_omitted_honestly_but_final_data_is_complete");
    let tool = EvalTool::new(&harness.temp_path("."));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let out = run(tool.execute(
        "large",
        json!({"code":"display({'large':'x'*300000})"}),
        Some(Box::new(move |event: ToolUpdate| {
            sink.lock().unwrap().push(event);
        })),
    ))
    .unwrap();
    assert!(!out.is_error, "{}", text(&out));
    let events = seen.lock().unwrap();
    assert!(!events.is_empty());
    for event in events.iter() {
        let details = event.details.as_ref().unwrap();
        assert_eq!(details["jsonOutputs"], json!([]));
        assert_eq!(details["displays"][0]["previewOmitted"], true);
        assert_eq!(details["displayStream"]["omittedDisplays"], 1);
    }
    drop(events);
    assert_eq!(
        out.details.as_ref().unwrap()["jsonOutputs"][0]["large"]
            .as_str()
            .unwrap()
            .len(),
        300_000
    );
    assert_eq!(
        out.details.as_ref().unwrap()["displayStream"]["complete"],
        true
    );
    finish(&harness);
}

#[test]
fn python_rich_stream_archives_results_without_changing_console_sequence() {
    if !python_available() {
        return;
    }
    let harness =
        TestHarness::new("python_rich_stream_archives_results_without_changing_console_sequence");
    let root = harness.temp_path(".");
    let tool = EvalTool::new(&root);
    let out = run(tool.execute("archive", json!({
        "code":"print('before')\ndisplay({'answer':42})\nprint('after')\nraise ValueError('expected')",
        "output_path":"console.jsonl"
    }), None)).unwrap();
    assert!(out.is_error);
    assert_eq!(
        out.details.as_ref().unwrap()["outputArtifact"]["status"],
        "written"
    );
    assert_eq!(
        out.details.as_ref().unwrap()["displayStream"]["complete"],
        true
    );
    let rows: Vec<Value> = std::fs::read_to_string(root.join("console.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows[0]["schema"], "pi.eval.output_artifact.v1");
    let frames: Vec<_> = rows
        .iter()
        .filter(|row| row.get("name").is_some())
        .collect();
    assert_eq!(frames.len(), 4);
    assert_eq!(rows.len(), frames.len() + 3);
    let result = &rows[rows.len() - 2];
    assert_eq!(result["kind"], "result");
    assert_eq!(
        result["output"]["details"]["jsonOutputs"],
        json!([{"answer":42}])
    );
    assert_eq!(result["output"]["isError"], true);
    assert!(result["output"]["details"].get("outputArtifact").is_none());
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(frame["name"], "stdout");
        assert_eq!(frame["sequence"], index + 1);
        assert!(frame.get("display").is_none());
    }
    assert_eq!(rows.last().unwrap()["status"], "error");
    assert_eq!(rows.last().unwrap()["sequence"], 4);
    finish(&harness);
}

#[test]
fn javascript_json_and_png_callbacks_unlock_their_own_blocked_cell() {
    let harness =
        TestHarness::new("javascript_json_and_png_callbacks_unlock_their_own_blocked_cell");
    let root = harness.temp_path(".");
    let tool = EvalTool::new(&root);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let gate_root = root;
    let callback = Box::new(move |event: ToolUpdate| {
        let details = event.details.as_ref().unwrap();
        assert_eq!(details["status"], "running");
        assert_eq!(details["displayStream"]["complete"], false);
        if details["jsonOutputs"] == json!([{"answer":42}]) {
            std::fs::write(gate_root.join("json-gate"), "ready").unwrap();
        }
        if event
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image(image) if image.data == PNG))
        {
            std::fs::write(gate_root.join("png-gate"), "ready").unwrap();
        }
        sink.lock().unwrap().push(event);
    });
    let code = format!(
        "const value={{answer:42}}; display(value); \
         if (!tool.read('json-gate').includes('ready')) throw new Error('JSON not delivered'); \
         value.answer=0; await sleep(1); display('{PNG}','image/png'); \
         if (!(await tool.call('read',{{path:'png-gate'}})).includes('ready')) throw new Error('PNG not delivered'); \
         display('# live markdown','text/markdown'); 42"
    );
    let out = run(tool.execute(
        "live-js",
        json!({"kernel":"js","code":code}),
        Some(callback),
    ))
    .unwrap();
    assert!(!out.is_error, "{}", text(&out));
    let details = out.details.as_ref().unwrap();
    assert_eq!(details["jsonOutputs"], json!([{"answer":42}]));
    assert_eq!(details["displayStream"]["sequence"], 3);
    assert_eq!(details["displayStream"]["complete"], true);
    assert_eq!(
        out.content
            .iter()
            .filter(|block| matches!(block, ContentBlock::Image(_)))
            .count(),
        1
    );
    assert_eq!(text(&out).matches("# live markdown").count(), 1);
    assert!(events.lock().unwrap().len() >= 2);
    finish(&harness);
}

#[test]
#[allow(clippy::literal_string_with_formatting_args)] // JS object literals, not format args
fn javascript_rich_callback_cancellation_retires_state_and_refuses_archive() {
    let harness =
        TestHarness::new("javascript_rich_callback_cancellation_retires_state_and_refuses_archive");
    let root = harness.temp_path(".");
    std::fs::write(root.join("source"), "ready").unwrap();
    let tool = EvalTool::new(&root);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::clone(&seen);
    let out = run(async {
        let owner = pi::agent_cx::AgentCx::for_current_or_request();
        let cancel = owner.clone();
        let callback = Box::new(move |event: ToolUpdate| {
            if event.details.as_ref().unwrap()["jsonOutputs"] == json!([{"cancel":true}]) {
                cancel.set_cancel_requested(true);
            }
            events.lock().unwrap().push(event);
        });
        owner.with_current(tool.execute("cancel-js", json!({
            "kernel":"js", "output_path":"cancelled.jsonl",
            "code":"globalThis.keep=42; display({cancel:true}); tool.read('source'); display({escaped:true});"
        }), Some(callback))).await
    }).unwrap();
    assert!(out.is_error, "{}", text(&out));
    assert!(text(&out).contains("EVAL_CANCELLED"));
    assert_eq!(out.details.as_ref().unwrap()["stateReset"], true);
    assert_eq!(
        out.details.as_ref().unwrap()["displayStream"]["complete"],
        false
    );
    assert!(!root.join("cancelled.jsonl").exists());
    let events = seen.lock().unwrap();
    assert!(!events.is_empty());
    for event in events.iter() {
        assert_eq!(
            event.details.as_ref().unwrap()["jsonOutputs"],
            json!([{"cancel":true}])
        );
    }
    drop(events);
    let next = run(tool.execute(
        "fresh-js",
        json!({"kernel":"js","code":"typeof keep === 'undefined'"}),
        None,
    ))
    .unwrap();
    assert!(!next.is_error, "{}", text(&next));
    assert!(text(&next).contains("true"));
    assert_eq!(next.details.as_ref().unwrap()["cell"], 1);
    assert!(
        next.details
            .as_ref()
            .unwrap()
            .get("displayStream")
            .is_none()
    );
    finish(&harness);
}

#[test]
fn javascript_large_live_json_is_omitted_but_final_snapshot_is_complete() {
    let harness =
        TestHarness::new("javascript_large_live_json_is_omitted_but_final_snapshot_is_complete");
    let tool = EvalTool::new(&harness.temp_path("."));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let out = run(tool.execute(
        "large-js",
        json!({
            "kernel":"js","code":"display({large:'x'.repeat(300000)})"
        }),
        Some(Box::new(move |event: ToolUpdate| {
            sink.lock().unwrap().push(event);
        })),
    ))
    .unwrap();
    assert!(!out.is_error, "{}", text(&out));
    let events = seen.lock().unwrap();
    assert!(!events.is_empty());
    for event in events.iter() {
        let details = event.details.as_ref().unwrap();
        assert_eq!(details["jsonOutputs"], json!([]));
        assert_eq!(details["displays"][0]["previewOmitted"], true);
        assert_eq!(details["displayStream"]["omittedDisplays"], 1);
    }
    drop(events);
    assert_eq!(
        out.details.as_ref().unwrap()["jsonOutputs"][0]["large"]
            .as_str()
            .unwrap()
            .len(),
        300_000
    );
    assert_eq!(
        out.details.as_ref().unwrap()["displayStream"]["complete"],
        true
    );
    finish(&harness);
}

#[test]
#[allow(clippy::literal_string_with_formatting_args)] // JS object literals, not format args
fn javascript_live_displays_preserve_console_archives_and_ordinary_error_state() {
    let harness = TestHarness::new(
        "javascript_live_displays_preserve_console_archives_and_ordinary_error_state",
    );
    let root = harness.temp_path(".");
    let tool = EvalTool::new(&root);
    let out = run(tool.execute("archive-js", json!({
        "kernel":"js", "output_path":"console.jsonl",
        "code":"globalThis.keep=42; console.log('before'); display({answer:keep}); console.error('after'); throw new Error('expected');"
    }), None)).unwrap();
    assert!(out.is_error);
    assert_eq!(
        out.details.as_ref().unwrap()["outputArtifact"]["status"],
        "written"
    );
    assert_eq!(
        out.details.as_ref().unwrap()["displayStream"]["complete"],
        true
    );
    assert_eq!(
        out.details.as_ref().unwrap()["jsonOutputs"],
        json!([{"answer":42}])
    );
    let rows: Vec<Value> = std::fs::read_to_string(root.join("console.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows[0]["schema"], "pi.eval.output_artifact.v1");
    let frames: Vec<_> = rows
        .iter()
        .filter(|row| row.get("name").is_some())
        .collect();
    assert_eq!(frames.len(), 2);
    assert_eq!(rows.len(), frames.len() + 3);
    let result = &rows[rows.len() - 2];
    assert_eq!(result["kind"], "result");
    assert_eq!(
        result["output"]["details"]["jsonOutputs"],
        json!([{"answer":42}])
    );
    assert_eq!(result["output"]["isError"], true);
    assert!(result["output"]["details"].get("outputArtifact").is_none());
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(frame["sequence"], index + 1);
        assert!(frame.get("display").is_none());
    }
    assert_eq!(frames[0]["name"], "stdout");
    assert_eq!(frames[0]["text"], "before\n");
    assert_eq!(frames[1]["name"], "stderr");
    assert_eq!(frames[1]["text"], "after\n");
    assert_eq!(rows.last().unwrap()["status"], "error");
    assert_eq!(rows.last().unwrap()["sequence"], 2);
    let next = run(tool.execute(
        "next-js",
        json!({"kernel":"js","code":"display({kept:keep})"}),
        None,
    ))
    .unwrap();
    assert!(!next.is_error, "{}", text(&next));
    assert_eq!(
        next.details.as_ref().unwrap()["jsonOutputs"],
        json!([{"kept":42}])
    );
    assert_eq!(
        next.details.as_ref().unwrap()["displayStream"]["sequence"],
        1
    );
    finish(&harness);
}

#[test]
fn javascript_invalid_live_png_never_reaches_a_callback_or_reusable_kernel() {
    let harness =
        TestHarness::new("javascript_invalid_live_png_never_reaches_a_callback_or_reusable_kernel");
    let tool = EvalTool::new(&harness.temp_path("."));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let error = run(tool.execute("invalid-js", json!({
        "kernel":"js","code":"globalThis.keep=42; display(new Uint8Array([0,255,1,2]),'image/png')"
    }), Some(Box::new(move |event: ToolUpdate| sink.lock().unwrap().push(event))))).unwrap_err();
    assert!(error.to_string().contains("EVAL_DISPLAY"), "{error}");
    assert!(
        seen.lock().unwrap().is_empty(),
        "invalid PNG must be rejected before any callback"
    );
    let next = run(tool.execute(
        "fresh-js",
        json!({"kernel":"js","code":"typeof keep === 'undefined'"}),
        None,
    ))
    .unwrap();
    assert!(!next.is_error, "{}", text(&next));
    assert!(text(&next).contains("true"));
    assert_eq!(next.details.as_ref().unwrap()["cell"], 1);
    finish(&harness);
}

#[test]
fn javascript_loaded_module_emits_live_progress_before_its_bridge_read() {
    let harness =
        TestHarness::new("javascript_loaded_module_emits_live_progress_before_its_bridge_read");
    let root = harness.temp_path(".");
    std::fs::write(
        root.join("progress.mjs"),
        concat!(
            "display({phase:'module'}); await sleep(1); ",
            "const value = await tool.call('read',{path:'module-gate'}); ",
            "if (!value.includes('ready')) throw new Error('no live module callback'); ",
            "export const answer = 42;"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("setup.js"),
        "const progress = await import('./progress.mjs'); display(progress.answer);",
    )
    .unwrap();
    let tool = EvalTool::new(&root);
    let output = run(tool.execute(
        "loaded-js",
        json!({"kernel":"js","code":"%load setup.js"}),
        Some(Box::new(move |event: ToolUpdate| {
            if event.details.as_ref().unwrap()["jsonOutputs"] == json!([{"phase":"module"}]) {
                std::fs::write(root.join("module-gate"), "ready").unwrap();
            }
        })),
    ))
    .unwrap();
    assert!(!output.is_error, "{}", text(&output));
    assert_eq!(
        output.details.as_ref().unwrap()["jsonOutputs"],
        json!([{"phase":"module"},42])
    );
    assert_eq!(
        output.details.as_ref().unwrap()["displayStream"]["sequence"],
        2
    );
    assert_eq!(
        output.details.as_ref().unwrap()["displayStream"]["complete"],
        true
    );
    finish(&harness);
}
