//! Wire-level regressions for native MCP authentication admission and failures.
//!
//! These fixtures exercise the production manager and HTTP transport together.
//! A rejected tool call must not be replayed or counted as a crashed server.

use super::*;
use std::io::{BufRead as _, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread::JoinHandle;

use serde_json::json;

struct AuthHttpServer {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

fn read_request(stream: &TcpStream) -> std::io::Result<(String, Value)> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = std::io::BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let method = line.split_whitespace().next().unwrap_or_default().to_string();
    let mut content_length = 0usize;
    let mut header_bytes = line.len();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        header_bytes += line.len();
        if header_bytes > 32 * 1024 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value
                .trim()
                .parse()
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
        }
    }
    if content_length > 64 * 1024 {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?
    };
    Ok((method, body))
}

fn response(status: u16, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn rejected_response(status: u16) -> String {
    // Neither health nor caller errors should quote an authentication body.
    response(status, "text/plain", "private-server-authentication-body")
}

#[allow(clippy::needless_pass_by_value)]
fn rpc_response(request: &Value, result: Value) -> String {
    response(
        200,
        "application/json",
        &json!({"jsonrpc": "2.0", "id": request["id"], "result": result}).to_string(),
    )
}

fn handle_request(
    method: &str,
    request: &Value,
    reject_initialize: Option<u16>,
    reject_call: u16,
    get_status: u16,
) -> String {
    match method {
        "GET" => rejected_response(get_status),
        "DELETE" => response(204, "text/plain", ""),
        "POST" => match request["method"].as_str() {
            Some("initialize") => reject_initialize.map_or_else(
                || {
                    rpc_response(
                        request,
                        json!({
                            "protocolVersion": MCP_PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "auth-fixture", "version": "1"}
                        }),
                    )
                },
                rejected_response,
            ),
            Some("notifications/initialized" | "notifications/cancelled") => {
                response(202, "text/plain", "")
            }
            Some("tools/list") => rpc_response(
                request,
                json!({"tools": [{
                    "name": "side_effect",
                    "description": "Reject without applying the side effect",
                    "inputSchema": {"type": "object"}
                }]}),
            ),
            Some("tools/call") => rejected_response(reject_call),
            _ => response(400, "text/plain", "unexpected fixture method"),
        },
        _ => response(400, "text/plain", "unexpected fixture HTTP method"),
    }
}

impl AuthHttpServer {
    fn start(reject_initialize: Option<u16>, reject_call: u16, get_status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind authentication fixture");
        let address = listener.local_addr().expect("fixture address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let thread = std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                if stop.load(Ordering::Acquire) {
                    break;
                }
                stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
                let Ok((method, request)) = read_request(&stream) else {
                    continue;
                };
                McpManager::lock(&captured).push((method.clone(), request.clone()));
                let reply = handle_request(
                    &method,
                    &request,
                    reject_initialize,
                    reject_call,
                    get_status,
                );
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            address,
            requests,
            stopping,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/mcp", self.address)
    }

    fn rpc_count(&self, method: &str) -> usize {
        McpManager::lock(&self.requests)
            .iter()
            .filter(|(_, request)| request["method"].as_str() == Some(method))
            .count()
    }

    fn http_count(&self, method: &str) -> usize {
        McpManager::lock(&self.requests)
            .iter()
            .filter(|(observed, _)| observed == method)
            .count()
    }

    fn count(&self) -> usize {
        McpManager::lock(&self.requests).len()
    }
}

impl Drop for AuthHttpServer {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(1));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn fixture(
    temporary: &tempfile::TempDir,
    url: String,
    headers: Vec<(String, String)>,
    trusted: bool,
) -> (McpManager, Arc<ServerEntry>) {
    let global = temporary.path().join("global");
    std::fs::create_dir_all(&global).expect("global credential directory");
    let manager = McpManager::new(
        temporary.path(),
        &global,
        McpDiscovery {
            servers: vec![ConfiguredServer {
                name: "remote auth".to_string(),
                command: None,
                args: Vec::new(),
                env: Vec::new(),
                url: Some(url),
                headers,
                oauth: None,
                transport_hint: Some("http".to_string()),
                enabled: true,
                cwd: None,
                timeout_ms: Some(5_000),
                provenance: Provenance::GlobalPi,
                source_file: global.join("mcp.json"),
            }],
            warnings: Vec::new(),
        },
    );
    let entry = manager.entry("remote auth").expect("configured server");
    if trusted {
        TrustStore::load(&manager.inner.trust_path)
            .expect("load trust store")
            .acknowledge(
                "remote auth",
                &manager.trust_fingerprint_for(&entry),
                "operator",
            )
            .expect("acknowledge fixture");
    }
    (manager, entry)
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new()
        .worker_threads(1)
        .blocking_threads(1, 2)
        .build()
        .expect("authentication fixture runtime")
}

fn assert_authentication_required(manager: &McpManager, entry: &Arc<ServerEntry>, error: &Error) {
    let message = error.to_string();
    assert!(message.contains("[MCP_AUTH_REQUIRED]"), "{message}");
    assert!(!message.contains("private-server-authentication-body"));
    assert_eq!(McpManager::lock(&entry.restarts).count, 0);
    assert!(McpManager::lock(&entry.restarts).next_retry_at.is_none());
    assert!(McpManager::lock(&entry.transport).is_none());
    assert!(manager.mounted_tool_metas().is_empty());
    assert!(matches!(
        &*McpManager::lock(&entry.health),
        ServerHealth::AuthenticationRequired { .. }
    ));
}

#[test]
fn repeated_authentication_rejections_do_not_exhaust_server_crash_retries() {
    for status in [401, 403] {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let server = AuthHttpServer::start(Some(status), status, 405);
        let (manager, entry) = fixture(&temporary, server.url(), Vec::new(), true);
        runtime().block_on(async {
            for expected in 1..=5 {
                let error = manager
                    .refresh_tools("remote auth")
                    .await
                    .expect_err("initialization requires authentication");
                assert_authentication_required(&manager, &entry, &error);
                assert_eq!(server.rpc_count("initialize"), expected);
            }
            assert_eq!(server.rpc_count("tools/call"), 0);
            manager.shutdown_all().await;
        });
    }
}

#[test]
fn rejected_tool_call_is_not_replayed_or_reconnected() {
    for status in [401, 403] {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let server = AuthHttpServer::start(None, status, 405);
        let (manager, entry) = fixture(&temporary, server.url(), Vec::new(), true);
        runtime().block_on(async {
            let tools = manager.refresh_tools("remote auth").await.expect("catalog");
            assert_eq!(tools.len(), 1);
            assert_eq!(server.rpc_count("initialize"), 1);
            assert_eq!(server.http_count("GET"), 1);
            let error = manager
                .call_tool("remote auth", "side_effect", json!({}))
                .await
                .expect_err("tool call authentication rejected");
            assert_authentication_required(&manager, &entry, &error);
            assert_eq!(server.rpc_count("tools/call"), 1);
            assert_eq!(
                server.rpc_count("initialize"),
                1,
                "authentication rejection must not start automatic recovery"
            );
            assert_eq!(server.rpc_count("notifications/cancelled"), 0);
            manager.shutdown_all().await;
        });
    }
}

#[test]
fn get_channel_authentication_rejection_prevents_catalog_publication() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let server = AuthHttpServer::start(None, 401, 403);
    let (manager, entry) = fixture(&temporary, server.url(), Vec::new(), true);
    runtime().block_on(async {
        let error = manager
            .refresh_tools("remote auth")
            .await
            .expect_err("GET channel requires authentication");
        assert_authentication_required(&manager, &entry, &error);
        assert_eq!(server.http_count("GET"), 1);
        assert_eq!(server.rpc_count("tools/list"), 0);
        manager.shutdown_all().await;
    });
}

#[test]
fn operator_authentication_requires_trust_and_cannot_replace_static_authorization() {
    for (headers, trusted, expected) in [
        (Vec::new(), false, "MCP_TRUST_PENDING"),
        (
            vec![(
                "aUtHoRiZaTiOn".to_string(),
                "Bearer explicit-fixture".to_string(),
            )],
            true,
            "MCP_AUTH_STATIC_HEADER",
        ),
    ] {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let server = AuthHttpServer::start(None, 401, 405);
        let (manager, _) = fixture(&temporary, server.url(), headers, trusted);
        runtime().block_on(async {
            let error = manager
                .reauthenticate(
                    "remote auth",
                    None,
                    Arc::new(|_: &str| panic!("rejected admission cannot emit a browser URL")),
                )
                .await
                .expect_err("operator authentication admission rejected");
            assert!(error.to_string().contains(expected), "{error}");
            if trusted {
                let error = manager
                    .unauthenticate("remote auth")
                    .await
                    .expect_err("static credentials are owned by configuration");
                assert!(error.to_string().contains("MCP_AUTH_STATIC_HEADER"));
            }
            assert_eq!(
                server.count(),
                0,
                "admission must precede all network requests"
            );
            manager.shutdown_all().await;
        });
    }
}


fn configured_oauth_fixture(
    temporary: &tempfile::TempDir,
    spec: &Value,
    trusted: bool,
) -> (McpManager, Arc<ServerEntry>) {
    let global = temporary.path().join("oauth-global");
    std::fs::create_dir_all(&global).expect("OAuth fixture global directory");
    let manager = McpManager::new(
        temporary.path(),
        &global,
        McpDiscovery { servers: Vec::new(), warnings: Vec::new() },
    );
    manager.register_extension_server("configured", spec);
    let entry = manager.entry("configured").expect("configured extension server");
    if trusted {
        TrustStore::load(&manager.inner.trust_path)
            .expect("load trust store")
            .acknowledge("configured", &manager.trust_fingerprint_for(&entry), "operator")
            .expect("acknowledge configured OAuth fixture");
    }
    (manager, entry)
}

#[test]
fn public_client_override_skips_both_configured_helpers_and_preserves_authored_settings() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (manager, entry) = configured_oauth_fixture(
        &temporary,
        &json!({
            "url": "http://127.0.0.1:1/mcp",
            "oauth": {
                "clientId": "$CMD:exit 97",
                "clientSecret": "$CMD:exit 98",
                "scope": "read offline_access",
                "prompt": ""
            }
        }),
        true,
    );
    let resolved = runtime().block_on(
        manager.resolve_authentication_configuration(&entry, Some("public-client".to_string())),
    ).expect("public override must not run either unused helper");
    assert_eq!(resolved.client_id.as_deref(), Some("public-client"));
    assert!(resolved.client_secret.is_none());
    assert_eq!(resolved.scope.as_deref(), Some("read offline_access"));
    assert_eq!(resolved.prompt.as_deref(), Some(""));
    let authored = entry.config.oauth.as_ref().expect("authored options");
    assert_eq!(authored.client_id.as_deref(), Some("$CMD:exit 97"));
    assert_eq!(authored.client_secret.as_deref(), Some("$CMD:exit 98"));

    // An RPC client ID is a literal identifier, not another configuration
    // reference. It is also valid without the process-spawn capability.
    let literal = resolve_oauth_settings(
        &entry.config,
        Some("$CMD:literal-client-identifier".to_string()),
        temporary.path(),
        temporary.path(),
        false,
        &|| Ok(()),
    ).expect("literal public identifier");
    assert_eq!(literal.client_id.as_deref(), Some("$CMD:literal-client-identifier"));
    assert!(literal.client_secret.is_none());
    let error = resolve_oauth_settings(
        &entry.config, None, temporary.path(), temporary.path(), false, &|| Ok(()),
    ).expect_err("selected helper requires process capability");
    assert!(error.to_string().contains("MCP_CAPABILITY_DENIED"), "{error}");
}

#[cfg(unix)]
#[test]
fn configured_oauth_helpers_and_network_wait_for_trust_and_enabled_admission() {
    for (enabled, trusted, denied, expected) in [
        (true, false, false, "MCP_TRUST_PENDING"),
        (false, true, false, "MCP_SERVER_DISABLED"),
        (true, true, true, "MCP_TRUST_DENIED"),
    ] {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let server = AuthHttpServer::start(None, 401, 405);
        let (manager, _) = configured_oauth_fixture(
            &temporary,
            &json!({
                "url": server.url(),
                "enabled": enabled,
                "oauth": {
                    "clientId": "registered",
                    "clientSecret": "$CMD:printf ran > oauth-helper-marker; printf private-secret"
                }
            }),
            trusted,
        );
        runtime().block_on(async {
            if denied {
                manager.deny("configured").await.expect("deny configured server");
            }
            let error = manager.reauthenticate(
                "configured",
                None,
                Arc::new(|_| panic!("rejected admission must not emit a sign-in URL")),
            ).await.expect_err("OAuth admission must reject this server");
            assert!(error.to_string().contains(expected), "{error}");
            assert!(!temporary.path().join("oauth-helper-marker").exists());
            assert_eq!(server.count(), 0, "admission must precede discovery");
        });
    }
}

#[cfg(unix)]
#[test]
fn configured_oauth_resolution_uses_server_cwd_and_session_workspace_without_persisting_secrets() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let helper_directory = temporary.path().join("helper");
    std::fs::create_dir_all(&helper_directory).expect("helper directory");
    std::fs::write(helper_directory.join("client-secret.txt"), "registered-secret")
        .expect("helper secret fixture");
    let (manager, entry) = configured_oauth_fixture(
        &temporary,
        &json!({
            "url": "http://127.0.0.1:1/mcp",
            "cwd": "helper",
            "oauth": {
                "clientId": "client-${workspaceFolder}",
                "clientSecret": "$CMD:cat client-secret.txt",
                "scope": "read offline_access",
                "prompt": ""
            }
        }),
        true,
    );
    let resolved = runtime().block_on(
        manager.resolve_authentication_configuration(&entry, None),
    ).expect("resolve trusted configured OAuth");
    assert_eq!(resolved.client_id, Some(format!("client-{}", manager.inner.cwd.display())));
    assert_eq!(resolved.client_secret.as_deref(), Some("registered-secret"));
    let authored = entry.config.oauth.as_ref().expect("authored configuration");
    assert_eq!(authored.client_id.as_deref(), Some("client-${workspaceFolder}"));
    assert_eq!(authored.client_secret.as_deref(), Some("$CMD:cat client-secret.txt"));
    assert!(!format!("{resolved:?}").contains("registered-secret"));
}

#[test]
fn changing_oauth_configuration_requires_a_new_trust_acknowledgment() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let spec = json!({
        "url": "http://127.0.0.1:1/mcp",
        "oauth": {"clientId": "original", "clientSecret": "$CMD:exit 97"}
    });
    let (original, entry) = configured_oauth_fixture(&temporary, &spec, true);
    let original_fingerprint = original.trust_fingerprint_for(&entry);
    let mut changed_spec = spec;
    changed_spec["oauth"]["clientId"] = Value::String("replacement".to_string());
    let (changed, changed_entry) = configured_oauth_fixture(&temporary, &changed_spec, false);
    assert_ne!(original_fingerprint, changed.trust_fingerprint_for(&changed_entry));
    let error = runtime().block_on(
        changed.resolve_authentication_configuration(&changed_entry, None),
    ).expect_err("changed client must not reuse prior trust or run its helper");
    assert!(error.to_string().contains("MCP_TRUST_PENDING"), "{error}");
}

#[cfg(unix)]
#[test]
fn oauth_helper_diagnostics_never_expose_command_output_or_secret_values() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let (manager, entry) = configured_oauth_fixture(
        &temporary,
        &json!({
            "url": "http://127.0.0.1:1/mcp",
            "oauth": {
                "clientId": "registered",
                "clientSecret": "$CMD:printf DO_NOT_ECHO_OAUTH_SECRET >&2; exit 23"
            }
        }),
        true,
    );
    let error = runtime().block_on(
        manager.resolve_authentication_configuration(&entry, None),
    ).expect_err("failed credential helper");
    assert!(error.to_string().contains("oauth.clientSecret"), "{error}");
    assert!(!error.to_string().contains("DO_NOT_ECHO_OAUTH_SECRET"));
    assert!(!error.to_string().contains("exit 23"));
}
