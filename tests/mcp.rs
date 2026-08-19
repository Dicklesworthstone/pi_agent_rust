//! Integration tests for the MCP client (bd-cv653.6.1).
//!
//! Lanes:
//! - Config/trust (no fixture binary): discovery through the manager, `/mcp`
//!   list rows, fail-closed trust gate (pending/denied), fingerprint re-pend.
//! - HTTP transport over the loopback mock: initialize with session-id
//!   continuity, JSON + SSE responses, tools/call round-trip.
//! - Stdio fixture lifecycle (feature `internal-mcp-fixture`): trust →
//!   mount → `mcp__fixture__echo` round-trips through a `ToolRegistry`,
//!   env-allowlist proof via `env_probe`, stderr capture, crash → bounded
//!   restart with backoff → recovery.
//!
//! The env-allowlist proof is strongest under `scripts/e2e/run_mcp.sh`,
//! which exports `PI_MCP_SECRET_MARKER` before the run (the crate forbids
//! unsafe, so tests never mutate process env themselves).
//!
//! Logging: structured JSONL per tests/common/logging.rs, v2-validated,
//! recorded as artifacts.

mod common;

use common::TestHarness;
use common::harness::MockHttpResponse;
use common::logging::validate_jsonl_v2_only;
use pi::mcp::McpManager;
use pi::mcp::transport::McpTransport;
#[cfg(feature = "internal-mcp-fixture")]
use pi::tools::{ToolOutput, ToolRegistry};
use serde_json::{Value, json};
use std::path::Path;

#[cfg(feature = "internal-mcp-fixture")]
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

/// Extract joined text blocks from a raw MCP `tools/call` result value.
#[cfg(feature = "internal-mcp-fixture")]
fn mcp_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

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
    assert!(
        errors.is_empty(),
        "JSONL schema violations in {case}.jsonl: {errors:?}"
    );
    harness.record_artifact(format!("{case}.jsonl"), &path);
}

fn block_on_local<Fut: Future>(future: Fut) -> Fut::Output {
    // enable_parking(false): works around the asupersync scheduler parking
    // bug that can livelock sleep() wakeups (see tests/common/mod.rs).
    let runtime = asupersync::runtime::RuntimeBuilder::new()
        .enable_parking(false)
        .worker_threads(1)
        .blocking_threads(1, 8)
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future)
}

/// Write a project `.pi/mcp.json` with one stdio server entry.
fn write_project_mcp_config(root: &Path, name: &str, command: &str, env: &[(&str, &str)]) {
    let dir = root.join(".pi");
    std::fs::create_dir_all(&dir).expect("create .pi");
    let env_map: serde_json::Map<String, Value> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), Value::String((*v).to_string())))
        .collect();
    std::fs::write(
        dir.join("mcp.json"),
        json!({
            "mcpServers": {
                name: { "command": command, "env": env_map }
            }
        })
        .to_string(),
    )
    .expect("write mcp.json");
}

// ---------------------------------------------------------------------------
// Config + trust lanes (no fixture binary)
// ---------------------------------------------------------------------------

#[test]
fn mcp_discovery_flows_into_manager_list() {
    let case = "mcp_discovery_flows_into_manager_list";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let global = harness.temp_path("global");
    write_project_mcp_config(&root, "docs", "docs-mcp", &[("API_KEY", "$ENV:DOCS_KEY")]);
    // A foreign config that should be discovered and marked.
    let claude = root.join(".claude");
    std::fs::create_dir_all(&claude).expect("claude dir");
    std::fs::write(
        claude.join("mcp.json"),
        r#"{"mcpServers": {"foreign-srv": {"command": "foreign-bin"}}}"#,
    )
    .expect("write foreign");

    let manager = McpManager::bootstrap(&root, &global, &[]).expect("bootstrap");
    let rows = manager.list();
    harness
        .log()
        .info("verify", format!("list rows: {}", rows.len()));
    assert_eq!(rows.len(), 2);
    let docs = rows.iter().find(|r| r.name == "docs").expect("docs row");
    assert_eq!(docs.provenance, ".pi");
    assert_eq!(docs.trust, "pending");
    assert!(docs.target.contains("docs-mcp"));
    let foreign = rows
        .iter()
        .find(|r| r.name == "foreign-srv")
        .expect("foreign");
    assert_eq!(foreign.provenance, "foreign");
    finish_case(&harness, case);
}

#[test]
fn mcp_trust_gate_is_fail_closed() {
    let case = "mcp_trust_gate_is_fail_closed";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let global = harness.temp_path("global");
    // The command intentionally does not exist: a correct trust gate never
    // spawns it, so the point is proven by the error taxonomy, not a spawn.
    write_project_mcp_config(&root, "guarded", "/nonexistent/guarded-server", &[]);
    let manager = McpManager::bootstrap(&root, &global, &[]).expect("bootstrap");

    // Pending: typed refusal with the remedy.
    let err = block_on_local(manager.call_tool("guarded", "anything", json!({})))
        .expect_err("pending server must refuse");
    let message = err.to_string();
    harness
        .log()
        .info("verify", format!("pending error: {message}"));
    assert!(message.contains("MCP_TRUST_PENDING"), "{message}");
    assert!(message.contains("/mcp trust guarded"), "{message}");

    // Deny through the manager: sticky fail-closed.
    block_on_local(manager.deny("guarded")).expect("deny succeeds");
    let err = block_on_local(manager.call_tool("guarded", "anything", json!({})))
        .expect_err("denied server must refuse");
    assert!(err.to_string().contains("MCP_TRUST_DENIED"), "{err}");
    finish_case(&harness, case);
}

#[test]
fn mcp_unknown_server_is_named_error() {
    let case = "mcp_unknown_server_is_named_error";
    let harness = TestHarness::new(case);
    let manager = McpManager::bootstrap(&harness.temp_path("."), &harness.temp_path("global"), &[])
        .expect("bootstrap");
    let err = block_on_local(manager.call_tool("nope", "x", json!({})))
        .expect_err("unknown server must fail");
    assert!(err.to_string().contains("MCP_UNKNOWN_SERVER"), "{err}");
    finish_case(&harness, case);
}

// ---------------------------------------------------------------------------
// HTTP transport over the loopback mock
// ---------------------------------------------------------------------------

#[test]
fn mcp_http_transport_json_and_sse() {
    let case = "mcp_http_transport_json_and_sse";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/mcp",
        MockHttpResponse {
            status: 200,
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Mcp-Session-Id".to_string(), "sess-123".to_string()),
            ],
            body: br#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"mock","version":"0"}}}"#
                .to_vec(),
        },
    );
    let transport = pi::mcp::transport::HttpTransport::new(
        &format!("{}/mcp", server.base_url()),
        vec![("Authorization".to_string(), "Bearer test".to_string())],
    )
    .expect("transport construction");
    let result = block_on_local(transport.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        std::time::Duration::from_secs(10),
    ))
    .expect("initialize over http");
    harness
        .log()
        .info("verify", format!("initialize result: {result}"));
    assert_eq!(result["protocolVersion"], "2025-06-18");

    // The mock saw the request with the custom header and Accept pair.
    let requests = server.requests();
    let first = requests.first().expect("one request");
    assert!(
        first
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v == "Bearer test"),
        "custom headers must flow: {:?}",
        first.headers
    );
    assert!(
        first
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("accept")
                && v.contains("application/json")
                && v.contains("text/event-stream")),
        "accept pair required: {:?}",
        first.headers
    );
    finish_case(&harness, case);
}

#[test]
fn mcp_http_transport_sse_response() {
    let case = "mcp_http_transport_sse_response";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    let sse_body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"search\",\"description\":\"find things\",\"inputSchema\":{\"type\":\"object\"}}]}}\n\n";
    server.add_route(
        "POST",
        "/sse",
        MockHttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body: sse_body.as_bytes().to_vec(),
        },
    );
    let transport =
        pi::mcp::transport::HttpTransport::new(&format!("{}/sse", server.base_url()), vec![])
            .expect("transport construction");
    let result = block_on_local(transport.request(
        "tools/list",
        json!({}),
        std::time::Duration::from_secs(10),
    ))
    .expect("tools/list over SSE");
    let tools = result["tools"].as_array().expect("tools array");
    assert_eq!(tools[0]["name"], "search");
    finish_case(&harness, case);
}

// ---------------------------------------------------------------------------
// Stdio fixture lifecycle (feature-gated)
// ---------------------------------------------------------------------------

#[cfg(feature = "internal-mcp-fixture")]
mod fixture_lanes {
    use super::*;

    const FIXTURE_BIN: &str = env!("CARGO_BIN_EXE_pi_mcp_fixture");

    fn fixture_manager(harness: &TestHarness, extra_env: &[(&str, &str)]) -> McpManager {
        let root = harness.temp_path(".");
        write_project_mcp_config(&root, "fixture", FIXTURE_BIN, extra_env);
        McpManager::bootstrap(&root, &harness.temp_path("global"), &[]).expect("bootstrap")
    }

    #[test]
    fn mcp_stdio_fixture_full_lifecycle() {
        let case = "mcp_stdio_fixture_full_lifecycle";
        let harness = TestHarness::new(case);
        let manager = fixture_manager(&harness, &[]);

        // Trust + eager connect: tools become available.
        let tools = block_on_local(manager.trust("fixture")).expect("trust + connect");
        harness
            .log()
            .info("verify", format!("tools: {}", tools.len()));
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"echo"), "tools: {names:?}");
        assert!(names.contains(&"env_probe"), "tools: {names:?}");

        // Health surfaces as ready with the tool count.
        let rows = manager.list();
        let row = rows.iter().find(|r| r.name == "fixture").expect("row");
        assert_eq!(row.trust, "acknowledged");
        assert!(row.health.contains("ready"), "{}", row.health);

        // Mount into a real registry: the tool is first-class.
        let mut registry = ToolRegistry::new(&[], &harness.temp_path("."), None);
        registry.extend(pi::mcp::mount_tools(&std::sync::Arc::new(manager)));
        let echo = registry
            .tools()
            .iter()
            .find(|tool| tool.name() == "mcp__fixture__echo")
            .expect("mcp__fixture__echo mounted");
        let out = block_on_local(echo.execute("call-1", json!({"text": "hello-mcp"}), None))
            .expect("echo executes");
        harness
            .log()
            .info("verify", format!("echo output: {}", first_text(&out)));
        assert!(
            first_text(&out).starts_with("echo: hello-mcp"),
            "{}",
            first_text(&out)
        );
        assert!(!out.is_error);
        finish_case(&harness, case);
    }

    #[test]
    fn mcp_stdio_env_allowlist_proven() {
        let case = "mcp_stdio_env_allowlist_proven";
        let harness = TestHarness::new(case);
        let manager = fixture_manager(&harness, &[]);
        block_on_local(manager.trust("fixture")).expect("trust");

        let err = manager.call_tool("fixture", "env_probe", json!({}));
        let out = block_on_local(err).expect("env_probe executes");
        let text = mcp_text(&out);
        harness.log().info("verify", format!("env_probe: {text}"));
        let report: Value = serde_json::from_str(&text).expect("env_probe JSON");
        assert_eq!(report["PATH"], true, "allowlisted PATH must pass through");
        // The marker is only present when run_mcp.sh exports it; either way
        // the fixture must never see it (allowlist has no secrets).
        assert_eq!(
            report["PI_MCP_SECRET_MARKER"], false,
            "ambient secret markers must not leak to servers"
        );
        assert_eq!(report["AWS_SECRET_ACCESS_KEY"], false);
        finish_case(&harness, case);
    }

    #[test]
    fn mcp_stdio_crash_transparent_restart() {
        let case = "mcp_stdio_crash_transparent_restart";
        let harness = TestHarness::new(case);
        // Crash after request 3: initialize(1), tools/list(2), first echo(3)
        // succeed; the second echo hits the dying process and is retried
        // through an immediate transparent restart.
        let manager = fixture_manager(&harness, &[("PI_MCP_FIXTURE_CRASH_AFTER", "3")]);
        block_on_local(manager.trust("fixture")).expect("trust");

        let first = block_on_local(manager.call_tool("fixture", "echo", json!({"text": "one"})))
            .expect("first echo works");
        let second = block_on_local(manager.call_tool("fixture", "echo", json!({"text": "two"})))
            .expect("crash triggers a transparent restart");
        let third = block_on_local(manager.call_tool("fixture", "echo", json!({"text": "three"})))
            .expect("fresh server keeps serving");
        harness.log().info(
            "verify",
            format!(
                "sequence: one={} two={} three={}",
                mcp_text(&first),
                mcp_text(&second),
                mcp_text(&third)
            ),
        );
        // The restart is observable: the answering pid changes exactly once.
        let pid_of = |value: &Value| {
            mcp_text(value)
                .split("pid=")
                .nth(1)
                .and_then(|rest| rest.split(' ').next())
                .map(str::to_string)
        };
        let pid1 = pid_of(&first).expect("pid in response");
        let pid2 = pid_of(&second).expect("pid in response");
        let pid3 = pid_of(&third).expect("pid in response");
        assert_ne!(pid1, pid2, "restart must produce a new process");
        assert_eq!(pid2, pid3, "the restarted server keeps serving");
        finish_case(&harness, case);
    }

    #[test]
    fn mcp_stdio_crash_loop_backoff_and_exhaustion() {
        let case = "mcp_stdio_crash_loop_backoff_and_exhaustion";
        let harness = TestHarness::new(case);
        // Crash on the FIRST request (initialize): every connect attempt
        // fails the handshake — a crash loop that engages the budget.
        let manager = fixture_manager(&harness, &[("PI_MCP_FIXTURE_CRASH_AFTER", "0")]);

        // Attempt 1: spawn + handshake fails; count=1, backoff armed (+2s).
        let err = block_on_local(manager.trust("fixture"))
            .expect_err("crash-at-initialize must fail the connect");
        harness
            .log()
            .info("verify", format!("first failure: {err}"));
        let row = manager
            .list()
            .into_iter()
            .find(|r| r.name == "fixture")
            .expect("row");
        assert!(row.health.contains("unhealthy"), "{}", row.health);

        // Attempt 2 (immediately): refused by the armed backoff.
        let err = block_on_local(manager.trust("fixture")).expect_err("inside the backoff window");
        assert!(err.to_string().contains("MCP_BACKOFF"), "{err}");

        // Attempt 3 (after the window): tried, fails again; count=2 (+4s).
        std::thread::sleep(std::time::Duration::from_millis(2300));
        let err = block_on_local(manager.trust("fixture")).expect_err("still crash-looping");
        assert!(!err.to_string().contains("MCP_BACKOFF"), "{err}");

        // Attempt 4 (after the longer window): count hits the cap → Failed.
        std::thread::sleep(std::time::Duration::from_millis(4300));
        let err = block_on_local(manager.trust("fixture")).expect_err("third failure");
        assert!(!err.to_string().contains("MCP_BACKOFF"), "{err}");
        let err = block_on_local(manager.trust("fixture")).expect_err("exhausted");
        assert!(err.to_string().contains("MCP_RESTART_EXHAUSTED"), "{err}");
        let row = manager
            .list()
            .into_iter()
            .find(|r| r.name == "fixture")
            .expect("row");
        assert!(row.health.contains("failed"), "{}", row.health);
        finish_case(&harness, case);
    }

    #[test]
    fn mcp_stdio_stderr_captured() {
        let case = "mcp_stdio_stderr_captured";
        let harness = TestHarness::new(case);
        let manager = fixture_manager(&harness, &[]);
        block_on_local(manager.trust("fixture")).expect("trust");
        // The fixture wrote its startup marker to stderr; the stdio
        // transport retains it for /mcp diagnostics.
        let tail = manager
            .server_diagnostics("fixture")
            .expect("diagnostics for live server");
        harness.log().info("verify", format!("stderr tail: {tail}"));
        assert!(
            tail.contains("7f3a9c-v2"),
            "fixture stderr marker must be captured: {tail}"
        );
        finish_case(&harness, case);
    }
}
