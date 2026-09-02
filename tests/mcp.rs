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
//!   restart with delivery-safe recovery.
//!
//! The env-allowlist test relaunches its own test process with a guaranteed
//! ambient secret marker, avoiding unsafe process-environment mutation while
//! ensuring the negative assertion is mutation-sensitive in every runner.
//! No-Claim: the local fixture does not certify any third-party MCP server.
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

fn write_project_mcp_value(root: &Path, value: &Value) {
    let dir = root.join(".pi");
    std::fs::create_dir_all(&dir).expect("create .pi");
    std::fs::write(dir.join("mcp.json"), value.to_string()).expect("write mcp.json");
}

#[cfg(unix)]
fn shell_single_quote(value: &Path) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "'\"'\"'"))
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
    assert_eq!(docs.target, "<stdio>");
    let foreign = rows
        .iter()
        .find(|r| r.name == "foreign-srv")
        .expect("foreign");
    assert_eq!(foreign.provenance, "foreign");
    finish_case(&harness, case);
}

#[test]
fn mcp_discovery_skips_untrusted_project_sources_without_reading_them() {
    let case = "mcp_discovery_skips_untrusted_project_sources_without_reading_them";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let global = harness.temp_path("global");
    std::fs::create_dir_all(root.join(".pi")).expect("create project config dir");
    std::fs::create_dir_all(root.join(".agents")).expect("create agents config dir");
    std::fs::create_dir_all(root.join(".claude")).expect("create foreign config dir");
    std::fs::create_dir_all(&global).expect("create global config dir");

    // This malformed high-precedence project file would block global config
    // if discovery opened it. An untrusted workspace must skip it entirely.
    std::fs::write(root.join(".pi/mcp.json"), "{ malformed")
        .expect("write malformed project config");
    std::fs::write(
        root.join(".agents/mcp.json"),
        r#"{"mcpServers":{"agents":{"command":"agents-bin"}}}"#,
    )
    .expect("write agents config");
    std::fs::write(
        root.join(".claude/mcp.json"),
        r#"{"mcpServers":{"foreign":{"command":"foreign-bin"}}}"#,
    )
    .expect("write foreign config");
    std::fs::write(
        global.join("mcp.json"),
        r#"{"mcpServers":{"global":{"command":"global-bin"}}}"#,
    )
    .expect("write global config");
    let explicit = harness.temp_path("explicit-mcp.json");
    std::fs::write(
        &explicit,
        r#"{"mcpServers":{"explicit":{"command":"explicit-bin"}}}"#,
    )
    .expect("write explicit config");

    let untrusted = pi::mcp::config::discover_with_project_trust(
        &root,
        &global,
        std::slice::from_ref(&explicit),
        false,
    );
    let mut names = untrusted
        .servers
        .iter()
        .map(|server| server.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(names, vec!["explicit", "global"]);
    assert!(
        untrusted.warnings.is_empty(),
        "skipped project files must not be opened or diagnosed: {:?}",
        untrusted.warnings
    );

    write_project_mcp_config(&root, "project", "project-bin", &[]);
    let trusted = pi::mcp::config::discover_with_project_trust(
        &root,
        &global,
        std::slice::from_ref(&explicit),
        true,
    );
    let mut names = trusted
        .servers
        .iter()
        .map(|server| server.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["agents", "explicit", "foreign", "global", "project"]
    );
    finish_case(&harness, case);
}

#[test]
fn mcp_pending_and_list_surfaces_do_not_expose_target_credentials() {
    let case = "mcp_pending_and_list_surfaces_do_not_expose_target_credentials";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let global = harness.temp_path("global");
    write_project_mcp_value(
        &root,
        &json!({
            "mcpServers": {
                "stdio-secret": {
                    "command": "/tmp/literal-command-secret",
                    "args": ["--token", "literal-argv-secret"]
                },
                "http-secret": {
                    "url": "https://user:password@example.test/mcp?token=query-secret"
                }
            }
        }),
    );
    let manager = McpManager::bootstrap(&root, &global, &[]).expect("bootstrap");
    let rows = manager.list();
    assert_eq!(
        rows.iter()
            .find(|row| row.name == "stdio-secret")
            .expect("stdio row")
            .target,
        "<stdio>"
    );
    assert_eq!(
        rows.iter()
            .find(|row| row.name == "http-secret")
            .expect("http row")
            .target,
        "<http>"
    );

    let error = block_on_local(manager.call_tool("stdio-secret", "anything", json!({})))
        .expect_err("pending server must refuse before exposing its target")
        .to_string();
    for secret in [
        "literal-command-secret",
        "literal-argv-secret",
        "password",
        "query-secret",
    ] {
        assert!(
            !error.contains(secret),
            "pending error leaked {secret:?}: {error}"
        );
    }
    assert!(error.contains("/mcp trust stdio-secret"), "{error}");
    finish_case(&harness, case);
}

#[test]
fn mcp_extension_server_names_reject_terminal_controls() {
    let case = "mcp_extension_server_names_reject_terminal_controls";
    let harness = TestHarness::new(case);
    let manager = McpManager::bootstrap(&harness.temp_path("."), &harness.temp_path("global"), &[])
        .expect("bootstrap");
    manager.register_extension_server(
        "hostile\u{202e}name",
        &json!({"command": "/nonexistent/server", "extension_id": "fixture"}),
    );
    assert!(manager.list().is_empty());
    finish_case(&harness, case);
}

#[test]
fn mcp_extension_server_specs_reject_non_string_execution_fields() {
    let case = "mcp_extension_server_specs_reject_non_string_execution_fields";
    let harness = TestHarness::new(case);
    let manager = McpManager::bootstrap(&harness.temp_path("."), &harness.temp_path("global"), &[])
        .expect("bootstrap");
    for (name, spec) in [
        (
            "bad-arg",
            json!({"command": "/nonexistent/server", "args": ["ok", 7]}),
        ),
        (
            "bad-env",
            json!({"command": "/nonexistent/server", "env": {"TOKEN": 7}}),
        ),
        (
            "bad-header",
            json!({"url": "https://example.test/mcp", "headers": {"Authorization": 7}}),
        ),
    ] {
        manager.register_extension_server(name, &spec);
    }
    assert!(
        manager.list().is_empty(),
        "malformed execution fields must reject the whole extension server"
    );
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

#[cfg(unix)]
#[test]
fn mcp_env_command_change_re_pends_before_resolution() {
    let case = "mcp_env_command_change_re_pends_before_resolution";
    let harness = TestHarness::new(case);
    let root = harness.temp_path("project");
    let global = harness.temp_path("global");
    let marker = harness.temp_path("env-command-ran");
    write_project_mcp_config(
        &root,
        "guarded",
        "/nonexistent/guarded-server",
        &[("TOKEN", "safe")],
    );
    let manager = McpManager::bootstrap(&root, &global, &[]).expect("bootstrap initial config");
    block_on_local(manager.trust("guarded")).expect_err("spawn should fail after persisting trust");

    let injected = format!("$CMD:touch {}", shell_single_quote(&marker));
    write_project_mcp_value(
        &root,
        &json!({
            "mcpServers": {
                "guarded": {
                    "command": "/nonexistent/guarded-server",
                    "env": {"TOKEN": injected}
                }
            }
        }),
    );
    let manager = McpManager::bootstrap(&root, &global, &[]).expect("bootstrap changed config");
    let err = block_on_local(manager.call_tool("guarded", "anything", json!({})))
        .expect_err("changed env definition must re-pend");
    assert!(err.to_string().contains("MCP_TRUST_PENDING"), "{err}");
    assert!(
        !marker.exists(),
        "pending trust must win before $CMD resolution"
    );
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn mcp_header_command_change_re_pends_before_resolution() {
    let case = "mcp_header_command_change_re_pends_before_resolution";
    let harness = TestHarness::new(case);
    let root = harness.temp_path("project");
    let global = harness.temp_path("global");
    let marker = harness.temp_path("header-command-ran");
    write_project_mcp_value(
        &root,
        &json!({
            "mcpServers": {
                "remote": {
                    "url": "not a valid URL",
                    "headers": {"Authorization": "safe"}
                }
            }
        }),
    );
    let manager = McpManager::bootstrap(&root, &global, &[]).expect("bootstrap initial config");
    block_on_local(manager.trust("remote")).expect_err("invalid URL must fail after trust write");

    let injected = format!("$CMD:touch {}", shell_single_quote(&marker));
    write_project_mcp_value(
        &root,
        &json!({
            "mcpServers": {
                "remote": {
                    "url": "not a valid URL",
                    "headers": {"Authorization": injected}
                }
            }
        }),
    );
    let manager = McpManager::bootstrap(&root, &global, &[]).expect("bootstrap changed config");
    let err = block_on_local(manager.call_tool("remote", "anything", json!({})))
        .expect_err("changed header definition must re-pend");
    assert!(err.to_string().contains("MCP_TRUST_PENDING"), "{err}");
    assert!(
        !marker.exists(),
        "pending trust must win before $CMD resolution"
    );
    finish_case(&harness, case);
}

#[test]
fn mcp_relative_stdio_trust_is_scoped_to_project_cwd() {
    let case = "mcp_relative_stdio_trust_is_scoped_to_project_cwd";
    let harness = TestHarness::new(case);
    let project_a = harness.temp_path("project-a");
    let project_b = harness.temp_path("project-b");
    let global = harness.temp_path("global");
    std::fs::create_dir_all(&project_a).expect("project A directory");
    std::fs::create_dir_all(&project_b).expect("project B directory");
    std::fs::create_dir_all(&global).expect("global directory");
    std::fs::write(
        global.join("mcp.json"),
        json!({
            "mcpServers": {
                "local": {"command": "./server"}
            }
        })
        .to_string(),
    )
    .expect("write shared global config");

    let manager_a = McpManager::bootstrap(&project_a, &global, &[]).expect("bootstrap project A");
    block_on_local(manager_a.trust("local")).expect_err("missing project A server");

    let manager_b = McpManager::bootstrap(&project_b, &global, &[]).expect("bootstrap project B");
    let row = manager_b
        .list()
        .into_iter()
        .find(|row| row.name == "local")
        .expect("project B row");
    assert_eq!(row.trust, "pending");
    finish_case(&harness, case);
}

#[test]
fn mcp_http_command_reference_trust_is_scoped_to_project_cwd() {
    let case = "mcp_http_command_reference_trust_is_scoped_to_project_cwd";
    let harness = TestHarness::new(case);
    let project_a = harness.temp_path("http-project-a");
    let project_b = harness.temp_path("http-project-b");
    let global = harness.temp_path("http-global");
    std::fs::create_dir_all(&project_a).expect("project A directory");
    std::fs::create_dir_all(&project_b).expect("project B directory");
    std::fs::create_dir_all(&global).expect("global directory");
    std::fs::write(
        global.join("mcp.json"),
        json!({
            "mcpServers": {
                "remote": {
                    "url": "not a valid URL",
                    "headers": {"Authorization": "$CMD:./token-helper"}
                }
            }
        })
        .to_string(),
    )
    .expect("write shared global config");

    let manager_a = McpManager::bootstrap(&project_a, &global, &[]).expect("bootstrap project A");
    let row_a = manager_a
        .list()
        .into_iter()
        .find(|row| row.name == "remote")
        .expect("project A row");
    assert_eq!(row_a.trust, "pending");
    block_on_local(manager_a.deny("remote")).expect("persist project A decision");

    let manager_b = McpManager::bootstrap(&project_b, &global, &[]).expect("bootstrap project B");
    let row_b = manager_b
        .list()
        .into_iter()
        .find(|row| row.name == "remote")
        .expect("project B row");
    assert_eq!(row_b.trust, "pending");
    finish_case(&harness, case);
}

// ---------------------------------------------------------------------------
// HTTP transport over the loopback mock
// ---------------------------------------------------------------------------

fn mock_http_initialize_response(id: u64, session: &str) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Mcp-Session-Id".to_string(), session.to_string()),
        ],
        body: serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "serverInfo": {"name": "mock", "version": "0"}
            }
        }))
        .expect("serialize initialize response"),
    }
}

#[test]
fn mcp_http_transport_propagates_validated_initialize_state() {
    let case = "mcp_http_transport_propagates_validated_initialize_state";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    server.add_route_queue(
        "POST",
        "/mcp",
        vec![
            MockHttpResponse {
                status: 200,
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("Mcp-Session-Id".to_string(), "sess-123".to_string()),
                ],
                body: br#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"mock","version":"0"}}}"#
                    .to_vec(),
            },
            MockHttpResponse {
                status: 202,
                headers: Vec::new(),
                body: Vec::new(),
            },
            MockHttpResponse::json(
                200,
                &json!({"jsonrpc": "2.0", "id": 2, "result": {"tools": []}}),
            ),
        ],
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
    block_on_local(transport.notify("notifications/initialized", json!({})))
        .expect("initialized notification over HTTP");
    let tools = block_on_local(transport.request(
        "tools/list",
        json!({}),
        std::time::Duration::from_secs(10),
    ))
    .expect("tools/list over HTTP");
    assert_eq!(tools["tools"], json!([]));

    // The mock saw the initial request with the custom header and Accept pair.
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
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
    assert!(
        !first.headers.iter().any(|(name, _)| {
            name.eq_ignore_ascii_case("mcp-session-id")
                || name.eq_ignore_ascii_case("mcp-protocol-version")
        }),
        "initialize must not send state that has not been negotiated"
    );
    for request in &requests[1..] {
        assert!(request.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("mcp-session-id") && value == "sess-123"
        }));
        assert!(request.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("mcp-protocol-version") && value == "2025-06-18"
        }));
    }
    let initialized_frame: Value =
        serde_json::from_slice(&requests[1].body).expect("initialized notification JSON");
    assert_eq!(initialized_frame["method"], "notifications/initialized");
    assert!(initialized_frame.get("id").is_none());
    let tools_frame: Value =
        serde_json::from_slice(&requests[2].body).expect("tools/list request JSON");
    assert_eq!(tools_frame["id"], 2);
    finish_case(&harness, case);
}

#[test]
fn mcp_http_transport_sse_response() {
    let case = "mcp_http_transport_sse_response";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    let sse_body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"search\",\"description\":\"find things\",\"inputSchema\":{\"type\":\"object\"}}]}}\n\n";
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

#[test]
fn mcp_http_transport_handles_ping_before_streamed_response() {
    let case = "mcp_http_transport_handles_ping_before_streamed_response";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    let sse_body = concat!(
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":\"server-ping\",\"method\":\"ping\"}\n\n",
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n"
    );
    server.add_route_queue(
        "POST",
        "/sse-ping",
        vec![
            MockHttpResponse {
                status: 200,
                headers: vec![(
                    "Content-Type".to_string(),
                    "text/event-stream; charset=utf-8".to_string(),
                )],
                body: sse_body.as_bytes().to_vec(),
            },
            MockHttpResponse {
                status: 202,
                headers: Vec::new(),
                body: Vec::new(),
            },
        ],
    );
    let transport =
        pi::mcp::transport::HttpTransport::new(&format!("{}/sse-ping", server.base_url()), vec![])
            .expect("transport construction");
    let result = block_on_local(transport.request(
        "tools/list",
        json!({}),
        std::time::Duration::from_secs(10),
    ))
    .expect("streamed response after server ping");
    assert_eq!(result["tools"], json!([]));

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let ping_response: Value =
        serde_json::from_slice(&requests[1].body).expect("ping response JSON");
    assert_eq!(
        ping_response,
        json!({"jsonrpc": "2.0", "id": "server-ping", "result": {}})
    );
    finish_case(&harness, case);
}

#[test]
fn mcp_http_transport_initialize_sse_ping_uses_provisional_session() {
    let case = "mcp_http_transport_initialize_sse_ping_uses_provisional_session";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    let sse_body = concat!(
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":\"initialize-ping\",\"method\":\"ping\"}\n\n",
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"serverInfo\":{\"name\":\"mock\",\"version\":\"0\"}}}\n\n"
    );
    server.add_route_queue(
        "POST",
        "/initialize-sse-ping",
        vec![
            MockHttpResponse {
                status: 200,
                headers: vec![
                    ("Content-Type".to_string(), "text/event-stream".to_string()),
                    ("Mcp-Session-Id".to_string(), "session-new".to_string()),
                ],
                body: sse_body.as_bytes().to_vec(),
            },
            MockHttpResponse {
                status: 202,
                headers: Vec::new(),
                body: Vec::new(),
            },
            MockHttpResponse {
                status: 202,
                headers: Vec::new(),
                body: Vec::new(),
            },
        ],
    );
    let transport = pi::mcp::transport::HttpTransport::new(
        &format!("{}/initialize-sse-ping", server.base_url()),
        vec![],
    )
    .expect("transport construction");

    let result = block_on_local(transport.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "initialize-ping-test", "version": "1"}
        }),
        std::time::Duration::from_secs(10),
    ))
    .expect("initialize response after server ping");
    assert_eq!(result["protocolVersion"], "2025-06-18");
    block_on_local(transport.notify("notifications/initialized", json!({})))
        .expect("initialized notification");

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        !requests[0].headers.iter().any(|(name, _)| {
            name.eq_ignore_ascii_case("mcp-session-id")
                || name.eq_ignore_ascii_case("mcp-protocol-version")
        }),
        "initialize request must not claim unnegotiated state"
    );
    assert!(requests[1].headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("mcp-session-id") && value == "session-new"
    }));
    assert!(
        !requests[1]
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("mcp-protocol-version")),
        "nested ping response precedes successful protocol negotiation"
    );
    assert!(requests[2].headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("mcp-session-id") && value == "session-new"
    }));
    assert!(requests[2].headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("mcp-protocol-version") && value == "2025-06-18"
    }));

    let ping_response: Value =
        serde_json::from_slice(&requests[1].body).expect("ping response JSON");
    assert_eq!(
        ping_response,
        json!({"jsonrpc": "2.0", "id": "initialize-ping", "result": {}})
    );
    let initialized_frame: Value =
        serde_json::from_slice(&requests[2].body).expect("initialized notification JSON");
    assert_eq!(initialized_frame["method"], "notifications/initialized");
    finish_case(&harness, case);
}

#[test]
fn mcp_http_transport_never_replays_after_nested_response_session_404() {
    let case = "mcp_http_transport_never_replays_after_nested_response_session_404";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    let sse_body = concat!(
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":\"server-ping\",\"method\":\"ping\"}\n\n",
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[]}}\n\n"
    );
    server.add_route_queue(
        "POST",
        "/sse-nested-404",
        vec![
            mock_http_initialize_response(1, "session-old"),
            MockHttpResponse {
                status: 202,
                headers: Vec::new(),
                body: Vec::new(),
            },
            MockHttpResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
                body: sse_body.as_bytes().to_vec(),
            },
            MockHttpResponse::text(404, "session expired while answering ping"),
        ],
    );
    let transport = pi::mcp::transport::HttpTransport::new(
        &format!("{}/sse-nested-404", server.base_url()),
        vec![],
    )
    .expect("transport construction");
    block_on_local(transport.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "nested-404-test", "version": "1"}
        }),
        std::time::Duration::from_secs(10),
    ))
    .expect("initialize");
    block_on_local(transport.notify("notifications/initialized", json!({})))
        .expect("initialized notification");

    let error = block_on_local(transport.request(
        "tools/call",
        json!({"name": "non_idempotent", "arguments": {}}),
        std::time::Duration::from_secs(10),
    ))
    .expect_err("nested response 404 makes original request delivery indeterminate");
    assert!(
        error.to_string().contains("MCP_DELIVERY_INDETERMINATE"),
        "{error}"
    );
    assert!(!transport.is_alive(), "indeterminate transport must abort");

    let requests = server.requests();
    // Compare the full wire sequence first so an unexpected extra round-trip
    // is named in the failure instead of reported as a bare count.
    let methods: Vec<String> = requests
        .iter()
        .map(|request| {
            let frame: Value = serde_json::from_slice(&request.body).unwrap_or_else(
                |_| json!({"method": format!("{} {}", request.method, request.path)}),
            );
            frame
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("response")
                .to_string()
        })
        .collect();
    // The indeterminate abort sends one best-effort `notifications/cancelled`
    // for the accepted tools/call before retiring the transport. That is a
    // cancellation of the original request, never a renewal or a replay.
    assert_eq!(
        methods,
        vec![
            String::from("initialize"),
            String::from("notifications/initialized"),
            String::from("tools/call"),
            String::from("response"),
            String::from("notifications/cancelled")
        ],
        "accepted outer tools/call must never be renewed or replayed"
    );
    let cancel_frame: Value =
        serde_json::from_slice(&requests[4].body).expect("cancel notification frame JSON");
    assert_eq!(
        cancel_frame["params"]["requestId"], 2,
        "the cancellation must name the accepted tools/call: {cancel_frame}"
    );
    finish_case(&harness, case);
}

#[test]
fn mcp_http_transport_rejects_malformed_streamed_server_params() {
    let case = "mcp_http_transport_rejects_malformed_streamed_server_params";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    let sse_body = concat!(
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":\"bad-ping\",\"method\":\"ping\",\"params\":7}\n\n",
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n"
    );
    server.add_route(
        "POST",
        "/sse-invalid-params",
        MockHttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body: sse_body.as_bytes().to_vec(),
        },
    );
    let transport = pi::mcp::transport::HttpTransport::new(
        &format!("{}/sse-invalid-params", server.base_url()),
        vec![],
    )
    .expect("transport construction");
    let error = block_on_local(transport.request(
        "tools/list",
        json!({}),
        std::time::Duration::from_secs(10),
    ))
    .expect_err("non-object MCP params must fail before a streamed response is accepted");
    assert!(
        error.to_string().contains("params must be an object"),
        "{error}"
    );
    assert_eq!(
        server.requests().len(),
        1,
        "malformed ping must not emit a client response POST"
    );
    finish_case(&harness, case);
}

#[test]
fn mcp_http_transport_rejects_result_or_error_on_streamed_method_envelopes() {
    let case = "mcp_http_transport_rejects_result_or_error_on_streamed_method_envelopes";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    server.add_route_queue(
        "POST",
        "/sse-mixed-envelope",
        vec![
            MockHttpResponse {
                status: 200,
                headers: vec![(
                    "Content-Type".to_string(),
                    "text/event-stream".to_string(),
                )],
                body: b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":\"bad\",\"method\":\"ping\",\"result\":{}}\n\n"
                    .to_vec(),
            },
            MockHttpResponse {
                status: 200,
                headers: vec![(
                    "Content-Type".to_string(),
                    "text/event-stream".to_string(),
                )],
                body: b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"error\":{\"code\":-1,\"message\":\"bad\"}}\n\n"
                    .to_vec(),
            },
        ],
    );
    // A malformed streamed envelope retires the transport, so each case gets
    // a fresh transport that consumes the next queued response.
    for label in ["request with result", "notification with error"] {
        let transport = pi::mcp::transport::HttpTransport::new(
            &format!("{}/sse-mixed-envelope", server.base_url()),
            vec![],
        )
        .expect("transport construction");
        let error = block_on_local(transport.request(
            "tools/list",
            json!({}),
            std::time::Duration::from_secs(10),
        ))
        .expect_err(label);
        assert!(
            error
                .to_string()
                .contains("must not contain result or error"),
            "unexpected {label} error: {error}"
        );
        assert!(
            !transport.is_alive(),
            "{label}: a malformed streamed envelope must retire the transport"
        );
    }
    assert_eq!(
        server.requests().len(),
        2,
        "malformed method envelopes must not emit nested responses"
    );
    finish_case(&harness, case);
}

#[test]
fn mcp_http_transport_rejects_non_object_outgoing_params_before_dispatch() {
    let case = "mcp_http_transport_rejects_non_object_outgoing_params_before_dispatch";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    server.add_route_queue(
        "POST",
        "/params",
        vec![
            MockHttpResponse {
                status: 202,
                headers: Vec::new(),
                body: Vec::new(),
            },
            MockHttpResponse::json(
                200,
                &json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}}),
            ),
        ],
    );
    let transport =
        pi::mcp::transport::HttpTransport::new(&format!("{}/params", server.base_url()), vec![])
            .expect("transport construction");

    let request_error = block_on_local(transport.request(
        "tools/list",
        json!(7),
        std::time::Duration::from_secs(10),
    ))
    .expect_err("scalar request params must fail locally");
    assert!(request_error.to_string().contains("object-valued params"));
    block_on_local(transport.notify("notifications/test", Value::Null))
        .expect("null is the no-params sentinel");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let notification: Value =
        serde_json::from_slice(&requests[0].body).expect("notification frame JSON");
    assert!(
        notification.get("params").is_none(),
        "null sentinel must omit params from the wire frame"
    );

    let result = block_on_local(transport.request(
        "tools/list",
        Value::Null,
        std::time::Duration::from_secs(10),
    ))
    .expect("null request params are the no-params sentinel");
    assert_eq!(result["tools"], json!([]));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let frame: Value = serde_json::from_slice(&requests[1].body).expect("request frame JSON");
    assert_eq!(
        frame["id"], 1,
        "invalid params must not consume request ids"
    );
    assert!(
        frame.get("params").is_none(),
        "null request sentinel must omit params from the wire frame"
    );
    finish_case(&harness, case);
}

#[test]
fn mcp_http_transport_distinguishes_request_and_notification_202() {
    let case = "mcp_http_transport_distinguishes_request_and_notification_202";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    server.add_route_queue(
        "POST",
        "/accepted",
        vec![
            MockHttpResponse {
                status: 202,
                headers: Vec::new(),
                body: Vec::new(),
            },
            MockHttpResponse {
                status: 202,
                headers: Vec::new(),
                body: Vec::new(),
            },
        ],
    );
    let transport =
        pi::mcp::transport::HttpTransport::new(&format!("{}/accepted", server.base_url()), vec![])
            .expect("transport construction");

    let request_error = block_on_local(transport.request(
        "tools/list",
        json!({}),
        std::time::Duration::from_secs(10),
    ))
    .expect_err("HTTP 202 must not satisfy a JSON-RPC request");
    assert!(
        request_error.to_string().contains("MCP_PROTOCOL")
            && request_error.to_string().contains("HTTP 202"),
        "unexpected request error: {request_error}"
    );
    // A 202 to a request proves the server accepted it without ever telling
    // us the outcome. The transport retires that logical session instead of
    // sending further side effects over it, so the notification is refused
    // before dispatch rather than acknowledged.
    assert!(
        !transport.is_alive(),
        "an unanswerable accepted request must retire the transport"
    );
    let notify_error = block_on_local(transport.notify("notifications/initialized", json!({})))
        .expect_err("a retired transport must not dispatch further notifications");
    assert!(
        notify_error
            .to_string()
            .contains("MCP_TRANSPORT_UNAVAILABLE"),
        "unexpected notification error: {notify_error}"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "nothing may follow the accepted request");
    let request_frame: Value =
        serde_json::from_slice(&requests[0].body).expect("request frame JSON");
    assert_eq!(request_frame["id"], 1);
    finish_case(&harness, case);
}

#[test]
fn mcp_http_transport_rejects_wrong_media_type_and_nonempty_202() {
    let case = "mcp_http_transport_rejects_wrong_media_type_and_nonempty_202";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/wrong-media",
        MockHttpResponse::text(200, r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#),
    );
    server.add_route(
        "POST",
        "/nonempty-accepted",
        MockHttpResponse::text(202, "unexpected"),
    );
    for (path, content_type) in [
        ("json-lookalike", "application/json-seq"),
        ("sse-lookalike", "text/event-streamx"),
    ] {
        server.add_route(
            "POST",
            &format!("/{path}"),
            MockHttpResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), content_type.to_string())],
                body: br#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#.to_vec(),
            },
        );
    }

    let wrong_media = pi::mcp::transport::HttpTransport::new(
        &format!("{}/wrong-media", server.base_url()),
        vec![],
    )
    .expect("wrong-media transport construction");
    let media_error = block_on_local(wrong_media.request(
        "tools/list",
        json!({}),
        std::time::Duration::from_secs(10),
    ))
    .expect_err("text/plain must not carry a successful JSON-RPC request response");
    assert!(
        media_error.to_string().contains("Content-Type"),
        "unexpected media-type error: {media_error}"
    );
    for path in ["json-lookalike", "sse-lookalike"] {
        let transport = pi::mcp::transport::HttpTransport::new(
            &format!("{}/{path}", server.base_url()),
            vec![],
        )
        .expect("lookalike-media transport construction");
        let error = block_on_local(transport.request(
            "tools/list",
            json!({}),
            std::time::Duration::from_secs(10),
        ))
        .expect_err("media-type lookalikes must not pass exact classification");
        assert!(
            error.to_string().contains("Content-Type"),
            "unexpected {path} error: {error}"
        );
    }

    let nonempty_accepted = pi::mcp::transport::HttpTransport::new(
        &format!("{}/nonempty-accepted", server.base_url()),
        vec![],
    )
    .expect("nonempty-accepted transport construction");
    let body_error =
        block_on_local(nonempty_accepted.notify("notifications/initialized", json!({})))
            .expect_err("HTTP 202 acknowledgement must not carry a body");
    assert!(
        body_error.to_string().contains("no body") || body_error.to_string().contains("not empty"),
        "unexpected 202-body error: {body_error}"
    );
    finish_case(&harness, case);
}

#[test]
fn mcp_http_transport_rejects_mismatched_jsonrpc_envelopes() {
    let case = "mcp_http_transport_rejects_mismatched_jsonrpc_envelopes";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    server.add_route_queue(
        "POST",
        "/mismatch",
        vec![
            MockHttpResponse::json(
                200,
                &json!({"jsonrpc": "1.0", "id": 1, "result": {"tools": []}}),
            ),
            MockHttpResponse::json(
                200,
                &json!({"jsonrpc": "2.0", "id": 99, "result": {"tools": []}}),
            ),
            MockHttpResponse {
                status: 200,
                headers: vec![(
                    "Content-Type".to_string(),
                    "text/event-stream".to_string(),
                )],
                body: b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{\"tools\":[]}}\n\n"
                    .to_vec(),
            },
        ],
    );
    // Each mismatched envelope proves the server accepted a request whose
    // outcome is unknowable, which retires the transport. A fresh transport
    // per case keeps the queued responses flowing in order.
    for reason in ["JSON version", "JSON id", "SSE id"] {
        let transport = pi::mcp::transport::HttpTransport::new(
            &format!("{}/mismatch", server.base_url()),
            vec![],
        )
        .expect("transport construction");
        let error = block_on_local(transport.request(
            "tools/list",
            json!({}),
            std::time::Duration::from_secs(10),
        ))
        .expect_err(reason);
        assert!(
            error.to_string().contains("MCP_PROTOCOL"),
            "{reason} mismatch produced unexpected error: {error}"
        );
        assert!(
            !transport.is_alive(),
            "{reason} mismatch must retire the transport"
        );
        let retired = block_on_local(transport.request(
            "tools/list",
            json!({}),
            std::time::Duration::from_secs(10),
        ))
        .expect_err("a retired transport must refuse further requests");
        assert!(
            retired.to_string().contains("MCP_TRANSPORT_UNAVAILABLE"),
            "{reason}: unexpected post-retirement error: {retired}"
        );
    }
    assert_eq!(
        server.requests().len(),
        3,
        "retired transports must not dispatch follow-up requests"
    );
    finish_case(&harness, case);
}

#[test]
fn mcp_http_transport_does_not_retain_invalid_initialize_state() {
    let case = "mcp_http_transport_does_not_retain_invalid_initialize_state";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    server.add_route_queue(
        "POST",
        "/invalid-initialize",
        vec![
            MockHttpResponse {
                status: 200,
                headers: vec![
                    ("Content-Type".to_string(), "text/event-stream".to_string()),
                    ("Mcp-Session-Id".to_string(), "hostile-session".to_string()),
                ],
                body: format!(
                    "event: message\ndata: {}\n\n",
                    serde_json::to_string(&json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {"protocolVersion": "bad\u{001b}", "capabilities": {}}
                    }))
                    .expect("serialize hostile initialize response")
                )
                .into_bytes(),
            },
            MockHttpResponse::json(
                200,
                &json!({"jsonrpc": "2.0", "id": 2, "result": {"tools": []}}),
            ),
        ],
    );
    let transport = pi::mcp::transport::HttpTransport::new(
        &format!("{}/invalid-initialize", server.base_url()),
        vec![],
    )
    .expect("transport construction");

    let initialize_error = block_on_local(transport.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}}),
        std::time::Duration::from_secs(10),
    ))
    .expect_err("hostile initialize metadata must fail closed");
    assert!(
        initialize_error.to_string().contains("MCP_PROTOCOL"),
        "unexpected initialize error: {initialize_error}"
    );
    // The hostile initialize response was accepted by the server but is
    // unusable, so the transport retires rather than carrying the invalid
    // session state into any later request.
    assert!(
        !transport.is_alive(),
        "invalid initialize state must retire the transport"
    );
    let later_error = block_on_local(transport.request(
        "tools/list",
        json!({}),
        std::time::Duration::from_secs(10),
    ))
    .expect_err("a retired transport must not dispatch later requests");
    assert!(
        later_error
            .to_string()
            .contains("MCP_TRANSPORT_UNAVAILABLE"),
        "unexpected later request error: {later_error}"
    );

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        1,
        "invalid initialize state must never be replayed: {:?}",
        requests
            .iter()
            .map(|request| &request.headers)
            .collect::<Vec<_>>()
    );
    finish_case(&harness, case);
}

#[test]
#[allow(clippy::too_many_lines)]
fn mcp_http_transport_rejects_unsupported_version_and_invisible_session_id() {
    let case = "mcp_http_transport_rejects_unsupported_version_and_invisible_session_id";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/unsupported-version",
        MockHttpResponse::json(
            200,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"protocolVersion": "2024-11-05", "capabilities": {}}
            }),
        ),
    );
    server.add_route(
        "POST",
        "/invisible-session",
        MockHttpResponse {
            status: 200,
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Mcp-Session-Id".to_string(), "contains space".to_string()),
            ],
            body: serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "serverInfo": {"name": "mock", "version": "0"}
                }
            }))
            .expect("serialize initialize response"),
        },
    );
    server.add_route(
        "POST",
        "/empty-session",
        MockHttpResponse {
            status: 200,
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Mcp-Session-Id".to_string(), String::new()),
            ],
            body: serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "serverInfo": {"name": "mock", "version": "0"}
                }
            }))
            .expect("serialize initialize response"),
        },
    );
    server.add_route(
        "POST",
        "/missing-capabilities",
        MockHttpResponse::json(
            200,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "serverInfo": {"name": "mock", "version": "0"}
                }
            }),
        ),
    );
    server.add_route(
        "POST",
        "/missing-server-info",
        MockHttpResponse::json(
            200,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"protocolVersion": "2025-06-18", "capabilities": {}}
            }),
        ),
    );
    server.add_route(
        "POST",
        "/invalid-server-info",
        MockHttpResponse::json(
            200,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "serverInfo": {"name": "mock"}
                }
            }),
        ),
    );

    for (path, expected) in [
        ("unsupported-version", "unsupported protocolVersion"),
        ("invisible-session", "visible ASCII"),
        ("empty-session", "visible ASCII"),
        ("missing-capabilities", "capabilities field"),
        ("missing-server-info", "serverInfo field"),
        ("invalid-server-info", "name and version"),
    ] {
        let transport = pi::mcp::transport::HttpTransport::new(
            &format!("{}/{path}", server.base_url()),
            vec![],
        )
        .expect("transport construction");
        let error = block_on_local(transport.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}}),
            std::time::Duration::from_secs(10),
        ))
        .expect_err("invalid initialize negotiation must fail");
        assert!(
            error.to_string().contains(expected),
            "unexpected negotiation error for {path}: {error}"
        );
    }
    finish_case(&harness, case);
}

#[test]
fn mcp_http_transport_renews_expired_session_once() {
    let case = "mcp_http_transport_renews_expired_session_once";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    server.add_route_queue(
        "POST",
        "/renew",
        vec![
            mock_http_initialize_response(1, "session-old"),
            MockHttpResponse {
                status: 202,
                headers: Vec::new(),
                body: Vec::new(),
            },
            MockHttpResponse::text(404, "expired"),
            mock_http_initialize_response(3, "session-new"),
            MockHttpResponse {
                status: 202,
                headers: Vec::new(),
                body: Vec::new(),
            },
            MockHttpResponse::json(
                200,
                &json!({"jsonrpc": "2.0", "id": 4, "result": {"tools": []}}),
            ),
        ],
    );
    let transport =
        pi::mcp::transport::HttpTransport::new(&format!("{}/renew", server.base_url()), vec![])
            .expect("transport construction");
    let initialize_params = json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {"sampling": {}},
        "clientInfo": {"name": "renew-test", "version": "1"}
    });

    block_on_local(transport.request(
        "initialize",
        initialize_params.clone(),
        std::time::Duration::from_secs(10),
    ))
    .expect("initial initialize");
    block_on_local(transport.notify("notifications/initialized", json!({})))
        .expect("initial initialized notification");
    let tools = block_on_local(transport.request(
        "tools/list",
        json!({}),
        std::time::Duration::from_secs(10),
    ))
    .expect("expired session must be renewed and the rejected request retried once");
    assert_eq!(tools["tools"], json!([]));

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        6,
        "renewal must perform exactly three posts"
    );
    let frames: Vec<Value> = requests
        .iter()
        .map(|request| serde_json::from_slice(&request.body).expect("request frame JSON"))
        .collect();
    assert_eq!(frames[0]["method"], "initialize");
    assert_eq!(frames[0]["id"], 1);
    assert_eq!(frames[2]["method"], "tools/list");
    assert_eq!(frames[2]["id"], 2);
    assert_eq!(frames[3]["method"], "initialize");
    assert_eq!(frames[3]["id"], 3);
    assert_eq!(frames[3]["params"], initialize_params);
    assert_eq!(frames[4]["method"], "notifications/initialized");
    assert!(frames[4].get("id").is_none());
    assert_eq!(frames[5]["method"], "tools/list");
    assert_eq!(frames[5]["id"], 4);

    for request in &requests[1..=2] {
        assert!(request.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("mcp-session-id") && value == "session-old"
        }));
    }
    assert!(
        !requests[3].headers.iter().any(|(name, _)| {
            name.eq_ignore_ascii_case("mcp-session-id")
                || name.eq_ignore_ascii_case("mcp-protocol-version")
        }),
        "renewal initialize must not replay expired state"
    );
    for request in &requests[4..] {
        assert!(request.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("mcp-session-id") && value == "session-new"
        }));
        assert!(request.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("mcp-protocol-version") && value == "2025-06-18"
        }));
    }
    finish_case(&harness, case);
}

#[test]
fn mcp_http_transport_aborts_after_second_session_404() {
    let case = "mcp_http_transport_aborts_after_second_session_404";
    let harness = TestHarness::new(case);
    let server = harness.start_mock_http_server();
    server.add_route_queue(
        "POST",
        "/renew-twice",
        vec![
            mock_http_initialize_response(1, "session-old"),
            MockHttpResponse {
                status: 202,
                headers: Vec::new(),
                body: Vec::new(),
            },
            MockHttpResponse::text(404, "expired"),
            mock_http_initialize_response(3, "session-new"),
            MockHttpResponse {
                status: 202,
                headers: Vec::new(),
                body: Vec::new(),
            },
            MockHttpResponse::text(404, "expired again"),
        ],
    );
    let transport = pi::mcp::transport::HttpTransport::new(
        &format!("{}/renew-twice", server.base_url()),
        vec![],
    )
    .expect("transport construction");
    block_on_local(transport.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "renew-test", "version": "1"}
        }),
        std::time::Duration::from_secs(10),
    ))
    .expect("initial initialize");
    block_on_local(transport.notify("notifications/initialized", json!({})))
        .expect("initial initialized notification");
    let error = block_on_local(transport.request(
        "tools/list",
        json!({}),
        std::time::Duration::from_secs(10),
    ))
    .expect_err("a second session 404 must escape rather than renew recursively");
    assert!(error.to_string().contains("MCP_SESSION_EXPIRED"), "{error}");
    assert!(!transport.is_alive(), "failed one-shot renewal must abort");
    assert_eq!(server.requests().len(), 6);

    let later = block_on_local(transport.request(
        "tools/list",
        json!({}),
        std::time::Duration::from_secs(10),
    ))
    .expect_err("aborted transport must reject later dispatch locally");
    assert!(later.to_string().contains("MCP_TRANSPORT_UNAVAILABLE"));
    assert_eq!(
        server.requests().len(),
        6,
        "aborted transport must not emit another renewal attempt"
    );
    finish_case(&harness, case);
}

/// bd-8m21l: an MCP server that an extension registers after startup (the
/// `registerMcpServer` hostcall only updates the extension manager's
/// snapshot) is brought into the live session by the SDK's per-prompt sync:
/// registered once under the same trust gate as at startup, listed with
/// extension provenance, pending until acknowledged (so nothing mounts), and
/// never registered twice.
#[test]
fn mcp_extension_servers_registered_after_startup_sync_into_the_live_session() {
    let case = "mcp_extension_servers_registered_after_startup_sync_into_the_live_session";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let global = harness.temp_path("global");
    let extension_path = root.join("late-extension.native.json");
    std::fs::write(
        &extension_path,
        serde_json::to_vec(&json!({
            "id": "late-extension",
            "name": "late-extension",
            "version": "1.0.0",
            "apiVersion": pi::extensions::PROTOCOL_VERSION,
            "mcpServers": []
        }))
        .expect("serialize native extension"),
    )
    .expect("write native extension");
    let mut handle = block_on_local(pi::sdk::create_agent_session(pi::sdk::SessionOptions {
        provider: Some("openai".to_string()),
        model: Some("gpt-4o".to_string()),
        api_key: Some("dummy-key".to_string()),
        working_directory: Some(root),
        no_session: true,
        enabled_tools: Some(Vec::new()),
        extension_paths: vec![extension_path],
        mcp: Some(pi::sdk::McpSessionOptions {
            config_paths: Vec::new(),
            global_dir: Some(global),
        }),
        ..pi::sdk::SessionOptions::default()
    }))
    .expect("create MCP-enabled SDK session");
    let manager = handle.mcp_manager().expect("SDK-owned MCP manager");
    assert!(
        manager.list().iter().all(|row| row.name != "late-fixture"),
        "nothing is registered at startup"
    );
    assert_eq!(
        block_on_local(handle.sync_extension_mcp_registrations()),
        0,
        "nothing to sync before the extension registers anything"
    );

    // What the `registerMcpServer` hostcall does once startup is over.
    handle
        .extension_manager()
        .expect("extension manager")
        .register_mcp_server(json!({
            "name": "late-fixture",
            "command": "pi-mcp-late-fixture-does-not-exist",
            "extension_id": "late-extension"
        }));
    assert!(
        manager.list().iter().all(|row| row.name != "late-fixture"),
        "the extension snapshot alone must not reach the MCP manager"
    );

    let registered = block_on_local(handle.sync_extension_mcp_registrations());
    assert_eq!(
        registered, 1,
        "the sync must register the late definition once"
    );
    let row = manager
        .list()
        .into_iter()
        .find(|row| row.name == "late-fixture")
        .expect("late server listed after the sync");
    assert_eq!(row.provenance, "extension");
    assert_eq!(
        row.trust, "pending",
        "a server first seen at runtime waits for acknowledgement exactly like at startup"
    );
    assert!(
        !handle.session().agent.has_tool("mcp__late-fixture__echo"),
        "a pending server must not mount wrappers"
    );
    assert_eq!(
        block_on_local(handle.sync_extension_mcp_registrations()),
        0,
        "a second sync must not register the same server again"
    );
    assert_eq!(
        manager
            .list()
            .iter()
            .filter(|row| row.name == "late-fixture")
            .count(),
        1,
        "the manager must hold one definition for the late server"
    );
    finish_case(&harness, case);
}

// ---------------------------------------------------------------------------
// Stdio fixture lifecycle (feature-gated)
// ---------------------------------------------------------------------------

#[cfg(feature = "internal-mcp-fixture")]
mod fixture_lanes {
    use super::*;
    use pi::mcp::transport::StdioTransport;
    use std::io::Write as _;

    const FIXTURE_BIN: &str = env!("CARGO_BIN_EXE_pi_mcp_fixture");
    const ENV_ALLOWLIST_CHILD_ATTESTATION: &str = "pi-mcp-env-allowlist-child-complete";

    fn fixture_manager(harness: &TestHarness, extra_env: &[(&str, &str)]) -> McpManager {
        let root = harness.temp_path(".");
        write_project_mcp_config(&root, "fixture", FIXTURE_BIN, extra_env);
        McpManager::bootstrap(&root, &harness.temp_path("global"), &[]).expect("bootstrap")
    }

    fn fixture_transport(harness: &TestHarness, extra_env: &[(&str, &str)]) -> StdioTransport {
        let env: Vec<(String, String)> = extra_env
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        StdioTransport::spawn(FIXTURE_BIN, &[], &env, &harness.temp_path("."))
            .expect("spawn stdio fixture")
    }

    fn wait_for_transport_diagnostics(
        transport: &StdioTransport,
        needle: &str,
        timeout: std::time::Duration,
    ) -> String {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let tail = transport.diagnostics_tail();
            if tail.contains(needle) || std::time::Instant::now() >= deadline {
                return tail;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn wait_for_manager_diagnostics(
        manager: &McpManager,
        needle: &str,
        timeout: std::time::Duration,
    ) -> String {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let tail = manager.server_diagnostics("fixture").unwrap_or_default();
            if tail.contains(needle) || std::time::Instant::now() >= deadline {
                return tail;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn wait_for_process_exit(pid: u32, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let target = sysinfo::Pid::from_u32(pid);
            let mut system = sysinfo::System::new();
            system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[target]), true);
            if system.process(target).is_none() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn fixture_descendant_pid(transport: &StdioTransport) -> u32 {
        let tail = wait_for_transport_diagnostics(
            transport,
            "descendant pid=",
            std::time::Duration::from_secs(1),
        );
        tail.split("descendant pid=")
            .nth(1)
            .and_then(|suffix| suffix.lines().next())
            .and_then(|pid| pid.trim().parse::<u32>().ok())
            .expect("descendant pid in fixture diagnostics")
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
    fn acknowledged_extension_server_is_mounted_by_startup_connect_seam() {
        let case = "acknowledged_extension_server_is_mounted_by_startup_connect_seam";
        let harness = TestHarness::new(case);
        let root = harness.temp_path(".");
        let global = harness.temp_path("global");
        let spec = json!({
            "name": "extension-fixture",
            "command": FIXTURE_BIN,
            "extension_id": "fixture-extension"
        });
        let pending_spec = json!({
            "name": "extension-pending",
            "command": FIXTURE_BIN,
            "extension_id": "fixture-extension"
        });

        // First launch: the operator acknowledges this exact extension-owned
        // definition, which persists trust for later sessions.
        let first = McpManager::bootstrap(&root, &global, &[]).expect("first bootstrap");
        first.register_extension_server("extension-fixture", &spec);
        block_on_local(first.trust("extension-fixture")).expect("persist extension trust");
        drop(first);

        // Next launch: the production SDK/FTUI seam must load its own
        // extension, register that extension's server into its own manager,
        // connect after registration, and mount the wrapper into its live
        // Agent rather than the discarded classic session (bd-vjfol).
        let extension_path = root.join("fixture-extension.native.json");
        std::fs::write(
            &extension_path,
            serde_json::to_vec(&json!({
                "id": "fixture-extension",
                "name": "fixture-extension",
                "version": "1.0.0",
                "apiVersion": pi::extensions::PROTOCOL_VERSION,
                "mcpServers": [spec, pending_spec]
            }))
            .expect("serialize native extension"),
        )
        .expect("write native extension");
        let mut handle = block_on_local(pi::sdk::create_agent_session(pi::sdk::SessionOptions {
            provider: Some("openai".to_string()),
            model: Some("gpt-4o".to_string()),
            api_key: Some("dummy-key".to_string()),
            working_directory: Some(root.clone()),
            no_session: true,
            enabled_tools: Some(Vec::new()),
            extension_paths: vec![extension_path],
            mcp: Some(pi::sdk::McpSessionOptions {
                config_paths: Vec::new(),
                global_dir: Some(global.clone()),
            }),
            ..pi::sdk::SessionOptions::default()
        }))
        .expect("create MCP-enabled SDK session");
        assert!(
            handle
                .session()
                .agent
                .has_tool("mcp__extension-fixture__echo"),
            "the actual SDK Agent must own the mounted extension tool"
        );
        let manager = handle.mcp_manager().expect("SDK-owned MCP manager");
        let row = manager
            .list()
            .into_iter()
            .find(|row| row.name == "extension-fixture")
            .expect("extension server listed");
        assert_eq!(row.provenance, "extension");
        assert_eq!(row.trust, "acknowledged");
        assert!(
            !handle
                .session()
                .agent
                .has_tool("mcp__extension-pending__echo"),
            "a pending extension server must not leak wrappers at startup"
        );

        // The runtime trust path mounts only the selected server and filters
        // names already present in the live Agent. This is the exact
        // algorithm used by FTUI `/mcp trust` and `/mcp test`.
        block_on_local(manager.trust("extension-pending"))
            .expect("trust the pending extension server at runtime");
        let pending_wrappers = pi::mcp::mount_server_tools(&manager, "extension-pending");
        assert!(
            !pending_wrappers.is_empty(),
            "the newly trusted server must expose wrappers"
        );
        assert!(
            pending_wrappers
                .iter()
                .all(|tool| tool.name().starts_with("mcp__extension-pending__")),
            "targeted mounting must not re-append another server's wrappers"
        );
        let mounted = handle.mount_mcp_server_tools_if_absent("extension-pending");
        assert!(
            mounted > 0,
            "the first runtime mount must add the selected server"
        );
        assert!(
            handle
                .session()
                .agent
                .has_tool("mcp__extension-pending__echo"),
            "the live SDK Agent must receive runtime-trusted wrappers"
        );
        assert_eq!(
            handle.mount_mcp_server_tools_if_absent("extension-pending"),
            0,
            "repeating trust/test must not duplicate live Agent tools"
        );

        let tools = pi::mcp::mount_tools(&manager);
        let echo = tools
            .iter()
            .find(|tool| tool.name() == "mcp__extension-fixture__echo")
            .expect("acknowledged extension tool mounted during startup");
        let output = block_on_local(echo.execute(
            "extension-call",
            json!({"text": "startup-extension"}),
            None,
        ))
        .expect("mounted extension MCP tool executes");
        assert!(
            first_text(&output).starts_with("echo: startup-extension"),
            "{}",
            first_text(&output)
        );
        assert!(!output.is_error);
        finish_case(&harness, case);
    }

    #[test]
    fn mcp_fixture_rejects_lsp_content_length_framing() {
        let case = "mcp_fixture_rejects_lsp_content_length_framing";
        let harness = TestHarness::new(case);
        let mut child = std::process::Command::new(FIXTURE_BIN)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn raw fixture");
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let mut stdin = child.stdin.take().expect("fixture stdin");
        write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write LSP header");
        stdin.write_all(body).expect("write LSP body");
        drop(stdin);
        let output = child
            .wait_with_output()
            .expect("wait for fixture rejection");
        assert!(
            output.stdout.is_empty(),
            "fixture must not answer LSP framing"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("protocol input rejected"),
            "fixture did not report rejecting LSP framing: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        finish_case(&harness, case);
    }

    #[test]
    fn mcp_stdio_fixture_surfaces_malformed_oversize_eof_and_wrong_id() {
        for (mode, expected_code) in [
            ("malformed", "MCP_PROTOCOL"),
            ("oversize", "MCP_PROTOCOL"),
            ("eof", "MCP_TRANSPORT_CLOSED"),
            ("wrong-id", "MCP_TRANSPORT_CLOSED"),
        ] {
            let case = format!("mcp_stdio_fixture_rejects_{mode}");
            let harness = TestHarness::new(&case);
            let manager = fixture_manager(&harness, &[("PI_MCP_FIXTURE_RESPONSE_MODE", mode)]);
            let error = block_on_local(manager.trust("fixture"))
                .expect_err("hostile fixture response must fail connection");
            assert!(
                error.to_string().contains(expected_code),
                "mode {mode} returned unexpected error: {error}"
            );
            finish_case(&harness, &case);
        }
    }

    #[test]
    fn mcp_stdio_timeout_sends_cancellation_and_aborts_connection() {
        let case = "mcp_stdio_timeout_sends_cancellation_and_aborts_connection";
        let harness = TestHarness::new(case);
        let transport = fixture_transport(&harness, &[]);
        let error = block_on_local(transport.request(
            "fixture/await-cancellation",
            json!({}),
            std::time::Duration::from_millis(100),
        ))
        .expect_err("fixture deliberately withholds the response");
        assert!(error.to_string().contains("MCP_TIMEOUT"), "{error}");
        let tail = wait_for_transport_diagnostics(
            &transport,
            "observed cancellation",
            std::time::Duration::from_secs(1),
        );
        assert!(
            tail.contains("observed cancellation for pending request"),
            "fixture did not observe MCP cancellation: {tail}"
        );
        assert!(!transport.is_alive(), "timed-out transport must be aborted");
        finish_case(&harness, case);
    }

    #[test]
    fn mcp_stdio_blocked_writer_timeout_reaps_descendant_tree() {
        let case = "mcp_stdio_blocked_writer_timeout_reaps_descendant_tree";
        let harness = TestHarness::new(case);
        let transport = fixture_transport(
            &harness,
            &[
                ("PI_MCP_FIXTURE_RESPONSE_MODE", "no-read"),
                ("PI_MCP_FIXTURE_SPAWN_DESCENDANT", "1"),
            ],
        );
        let descendant_pid = fixture_descendant_pid(&transport);

        let started = std::time::Instant::now();
        let error = block_on_local(transport.request(
            "tools/call",
            json!({ "blob": "x".repeat(2 * 1024 * 1024) }),
            std::time::Duration::from_millis(100),
        ))
        .expect_err("non-reading server must time out");
        assert!(error.to_string().contains("MCP_TIMEOUT"), "{error}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "blocked pipe write escaped the request deadline: {:?}",
            started.elapsed()
        );
        assert!(
            wait_for_process_exit(descendant_pid, std::time::Duration::from_secs(2)),
            "descendant process {descendant_pid} survived timeout tree cleanup"
        );
        finish_case(&harness, case);
    }

    #[test]
    fn mcp_stdio_close_and_drop_reap_descendant_trees() {
        let case = "mcp_stdio_close_and_drop_reap_descendant_trees";
        let harness = TestHarness::new(case);

        let closing = fixture_transport(&harness, &[("PI_MCP_FIXTURE_SPAWN_DESCENDANT", "1")]);
        let closing_descendant = fixture_descendant_pid(&closing);
        block_on_local(closing.close());
        assert!(
            wait_for_process_exit(closing_descendant, std::time::Duration::from_secs(2)),
            "descendant process {closing_descendant} survived graceful close"
        );

        let dropping = fixture_transport(
            &harness,
            &[
                ("PI_MCP_FIXTURE_RESPONSE_MODE", "no-read"),
                ("PI_MCP_FIXTURE_SPAWN_DESCENDANT", "1"),
            ],
        );
        let dropping_descendant = fixture_descendant_pid(&dropping);
        drop(dropping);
        assert!(
            wait_for_process_exit(dropping_descendant, std::time::Duration::from_secs(2)),
            "descendant process {dropping_descendant} survived transport drop"
        );
        finish_case(&harness, case);
    }

    #[test]
    fn mcp_stdio_dropped_request_and_close_futures_reap_descendant_trees() {
        let case = "mcp_stdio_dropped_request_and_close_futures_reap_descendant_trees";
        let harness = TestHarness::new(case);

        let requesting = fixture_transport(&harness, &[("PI_MCP_FIXTURE_SPAWN_DESCENDANT", "1")]);
        let requesting_descendant = fixture_descendant_pid(&requesting);
        block_on_local(async {
            let request = Box::pin(requesting.request(
                "fixture/await-cancellation",
                json!({}),
                std::time::Duration::MAX,
            ));
            let dispatched = Box::pin(async {
                let tail = wait_for_transport_diagnostics(
                    &requesting,
                    "method=fixture/await-cancellation",
                    std::time::Duration::from_secs(1),
                );
                assert!(
                    tail.contains("method=fixture/await-cancellation"),
                    "request was not dispatched before its future was dropped: {tail}"
                );
            });
            match futures::future::select(request, dispatched).await {
                futures::future::Either::Left((result, _)) => {
                    panic!("fixture unexpectedly completed held request: {result:?}");
                }
                futures::future::Either::Right(((), pending_request)) => {
                    drop(pending_request);
                }
            }
        });
        assert!(
            wait_for_process_exit(requesting_descendant, std::time::Duration::from_secs(2),),
            "descendant process {requesting_descendant} survived request-future cancellation"
        );

        let closing = fixture_transport(
            &harness,
            &[
                ("PI_MCP_FIXTURE_RESPONSE_MODE", "no-read"),
                ("PI_MCP_FIXTURE_SPAWN_DESCENDANT", "1"),
            ],
        );
        let closing_descendant = fixture_descendant_pid(&closing);
        block_on_local(async {
            let close = Box::pin(closing.close());
            let cx = pi::agent_cx::AgentCx::for_current_or_request();
            let now = cx
                .cx()
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
            let observed_pending = Box::pin(asupersync::time::sleep(
                now,
                std::time::Duration::from_millis(20),
            ));
            match futures::future::select(close, observed_pending).await {
                futures::future::Either::Left(((), _)) => {
                    panic!("close unexpectedly finished before its grace period");
                }
                futures::future::Either::Right(((), pending_close)) => {
                    drop(pending_close);
                }
            }
        });
        assert!(
            wait_for_process_exit(closing_descendant, std::time::Duration::from_secs(2)),
            "descendant process {closing_descendant} survived close-future cancellation"
        );
        finish_case(&harness, case);
    }

    #[test]
    fn mcp_stdio_dead_root_reaps_descendant_that_holds_output_open() {
        let case = "mcp_stdio_dead_root_reaps_descendant_that_holds_output_open";
        let harness = TestHarness::new(case);
        let transport = fixture_transport(
            &harness,
            &[
                ("PI_MCP_FIXTURE_RESPONSE_MODE", "root-exit"),
                ("PI_MCP_FIXTURE_SPAWN_DESCENDANT", "1"),
                ("PI_MCP_FIXTURE_DESCENDANT_INHERIT_OUTPUT", "1"),
            ],
        );
        let descendant_pid = fixture_descendant_pid(&transport);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while transport.is_alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !transport.is_alive(),
            "transport must observe the exited root even while a descendant holds its pipes open"
        );
        assert!(
            wait_for_process_exit(descendant_pid, std::time::Duration::from_secs(2)),
            "descendant process {descendant_pid} survived dead-root cleanup"
        );
        finish_case(&harness, case);
    }

    #[test]
    fn mcp_live_transport_rechecks_shared_denial() {
        let case = "mcp_live_transport_rechecks_shared_denial";
        let harness = TestHarness::new(case);
        let manager_a = fixture_manager(&harness, &[]);
        block_on_local(manager_a.trust("fixture")).expect("manager A trust + connect");

        let manager_b = fixture_manager(&harness, &[]);
        block_on_local(manager_b.deny("fixture")).expect("manager B denial");
        assert!(
            manager_a.mounted_tool_metas().is_empty(),
            "a shared denial must hide previously cached tools immediately"
        );

        let err = block_on_local(manager_a.call_tool("fixture", "echo", json!({"text": "no"})))
            .expect_err("manager A must re-read shared denial before using live transport");
        assert!(err.to_string().contains("MCP_TRUST_DENIED"), "{err}");
        let row = manager_a
            .list()
            .into_iter()
            .find(|row| row.name == "fixture")
            .expect("manager A row");
        assert_eq!(row.trust, "denied");
        assert_eq!(row.health, "not started");
        finish_case(&harness, case);
    }

    #[test]
    fn mcp_stdio_env_allowlist_proven() {
        if std::env::var_os("PI_MCP_SECRET_MARKER").is_none() {
            let output = std::process::Command::new(
                std::env::current_exe().expect("current integration-test executable"),
            )
            .arg("--exact")
            .arg("fixture_lanes::mcp_stdio_env_allowlist_proven")
            .arg("--nocapture")
            .env("PI_MCP_SECRET_MARKER", "controlled-parent-secret")
            .output()
            .expect("launch controlled env-allowlist child test");
            assert!(
                output.status.success(),
                "controlled env-allowlist child failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains(ENV_ALLOWLIST_CHILD_ATTESTATION),
                "controlled child exited without executing the env-allowlist assertions"
            );
            return;
        }

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
        assert_eq!(
            report["PI_MCP_SECRET_MARKER"], false,
            "ambient secret markers must not leak to servers"
        );
        assert_eq!(report["AWS_SECRET_ACCESS_KEY"], false);
        finish_case(&harness, case);
        println!("{ENV_ALLOWLIST_CHILD_ATTESTATION}");
    }

    #[test]
    fn mcp_stdio_crash_reports_indeterminate_delivery_then_recovers() {
        let case = "mcp_stdio_crash_reports_indeterminate_delivery_then_recovers";
        let harness = TestHarness::new(case);
        // Crash after request 3: initialize(1), tools/list(2), first echo(3)
        // succeed; the second echo hits the dying process. Pi reconnects, but
        // must not replay a call whose delivery cannot be known.
        let manager = fixture_manager(&harness, &[("PI_MCP_FIXTURE_CRASH_AFTER", "3")]);
        block_on_local(manager.trust("fixture")).expect("trust");

        let first = block_on_local(manager.call_tool("fixture", "echo", json!({"text": "one"})))
            .expect("first echo works");
        let second = block_on_local(manager.call_tool("fixture", "echo", json!({"text": "two"})))
            .expect_err("ambiguous delivery must not be retried");
        assert!(
            second.to_string().contains("MCP_DELIVERY_INDETERMINATE"),
            "{second}"
        );
        let third = block_on_local(manager.call_tool("fixture", "echo", json!({"text": "three"})))
            .expect("fresh server keeps serving");
        harness.log().info(
            "verify",
            format!(
                "sequence: one={} two_error={} three={}",
                mcp_text(&first),
                second,
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
        let pid3 = pid_of(&third).expect("pid in response");
        assert_ne!(pid1, pid3, "recovery must produce a new process");
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

        // `/mcp test` is the documented manual recovery path. It must clear
        // the exhausted budget and make one real connection attempt; this
        // fixture still crashes, so the attempt fails for the transport reason
        // rather than being rejected by the old budget.
        let err = block_on_local(manager.test("fixture"))
            .expect_err("manual retry reaches the still-crashing fixture");
        assert!(!err.to_string().contains("MCP_RESTART_EXHAUSTED"), "{err}");
        assert!(!err.to_string().contains("MCP_BACKOFF"), "{err}");
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
        let tail =
            wait_for_manager_diagnostics(&manager, "7f3a9c-v2", std::time::Duration::from_secs(1));
        harness.log().info("verify", format!("stderr tail: {tail}"));
        assert!(
            tail.contains("7f3a9c-v2"),
            "fixture stderr marker must be captured: {tail}"
        );
        finish_case(&harness, case);
    }
}
