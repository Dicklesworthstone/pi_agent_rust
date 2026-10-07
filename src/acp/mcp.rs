//! Session-owned MCP tools for ACP. Configuration is not execution authority.
//!
//! Setup stays synchronous and inert; native trust admission and connection
//! work run inside the existing reserved prompt task. The dispatcher therefore
//! remains available to process cancellation and tool-permission responses.
//! Only an actual, standalone ACP text prompt can invoke an operator command.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{
    ACP_STOP_REASON_CANCELLED, ACP_STOP_REASON_END_TURN, ACP_STOP_REASON_ERROR,
    AcpSessionsMap, AbortSignal, AgentCx, AgentSession, history, json_rpc_notification,
};
use crate::mcp::{ConfiguredServer, McpDiscovery, McpManager, ServerInfo};

// Refresh before the native manager's five-minute tool-cache TTL expires.
// Failed setup is eligible sooner, but the native restart budget still owns
// whether any connection is attempted. No background task or timer is spawned.
const CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const CATALOG_RETRY_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Default)]
struct CatalogRefresh {
    completed_at: Option<Instant>,
    acknowledged: Vec<String>,
}

fn acknowledged_servers(rows: &[ServerInfo]) -> Vec<String> {
    let mut names: Vec<_> = rows.iter()
        .filter(|row| row.trust == "acknowledged")
        .map(|row| row.name.clone()).collect();
    names.sort();
    names
}

impl CatalogRefresh {
    fn due(&self, rows: &[ServerInfo], now: Instant) -> bool {
        let Some(completed_at) = self.completed_at else { return true };
        if acknowledged_servers(rows) != self.acknowledged {
            return true;
        }
        let trusted: Vec<_> = rows.iter()
            .filter(|row| row.trust == "acknowledged").collect();
        let retryable = trusted.iter().any(|row|
            row.health == "not started" || row.health.starts_with("unhealthy"));
        let ready = trusted.iter().any(|row| row.health.starts_with("ready"));
        if !retryable && !ready {
            // Pending/denied servers must not be contacted. A terminally
            // failed server requires the explicit native /mcp test remedy.
            return false;
        }
        let interval = if retryable { CATALOG_RETRY_INTERVAL } else { CATALOG_REFRESH_INTERVAL };
        now.saturating_duration_since(completed_at) >= interval
    }

    fn complete(&mut self, acknowledged: Vec<String>, now: Instant) {
        self.completed_at = Some(now);
        self.acknowledged = acknowledged;
    }
}

pub(super) struct SessionMcp {
    pub(super) manager: Arc<McpManager>,
    signatures: Vec<(String, String)>,
    descriptions: Vec<(String, String)>,
    started: AtomicBool,
    refresh: StdMutex<CatalogRefresh>,
    // ACP's host tool set is fixed at session construction. Preserve the
    // actual handles and registry metadata, not newly constructed built-ins.
    // Only this session's MCP overlay is replaced during catalog refresh.
    host_registry: OnceLock<crate::tools::ToolRegistry>,
}

fn signatures(servers: &[ConfiguredServer], cwd: &Path) -> Vec<(String, String)> {
    let mut signatures: Vec<_> = servers.iter()
        .map(|server| (server.name.clone(), server.fingerprint(cwd))).collect();
    signatures.sort();
    signatures
}

pub(super) fn prepare(
    cwd: &Path,
    global_dir: &Path,
    servers: Vec<ConfiguredServer>,
) -> Option<Arc<SessionMcp>> {
    if servers.is_empty() {
        return None;
    }
    let signatures = signatures(&servers, cwd);
    let descriptions = servers.iter().map(|server| {
        // Do not copy argv, URLs, environment/header values, or filesystem
        // source paths into the conversation. The editor owns those values.
        let description = format!(
            "{}: {}; {} arguments, {} environment entries, {} headers. Inspect the definition in your editor before trusting it.",
            server.name,
            if server.is_http() { "HTTP" } else { "stdio" },
            server.args.len(), server.env.len(), server.headers.len(),
        );
        (server.name.clone(), description)
    }).collect();
    Some(Arc::new(SessionMcp {
        manager: Arc::new(McpManager::new(cwd, global_dir, McpDiscovery {
            servers,
            warnings: Vec::new(),
        })),
        signatures,
        descriptions,
        started: AtomicBool::new(false),
        refresh: StdMutex::new(CatalogRefresh::default()),
        host_registry: OnceLock::new(),
    }))
}

pub(super) fn mount(agent: &mut AgentSession, state: &Arc<SessionMcp>) {
    let shared = agent.agent.shared_tools();
    let current = shared.snapshot();
    let host = state.host_registry.get_or_init(|| current.clone_shallow());
    let mut next = host.clone_shallow();

    // xdev promotions can change after construction. Keep those decisions
    // without retaining the previous remote catalog. All tool handles, job
    // scopes, workspace confinement, host picker, and undo recorder are shared
    // by the shallow clone; none are reconstructed on an ordinary prompt.
    for tool in host.tools() {
        if !current.is_discoverable(tool.name()) {
            next.mark_promoted(tool.name());
        }
    }

    // The native manager exposes only complete, fresh, healthy, trusted
    // catalogs. Replacing the overlay withdraws removed tools and resource/
    // prompt context wrappers as well as adding or revising current tools.
    // Do not filter by name prefix: a host tool is not owned by this manager
    // merely because it has an MCP-looking name.
    next.extend(crate::mcp::mount_tools(&state.manager));
    shared.update(|registry| *registry = next);
    // SharedToolRegistry's version invalidates the provider schema cache.
    // Old snapshots held by an in-flight caller still use native per-call
    // trust/catalog checks; publishing a new snapshot does not grant access.
}

/// Reattaching an existing live session must not silently swap out its tool
/// authority. Identical definitions (including reordered lists) are accepted;
/// changed definitions require a new session or a process restart. On restart
/// the supplied definitions get a new manager and native fingerprint checks.
pub(super) fn check_reattach(
    current: Option<&Arc<SessionMcp>>,
    supplied: Option<&[ConfiguredServer]>,
    cwd: &Path,
) -> Result<(), String> {
    let Some(supplied) = supplied else { return Ok(()) };
    let actual = signatures(supplied, cwd);
    let previous = current.map_or(&[][..], |state| state.signatures.as_slice());
    if previous != actual.as_slice() {
        return Err("MCP definitions differ from this live session; create a new session or restart ACP before loading it with changed servers".to_string());
    }
    Ok(())
}

pub(super) fn commands_notification(id: &str) -> String {
    json_rpc_notification("session/update", json!({
        "sessionId": id,
        "update": {
            "sessionUpdate": "available_commands_update",
            "availableCommands": [{
                "name": "mcp",
                "description": "Inspect, refresh, trust, deny, or test this session's MCP servers",
                "input": { "hint": "list | refresh | inspect NAME | trust NAME | deny NAME | test NAME" },
            }],
        },
    }))
}

fn status(state: &SessionMcp) -> String {
    let mut lines = vec![String::from(
        "MCP servers (trust is required before connecting; trust persists for the exact workspace and definition):",
    )];
    for row in state.manager.list() {
        // Transport errors may quote remote stderr or credential-bearing
        // targets. Expose a bounded health category, not raw diagnostics.
        let health = if row.health.starts_with("ready") {
            "ready"
        } else if row.health.starts_with("unhealthy") {
            "unhealthy; use /mcp test after checking the server"
        } else if row.health.starts_with("failed") {
            "failed; use /mcp test after checking the server"
        } else {
            "not started"
        };
        lines.push(format!("{}: {}, {}, {} tools", row.name, row.trust, health, row.tools));
    }
    lines.push("Use /mcp inspect NAME, then /mcp trust NAME to approve a server. /mcp refresh reloads trusted tool catalogs; /mcp deny NAME revokes a server.".into());
    lines.join("\n")
}

pub(super) async fn announce(
    state: &Arc<SessionMcp>, id: &str, out: &SyncSender<String>,
) -> Result<(), String> {
    history::send_line(out, commands_notification(id)).await
        .map_err(|_| "Cannot deliver MCP command availability".to_string())?;
    emit_text(out, id, &status(state)).await
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    List,
    Refresh,
    Inspect(String),
    Trust(String),
    Deny(String),
    Test(String),
}

/// Inspect original wire blocks, NOT flattened model content. An embedded
/// resource, tool result, image caption, or replayed message cannot grant trust.
pub(super) fn command_from_prompt(blocks: &[Value]) -> Result<Option<Command>, String> {
    let first_text = blocks.first().filter(|block| block["type"] == "text")
        .and_then(|block| block.get("text")).and_then(Value::as_str);
    let Some(text) = first_text else { return Ok(None) };
    let text = text.trim();
    let Some(rest) = text.strip_prefix("/mcp") else { return Ok(None) };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return Ok(None);
    }
    if blocks.len() != 1 || text.contains(['\r', '\n']) {
        return Err("An MCP operator command must be one standalone text block on one line".to_string());
    }
    let words: Vec<_> = rest.split_whitespace().collect();
    let command = match words.as_slice() {
        [] | ["list"] => Command::List,
        ["refresh"] => Command::Refresh,
        ["inspect", name] => Command::Inspect((*name).to_string()),
        ["trust", name] => Command::Trust((*name).to_string()),
        ["deny", name] => Command::Deny((*name).to_string()),
        ["test", name] => Command::Test((*name).to_string()),
        _ => return Err("Usage: /mcp list | refresh | inspect NAME | trust NAME | deny NAME | test NAME".to_string()),
    };
    Ok(Some(command))
}

async fn emit_text(out: &SyncSender<String>, id: &str, text: &str) -> Result<(), String> {
    history::send_line(out, json_rpc_notification("session/update", json!({
        "sessionId": id,
        "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } },
    }))).await.map_err(|_| "Cannot deliver MCP response".to_string())
}

/// Abort wins before the first operation poll. Dropping an in-flight native
/// MCP connection future invokes its construction/handshake cleanup guards.
/// A durable trust decision already written is NOT rolled back by cancellation.
async fn cancellable<F: Future>(
    signal: &AbortSignal, cx: &AgentCx, work: F,
) -> Result<F::Output, ()> {
    if signal.is_aborted() || cx.is_cancel_requested() {
        return Err(());
    }
    let cancelled = async {
        while !signal.is_aborted() && !cx.is_cancel_requested() {
            cx.time().sleep(Duration::from_millis(10)).await;
        }
    };
    match futures::future::select(Box::pin(cancelled), Box::pin(cx.with_current(work))).await {
        futures::future::Either::Left(((), pending)) => { drop(pending); Err(()) }
        futures::future::Either::Right((result, _)) => Ok(result),
    }
}

/// The native manager bounds the whole pass, validates complete catalogs,
/// rechecks trust, and applies restart backoff. Only a completed pass advances
/// this schedule; cancellation must leave the next prompt eligible to retry.
async fn refresh_catalogs(state: &SessionMcp) {
    let acknowledged = acknowledged_servers(&state.manager.list());
    state.manager.connect_trusted().await;
    state.refresh.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        .complete(acknowledged, Instant::now());
}

async fn execute(state: &SessionMcp, command: Command) -> String {
    match command {
        Command::List => status(state),
        Command::Refresh => {
            refresh_catalogs(state).await;
            status(state)
        }
        Command::Inspect(name) => state.descriptions.iter()
            .find(|(candidate, _)| candidate == &name)
            .map(|(_, text)| text.clone())
            .unwrap_or_else(|| "Unknown MCP server; use /mcp list".to_string()),
        Command::Trust(name) => match state.manager.trust(&name).await {
            Ok(tools) => format!("Trusted and connected MCP server {name}: {} tools", tools.len()),
            Err(_) => "MCP trust/connect did not complete. Any trust decision already persisted remains in effect; inspect the editor configuration, then use /mcp list or /mcp deny NAME.".to_string(),
        },
        Command::Deny(name) => match state.manager.deny(&name).await {
            Ok(()) => format!("Denied MCP server {name}; further tool execution is blocked"),
            Err(_) => "MCP denial did not complete; use /mcp list and retry after checking the trust store.".to_string(),
        },
        Command::Test(name) => match state.manager.test(&name).await {
            Ok(tools) => format!("Connected MCP server {name}: {} tools", tools.len()),
            Err(_) => "MCP test failed. Inspect the server configuration and its trust state with /mcp list.".to_string(),
        },
    }
}

/// `Some(stop_reason)` finishes an operator command or cancelled preparation
/// without starting a provider turn. `None` leaves ordinary prompts on the
/// existing agent path, including its per-tool ACP permission handler.
pub(super) async fn before_prompt(
    state: Option<&Arc<SessionMcp>>,
    agent: &mut AgentSession,
    command: Option<Command>,
    signal: &AbortSignal,
    cx: &AgentCx,
    out: &SyncSender<String>,
    id: &str,
) -> Option<&'static str> {
    if signal.is_aborted() || cx.is_cancel_requested() {
        return Some(ACP_STOP_REASON_CANCELLED);
    }
    if let Some(command) = command {
        let text = if let Some(state) = state {
            let Ok(text) = cancellable(signal, cx, execute(state, command)).await else {
                return Some(ACP_STOP_REASON_CANCELLED);
            };
            // Publish the whole current overlay, including removals on deny.
            mount(agent, state);
            text
        } else {
            "No MCP servers were supplied for this session.".to_string()
        };
        return Some(match cancellable(signal, cx, emit_text(out, id, &text)).await {
            Ok(Ok(())) => ACP_STOP_REASON_END_TURN,
            Ok(Err(_)) => ACP_STOP_REASON_ERROR,
            Err(()) => ACP_STOP_REASON_CANCELLED,
        });
    }
    if let Some(state) = state {
        let first = !state.started.load(Ordering::Acquire);
        let rows = state.manager.list();
        let refresh_due = state.refresh.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .due(&rows, Instant::now());
        if refresh_due && cancellable(signal, cx, refresh_catalogs(state)).await.is_err() {
            return Some(ACP_STOP_REASON_CANCELLED);
        }
        mount(agent, state);
        if first {
            match cancellable(signal, cx, emit_text(out, id, &status(state))).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Some(ACP_STOP_REASON_ERROR),
                Err(()) => return Some(ACP_STOP_REASON_CANCELLED),
            }
            state.started.store(true, Ordering::Release);
        }
    }
    None
}

/// Invoked for EOF, exit, and dispatcher errors after all prompt abort handles
/// are signalled. Managers remain reachable while prompts own agent sessions.
pub(super) async fn shutdown(sessions: &AcpSessionsMap) {
    let cleanup = AgentCx::for_request();
    let states = match sessions.lock(&cleanup).await {
        Ok(guard) => guard.values().cloned().collect::<Vec<_>>(),
        Err(_) => return,
    };
    let mut managers = Vec::new();
    for state in states {
        if let Ok(guard) = state.lock(&cleanup).await
            && let Some(mcp) = guard.mcp.as_ref()
        {
            managers.push(Arc::clone(&mcp.manager));
        }
    }
    futures::future::join_all(managers.iter().map(|manager| manager.shutdown_all())).await;
}

#[cfg(test)]
mod integration_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::current_thread().build().expect("runtime")
    }

    fn http_servers(cwd: &Path) -> Vec<ConfiguredServer> {
        crate::mcp::config::parse_acp_servers(&json!({"mcpServers":[{
            "type":"http","name":"remote","url":"https://example.invalid/mcp",
            "headers":[{"name":"Authorization","value":"PRIVATE-HEADER-VALUE"}],
        }]}), cwd).expect("decode").expect("supplied")
    }

    fn row(trust: &str, health: &str) -> ServerInfo {
        ServerInfo {
            name: "remote".into(), target: "<http>".into(), provenance: "acp".into(),
            trust: trust.into(), health: health.into(), tools: 0,
            source_file: std::path::PathBuf::from("unused"),
        }
    }

    #[test]
    fn catalog_refresh_is_bounded_and_notices_external_trust_changes() {
        let now = Instant::now();
        let mut refresh = CatalogRefresh::default();
        let ready = [row("acknowledged", "ready (1 tools)")];
        assert!(refresh.due(&ready, now));
        refresh.complete(acknowledged_servers(&ready), now);
        assert!(!refresh.due(&ready, now + CATALOG_REFRESH_INTERVAL - Duration::from_millis(1)));
        assert!(refresh.due(&ready, now + CATALOG_REFRESH_INTERVAL));

        let pending = [row("pending", "not started")];
        assert!(refresh.due(&pending, now), "a revocation changes the acknowledged set");
        refresh.complete(Vec::new(), now);
        assert!(!refresh.due(&pending, now + Duration::from_secs(3600)));
        assert!(!refresh.due(&[row("denied", "not started")], now));
        assert!(refresh.due(&ready, now), "new external trust is not hidden by the refresh interval");
    }

    #[test]
    fn failed_setup_is_retried_without_resetting_terminal_restart_failures() {
        let now = Instant::now();
        let mut refresh = CatalogRefresh::default();
        for health in ["not started", "unhealthy (retry 1/3): unavailable"] {
            let rows = [row("acknowledged", health)];
            refresh.complete(acknowledged_servers(&rows), now);
            assert!(!refresh.due(&rows, now + CATALOG_RETRY_INTERVAL - Duration::from_millis(1)));
            assert!(refresh.due(&rows, now + CATALOG_RETRY_INTERVAL));
        }
        let failed = [row("acknowledged", "failed: exhausted 3 retries")];
        refresh.complete(acknowledged_servers(&failed), now);
        assert!(!refresh.due(&failed, now + Duration::from_secs(3600)));
    }

    /// Both MCP and provider traffic use real loopback HTTP. Mutable catalog
    /// data is server state, not an injected manager or registry implementation.
    struct CatalogServer {
        url: String,
        catalog: Arc<StdMutex<Value>>,
        requests: Arc<StdMutex<Vec<Value>>>,
        lists: Arc<std::sync::atomic::AtomicUsize>,
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl CatalogServer {
        fn start() -> Self {
            use std::io::{Read as _, Write as _};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let catalog = Arc::new(StdMutex::new(json!({"tools": [{
                "name": "echo", "description": "original catalog description",
                "inputSchema": {"type": "object", "properties": {"old_field": {"type": "string"}}},
            }]})));
            let requests = Arc::new(StdMutex::new(Vec::new()));
            let lists = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let worker_catalog = Arc::clone(&catalog);
            let worker_requests = Arc::clone(&requests);
            let worker_lists = Arc::clone(&lists);
            let worker_stop = Arc::clone(&stop);
            let worker = std::thread::spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => panic!("catalog fixture accept: {error}"),
                    };
                    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
                    let mut headers = Vec::new();
                    while !headers.ends_with(b"\r\n\r\n") {
                        let mut byte = [0_u8; 1];
                        stream.read_exact(&mut byte).unwrap();
                        headers.push(byte[0]);
                        assert!(headers.len() <= 32 * 1024, "bounded fixture headers");
                    }
                    let headers = String::from_utf8(headers).unwrap();
                    let length = headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    }).unwrap_or(0);
                    assert!(length <= 2 * 1024 * 1024, "bounded fixture body");
                    let mut body = vec![0_u8; length];
                    stream.read_exact(&mut body).unwrap();
                    if headers.starts_with("GET /mcp ") {
                        // The real transport probes its optional receive
                        // stream during activation, even without a session ID.
                        // This fixture implements POST only, as MCP permits.
                        stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                        continue;
                    }
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    let (status, content_type, body) = if headers.starts_with("POST /mcp ") {
                        let result = match request["method"].as_str().unwrap() {
                            "initialize" => Some(json!({
                                "protocolVersion": crate::mcp::transport::MCP_PROTOCOL_VERSION,
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "catalog-fixture", "version": "1"},
                            })),
                            "notifications/initialized" => None,
                            "tools/list" => {
                                worker_lists.fetch_add(1, Ordering::AcqRel);
                                Some(worker_catalog.lock().unwrap().clone())
                            }
                            other => panic!("no tool should execute in the schema probe: {other}"),
                        };
                        result.map_or_else(
                            || ("202 Accepted", "application/json", String::new()),
                            |result| ("200 OK", "application/json", json!({
                                "jsonrpc": "2.0", "id": request["id"], "result": result,
                            }).to_string()),
                        )
                    } else {
                        assert!(headers.starts_with("POST /v1/chat/completions "), "{headers}");
                        worker_requests.lock().unwrap().push(request);
                        let body = format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                            json!({"id":"probe","object":"chat.completion.chunk","model":"gpt-4o",
                                "choices":[{"index":0,"delta":{"role":"assistant","content":"schema observed"},"finish_reason":null}]}),
                            json!({"id":"probe","object":"chat.completion.chunk","model":"gpt-4o",
                                "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
                        ("200 OK", "text/event-stream", body)
                    };
                    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            });
            Self { url, catalog, requests, lists, stop, worker: Some(worker) }
        }
    }

    impl Drop for CatalogServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let result = worker.join();
                if !std::thread::panicking() { result.expect("catalog fixture worker"); }
            }
        }
    }

    #[test]
    fn refreshing_a_live_catalog_replaces_provider_schemas_without_duplicate_names() {
        let root = tempfile::tempdir().unwrap();
        let server = CatalogServer::start();
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .worker_threads(1).blocking_threads(1, 2).build().unwrap();
        let handle = runtime.handle();
        runtime.block_on(async {
            let cx = AgentCx::for_current_or_request();
            let work = async {
                let servers = crate::mcp::config::parse_acp_servers(&json!({"mcpServers":[{
                    "type":"http", "name":"remote", "url":format!("{}/mcp", server.url),
                }]}), root.path()).unwrap().unwrap();
                let mcp = prepare(root.path(), root.path(), servers).unwrap();
                let mut entry = crate::models::ad_hoc_model_entry("openai", "gpt-4o").unwrap();
                entry.model.api = "openai-completions".into();
                entry.model.base_url = format!("{}/v1", server.url);
                let provider = crate::providers::create_provider(&entry, None).unwrap();
                let agent = crate::agent::Agent::new(
                    provider, crate::tools::ToolRegistry::new(&["read", "lsp"], root.path(), None),
                    crate::agent::AgentConfig {
                        stream_options: crate::provider::StreamOptions {
                            api_key: Some("local-fixture-only".into()),
                            ..crate::provider::StreamOptions::default()
                        },
                        ..crate::agent::AgentConfig::default()
                    },
                );
                let session = AgentSession::new(agent,
                    Arc::new(asupersync::sync::Mutex::new(crate::session::Session::in_memory())), false,
                    crate::compaction::ResolvedCompactionSettings { enabled: false,
                        ..crate::compaction::ResolvedCompactionSettings::default() })
                    .with_runtime_handle(handle);
                let shared = session.agent.shared_tools();
                let host_snapshot = shared.snapshot();
                assert!(host_snapshot.is_discoverable("lsp"));
                let state = Arc::new(asupersync::sync::Mutex::new(super::super::AcpSessionState {
                    agent_session: Some(crate::sdk::AgentSessionHandle::from_session_with_listeners(
                        session, crate::sdk::EventListeners::default(),
                    )),
                    cwd: root.path().into(), mcp: Some(Arc::clone(&mcp)),
                }));
                let (out, receiver) = std::sync::mpsc::sync_channel(128);
                let run = |command| {
                    let (_, signal) = super::super::AbortHandle::new();
                    super::super::run_prompt(Arc::clone(&state), vec![crate::model::ContentBlock::Text(
                        crate::model::TextContent::new("Observe the tool schema"))], command,
                        signal, out.clone(), "schema-probe".into(), cx.clone())
                };
                assert_eq!(run(Some(Command::Trust("remote".into()))).await, ACP_STOP_REASON_END_TURN);
                assert_eq!(run(None).await, ACP_STOP_REASON_END_TURN); // Warm the provider schema cache.
                let name = crate::mcp::mounted_name("remote", "echo");
                let first = server.requests.lock().unwrap()[0].clone();
                let original = first["tools"].as_array().unwrap().iter()
                    .find(|tool| tool["function"]["name"] == name).unwrap();
                assert!(original["function"]["parameters"]["properties"].get("old_field").is_some());
                let admitted_snapshot = shared.snapshot();
                // A real xdev promotion publishes through this same registry.
                // Refresh must not reset it to the construction-time tier.
                shared.update(|registry| registry.mark_promoted("lsp"));

                *server.catalog.lock().unwrap() = json!({"tools": [
                    {"name":"echo", "description":"revised catalog description",
                     "inputSchema":{"type":"object", "required":["new_field"],
                        "properties":{"new_field":{"type":"integer"}}}},
                    {"name":"added", "inputSchema":{"type":"object"}},
                ]});
                assert_eq!(run(Some(Command::Refresh)).await, ACP_STOP_REASON_END_TURN);
                let refreshed_lists = server.lists.load(Ordering::Acquire);
                assert_eq!(run(None).await, ACP_STOP_REASON_END_TURN);
                assert_eq!(run(None).await, ACP_STOP_REASON_END_TURN);
                assert_eq!(server.lists.load(Ordering::Acquire), refreshed_lists,
                    "ordinary prompts reuse a fresh catalog instead of issuing tools/list every turn");
                let requests = server.requests.lock().unwrap().clone();
                assert_eq!(requests.len(), 3, "operator commands must not start a provider turn");
                for request in &requests[1..] {
                    let tools = request["tools"].as_array().unwrap();
                    let matches: Vec<_> = tools.iter().filter(|tool| tool["function"]["name"] == name).collect();
                    assert_eq!(matches.len(), 1, "remounting must replace, not duplicate");
                    let function = &matches[0]["function"];
                    assert!(function["description"].as_str().unwrap().contains("revised catalog description"));
                    assert!(function["parameters"]["properties"].get("old_field").is_none());
                    assert_eq!(function["parameters"]["properties"]["new_field"]["type"], "integer");
                    assert!(tools.iter().any(|tool| tool["function"]["name"] == crate::mcp::mounted_name("remote", "added")));
                    assert!(tools.iter().any(|tool| tool["function"]["name"] == "read"));
                    assert!(tools.iter().any(|tool| tool["function"]["name"] == "lsp"));
                }

                // Advance the controller's clock seam, not native transport
                // state: the next ordinary prompt must perform a real list.
                server.catalog.lock().unwrap()["tools"][0]["description"] = json!("periodically refreshed description");
                mcp.refresh.lock().unwrap().completed_at = Instant::now().checked_sub(CATALOG_REFRESH_INTERVAL);
                assert_eq!(run(None).await, ACP_STOP_REASON_END_TURN);
                assert!(server.lists.load(Ordering::Acquire) > refreshed_lists);
                let periodic = server.requests.lock().unwrap().last().unwrap().clone();
                assert!(periodic["tools"].as_array().unwrap().iter().any(|tool|
                    tool["function"]["name"] == name && tool["function"]["description"].as_str()
                        .is_some_and(|description| description.contains("periodically refreshed description"))));

                // Removal is not a schema update: the old name must disappear
                // from both live dispatch and the next actual provider body.
                *server.catalog.lock().unwrap() = json!({"tools":[{
                    "name":"added", "inputSchema":{"type":"object"},
                }]});
                assert_eq!(run(Some(Command::Refresh)).await, ACP_STOP_REASON_END_TURN);
                assert!(shared.snapshot().get(&name).is_none());
                assert_eq!(run(None).await, ACP_STOP_REASON_END_TURN);
                let removed = server.requests.lock().unwrap().last().unwrap().clone();
                assert!(!provider_names(&removed).contains(&name));
                // An already-held immutable snapshot is not retroactive
                // authority. Its original wrapper still checks the manager.
                let error = admitted_snapshot.get(&name).unwrap()
                    .execute("obsolete-call", json!({"old_field":"value"}), None)
                    .await.unwrap_err();
                assert!(error.to_string().contains("MCP_UNKNOWN_TOOL"), "{error}");

                // A valid zero-tool catalog keeps its resource/prompt context
                // tool. An unavailable catalog, by contrast, withdraws all
                // wrappers rather than advertising last-known-good authority.
                *server.catalog.lock().unwrap() = json!({"tools":[]});
                assert_eq!(run(Some(Command::Refresh)).await, ACP_STOP_REASON_END_TURN);
                assert_eq!(run(None).await, ACP_STOP_REASON_END_TURN);
                let empty = server.requests.lock().unwrap().last().unwrap().clone();
                let names = provider_names(&empty);
                assert!(!names.iter().any(|name| name.starts_with("mcp__remote__")));
                assert!(names.iter().any(|name| name.starts_with("mcp_context_")));

                // An invalid catalog genuinely retires the native connection.
                // A later prompt must retry after its real first backoff,
                // instead of treating the original startup as permanently done.
                *server.catalog.lock().unwrap() = json!({"tools": null});
                assert_eq!(run(Some(Command::Refresh)).await, ACP_STOP_REASON_END_TURN);
                assert!(mcp.manager.list()[0].health.starts_with("unhealthy"));
                assert!(!shared.snapshot().tools().iter().any(|tool|
                    tool.name().starts_with("mcp__remote__") || tool.name().starts_with("mcp_context_")));
                *server.catalog.lock().unwrap() = json!({"tools":[{
                    "name":"recovered", "description":"recovered catalog", "inputSchema":{"type":"object"},
                }]});
                cx.time().sleep(CATALOG_RETRY_INTERVAL + Duration::from_millis(100)).await;
                assert_eq!(run(None).await, ACP_STOP_REASON_END_TURN);
                assert!(mcp.manager.list()[0].health.starts_with("ready"));
                let recovered = server.requests.lock().unwrap().last().unwrap().clone();
                assert!(recovered["tools"].as_array().unwrap().iter().any(|tool|
                    tool["function"]["name"] == crate::mcp::mounted_name("remote", "recovered")));
                assert!(!provider_names(&recovered).contains(&name));

                // Persisted revocation by another owner is noticed at the next
                // prompt, not only when this editor invokes /mcp deny.
                crate::mcp::TrustStore::load(&root.path().join("mcp-trust.json")).unwrap()
                    .deny("remote", &mcp.signatures[0].1, "operator").unwrap();
                let lists_before_external_deny = server.lists.load(Ordering::Acquire);
                assert_eq!(run(None).await, ACP_STOP_REASON_END_TURN);
                assert_eq!(server.lists.load(Ordering::Acquire), lists_before_external_deny);
                let denied = server.requests.lock().unwrap().last().unwrap().clone();
                assert!(provider_names(&denied).iter().all(|name|
                    !name.starts_with("mcp__remote__") && !name.starts_with("mcp_context_")));
                assert_eq!(run(Some(Command::Trust("remote".into()))).await, ACP_STOP_REASON_END_TURN);
                assert_eq!(run(None).await, ACP_STOP_REASON_END_TURN);
                let trusted_again = server.requests.lock().unwrap().last().unwrap().clone();
                assert!(provider_names(&trusted_again).contains(&crate::mcp::mounted_name("remote", "recovered")));
                let refreshed_lists = server.lists.load(Ordering::Acquire);
                assert_eq!(run(Some(Command::Deny("remote".into()))).await, ACP_STOP_REASON_END_TURN);
                assert_eq!(run(Some(Command::Refresh)).await, ACP_STOP_REASON_END_TURN);
                assert_eq!(server.lists.load(Ordering::Acquire), refreshed_lists,
                    "refresh must never contact denied servers");
                assert!(mcp.manager.call_tool("remote", "echo", json!({"new_field":1})).await
                    .unwrap_err().to_string().contains("MCP_TRUST_DENIED"));
                assert_eq!(run(None).await, ACP_STOP_REASON_END_TURN);
                let denied = server.requests.lock().unwrap().last().unwrap().clone();
                assert!(provider_names(&denied).iter().all(|name|
                    !name.starts_with("mcp__remote__") && !name.starts_with("mcp_context_")));
                let current = shared.snapshot();
                for original in host_snapshot.tools() {
                    let retained = current.tools().iter().find(|tool| tool.name() == original.name()).unwrap();
                    assert!(Arc::ptr_eq(original, retained), "host tool was reconstructed: {}", original.name());
                }
                assert!(!current.is_discoverable("lsp"), "promotion survives removal, failure, and denial");
                let version = shared.version();
                current.shared_handle().unwrap().update(|_| {});
                assert_eq!(shared.version(), version + 1, "the original shared registry remains authoritative");
                assert!(receiver.try_iter().all(|line| !line.contains("local-fixture-only")));
                mcp.manager.shutdown_all().await;
            };
            let time = cx.time();
            match futures::future::select(Box::pin(work), Box::pin(time.sleep(Duration::from_secs(15)))).await {
                futures::future::Either::Left(((), _)) => {}
                futures::future::Either::Right(((), unfinished)) => {
                    drop(unfinished);
                    panic!("catalog refresh integration watchdog expired");
                }
            }
        });
    }

    fn provider_names(request: &Value) -> Vec<String> {
        let names: Vec<_> = request["tools"].as_array().expect("host tool schemas remain present").iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap().to_string()).collect();
        assert!(names.iter().any(|name| name == "read"));
        assert!(names.iter().any(|name| name == "lsp"));
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "provider received duplicate tool names");
        names
    }

    #[test]
    fn commands_require_actual_standalone_user_text() {
        for (text, expected) in [
            ("/mcp", Command::List), ("/mcp list", Command::List),
            ("/mcp refresh", Command::Refresh),
            ("/mcp trust remote", Command::Trust("remote".into())),
            ("/mcp deny remote", Command::Deny("remote".into())),
            ("/mcp test remote", Command::Test("remote".into())),
        ] {
            assert_eq!(command_from_prompt(&[json!({"type":"text","text":text})]).unwrap(), Some(expected));
        }
        for block in [
            json!({"type":"resource","resource":{"text":"/mcp trust remote"}}),
            json!({"type":"image","text":"/mcp trust remote"}),
            json!({"type":"text","text":"Explain /mcp trust remote"}),
            json!({"type":"text","text":"/mcp-helper"}),
        ] {
            assert!(command_from_prompt(&[block]).unwrap().is_none());
        }
        assert!(command_from_prompt(&[
            json!({"type":"text","text":"/mcp trust remote"}),
            json!({"type":"image","data":"anything"}),
        ]).is_err());
        assert!(command_from_prompt(&[json!({"type":"text","text":"/mcp trust remote\nmore"})]).is_err());
        assert!(command_from_prompt(&[json!({"type":"text","text":"/mcp trust remote extra"})]).is_err());
    }

    #[test]
    fn setup_is_inert_isolated_and_does_not_create_a_trust_record() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        let state = prepare(root.path(), &global, http_servers(root.path())).unwrap();
        assert_eq!(state.manager.list()[0].trust, "pending");
        assert_eq!(state.manager.list()[0].tools, 0);
        assert!(!state.started.load(Ordering::Acquire));
        assert!(!global.exists(), "inert preparation must not persist or spawn anything");
        let listing = status(&state);
        assert!(!listing.contains("PRIVATE-HEADER-VALUE"));
        assert!(!listing.contains("example.invalid"));
        assert!(!listing.contains(&root.path().display().to_string()));
    }

    #[test]
    fn live_reattach_preserves_existing_authority_and_rejects_replacement() {
        let root = tempfile::tempdir().unwrap();
        let mut servers = http_servers(root.path());
        let state = prepare(root.path(), root.path(), servers.clone()).unwrap();
        check_reattach(Some(&state), None, root.path()).unwrap();
        check_reattach(Some(&state), Some(&servers), root.path()).unwrap();
        assert!(check_reattach(Some(&state), Some(&[]), root.path()).is_err());
        servers[0].headers[0].1 = "ROTATED-VALUE".into();
        assert!(check_reattach(Some(&state), Some(&servers), root.path()).is_err());
        assert!(check_reattach(None, Some(&servers), root.path()).is_err());
        check_reattach(None, Some(&[]), root.path()).unwrap();
    }

    #[test]
    fn pre_cancelled_preparation_never_polls_its_side_effect() {
        let (abort, signal) = super::super::AbortHandle::new();
        abort.abort();
        let polled = AtomicBool::new(false);
        let cx = AgentCx::for_testing();
        let result = runtime().block_on(cancellable(&signal, &cx, async {
            polled.store(true, Ordering::Release);
        }));
        assert!(result.is_err());
        assert!(!polled.load(Ordering::Acquire));
    }

    #[test]
    fn operator_inspection_never_exposes_connection_secrets() {
        let root = tempfile::tempdir().unwrap();
        let state = prepare(root.path(), root.path(), http_servers(root.path())).unwrap();
        let text = runtime().block_on(execute(&state, Command::Inspect("remote".into())));
        assert!(text.contains("HTTP"));
        assert!(text.contains("1 headers"));
        assert!(!text.contains("PRIVATE-HEADER-VALUE"));
        assert!(!text.contains("https://"));
        let denied = runtime().block_on(execute(&state, Command::Deny("remote".into())));
        assert!(denied.contains("Denied"));
        assert_eq!(state.manager.list()[0].trust, "denied");
        assert_eq!(state.manager.list()[0].tools, 0);
    }

    #[test]
    fn command_advertisement_uses_acp_envelope() {
        let value: Value = serde_json::from_str(&commands_notification("session-1")).unwrap();
        assert_eq!(value["method"], "session/update");
        assert_eq!(value["params"]["sessionId"], "session-1");
        assert_eq!(value["params"]["update"]["sessionUpdate"], "available_commands_update");
        assert_eq!(value["params"]["update"]["availableCommands"][0]["name"], "mcp");
    }
}
