//! Unit tests for pi.* connector shims (bd-354t).
//!
//! Tests verify the JS→Rust bridge boundary for `pi.exec`, `pi.http`,
//! `pi.session`, `pi.events`, and `pi.log` hostcalls. Each test creates
//! a `PiJsRuntime` with `DeterministicClock`, evaluates JS that calls
//! `pi.*`, drains the resulting `HostcallRequest`, and verifies the
//! `HostcallKind`, payload, and JS-side result when completed.

use pi::extensions_js::{HostcallKind, HostcallRequest, PiJsRuntime};
use pi::scheduler::{DeterministicClock, HostcallOutcome};
use serde_json::{Value, json};
use std::collections::VecDeque;

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn drain_one(runtime: &PiJsRuntime<DeterministicClock>) -> HostcallRequest {
    let mut queue = runtime.drain_hostcall_requests();
    queue
        .pop_front()
        .expect("expected a hostcall request to be queued")
}

fn drain_all(runtime: &PiJsRuntime<DeterministicClock>) -> VecDeque<HostcallRequest> {
    runtime.drain_hostcall_requests()
}

/// Evaluate JS that calls a pi.* API inside an async wrapper,
/// reporting the result back through `pi.tool("__report", ...)`.
/// Returns the payload of the `__report` tool call.
fn eval_and_report(js_body: &str) -> Value {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        let wrapped = format!(
            r#"
(async () => {{
  try {{
    {js_body}
  }} catch (e) {{
    await pi.tool("__report", {{ error: e.message, code: e.code ?? null }});
  }}
}})();
"#
        );

        runtime.eval(&wrapped).await.expect("eval");

        // Drain all requests - there may be a pi.* hostcall first,
        // then complete it, then get the __report.
        process_until_report(&runtime).await
    })
}

/// Process the runtime until we see a `__report` tool call, completing
/// any intermediate hostcalls with their test-specified outcomes.
/// This default version completes non-report hostcalls with `Success(Null)`.
#[allow(clippy::future_not_send)]
async fn process_until_report(runtime: &PiJsRuntime<DeterministicClock>) -> Value {
    process_until_report_with(runtime, |_req| HostcallOutcome::Success(Value::Null)).await
}

/// Like `process_until_report` but with a custom completer for non-report hostcalls.
#[allow(clippy::future_not_send)]
async fn process_until_report_with<F>(
    runtime: &PiJsRuntime<DeterministicClock>,
    completer: F,
) -> Value
where
    F: Fn(&HostcallRequest) -> HostcallOutcome,
{
    for _ in 0..50 {
        let reqs = drain_all(runtime);
        for req in reqs {
            match &req.kind {
                HostcallKind::Tool { name } if name == "__report" => {
                    return req.payload;
                }
                _ => {
                    let outcome = completer(&req);
                    runtime.complete_hostcall(req.call_id, outcome);
                }
            }
        }
        runtime.tick().await.expect("tick");
    }
    panic!("never saw __report tool call after 50 ticks");
}

// ═══════════════════════════════════════════════════════════════════════════════
// pi.exec — command/args mapping
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn exec_produces_correct_hostcall_kind() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.exec("ls", ["-la"]);"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        match &req.kind {
            HostcallKind::Exec { cmd } => assert_eq!(cmd, "ls"),
            other => panic!("expected Exec, got {other:?}"),
        }
    });
}

#[test]
fn exec_includes_args_in_payload() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.exec("git", ["status", "--short"]);"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        let args = req.payload.get("args").expect("args field");
        assert_eq!(args, &json!(["status", "--short"]));
    });
}

#[test]
fn exec_success_resolves_promise() {
    let result = eval_and_report(
        r#"
        const result = await pi.exec("echo", ["hello"]);
        await pi.tool("__report", { result });
    "#,
    );
    // Result should contain whatever we completed with (Success(Null) by default).
    assert!(result.get("result").is_some());
}

#[test]
fn exec_error_rejects_promise() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  try {
    const result = await pi.exec("fail_cmd");
    await pi.tool("__report", { ok: true });
  } catch (e) {
    await pi.tool("__report", { error: e.message, code: e.code ?? null });
  }
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| HostcallOutcome::Error {
            code: "io".to_string(),
            message: "command not found".to_string(),
        })
        .await
    });

    assert_eq!(result.get("error").and_then(Value::as_str), Some("command not found"));
    assert_eq!(result.get("code").and_then(Value::as_str), Some("io"));
}

#[test]
fn exec_with_cwd_option() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.exec("ls", [], { cwd: "/tmp" });"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        // Options are nested under "options" in the exec payload
        let cwd = req
            .payload
            .get("options")
            .and_then(|o| o.get("cwd"))
            .and_then(Value::as_str)
            .or_else(|| req.payload.get("cwd").and_then(Value::as_str));
        assert_eq!(cwd, Some("/tmp"));
    });
}

#[test]
fn exec_with_timeout_option() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.exec("sleep", ["10"], { timeout: 5000 });"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        // Options are nested under "options" in the exec payload
        let timeout = req
            .payload
            .get("options")
            .and_then(|o| o.get("timeout").or_else(|| o.get("timeoutMs")))
            .or_else(|| req.payload.get("timeout"))
            .or_else(|| req.payload.get("timeoutMs"));
        assert!(
            timeout.is_some(),
            "expected timeout in payload: {:?}",
            req.payload
        );
    });
}

#[test]
fn exec_with_stream_option() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.exec("tail", ["-f", "/dev/null"], { stream: true });"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        // stream flag is nested under "options" in the exec payload
        let stream = req
            .payload
            .get("options")
            .and_then(|o| o.get("stream"))
            .and_then(Value::as_bool)
            .or_else(|| req.payload.get("stream").and_then(Value::as_bool));
        assert_eq!(stream, Some(true));
    });
}

#[test]
fn exec_no_args_defaults_to_empty() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime.eval(r#"pi.exec("pwd");"#).await.expect("eval");

        let req = drain_one(&runtime);
        match &req.kind {
            HostcallKind::Exec { cmd } => assert_eq!(cmd, "pwd"),
            other => panic!("expected Exec, got {other:?}"),
        }
    });
}

// ═══════════════════════════════════════════════════════════════════════════════
// pi.http — request/response mapping
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn http_produces_correct_hostcall_kind() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.http({ url: "https://example.com/api", method: "GET" });"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        assert!(
            matches!(&req.kind, HostcallKind::Http),
            "expected Http, got {:?}",
            req.kind
        );
    });
}

#[test]
fn http_payload_includes_url_and_method() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.http({ url: "https://api.test.com/v1/data", method: "POST", headers: { "Content-Type": "application/json" }, body: '{"key":"value"}' });"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        assert_eq!(
            req.payload.get("url").and_then(Value::as_str),
            Some("https://api.test.com/v1/data")
        );
        assert_eq!(
            req.payload.get("method").and_then(Value::as_str),
            Some("POST")
        );
        assert!(req.payload.get("headers").is_some());
        assert!(req.payload.get("body").is_some());
    });
}

#[test]
fn http_success_resolves_with_response() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  try {
    const resp = await pi.http({ url: "https://example.com/api", method: "GET" });
    await pi.tool("__report", { status: resp?.status, body: resp?.body });
  } catch (e) {
    await pi.tool("__report", { error: e.message });
  }
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| {
            HostcallOutcome::Success(json!({
                "status": 200,
                "body": "hello world",
                "headers": {}
            }))
        })
        .await
    });

    assert_eq!(result.get("status").and_then(Value::as_u64), Some(200));
    assert_eq!(
        result.get("body").and_then(Value::as_str),
        Some("hello world")
    );
}

#[test]
fn http_error_rejects_promise() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  try {
    await pi.http({ url: "https://blocked.com", method: "GET" });
    await pi.tool("__report", { ok: true });
  } catch (e) {
    await pi.tool("__report", { error: e.message, code: e.code ?? null });
  }
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| HostcallOutcome::Error {
            code: "denied".to_string(),
            message: "HTTP access denied by policy".to_string(),
        })
        .await
    });

    assert_eq!(
        result.get("error").and_then(Value::as_str),
        Some("HTTP access denied by policy")
    );
    assert_eq!(result.get("code").and_then(Value::as_str), Some("denied"));
}

#[test]
fn http_timeout_error_mapping() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  try {
    await pi.http({ url: "https://slow.com", method: "GET" });
    await pi.tool("__report", { ok: true });
  } catch (e) {
    await pi.tool("__report", { error: e.message, code: e.code ?? null });
  }
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| HostcallOutcome::Error {
            code: "timeout".to_string(),
            message: "request timed out after 30s".to_string(),
        })
        .await
    });

    assert_eq!(
        result.get("error").and_then(Value::as_str),
        Some("request timed out after 30s")
    );
    assert_eq!(result.get("code").and_then(Value::as_str), Some("timeout"));
}

// ═══════════════════════════════════════════════════════════════════════════════
// pi.session — request/response mapping
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn session_produces_correct_hostcall_kind_with_op() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.session("getState", {});"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        match &req.kind {
            HostcallKind::Session { op } => assert_eq!(op, "getState"),
            other => panic!("expected Session, got {other:?}"),
        }
    });
}

#[test]
fn session_set_model_payload() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.session("setModel", { provider: "anthropic", modelId: "claude-sonnet-4-5" });"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        match &req.kind {
            HostcallKind::Session { op } => assert_eq!(op, "setModel"),
            other => panic!("expected Session, got {other:?}"),
        }
        assert_eq!(
            req.payload.get("provider").and_then(Value::as_str),
            Some("anthropic")
        );
        assert_eq!(
            req.payload.get("modelId").and_then(Value::as_str),
            Some("claude-sonnet-4-5")
        );
    });
}

#[test]
fn session_get_state_resolves() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  const state = await pi.session("getState", {});
  await pi.tool("__report", { state });
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| {
            HostcallOutcome::Success(json!({
                "sessionName": "test-session",
                "sessionFile": "/tmp/test.json"
            }))
        })
        .await
    });

    let state = result.get("state").expect("state field");
    assert_eq!(
        state.get("sessionName").and_then(Value::as_str),
        Some("test-session")
    );
}

#[test]
fn session_set_name_resolves() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  await pi.session("setName", { name: "My Session" });
  await pi.tool("__report", { ok: true });
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report(&runtime).await
    });

    assert_eq!(result.get("ok").and_then(Value::as_bool), Some(true));
}

#[test]
fn session_unknown_op_error() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  try {
    await pi.session("nonExistentOp", {});
    await pi.tool("__report", { ok: true });
  } catch (e) {
    await pi.tool("__report", { error: e.message, code: e.code ?? null });
  }
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| HostcallOutcome::Error {
            code: "invalid_request".to_string(),
            message: "Unknown session op: nonExistentOp".to_string(),
        })
        .await
    });

    assert!(
        result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("nonExistentOp"),
        "expected error about unknown op, got: {result:?}"
    );
}

#[test]
fn session_get_model_resolves() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  const model = await pi.session("getModel", {});
  await pi.tool("__report", { model });
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| {
            HostcallOutcome::Success(json!({
                "provider": "anthropic",
                "modelId": "claude-sonnet-4-5"
            }))
        })
        .await
    });

    let model = result.get("model").expect("model field");
    assert_eq!(
        model.get("provider").and_then(Value::as_str),
        Some("anthropic")
    );
}

#[test]
fn session_set_thinking_level_payload() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.session("setThinkingLevel", { level: "high" });"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        match &req.kind {
            HostcallKind::Session { op } => assert_eq!(op, "setThinkingLevel"),
            other => panic!("expected Session, got {other:?}"),
        }
        assert_eq!(
            req.payload.get("level").and_then(Value::as_str),
            Some("high")
        );
    });
}

#[test]
fn session_append_entry_payload() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.session("appendEntry", { customType: "note", data: { text: "hello" } });"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        match &req.kind {
            HostcallKind::Session { op } => assert_eq!(op, "appendEntry"),
            other => panic!("expected Session, got {other:?}"),
        }
        assert_eq!(
            req.payload.get("customType").and_then(Value::as_str),
            Some("note")
        );
        assert!(req.payload.get("data").is_some());
    });
}

// ═══════════════════════════════════════════════════════════════════════════════
// pi.events — event operations
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn events_produces_correct_hostcall_kind() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.events("list", {});"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        match &req.kind {
            HostcallKind::Events { op } => assert_eq!(op, "list"),
            other => panic!("expected Events, got {other:?}"),
        }
    });
}

#[test]
fn events_emit_payload() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.events("emit", { event: "custom_event", data: { key: "value" } });"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        match &req.kind {
            HostcallKind::Events { op } => assert_eq!(op, "emit"),
            other => panic!("expected Events, got {other:?}"),
        }
        assert_eq!(
            req.payload.get("event").and_then(Value::as_str),
            Some("custom_event")
        );
        assert_eq!(
            req.payload.get("data"),
            Some(&json!({ "key": "value" }))
        );
    });
}

#[test]
fn events_emit_resolves_with_dispatch_result() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  const result = await pi.events("emit", { event: "test_event", data: {} });
  await pi.tool("__report", { result });
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| {
            HostcallOutcome::Success(json!({
                "dispatched": true,
                "event": "test_event",
                "handler_count": 0,
                "result": null
            }))
        })
        .await
    });

    let result = result.get("result").expect("result field");
    assert_eq!(result.get("dispatched").and_then(Value::as_bool), Some(true));
}

#[test]
fn events_unsupported_op_error() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  try {
    await pi.events("invalidOp", {});
    await pi.tool("__report", { ok: true });
  } catch (e) {
    await pi.tool("__report", { error: e.message, code: e.code ?? null });
  }
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| HostcallOutcome::Error {
            code: "invalid_request".to_string(),
            message: "Unsupported events op: invalidOp".to_string(),
        })
        .await
    });

    assert!(
        result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("invalidOp"),
        "expected error about unsupported op, got: {result:?}"
    );
}

#[test]
fn events_list_resolves() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  const list = await pi.events("list", {});
  await pi.tool("__report", { list });
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| {
            HostcallOutcome::Success(json!({ "events": ["agent_start", "agent_end"] }))
        })
        .await
    });

    let list = result.get("list").expect("list field");
    assert!(list.get("events").is_some());
}

// ═══════════════════════════════════════════════════════════════════════════════
// pi.log — structured logging
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn log_produces_correct_hostcall_kind() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.log({ level: "info", message: "test log" });"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        assert!(
            matches!(&req.kind, HostcallKind::Log),
            "expected Log, got {:?}",
            req.kind
        );
    });
}

#[test]
fn log_payload_includes_fields() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.log({ level: "warn", message: "something happened", context: { key: 42 } });"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        assert_eq!(
            req.payload.get("level").and_then(Value::as_str),
            Some("warn")
        );
        assert_eq!(
            req.payload.get("message").and_then(Value::as_str),
            Some("something happened")
        );
        assert_eq!(
            req.payload.get("context"),
            Some(&json!({ "key": 42 }))
        );
    });
}

#[test]
fn log_resolves_on_success() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  const result = await pi.log({ level: "info", message: "hello" });
  await pi.tool("__report", { result });
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| {
            HostcallOutcome::Success(json!({ "logged": true }))
        })
        .await
    });

    let result = result.get("result").expect("result field");
    assert_eq!(result.get("logged").and_then(Value::as_bool), Some(true));
}

// ═══════════════════════════════════════════════════════════════════════════════
// pi.ui — UI operations
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn ui_produces_correct_hostcall_kind() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.ui("select", { title: "Choose", items: ["A", "B"] });"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        match &req.kind {
            HostcallKind::Ui { op } => assert_eq!(op, "select"),
            other => panic!("expected Ui, got {other:?}"),
        }
    });
}

#[test]
fn ui_confirm_payload() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(r#"pi.ui("confirm", { message: "Are you sure?" });"#)
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        match &req.kind {
            HostcallKind::Ui { op } => assert_eq!(op, "confirm"),
            other => panic!("expected Ui, got {other:?}"),
        }
        assert_eq!(
            req.payload.get("message").and_then(Value::as_str),
            Some("Are you sure?")
        );
    });
}

#[test]
fn ui_input_resolves_with_value() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  const answer = await pi.ui("input", { prompt: "Enter name:" });
  await pi.tool("__report", { answer });
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| {
            HostcallOutcome::Success(Value::String("Alice".to_string()))
        })
        .await
    });

    assert_eq!(result.get("answer").and_then(Value::as_str), Some("Alice"));
}

#[test]
fn ui_denied_error() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  try {
    await pi.ui("editor", { content: "edit me" });
    await pi.tool("__report", { ok: true });
  } catch (e) {
    await pi.tool("__report", { error: e.message, code: e.code ?? null });
  }
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| HostcallOutcome::Error {
            code: "denied".to_string(),
            message: "UI not available in headless mode".to_string(),
        })
        .await
    });

    assert_eq!(
        result.get("error").and_then(Value::as_str),
        Some("UI not available in headless mode")
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Cross-cutting: extension_id correlation
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn hostcall_includes_extension_id_when_set() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        // Set the extension id global that the bridge reads
        runtime
            .eval(
                r#"
globalThis.__pi_current_extension_id = "my-test-extension";
pi.exec("echo", ["hi"]);
"#,
            )
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        assert_eq!(
            req.extension_id.as_deref(),
            Some("my-test-extension"),
            "expected extension_id to be set from global"
        );
    });
}

#[test]
fn hostcall_extension_id_none_when_not_set() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        // Ensure no extension id is set (clear it in case of prior state)
        runtime
            .eval(
                r#"
globalThis.__pi_current_extension_id = undefined;
pi.exec("ls");
"#,
            )
            .await
            .expect("eval");

        let req = drain_one(&runtime);
        assert!(
            req.extension_id.is_none(),
            "expected no extension_id, got {:?}",
            req.extension_id
        );
    });
}

// ═══════════════════════════════════════════════════════════════════════════════
// Cross-cutting: pending hostcall tracking
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn pending_hostcall_count_tracks_lifecycle() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        assert_eq!(runtime.pending_hostcall_count(), 0);

        runtime
            .eval(r#"pi.exec("echo", ["hello"]);"#)
            .await
            .expect("eval");

        assert_eq!(runtime.pending_hostcall_count(), 1);

        let req = drain_one(&runtime);
        runtime.complete_hostcall(
            req.call_id,
            HostcallOutcome::Success(json!({ "stdout": "hello\n", "code": 0 })),
        );
        runtime.tick().await.expect("tick");

        assert_eq!(runtime.pending_hostcall_count(), 0);
    });
}

#[test]
fn multiple_concurrent_hostcalls_tracked() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
pi.exec("cmd1");
pi.exec("cmd2");
pi.http({ url: "https://example.com", method: "GET" });
"#,
            )
            .await
            .expect("eval");

        let reqs = drain_all(&runtime);
        let count = reqs.len();
        assert!(
            count >= 2,
            "expected at least 2 hostcall requests, got {count}"
        );

        let pending_before = runtime.pending_hostcall_count();
        assert!(
            pending_before >= 2,
            "expected at least 2 pending, got {pending_before}"
        );

        // Complete all and tick after each to process completions
        for req in reqs {
            runtime.complete_hostcall(req.call_id, HostcallOutcome::Success(Value::Null));
            runtime.tick().await.expect("tick");
        }

        // The pending count should be 0 after all completions processed
        assert_eq!(runtime.pending_hostcall_count(), 0);
    });
}

// ═══════════════════════════════════════════════════════════════════════════════
// Error taxonomy mapping
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn error_code_io_maps_correctly() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  try {
    await pi.exec("broken");
    await pi.tool("__report", { ok: true });
  } catch (e) {
    await pi.tool("__report", { code: e.code ?? null, message: e.message });
  }
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| HostcallOutcome::Error {
            code: "io".to_string(),
            message: "disk full".to_string(),
        })
        .await
    });

    assert_eq!(result.get("code").and_then(Value::as_str), Some("io"));
    assert_eq!(
        result.get("message").and_then(Value::as_str),
        Some("disk full")
    );
}

#[test]
fn error_code_internal_maps_correctly() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  try {
    await pi.http({ url: "https://example.com" });
    await pi.tool("__report", { ok: true });
  } catch (e) {
    await pi.tool("__report", { code: e.code ?? null, message: e.message });
  }
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| HostcallOutcome::Error {
            code: "internal".to_string(),
            message: "unexpected panic".to_string(),
        })
        .await
    });

    assert_eq!(
        result.get("code").and_then(Value::as_str),
        Some("internal")
    );
}

#[test]
fn error_code_denied_maps_correctly() {
    let result = futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
(async () => {
  try {
    await pi.session("setModel", { provider: "test", modelId: "test" });
    await pi.tool("__report", { ok: true });
  } catch (e) {
    await pi.tool("__report", { code: e.code ?? null, message: e.message });
  }
})();
"#,
            )
            .await
            .expect("eval");

        process_until_report_with(&runtime, |_| HostcallOutcome::Error {
            code: "denied".to_string(),
            message: "session access denied".to_string(),
        })
        .await
    });

    assert_eq!(result.get("code").and_then(Value::as_str), Some("denied"));
}
