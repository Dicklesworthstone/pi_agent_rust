//! Real loopback transports and a local scripted provider exercise the ACP
//! path without calling a paid provider, spawning a fake MCP binary, or
//! bypassing the production permission callback.

use super::*;
use crate::acp::{
    AcpOptions, AcpPermissionClient, AcpSessionState, AbortHandle, PendingPermissionMap,
};
use crate::agent::{Agent, AgentConfig};
use crate::auth::{AuthCredential, AuthStorage};
use crate::compaction::ResolvedCompactionSettings;
use crate::model::{AssistantMessage, ContentBlock, Message, StopReason, TextContent, UserContent, UserMessage};
use crate::models::ModelRegistry;
use crate::provider::StreamOptions;
use crate::session::{Session, SessionStoreKind};
use crate::tools::ToolRegistry;
use asupersync::sync::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, AtomicUsize};

const TOOL: &str = "mcp__remote__echo";
const LITERAL_HEADER: &str = "$CMD:must-remain-a-literal";

struct HttpFixture {
    url: String,
    running: Arc<AtomicBool>,
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    records: Arc<StdMutex<Vec<Value>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

fn read_request(stream: &mut TcpStream) -> (String, Value) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() <= 32 * 1024, "bounded request headers");
        if bytes.ends_with(b"\r\n\r\n") { break bytes.len() }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
    }).unwrap_or(0);
    assert!(length <= 1024 * 1024);
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body).unwrap();
    (headers, if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap() })
}

fn reply(stream: &mut TcpStream, body: Option<Value>) {
    if let Some(body) = body {
        let bytes = body.to_string();
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{bytes}", bytes.len());
    } else {
        let _ = stream.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    }
}

impl HttpFixture {
    fn start(hang_initialize: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let running = Arc::new(AtomicBool::new(true));
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let records = Arc::new(StdMutex::new(Vec::new()));
        let worker_running = Arc::clone(&running);
        let worker_entered = Arc::clone(&entered);
        let worker_release = Arc::clone(&release);
        let worker_records = Arc::clone(&records);
        let worker = std::thread::spawn(move || {
            while worker_running.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2)); continue;
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                };
                let (headers, request) = read_request(&mut stream);
                assert!(headers.lines().any(|line| line.split_once(':').is_some_and(|(name, value)|
                    name.eq_ignore_ascii_case("x-acp-literal") && value.trim() == LITERAL_HEADER)));
                if headers.starts_with("GET /mcp ") {
                    // Native activation probes the optional receive channel.
                    // Decline it explicitly without inventing a JSON-RPC call.
                    stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                }
                worker_records.lock().unwrap().push(request.clone());
                let method = request["method"].as_str().unwrap_or("");
                if method == "initialize" {
                    worker_entered.store(true, Ordering::Release);
                    while hang_initialize && !worker_release.load(Ordering::Acquire)
                        && worker_running.load(Ordering::Acquire)
                    {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
                let result = match method {
                    "initialize" => Some(json!({
                        "protocolVersion": crate::mcp::transport::MCP_PROTOCOL_VERSION,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name":"acp-fixture","version":"1"},
                    })),
                    "tools/list" => Some(json!({"tools":[{
                        "name":"echo","description":"Echo from a real loopback MCP server",
                        "inputSchema":{"type":"object","properties":{"text":{"type":"string"}}},
                    }]})),
                    "tools/call" => Some(json!({"content":[{
                        "type":"text","text":format!("mcp-echo:{}", request["params"]["arguments"]["text"].as_str().unwrap()),
                    }]})),
                    "notifications/initialized" => None,
                    _ => panic!("unexpected fixture method {method:?}"),
                };
                reply(&mut stream, result.map(|result| json!({"jsonrpc":"2.0","id":request["id"],"result":result})));
            }
        });
        Self { url, running, entered, release, records, worker: Some(worker) }
    }

    fn config(&self) -> Value {
        json!([{"type":"http","name":"remote","url":self.url,
            "headers":[{"name":"X-Acp-Literal","value":LITERAL_HEADER}]}])
    }

    fn count(&self, method: &str) -> usize {
        self.records.lock().unwrap().iter().filter(|request| request["method"] == method).count()
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.release.store(true, Ordering::Release);
        self.running.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() { result.expect("fixture worker"); }
        }
    }
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new()
        .worker_threads(1).blocking_threads(1, 2).build().unwrap()
}

async fn bounded<F: Future>(work: F) -> F::Output {
    let cx = AgentCx::for_current_or_request();
    let time = cx.time();
    match futures::future::select(Box::pin(work), Box::pin(time.sleep(Duration::from_secs(15)))).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(((), pending)) => { drop(pending); panic!("ACP MCP watchdog expired"); }
    }
}

fn text(value: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new(value))]
}

fn permission_client(out: &SyncSender<String>, cx: &AgentCx) -> AcpPermissionClient {
    AcpPermissionClient {
        out_tx: out.clone(), pending: Arc::new(StdMutex::new(HashMap::new())),
        request_counter: Arc::new(AtomicU64::new(0)),
        timeout: Duration::from_secs(2), cx: cx.clone(),
    }
}

/// Loopback OpenAI-compatible provider: first call asks for the mounted MCP
/// tool and second completes. This exercises AgentSession's actual tool loop.
struct ProviderFixture {
    url: String,
    turns: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ProviderFixture {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let turns = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&turns);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(12);
            while observed.load(Ordering::Acquire) < 2 && !worker_stop.load(Ordering::Acquire) {
                assert!(std::time::Instant::now() < deadline, "provider fixture timed out");
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2)); continue;
                    }
                    Err(error) => panic!("provider fixture accept: {error}"),
                };
                let (_, request) = read_request(&mut stream);
                let index = observed.fetch_add(1, Ordering::AcqRel);
                assert!(request["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == TOOL));
                let delta = if index == 0 {
                    json!({"role":"assistant","tool_calls":[{"index":0,"id":"acp-call-1",
                        "type":"function","function":{"name":TOOL,"arguments":"{\"text\":\"hello\"}"}}]})
                } else {
                    assert!(request["messages"].as_array().unwrap().iter().any(|message| message["role"] == "tool"));
                    json!({"role":"assistant","content":"finished"})
                };
                let body = format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    json!({"id":"fixture","object":"chat.completion.chunk","model":"gpt-4o",
                        "choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
                    json!({"id":"fixture","object":"chat.completion.chunk","model":"gpt-4o",
                        "choices":[{"index":0,"delta":{},"finish_reason":if index == 0 {"tool_calls"} else {"stop"}}]}));
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self { url, turns, stop, worker: Some(worker) }
    }
}

impl Drop for ProviderFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() { result.expect("provider worker"); }
        }
    }
}

fn live_state(
    cwd: &Path, root: &Path, server: &HttpFixture, provider_url: &str,
    client: &AcpPermissionClient, handle: asupersync::runtime::RuntimeHandle,
) -> (String, Arc<Mutex<AcpSessionState>>, Arc<SessionMcp>) {
    let mut session = Session::in_memory();
    session.header.cwd = cwd.display().to_string();
    let id = session.header.id.clone();
    let mut model = crate::models::ad_hoc_model_entry("openai", "gpt-4o").unwrap();
    model.model.api = "openai-completions".into();
    model.model.base_url = provider_url.into();
    let provider = crate::providers::create_provider(&model, None).unwrap();
    let agent = Agent::new(provider, ToolRegistry::new(&[], cwd, None), AgentConfig {
        stream_options: StreamOptions { api_key: Some("local-fixture-key".into()), ..StreamOptions::default() },
        tool_approval: Some(client.handler_for_session(id.clone())),
        ..AgentConfig::default()
    });
    let mut agent_session = AgentSession::new(agent, Arc::new(Mutex::new(session)), false,
        ResolvedCompactionSettings { enabled: false, ..ResolvedCompactionSettings::default() })
        .with_runtime_handle(handle);
    let servers = crate::mcp::config::parse_acp_servers(&json!({"mcpServers":server.config()}), cwd).unwrap().unwrap();
    let mcp = prepare(cwd, root, servers).unwrap();
    mount(&mut agent_session, &mcp);
    let state = Arc::new(Mutex::new(AcpSessionState {
        agent_session: Some(crate::sdk::AgentSessionHandle::from_session_with_listeners(
            agent_session, crate::sdk::EventListeners::default(),
        )),
        cwd: cwd.into(), mcp: Some(Arc::clone(&mcp)),
    }));
    (id, state, mcp)
}

async fn read_permission(
    rx: &std::sync::mpsc::Receiver<String>, out: &mut Vec<Value>,
    pending: &PendingPermissionMap, cx: &AgentCx, approve: bool,
) {
    loop {
        while let Ok(line) = rx.try_recv() {
            let value: Value = serde_json::from_str(&line).unwrap();
            let is_request = value["method"] == "session/request_permission";
            out.push(value.clone());
            if is_request {
                assert_eq!(value["params"]["toolCall"]["title"], TOOL);
                assert!(crate::acp::route_permission_response(&json!({
                    "jsonrpc":"2.0","id":value["id"],"result":{"outcome":{
                        "outcome":"selected","optionId":if approve {"allow-once"} else {"reject-once"},
                    }},
                }), pending, cx));
                return;
            }
        }
        cx.time().sleep(Duration::from_millis(2)).await;
    }
}

#[test]
fn real_mcp_tool_round_trip_requires_editor_permission_and_preserves_literal_headers() {
    for approve in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let server = HttpFixture::start(false);
        let provider = ProviderFixture::start();
        let runtime = runtime();
        let handle = runtime.handle();
        runtime.block_on(bounded(async {
            let cx = AgentCx::for_current_or_request();
            let (tx, rx) = std::sync::mpsc::sync_channel(128);
            let client = permission_client(&tx, &cx);
            let (id, state, mcp) = live_state(temp.path(), &temp.path().join("global"), &server, &provider.url, &client, handle);
            let (_, signal) = AbortHandle::new();
            let reason = crate::acp::run_prompt(Arc::clone(&state), text("/mcp trust remote"),
                Some(Command::Trust("remote".into())), signal, tx.clone(), id.clone(), cx.clone()).await;
            assert_eq!(reason, "end_turn");
            assert_eq!(provider.turns.load(Ordering::Acquire), 0, "host commands do not start provider turns");
            assert_eq!(server.count("tools/call"), 0);
            assert!(state.lock(&cx).await.unwrap().agent_session.as_ref().unwrap().has_tool(TOOL));
            let mut seen = rx.try_iter().map(|line| serde_json::from_str::<Value>(&line).unwrap()).collect::<Vec<_>>();
            let (_, signal) = AbortHandle::new();
            let prompt = crate::acp::run_prompt(Arc::clone(&state), text("Call echo"), None,
                signal, tx.clone(), id.clone(), cx.clone());
            let (reason, ()) = futures::join!(prompt, read_permission(&rx, &mut seen, &client.pending, &cx, approve));
            assert_eq!(reason, "end_turn");
            seen.extend(rx.try_iter().map(|line| serde_json::from_str::<Value>(&line).unwrap()));
            assert_eq!(server.count("tools/call"), usize::from(approve));
            assert!(seen.iter().any(|value| value["params"]["update"]["toolCallId"] == "acp-call-1"
                && value["params"]["update"]["status"] == if approve {"completed"} else {"failed"}));
            assert!(!serde_json::to_string(&seen).unwrap().contains(LITERAL_HEADER));
            assert!(client.pending.lock().unwrap().is_empty());

            let (_, signal) = AbortHandle::new();
            crate::acp::run_prompt(Arc::clone(&state), text("/mcp deny remote"),
                Some(Command::Deny("remote".into())), signal, tx, id, cx.clone()).await;
            let before = server.count("tools/call");
            let error = mcp.manager.call_tool("remote", "echo", json!({"text":"blocked"})).await.unwrap_err();
            assert!(error.to_string().contains("MCP_TRUST_DENIED"));
            assert_eq!(server.count("tools/call"), before);
            mcp.manager.shutdown_all().await;
        }));
        assert_eq!(provider.turns.load(Ordering::Acquire), 2);
    }
}

#[test]
fn cancelling_mcp_setup_restores_the_agent_and_busy_session_shutdown_reaches_the_manager() {
    let temp = tempfile::tempdir().unwrap();
    let server = HttpFixture::start(true);
    let runtime = runtime();
    let handle = runtime.handle();
    runtime.block_on(bounded(async {
        let cx = AgentCx::for_current_or_request();
        let (tx, _rx) = std::sync::mpsc::sync_channel(32);
        let client = permission_client(&tx, &cx);
        let (id, state, mcp) = live_state(temp.path(), &temp.path().join("global"), &server,
            "http://127.0.0.1:1/v1", &client, handle);
        let (abort, signal) = AbortHandle::new();
        let prompt = crate::acp::run_prompt(Arc::clone(&state), text("/mcp trust remote"),
            Some(Command::Trust("remote".into())), signal, tx, id.clone(), cx.clone());
        let cancel = async {
            while !server.entered.load(Ordering::Acquire) {
                cx.time().sleep(Duration::from_millis(2)).await;
            }
            assert!(state.lock(&cx).await.unwrap().agent_session.is_none());
            abort.abort();
        };
        let (reason, ()) = futures::join!(prompt, cancel);
        assert_eq!(reason, "cancelled");
        assert!(state.lock(&cx).await.unwrap().agent_session.is_some());
        assert_eq!(server.count("tools/call"), 0);
        // Model the same ownership state as an active turn: agent temporarily
        // taken, manager still reachable from the session for dispatcher exit.
        let agent = state.lock(&cx).await.unwrap().agent_session.take().unwrap();
        let sessions = Arc::new(Mutex::new(HashMap::from([(id, state)])));
        shutdown(&sessions).await;
        let error = mcp.manager.call_tool("remote", "echo", json!({})).await.unwrap_err();
        assert!(error.to_string().contains("MCP_MANAGER_SHUTDOWN"));
        drop(agent);
        server.release.store(true, Ordering::Release);
    }));
}

fn options(root: &Path, handle: asupersync::runtime::RuntimeHandle) -> AcpOptions {
    let mut auth = AuthStorage::load(root.join("auth.json")).unwrap();
    auth.set("anthropic", AuthCredential::ApiKey { key: "not-live".into() });
    let model_registry = ModelRegistry::load(&auth, None);
    let model = model_registry.find("anthropic", "claude-sonnet-4-5").unwrap();
    AcpOptions {
        config: crate::config::Config::default(), available_models: vec![model],
        model_registry, auth, runtime_handle: handle,
        session_dir: Some(root.into()), skills_prompt: None,
    }
}

#[test]
fn new_and_restored_sessions_accept_mcp_definitions_without_connecting_or_replaying_trust() {
    let root = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let server = HttpFixture::start(false);
    let runtime = runtime();
    let handle = runtime.handle();
    runtime.block_on(bounded(async {
        let cwd = project.path().canonicalize().unwrap();
        let options = options(root.path(), handle);
        let cx = AgentCx::for_current_or_request();
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let client = permission_client(&tx, &cx);
        let params = json!({"cwd":cwd,"mcpServers":server.config()});
        let (_, new) = crate::acp::handle_session_new(&params, &options, Some(&client)).unwrap();
        assert!(new.mcp.is_some());
        assert_eq!(server.count("initialize"), 0);

        let mut saved = Session::create_with_dir_and_store(Some(root.path().into()), SessionStoreKind::Jsonl);
        saved.header.cwd = cwd.display().to_string();
        saved.set_model_header(Some("anthropic".into()), Some("claude-sonnet-4-5".into()), Some("off".into()));
        saved.append_model_message(Message::User(UserMessage {
            content: UserContent::Text("/mcp trust remote".into()), timestamp: 1,
        }));
        saved.append_model_message(Message::assistant(AssistantMessage {
            content: text("historical response"), stop_reason: StopReason::Stop,
            ..AssistantMessage::default()
        }));
        saved.save().await.unwrap();
        let before = std::fs::read(saved.path.as_ref().unwrap()).unwrap();
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let request = json!({"sessionId":saved.header.id,"cwd":cwd,"mcpServers":server.config()});
        let result = history::load(&request, &options, &client, &sessions, &cx, &tx, true).await.unwrap();
        assert_eq!(result["sessionId"], saved.header.id);
        let notifications = rx.try_iter().map(|line| serde_json::from_str::<Value>(&line).unwrap()).collect::<Vec<_>>();
        assert!(notifications.iter().any(|value| value["params"]["update"]["content"]["text"] == "/mcp trust remote"));
        assert!(notifications.iter().any(|value| value["params"]["update"]["sessionUpdate"] == "available_commands_update"));
        assert_eq!(server.count("initialize"), 0, "replay must never execute historic operator commands");
        let state = sessions.lock(&cx).await.unwrap().get(&saved.header.id).unwrap().clone();
        let mcp = state.lock(&cx).await.unwrap().mcp.clone().unwrap();
        assert_eq!(mcp.manager.list()[0].trust, "pending");
        history::load(&request, &options, &client, &sessions, &cx, &tx, false).await.unwrap();
        assert!(rx.try_iter().all(|line| !line.contains("historical response")));
        let mut changed = request;
        changed["mcpServers"][0]["headers"][0]["value"] = json!("changed-credential");
        assert_eq!(history::load(&changed, &options, &client, &sessions, &cx, &tx, false).await.unwrap_err().code, crate::acp::INVALID_PARAMS);
        assert_eq!(std::fs::read(saved.path.as_ref().unwrap()).unwrap(), before);
        shutdown(&sessions).await;
    }));
}

#[test]
fn dispatcher_rejects_invalid_mcp_definition_before_creating_a_session() {
    let root = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let handle = runtime.handle();
    runtime.block_on(bounded(async {
        let options = options(root.path(), handle);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel(8);
        let (out_tx, out_rx) = std::sync::mpsc::sync_channel(16);
        for request in [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{
                "cwd":project.path(),"mcpServers":[{"name":"bad","command":"x","url":"https://example.invalid"}],
            }}),
            json!({"jsonrpc":"2.0","id":3,"method":"session/list","params":{}}),
            json!({"jsonrpc":"2.0","method":"exit"}),
        ] { in_tx.try_send(request.to_string()).unwrap(); }
        crate::acp::run(options, in_rx, out_tx).await.unwrap();
        let results = out_rx.try_iter().map(|line| serde_json::from_str::<Value>(&line).unwrap()).collect::<Vec<_>>();
        assert_eq!(results[0]["result"]["agentCapabilities"]["mcpCapabilities"]["http"], true);
        assert_eq!(results[1]["error"]["code"], crate::acp::INVALID_PARAMS);
        assert!(results[2]["result"]["sessions"].as_array().unwrap().is_empty());
    }));
}
