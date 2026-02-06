//! Tests for streaming hostcall delivery via the `QuickJS` bridge.
//!
//! These are integration tests, so they only use `PiJsRuntime`'s public API.
//! We validate streaming semantics by having JS report observed chunks back
//! to the host via `pi.tool("__report", ...)` hostcalls.

use pi::extensions_js::{HostcallKind, HostcallRequest, PiJsRuntime};
use pi::scheduler::{DeterministicClock, HostcallOutcome};
use serde_json::{Value, json};

fn drain_one(runtime: &PiJsRuntime<DeterministicClock>) -> HostcallRequest {
    let mut queue = runtime.drain_hostcall_requests();
    queue
        .pop_front()
        .expect("expected a hostcall request to be queued")
}

fn assert_tool(req: &HostcallRequest, expected_name: &str) {
    match &req.kind {
        HostcallKind::Tool { name } => assert_eq!(name, expected_name),
        other => unreachable!("expected tool hostcall {expected_name}, got {other:?}"),
    }
}

#[test]
fn streaming_async_iterator_delivers_chunks() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
globalThis.done = false;
(async () => {
  const stream = pi.exec("tail", ["-f", "/dev/null"], { stream: true });
  for await (const chunk of stream) {
    await pi.tool("__report", { chunk });
  }
  await pi.tool("__done", { ok: true });
  globalThis.done = true;
})();
"#,
            )
            .await
            .expect("eval");

        // Initial hostcall: streaming exec.
        let exec_req = drain_one(&runtime);
        let exec_call_id = exec_req.call_id.clone();

        // Chunk 0
        runtime.complete_hostcall(
            exec_call_id.clone(),
            HostcallOutcome::StreamChunk {
                sequence: 0,
                chunk: json!("line 1\n"),
                is_final: false,
            },
        );
        runtime.tick().await.expect("tick chunk 0");

        let report_req = drain_one(&runtime);
        assert_tool(&report_req, "__report");
        assert_eq!(report_req.payload, json!({ "chunk": "line 1\n" }));
        runtime.complete_hostcall(report_req.call_id, HostcallOutcome::Success(Value::Null));
        runtime.tick().await.expect("tick report 0");

        // Chunk 1
        runtime.complete_hostcall(
            exec_call_id.clone(),
            HostcallOutcome::StreamChunk {
                sequence: 1,
                chunk: json!("line 2\n"),
                is_final: false,
            },
        );
        runtime.tick().await.expect("tick chunk 1");

        let report_req = drain_one(&runtime);
        assert_tool(&report_req, "__report");
        assert_eq!(report_req.payload, json!({ "chunk": "line 2\n" }));
        runtime.complete_hostcall(report_req.call_id, HostcallOutcome::Success(Value::Null));
        runtime.tick().await.expect("tick report 1");

        // Final chunk: end-of-stream signal (is_final + null).
        runtime.complete_hostcall(
            exec_call_id,
            HostcallOutcome::StreamChunk {
                sequence: 2,
                chunk: Value::Null,
                is_final: true,
            },
        );
        runtime.tick().await.expect("tick final");

        // The loop should terminate and emit a "__done" tool call.
        let done_req = drain_one(&runtime);
        assert_tool(&done_req, "__done");
        assert_eq!(done_req.payload, json!({ "ok": true }));
        runtime.complete_hostcall(done_req.call_id, HostcallOutcome::Success(Value::Null));
        runtime.tick().await.expect("tick done");

        assert!(runtime.drain_hostcall_requests().is_empty());
    });
}

#[test]
fn streaming_error_mid_stream_reports_error() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  const stream = pi.exec("cat", ["bigfile"], { stream: true });
  try {
    for await (const chunk of stream) {
      await pi.tool("__report", { chunk });
    }
  } catch (e) {
    await pi.tool("__report_error", { message: e.message, code: e.code ?? null });
  }
})();
"#,
            )
            .await
            .expect("eval");

        let exec_req = drain_one(&runtime);
        let exec_call_id = exec_req.call_id.clone();

        runtime.complete_hostcall(
            exec_call_id.clone(),
            HostcallOutcome::StreamChunk {
                sequence: 0,
                chunk: json!("partial"),
                is_final: false,
            },
        );
        runtime.tick().await.expect("tick chunk");

        let report_req = drain_one(&runtime);
        assert_tool(&report_req, "__report");
        assert_eq!(report_req.payload, json!({ "chunk": "partial" }));
        runtime.complete_hostcall(report_req.call_id, HostcallOutcome::Success(Value::Null));
        runtime.tick().await.expect("tick report");

        runtime.complete_hostcall(
            exec_call_id,
            HostcallOutcome::Error {
                code: "io".to_string(),
                message: "connection reset".to_string(),
            },
        );
        runtime.tick().await.expect("tick error");

        let err_req = drain_one(&runtime);
        assert_tool(&err_req, "__report_error");
        assert_eq!(
            err_req.payload,
            json!({ "message": "connection reset", "code": "io" })
        );
        runtime.complete_hostcall(err_req.call_id, HostcallOutcome::Success(Value::Null));
        runtime.tick().await.expect("tick report_error");

        assert!(runtime.drain_hostcall_requests().is_empty());
    });
}

#[test]
fn streaming_nonfinal_keeps_call_pending() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.exec("ls", [], { stream: true });"#)
            .await
            .expect("eval");

        let exec_req = drain_one(&runtime);
        let exec_call_id = exec_req.call_id.clone();
        assert_eq!(runtime.pending_hostcall_count(), 1);

        runtime.complete_hostcall(
            exec_call_id.clone(),
            HostcallOutcome::StreamChunk {
                sequence: 0,
                chunk: json!("data"),
                is_final: false,
            },
        );
        runtime.tick().await.expect("tick nonfinal");
        assert_eq!(runtime.pending_hostcall_count(), 1);

        runtime.complete_hostcall(
            exec_call_id,
            HostcallOutcome::StreamChunk {
                sequence: 1,
                chunk: Value::Null,
                is_final: true,
            },
        );
        runtime.tick().await.expect("tick final");
        assert_eq!(runtime.pending_hostcall_count(), 0);
    });
}

#[test]
fn streaming_callback_receives_chunks() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  const result = await pi.exec("echo", ["hello"], {
    stream: true,
    onChunk: (chunk, isFinal) => { pi.tool("__report", { chunk, isFinal }); },
  });
  await pi.tool("__resolved", { value: result });
})();
"#,
            )
            .await
            .expect("eval");

        let exec_req = drain_one(&runtime);
        let call_id = exec_req.call_id.clone();

        runtime.complete_hostcall(
            call_id.clone(),
            HostcallOutcome::StreamChunk {
                sequence: 0,
                chunk: json!("chunk-A"),
                is_final: false,
            },
        );
        runtime.tick().await.expect("tick chunk-A");

        let report_req = drain_one(&runtime);
        assert_tool(&report_req, "__report");
        assert_eq!(
            report_req.payload,
            json!({ "chunk": "chunk-A", "isFinal": false })
        );
        runtime.complete_hostcall(report_req.call_id, HostcallOutcome::Success(Value::Null));
        runtime.tick().await.expect("tick report chunk-A");

        runtime.complete_hostcall(
            call_id,
            HostcallOutcome::StreamChunk {
                sequence: 1,
                chunk: json!("chunk-B"),
                is_final: true,
            },
        );
        runtime.tick().await.expect("tick chunk-B");

        let mut reqs = runtime.drain_hostcall_requests();
        assert_eq!(reqs.len(), 2, "expected report + resolved tool calls");

        // Order is deterministic (hostcall queue is FIFO), but don't over-assume.
        let mut seen_report = false;
        let mut seen_resolved = false;

        while let Some(req) = reqs.pop_front() {
            match &req.kind {
                HostcallKind::Tool { name } if name == "__report" => {
                    assert_eq!(req.payload, json!({ "chunk": "chunk-B", "isFinal": true }));
                    seen_report = true;
                }
                HostcallKind::Tool { name } if name == "__resolved" => {
                    assert_eq!(req.payload, json!({ "value": "chunk-B" }));
                    seen_resolved = true;
                }
                other => unreachable!("unexpected hostcall after final chunk: {other:?}"),
            }
            runtime.complete_hostcall(req.call_id, HostcallOutcome::Success(Value::Null));
        }

        assert!(seen_report, "missing __report tool call");
        assert!(seen_resolved, "missing __resolved tool call");

        // Deliver both tool completions.
        runtime.tick().await.expect("tick tool completion 1");
        runtime.tick().await.expect("tick tool completion 2");

        assert!(runtime.drain_hostcall_requests().is_empty());
    });
}
