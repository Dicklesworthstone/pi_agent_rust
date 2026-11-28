//! ACP turns through the real configured session, provider HTTP/SSE transport,
//! durable recovery driver and editor notification adapter. Only the remote
//! provider response is scripted; no paid provider is contacted.

use super::*;
use crate::model::{ImageContent, MediaContent, Message, StopReason, TextContent, UserContent};
use asupersync::runtime::RuntimeBuilder;
use asupersync::runtime::reactor::create_reactor;
use std::future::Future;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::Instant;

const PRIMARY_PROVIDER: &str = "acp-recovery-primary";
const FALLBACK_PROVIDER: &str = "acp-recovery-backup";
const PRIMARY_MODEL: &str = "editor-primary";
const FALLBACK_MODEL: &str = "editor-backup";
const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+j5mEAAAAASUVORK5CYII=";

struct ProviderFixture {
    url: String,
    requests: Arc<StdMutex<Vec<(String, Value)>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ProviderFixture {
    fn new(statuses: Vec<u16>) -> Self {
        Self::with_tool(statuses, None)
    }

    fn with_tool(statuses: Vec<u16>, tool: Option<&'static str>) -> Self {
        let tools = statuses
            .iter()
            .map(|_| tool.map(|name| (name, json!({}))))
            .collect();
        Self::with_replies(statuses, tools)
    }

    fn with_replies(statuses: Vec<u16>, tools: Vec<Option<(&'static str, Value)>>) -> Self {
        Self::with_paused_replies(statuses, tools, None)
    }

    fn with_paused_replies(
        statuses: Vec<u16>,
        tools: Vec<Option<(&'static str, Value)>>,
        response_gate: Option<Arc<AtomicBool>>,
    ) -> Self {
        assert_eq!(statuses.len(), tools.len());
        let listener = TcpListener::bind("127.0.0.1:0").expect("provider listener");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let worker = std::thread::spawn(move || {
            for (reply_index, (status, tool)) in statuses.into_iter().zip(tools).enumerate() {
                let deadline = Instant::now() + Duration::from_secs(60);
                let mut stream = loop {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "provider request timed out");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("provider accept: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let (headers, request) = read_request(&mut stream, deadline);
                let model = request["model"].as_str().unwrap().to_string();
                captured.lock().unwrap().push((headers, request));
                if let Some(gate) = response_gate.as_ref() {
                    while !gate.load(Ordering::SeqCst) {
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        assert!(Instant::now() < deadline, "paused provider reply timed out");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                let (content_type, body) = if status == 200 {
                    let delta = tool.as_ref().map_or_else(
                        || {
                            json!({
                                "role": "assistant",
                                "content": "Recovered editor answer",
                            })
                        },
                        |(tool, arguments)| {
                            json!({
                                "role": "assistant",
                                "tool_calls": [{
                                    "index": 0,
                                    "id": format!("acp-tool-{reply_index}"),
                                    "type": "function",
                                    "function": { "name": tool, "arguments": arguments.to_string() },
                                }],
                            })
                        },
                    );
                    let finish_reason = if tool.is_some() { "tool_calls" } else { "stop" };
                    let start = json!({
                        "id": "acp-completion", "object": "chat.completion.chunk",
                        "model": model,
                        "choices": [{"index": 0, "delta": delta, "finish_reason": null}],
                    });
                    let end = json!({
                        "id": "acp-completion",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}],
                    });
                    (
                        "text/event-stream",
                        format!("data: {start}\n\ndata: {end}\n\ndata: [DONE]\n\n"),
                    )
                } else {
                    ("application/json", json!({"error": {
                        "message": "PRIVATE-PROMPT PRIVATE-CREDENTIAL",
                        "type": if status == 401 { "authentication_error" } else { "server_error" },
                    }}).to_string())
                };
                write!(stream,
                    "HTTP/1.1 {status} Fixture\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                ).unwrap();
                stream.flush().unwrap();
            }
        });
        Self {
            url,
            requests,
            stopped,
            worker: Some(worker),
        }
    }

    fn finish(&mut self, expected: usize) -> Vec<(String, Value)> {
        self.stopped.store(true, Ordering::SeqCst);
        self.worker
            .take()
            .unwrap()
            .join()
            .expect("provider fixture completed");
        let requests = self.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), expected, "exact provider admission count");
        requests
    }
}

impl Drop for ProviderFixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_request(stream: &mut TcpStream, deadline: Instant) -> (String, Value) {
    const LIMIT: usize = 512 * 1024;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        assert!(Instant::now() < deadline, "provider body deadline");
        match stream.read(&mut buffer) {
            Ok(0) => panic!("provider request ended early"),
            Ok(read) => bytes.extend_from_slice(&buffer[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) => panic!("provider request read: {error}"),
        }
        assert!(bytes.len() <= LIMIT, "bounded provider request");
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .expect("content-length");
            assert!(end + 4 + length <= LIMIT);
            if bytes.len() >= end + 4 + length {
                assert!(
                    headers
                        .lines()
                        .next()
                        .unwrap()
                        .contains("/chat/completions")
                );
                return (
                    headers.to_string(),
                    serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap(),
                );
            }
        }
    }
}

fn runtime() -> asupersync::runtime::Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("reactor"))
        .build()
        .expect("runtime")
}

fn options(root: &Path, url: &str, runtime: RuntimeHandle, retries: u32) -> AcpOptions {
    let entry = |provider: &str, model: &str, reasoning: bool, max_tokens: u32| {
        let mut entry = crate::models::ad_hoc_model_entry("openai", model).unwrap();
        entry.model.provider = provider.to_string();
        entry.model.api = "openai-completions".to_string();
        entry.model.base_url = url.to_string();
        entry.model.input = vec![
            crate::provider::InputType::Text,
            crate::provider::InputType::Image,
        ];
        entry.model.reasoning = reasoning;
        entry.model.context_window = if reasoning { 128_000 } else { 32_000 };
        entry.model.max_tokens = max_tokens;
        entry.api_key = Some(format!("{model}-fixture-key"));
        entry
            .headers
            .insert("x-acp-model".to_string(), model.to_string());
        entry
    };
    let models = vec![
        entry(PRIMARY_PROVIDER, PRIMARY_MODEL, true, 2_048),
        entry(FALLBACK_PROVIDER, FALLBACK_MODEL, false, 512),
    ];
    AcpOptions {
        settings_source: None,
        workspace_trust_guard: None,
        launch: AcpLaunchOptions::default(),
        config: Config {
            default_provider: Some(PRIMARY_PROVIDER.to_string()),
            default_model: Some(PRIMARY_MODEL.to_string()),
            default_thinking_level: Some("high".to_string()),
            compaction: Some(crate::config::CompactionSettings {
                enabled: Some(false),
                ..crate::config::CompactionSettings::default()
            }),
            retry: Some(crate::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(retries),
                base_delay_ms: Some(0),
                max_delay_ms: Some(0),
                fallback_chains: Some(HashMap::from([(
                    "default".to_string(),
                    vec![format!("{FALLBACK_PROVIDER}/{FALLBACK_MODEL}")],
                )])),
                failover_cooldown_secs: Some(0),
                max_failovers_per_turn: Some(2),
                ..crate::config::RetrySettings::default()
            }),
            ..Config::default()
        },
        available_models: models.clone(),
        model_registry: ModelRegistry::from_entries_for_tests(models),
        auth: AuthStorage::load(root.join("auth.json")).unwrap(),
        oauth_refresh_failures: Vec::new(),
        runtime_handle: runtime,
        session_dir: Some(root.to_path_buf()),
        skills_prompt: None,
    }
}

fn new_state(root: &Path, options: &AcpOptions) -> (String, Arc<Mutex<AcpSessionState>>) {
    let (id, state) =
        handle_session_new(&json!({"cwd": root, "mcpServers": []}), options, None).unwrap();
    (id, Arc::new(Mutex::new(state)))
}

fn text() -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new("Keep the editor turn"))]
}

fn native_content() -> Vec<ContentBlock> {
    vec![
        ContentBlock::Text(TextContent::new("  compare\n")),
        ContentBlock::Media(MediaContent {
            data: "YQ==".to_string(),
            mime_type: "audio/wav".to_string(),
            name: Some("voice.wav".to_string()),
        }),
        ContentBlock::Image(ImageContent {
            data: PNG.to_string(),
            mime_type: "image/png".to_string(),
        }),
        ContentBlock::Text(TextContent::new("this frame")),
    ]
}

async fn prompt(
    state: &Arc<Mutex<AcpSessionState>>,
    id: &str,
    content: Vec<ContentBlock>,
    signal: AbortSignal,
) -> (&'static str, Vec<Value>) {
    // This helper receives native test input, never decoded ACP resources.
    let authored_text = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let (tx, rx) = std::sync::mpsc::sync_channel(128);
    let tx = AcpOutput::new(tx);
    let reason = Box::pin(run_prompt(
        Arc::clone(state),
        AcpPrompt {
            content,
            authored_text,
        },
        None,
        signal,
        tx.clone(),
        id.to_string(),
        AgentCx::for_current_or_request(),
    ))
    .await;
    // Mirror the dispatcher: the response must follow recovery notices,
    // committed selection updates and the session's final persistence step.
    tx.send(json_rpc_ok(json!(7), json!({"stopReason": reason})))
        .unwrap();
    (
        reason,
        rx.try_iter()
            .map(|line| serde_json::from_str(&line).unwrap())
            .collect(),
    )
}

fn events(state: &Arc<Mutex<AcpSessionState>>) -> Arc<StdMutex<Vec<AgentEvent>>> {
    let recorded = Arc::new(StdMutex::new(Vec::new()));
    let captured = Arc::clone(&recorded);
    state
        .try_lock()
        .unwrap()
        .agent_session
        .as_ref()
        .unwrap()
        .subscribe(move |event| {
            captured.lock().unwrap().push(event);
        });
    recorded
}

fn session_store(state: &Arc<Mutex<AcpSessionState>>) -> Arc<Mutex<Session>> {
    state
        .try_lock()
        .unwrap()
        .agent_session
        .as_ref()
        .unwrap()
        .session_store()
}

async fn reopen(state: &Arc<Mutex<AcpSessionState>>) -> Session {
    let path = session_store(state)
        .try_lock()
        .unwrap()
        .path
        .clone()
        .expect("durable session path");
    Session::open(path.to_str().unwrap()).await.unwrap()
}

fn assert_no_private_diagnostics(updates: &[Value]) {
    let rendered = serde_json::to_string(updates).unwrap();
    assert!(!rendered.contains("PRIVATE-PROMPT"));
    assert!(!rendered.contains("PRIVATE-CREDENTIAL"));
    assert!(!rendered.contains("fixture-key"));
}

fn configuration_updates(updates: &[Value]) -> Vec<&Value> {
    updates
        .iter()
        .filter(|update| update["params"]["update"]["sessionUpdate"] == "config_option_update")
        .map(|update| &update["params"]["update"]["configOptions"])
        .collect()
}

const DISCOVERY_FIRST: &str = "acp-discovery-first";
const DISCOVERY_SECOND: &str = "acp-discovery-second";
const DISCOVERY_LOCKED: &str = "acp-discovery-locked";
const DISCOVERY_LOCAL: &str = "acp-discovery-local";
const DISCOVERY_BYO_MODEL: &str = "team/deepseek-flash?provider=sail&mode=balanced";

/// Load the fixture through models.json before isolating its providers from the
/// built-in catalog. No ambient credential or paid provider is involved.
fn discovery_options(root: &Path, url: &str, runtime: RuntimeHandle) -> AcpOptions {
    let model = |id: &str, name: &str, reasoning: bool| {
        json!({
            "id": id, "name": name, "reasoning": reasoning,
            "input": ["text"], "contextWindow": 32_000, "maxTokens": 512,
        })
    };
    let route = |models: Vec<Value>, auth_header: bool| {
        json!({
            "api": "openai-completions", "baseUrl": url,
            "authHeader": auth_header, "models": models,
        })
    };
    let path = root.join("models.json");
    std::fs::write(
        &path,
        json!({"providers": {
            DISCOVERY_FIRST: route(vec![model("shared", "First shared", true)], true),
            DISCOVERY_SECOND: route(vec![model("shared", "Second shared", true)], true),
            "dalsh": route(vec![model(DISCOVERY_BYO_MODEL, "DeepSeek [Sail]", true)], true),
            DISCOVERY_LOCKED: route(vec![model("private", "Missing credential", true)], true),
            DISCOVERY_LOCAL: route(vec![model("local", "Keyless local", false)], false),
        }})
        .to_string(),
    )
    .unwrap();
    let loaded = ModelRegistry::load_with_credential_resolver(Some(path), |provider| {
        matches!(provider, DISCOVERY_FIRST | DISCOVERY_SECOND | "dalsh")
            .then(|| format!("{provider}-fixture-key"))
    });
    assert!(loaded.error().is_none(), "{:?}", loaded.error());
    let registry = ModelRegistry::from_entries_for_tests(
        [
            (DISCOVERY_FIRST, "shared"),
            (DISCOVERY_SECOND, "shared"),
            ("dalsh", DISCOVERY_BYO_MODEL),
            (DISCOVERY_LOCKED, "private"),
            (DISCOVERY_LOCAL, "local"),
        ]
        .into_iter()
        .map(|(provider, id)| loaded.find(provider, id).expect("registered fixture model"))
        .collect(),
    );
    let mut options = options(root, url, runtime, 0);
    options.available_models = registry.get_available();
    options.model_registry = registry;
    options.auth = AuthStorage::empty_at(root.join("isolated-auth.json"));
    options.config.default_provider = Some(DISCOVERY_FIRST.to_string());
    options.config.default_model = Some("shared".to_string());
    options.config.retry = None;
    options.launch.no_tools = true;
    options.launch.no_mcp = true;
    options.launch.no_context_files = true;
    options.launch.system_prompt = Some("ACP model discovery fixture".to_string());
    options.session_dir = Some(root.join("sessions"));
    options
}

/// A small editor peer for the production JSON-RPC dispatcher. Requests are
/// serialized on the input channel; responses and notifications are decoded
/// independently of the server's response-building helpers.
async fn discovery_request(
    input: &asupersync::channel::mpsc::Sender<String>,
    output: &std::sync::mpsc::Receiver<String>,
    transcript: &mut Vec<Value>,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    input
        .try_send(
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        )
        .unwrap();
    loop {
        match output.try_recv() {
            Ok(line) => {
                let message: Value = serde_json::from_str(&line).expect("valid ACP frame");
                assert_eq!(message["jsonrpc"], "2.0");
                let response = message.get("id") == Some(&json!(id));
                transcript.push(message.clone());
                if response {
                    return message;
                }
                assert_eq!(
                    message["method"], "session/update",
                    "unexpected server request"
                );
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                sleep(wall_now(), Duration::from_millis(1)).await;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("ACP stopped before replying to {method}");
            }
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveredModelState {
    current_model_id: String,
    available_models: Vec<DiscoveredModel>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveredModel {
    model_id: String,
    name: String,
}

fn assert_discovery_state(result: &Value, current: &str, expected: &[&str]) {
    // This is the legacy ACP client's object contract, not Pi's former flat
    // array of {id, name, provider}. A valid configOptions field cannot repair
    // an incompatible sibling field for a client decoding the whole response.
    let models: DiscoveredModelState =
        serde_json::from_value(result["models"].clone()).expect("ACP model state object");
    assert_eq!(models.current_model_id, current);
    let config = result["configOptions"].as_array().expect("config options");
    let selector = config
        .iter()
        .find(|option| option["id"] == "model")
        .unwrap();
    assert_eq!(selector["category"], "model");
    assert_eq!(selector["type"], "select");
    assert_eq!(selector["currentValue"], current);
    let choices = selector["options"].as_array().unwrap();
    assert_eq!(choices.len(), models.available_models.len());
    for entry in &models.available_models {
        let choice = choices
            .iter()
            .find(|choice| choice["value"] == entry.model_id)
            .unwrap();
        assert!(!entry.name.is_empty());
        assert_eq!(choice["name"], entry.name);
    }
    let mut ids: Vec<_> = models
        .available_models
        .iter()
        .map(|entry| entry.model_id.as_str())
        .collect();
    ids.sort_unstable();
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    assert_eq!(ids, expected);
    assert!(ids.contains(&current), "active model must be selectable");
    assert_eq!(result["modes"]["currentModeId"], "code");
    assert_eq!(result["modes"]["availableModes"][1]["id"], "plan");
    let mode = config.iter().find(|option| option["id"] == "mode").unwrap();
    assert_eq!(mode["category"], "mode");
    assert_eq!(mode["currentValue"], "code");
}

fn assert_discovery_update(transcript: &[Value], id: u64, session: &str, current: &str) {
    let response = transcript
        .iter()
        .position(|frame| frame.get("id") == Some(&json!(id)))
        .unwrap();
    let update = &transcript[response.checked_sub(1).expect("update before response")];
    assert_eq!(update["method"], "session/update");
    assert_eq!(update["params"]["sessionId"], session);
    assert_eq!(
        update["params"]["update"]["sessionUpdate"],
        "config_option_update"
    );
    let options = update["params"]["update"]["configOptions"]
        .as_array()
        .unwrap();
    assert_eq!(
        options
            .iter()
            .find(|option| option["id"] == "model")
            .unwrap()["currentValue"],
        current
    );
    assert!(options.iter().any(|option| option["id"] == "thought_level"));
}

async fn approved_fork_prompt(
    input: &asupersync::channel::mpsc::Sender<String>,
    output: &std::sync::mpsc::Receiver<String>,
    transcript: &mut Vec<Value>,
    id: u64,
    session: &str,
    prompt: &str,
) {
    input
        .try_send(
            json!({
                "jsonrpc": "2.0", "id": id, "method": "session/prompt",
                "params": { "sessionId": session, "prompt": [{ "type": "text", "text": prompt }] },
            })
            .to_string(),
        )
        .unwrap();
    let mut permissions = 0;
    loop {
        let message = match output.try_recv() {
            Ok(line) => serde_json::from_str::<Value>(&line).unwrap(),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                sleep(wall_now(), Duration::from_millis(1)).await;
                continue;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("ACP fork prompt disconnected")
            }
        };
        transcript.push(message.clone());
        if message["method"] == "session/request_permission" {
            assert_eq!(
                message["params"]["sessionId"], session,
                "permission belongs to this conversation"
            );
            input.try_send(json!({
                "jsonrpc": "2.0", "id": message["id"],
                "result": { "outcome": { "outcome": "selected", "optionId": ACP_PERMISSION_ALLOW_ONCE } },
            }).to_string()).unwrap();
            permissions += 1;
        } else if message.get("id") == Some(&json!(id)) {
            assert_eq!(message["result"]["stopReason"], ACP_STOP_REASON_END_TURN);
            assert_eq!(
                permissions, 1,
                "the new session must consult its own editor permission handler"
            );
            return;
        } else {
            assert_eq!(message["method"], "session/update");
            assert_eq!(message["params"]["sessionId"], session);
        }
    }
}

#[test]
fn dispatcher_fork_saves_before_ack_and_both_sessions_continue_with_independent_permissions() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let sessions_root = root.path().join("sessions");
    let mut provider = ProviderFixture::with_replies(
        vec![200; 5],
        vec![
            None,
            Some(("current_time", json!({}))),
            None,
            Some(("current_time", json!({}))),
            None,
        ],
    );
    let runtime = runtime();
    let (source_id, fork_id, source_path, fork_path) = runtime.block_on(bounded_launch_controls(async {
        let mut options = options(root.path(), &provider.url, runtime.handle(), 0);
        options.session_dir = Some(sessions_root.clone());
        options.launch.tools = Some("current_time".into());
        options.launch.no_mcp = true;
        options.launch.no_context_files = true;
        let (input, incoming) = asupersync::channel::mpsc::channel(16);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let mut transcript = Vec::new();
        let editor = async {
            let initialized = discovery_request(&input, &outgoing, &mut transcript, 1, "initialize", json!({ "protocolVersion": 1 })).await;
            assert_eq!(initialized["result"]["agentCapabilities"]["sessionCapabilities"]["fork"], json!({}));
            let created = discovery_request(&input, &outgoing, &mut transcript, 2, "session/new", json!({ "cwd": cwd })).await;
            let source_id = created["result"]["sessionId"].as_str().unwrap().to_string();
            let original = discovery_request(&input, &outgoing, &mut transcript, 3, "session/prompt", json!({
                "sessionId": source_id, "prompt": [{ "type": "text", "text": "SOURCE-CONTEXT-ONLY" }],
            })).await;
            assert_eq!(original["result"]["stopReason"], ACP_STOP_REASON_END_TURN);
            let selected = discovery_request(&input, &outgoing, &mut transcript, 4, "session/set_model", json!({
                "sessionId": source_id, "modelId": format!("{FALLBACK_PROVIDER}/{FALLBACK_MODEL}"),
            })).await;
            assert!(selected.get("error").is_none(), "{selected}");
            let before_fork = transcript.len();
            let forked = discovery_request(&input, &outgoing, &mut transcript, 5, "session/fork", json!({
                "sessionId": source_id, "cwd": cwd,
            })).await;
            assert!(forked.get("error").is_none(), "{forked}");
            assert_eq!(transcript.len(), before_fork + 2, "fork publishes native commands without replaying history");
            assert_eq!(transcript[before_fork]["params"]["update"]["sessionUpdate"], "available_commands_update");
            assert_eq!(provider.requests.lock().unwrap().len(), 1, "fork does not run the provider");
            let fork_id = forked["result"]["sessionId"].as_str().unwrap().to_string();
            assert_ne!(fork_id, source_id);
            assert_eq!(forked["result"]["configOptions"][0]["currentValue"], format!("{FALLBACK_PROVIDER}/{FALLBACK_MODEL}"));
            assert_eq!(forked["result"]["configOptions"][1]["currentValue"], "off");
            assert!(forked["result"].get("resumed").is_none());
            let index = crate::session_index::SessionIndex::for_sessions_root(&sessions_root);
            index.reindex_all().unwrap();
            let rows = index.list_sessions(None).unwrap();
            assert_eq!(rows.len(), 2);
            let source_path = rows.iter().find(|row| row.id == source_id).unwrap().path.clone();
            let fork_path = rows.iter().find(|row| row.id == fork_id).unwrap().path.clone();
            let saved = Session::open(&fork_path).await.unwrap();
            assert_eq!(saved.header.id, fork_id);
            assert_eq!(saved.to_messages_for_current_path().len(), 2, "complete selected leaf is durable at ACK");
            let source_before_fork_prompt = std::fs::read(&source_path).unwrap();
            approved_fork_prompt(&input, &outgoing, &mut transcript, 6, &fork_id, "FORK-CONTINUATION-ONLY").await;
            assert_eq!(std::fs::read(&source_path).unwrap(), source_before_fork_prompt);
            approved_fork_prompt(&input, &outgoing, &mut transcript, 7, &source_id, "SOURCE-CONTINUATION-ONLY").await;
            input.try_send(json!({ "jsonrpc": "2.0", "method": "exit" }).to_string()).unwrap();
            (source_id, fork_id, source_path, fork_path)
        };
        let (served, result) = futures::join!(Box::pin(run(options, incoming, AcpOutput::new(output))), editor);
        served.unwrap();
        result
    }));
    let requests = provider.finish(5);
    for index in [1, 2] {
        let context = requests[index].1["messages"].to_string();
        assert!(context.contains("SOURCE-CONTEXT-ONLY"));
        assert!(context.contains("FORK-CONTINUATION-ONLY"));
        assert!(!context.contains("SOURCE-CONTINUATION-ONLY"));
        assert_eq!(requests[index].1["model"], FALLBACK_MODEL);
    }
    for index in [3, 4] {
        let context = requests[index].1["messages"].to_string();
        assert!(context.contains("SOURCE-CONTEXT-ONLY"));
        assert!(context.contains("SOURCE-CONTINUATION-ONLY"));
        assert!(!context.contains("FORK-CONTINUATION-ONLY"));
    }
    runtime.block_on(bounded_launch_controls(async {
        let source = Session::open(&source_path).await.unwrap();
        let fork = Session::open(&fork_path).await.unwrap();
        assert_eq!(source.header.id, source_id);
        assert_eq!(fork.header.id, fork_id);
        assert_eq!(
            fork.header.parent_session.as_deref(),
            Some(source_path.as_str())
        );
        let source_messages = serde_json::to_value(source.to_messages_for_current_path())
            .unwrap()
            .to_string();
        let fork_messages = serde_json::to_value(fork.to_messages_for_current_path())
            .unwrap()
            .to_string();
        assert!(source_messages.contains("SOURCE-CONTINUATION-ONLY"));
        assert!(!source_messages.contains("FORK-CONTINUATION-ONLY"));
        assert!(fork_messages.contains("FORK-CONTINUATION-ONLY"));
        assert!(!fork_messages.contains("SOURCE-CONTINUATION-ONLY"));
    }));
}

#[test]
#[allow(clippy::too_many_lines)]
fn dispatcher_discovers_selects_and_restores_provider_qualified_byo_models() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let mut provider = ProviderFixture::new(vec![200, 200]);
    let runtime = runtime();
    let first = format!("{DISCOVERY_FIRST}/shared");
    let second = format!("{DISCOVERY_SECOND}/shared");
    let byo = format!("dalsh/{DISCOVERY_BYO_MODEL}");
    let local = format!("{DISCOVERY_LOCAL}/local");
    let choices = [
        first.as_str(),
        second.as_str(),
        byo.as_str(),
        local.as_str(),
    ];
    let session_id = runtime.block_on(bounded_launch_controls(async {
        let options = discovery_options(root.path(), &provider.url, runtime.handle());
        let (input, incoming) = asupersync::channel::mpsc::channel(16);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let mut transcript = Vec::new();
        let editor = async {
            let initialized = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                1,
                "initialize",
                json!({"protocolVersion": 1}),
            )
            .await;
            assert_eq!(initialized["result"]["protocolVersion"], 1);
            assert_eq!(
                initialized["result"]["agentCapabilities"]["loadSession"],
                true
            );
            assert_eq!(
                initialized["result"]["agentCapabilities"]["sessionCapabilities"]["resume"],
                json!({})
            );
            let created = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                2,
                "session/new",
                json!({"cwd": cwd, "mcpServers": []}),
            )
            .await;
            assert!(created.get("error").is_none(), "{created}");
            let id = created["result"]["sessionId"].as_str().unwrap().to_string();
            assert_discovery_state(&created["result"], &first, &choices);
            assert!(!created.to_string().contains(DISCOVERY_LOCKED));

            let switched = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                3,
                "session/set_model",
                json!({"sessionId": id, "modelId": second}),
            )
            .await;
            assert_eq!(
                switched["result"]["model"],
                json!({"provider": DISCOVERY_SECOND, "id": "shared"})
            );
            assert_discovery_update(&transcript, 3, &id, &second);
            let switched = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                4,
                "session/set_config_option",
                json!({"sessionId": id, "configId": "model", "value": byo}),
            )
            .await;
            assert_eq!(switched["result"]["configOptions"][0]["currentValue"], byo);
            assert_discovery_update(&transcript, 4, &id, &byo);
            let effort = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                5,
                "session/set_config_option",
                json!({"sessionId": id, "configId": "thought_level", "value": "low"}),
            )
            .await;
            assert_eq!(effort["result"]["configOptions"][1]["currentValue"], "low");
            assert_discovery_update(&transcript, 5, &id, &byo);

            for (request_id, model) in [
                (6, format!("{DISCOVERY_LOCKED}/private")),
                (7, "unknown/no-model".to_string()),
            ] {
                let before = transcript.len();
                let rejected = discovery_request(
                    &input,
                    &outgoing,
                    &mut transcript,
                    request_id,
                    "session/set_model",
                    json!({"sessionId": id, "modelId": model}),
                )
                .await;
                assert_eq!(rejected["error"]["code"], INVALID_PARAMS);
                assert_eq!(
                    transcript.len(),
                    before + 1,
                    "rejection must not publish a model change"
                );
            }
            let answer = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                8,
                "session/prompt",
                json!({
                    "sessionId": id,
                    "prompt": [{"type": "text", "text": "Use the selected BYO model"}],
                }),
            )
            .await;
            assert_eq!(answer["result"]["stopReason"], ACP_STOP_REASON_END_TURN);
            let live = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                9,
                "session/resume",
                json!({"sessionId": id, "cwd": cwd, "mcpServers": []}),
            )
            .await;
            assert_discovery_state(&live["result"], &byo, &choices);
            assert_eq!(live["result"]["configOptions"][1]["currentValue"], "low");
            assert_no_private_diagnostics(&transcript);
            input
                .try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string())
                .unwrap();
            id
        };
        let (served, id) = futures::join!(
            Box::pin(run(options, incoming, AcpOutput::new(output))),
            editor
        );
        served.unwrap();
        id
    }));

    runtime.block_on(bounded_launch_controls(async {
        let mut options = discovery_options(root.path(), &provider.url, runtime.handle());
        options.launch.provider = Some(DISCOVERY_SECOND.to_string());
        options.launch.model = Some("shared".to_string());
        options.launch.thinking = Some("high".to_string());
        let (input, incoming) = asupersync::channel::mpsc::channel(16);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let mut transcript = Vec::new();
        let editor = async {
            discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                1,
                "initialize",
                json!({"protocolVersion": 1}),
            )
            .await;
            let restored = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                2,
                "session/load",
                json!({"sessionId": session_id, "cwd": cwd, "mcpServers": []}),
            )
            .await;
            assert_discovery_state(&restored["result"], &byo, &choices);
            assert_eq!(
                restored["result"]["configOptions"][1]["currentValue"],
                "low"
            );
            assert!(transcript.iter().any(|frame| {
                frame["params"]["update"]["sessionUpdate"] == "user_message_chunk"
            }));
            assert!(transcript.iter().any(|frame| {
                frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
            }));
            let before = transcript.len();
            let resumed = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                3,
                "session/resume",
                json!({"sessionId": session_id, "cwd": cwd, "mcpServers": []}),
            )
            .await;
            assert_discovery_state(&resumed["result"], &byo, &choices);
            assert_eq!(
                transcript.len(),
                before + 1,
                "resume does not replay history"
            );
            let switched = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                4,
                "session/set_config_option",
                json!({"sessionId": session_id, "configId": "model", "value": local}),
            )
            .await;
            assert_eq!(
                switched["result"]["configOptions"][0]["currentValue"],
                local
            );
            assert_eq!(
                switched["result"]["configOptions"][1]["currentValue"],
                "off"
            );
            assert_discovery_update(&transcript, 4, &session_id, &local);
            let answer = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                5,
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": "Use the selected keyless model"}],
                }),
            )
            .await;
            assert_eq!(answer["result"]["stopReason"], ACP_STOP_REASON_END_TURN);
            assert_no_private_diagnostics(&transcript);
            input
                .try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string())
                .unwrap();
        };
        let (served, ()) = futures::join!(
            Box::pin(run(options, incoming, AcpOutput::new(output))),
            editor
        );
        served.unwrap();
    }));

    // Resume must also restore from disk in a new server process, not only
    // reattach an in-memory session that session/load has already rebuilt.
    runtime.block_on(bounded_launch_controls(async {
        let options = discovery_options(root.path(), &provider.url, runtime.handle());
        let (input, incoming) = asupersync::channel::mpsc::channel(16);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let mut transcript = Vec::new();
        let editor = async {
            discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                1,
                "initialize",
                json!({"protocolVersion": 1}),
            )
            .await;
            let resumed = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                2,
                "session/resume",
                json!({"sessionId": session_id, "cwd": cwd, "mcpServers": []}),
            )
            .await;
            assert_discovery_state(&resumed["result"], &local, &choices);
            assert_eq!(resumed["result"]["configOptions"][1]["currentValue"], "off");
            assert_eq!(
                transcript.len(),
                2,
                "fresh-process resume must not replay stored messages"
            );
            input
                .try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string())
                .unwrap();
        };
        let (served, ()) = futures::join!(
            Box::pin(run(options, incoming, AcpOutput::new(output))),
            editor
        );
        served.unwrap();
    }));
    let requests = provider.finish(2);
    assert_eq!(requests[0].1["model"], DISCOVERY_BYO_MODEL);
    assert!(
        requests[0]
            .0
            .to_ascii_lowercase()
            .contains("authorization: bearer dalsh-fixture-key")
    );
    assert_eq!(requests[1].1["model"], "local");
    assert!(
        !requests[1]
            .0
            .to_ascii_lowercase()
            .contains("authorization:")
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn dispatcher_model_discovery_respects_workspace_visibility_and_startup_scopes() {
    let root = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let cwd = project.path().canonicalize().unwrap();
    let runtime = runtime();
    runtime.block_on(bounded_launch_controls(async {
        let mut options = discovery_options(
            root.path(),
            "https://acp-discovery.invalid/v1",
            runtime.handle(),
        );
        let first = format!("{DISCOVERY_FIRST}/shared");
        let second = format!("{DISCOVERY_SECOND}/shared");
        let byo = format!("dalsh/{DISCOVERY_BYO_MODEL}");
        options.config.default_provider = None;
        options.config.default_model = None;
        options.config.enabled_models = Some(vec![first.clone()]);
        options.config.disabled_providers = Some(vec![DISCOVERY_LOCAL.to_string()]);
        options.config.model_scope_overrides = Some(vec![crate::config::ModelScopeOverride {
            path: cwd.display().to_string(),
            enabled_models: Some(vec![format!("{second}:low")]),
            disabled_providers: Some(vec!["dalsh".to_string()]),
        }]);
        let (input, incoming) = asupersync::channel::mpsc::channel(16);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let mut transcript = Vec::new();
        let editor = async {
            discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                1,
                "initialize",
                json!({"protocolVersion": 1}),
            )
            .await;
            let created = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                2,
                "session/new",
                json!({"cwd": cwd, "mcpServers": []}),
            )
            .await;
            assert_discovery_state(&created["result"], &second, &[&first, &second]);
            assert_eq!(created["result"]["configOptions"][1]["currentValue"], "low");
            let id = created["result"]["sessionId"].as_str().unwrap();
            // enabled_models chooses startup/cycling; explicit editor selection
            // may still choose another ready provider outside that preference.
            let selected = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                3,
                "session/set_config_option",
                json!({"sessionId": id, "configId": "model", "value": first}),
            )
            .await;
            assert_eq!(
                selected["result"]["configOptions"][0]["currentValue"],
                first
            );
            assert_discovery_update(&transcript, 3, id, &first);
            // Provider visibility is not an authorization boundary for an
            // explicit selection. Its actual current value must be discoverable.
            let selected = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                4,
                "session/set_model",
                json!({"sessionId": id, "provider": "dalsh", "model": DISCOVERY_BYO_MODEL}),
            )
            .await;
            assert!(selected.get("error").is_none(), "{selected}");
            let resumed = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                5,
                "session/resume",
                json!({"sessionId": id, "cwd": cwd, "mcpServers": []}),
            )
            .await;
            assert_discovery_state(&resumed["result"], &byo, &[&first, &second, &byo]);
            assert_no_private_diagnostics(&transcript);
            input
                .try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string())
                .unwrap();
        };
        let (served, ()) = futures::join!(
            Box::pin(run(options, incoming, AcpOutput::new(output))),
            editor
        );
        served.unwrap();
    }));
}

#[test]
#[allow(clippy::too_many_lines)]
fn dispatcher_automatic_discovery_never_reintroduces_disabled_providers() {
    let root = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let cwd = project.path().canonicalize().unwrap();
    let runtime = runtime();
    for (preferred, all_disabled) in [
        (None, false),
        (Some(DISCOVERY_FIRST), false),
        (None, true),
        (Some(DISCOVERY_FIRST), true),
    ] {
        runtime.block_on(bounded_launch_controls(async {
            let mut options = discovery_options(
                root.path(),
                "https://acp-discovery.invalid/v1",
                runtime.handle(),
            );
            options.config.default_provider = preferred.map(str::to_string);
            options.config.default_model = None;
            options.config.enabled_models = None;
            options.config.disabled_providers = Some(vec![
                DISCOVERY_FIRST.to_string(),
                DISCOVERY_LOCAL.to_string(),
            ]);
            let mut disabled = vec![DISCOVERY_SECOND.to_string()];
            if all_disabled {
                disabled.push("dalsh".to_string());
            }
            options.config.model_scope_overrides = Some(vec![crate::config::ModelScopeOverride {
                path: cwd.display().to_string(),
                enabled_models: None,
                disabled_providers: Some(disabled),
            }]);
            options.session_dir = None;
            let (input, incoming) = asupersync::channel::mpsc::channel(16);
            let (output, outgoing) = std::sync::mpsc::sync_channel(128);
            let mut transcript = Vec::new();
            let editor = async {
                discovery_request(
                    &input,
                    &outgoing,
                    &mut transcript,
                    1,
                    "initialize",
                    json!({"protocolVersion": 1}),
                )
                .await;
                let created = discovery_request(
                    &input,
                    &outgoing,
                    &mut transcript,
                    2,
                    "session/new",
                    json!({"cwd": cwd, "mcpServers": []}),
                )
                .await;
                if all_disabled {
                    assert_eq!(created["error"]["code"], INTERNAL_ERROR);
                    assert!(created.get("result").is_none());
                } else {
                    let byo = format!("dalsh/{DISCOVERY_BYO_MODEL}");
                    assert_discovery_state(&created["result"], &byo, &[&byo]);
                }
                let listed = discovery_request(
                    &input,
                    &outgoing,
                    &mut transcript,
                    3,
                    "session/list",
                    json!({"cwd": cwd}),
                )
                .await;
                assert_eq!(
                    listed["result"]["sessions"].as_array().unwrap().len(),
                    usize::from(!all_disabled),
                    "failed discovery must not install a phantom session",
                );
                input
                    .try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string())
                    .unwrap();
            };
            let (served, ()) = futures::join!(
                Box::pin(run(options, incoming, AcpOutput::new(output))),
                editor
            );
            served.unwrap();
        }));
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn dispatcher_api_key_override_admits_only_its_exact_provider_models() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let mut provider = ProviderFixture::new(vec![200, 200]);
    let runtime = runtime();
    runtime.block_on(bounded_launch_controls(async {
        let mut options = discovery_options(root.path(), &provider.url, runtime.handle());
        let mut entries = options.model_registry.models().to_vec();
        for entry in &mut entries {
            if entry.model.provider == DISCOVERY_FIRST {
                entry.model.provider = "openrouter".to_string();
                entry.model.id = "override-first".to_string();
                entry.api_key = None;
            } else if entry.model.provider == DISCOVERY_SECOND {
                entry.api_key = None;
            }
        }
        let mut sibling = entries
            .iter()
            .find(|entry| entry.model.provider == "openrouter")
            .unwrap()
            .clone();
        sibling.model.id = "override-second".to_string();
        sibling.model.name = "Second override model".to_string();
        entries.push(sibling.clone());
        // Provider aliases may match for display/policy, but the runtime pins
        // --api-key to the exact provider identity installed at startup.
        sibling.model.provider = "open-router".to_string();
        sibling.model.id = "alias-only".to_string();
        entries.push(sibling);
        options.model_registry = ModelRegistry::from_entries_for_tests(entries);
        // main admits all registry entries with --api-key. ACP then narrows
        // that list to credentials usable under its provider-bound override.
        options.available_models = options.model_registry.models().to_vec();
        options.launch.provider = Some("openrouter".to_string());
        options.launch.model = Some("override-first".to_string());
        options.launch.api_key = Some("launch-fixture-key".to_string());
        let byo = format!("dalsh/{DISCOVERY_BYO_MODEL}");
        let local = format!("{DISCOVERY_LOCAL}/local");
        let choices = [
            "openrouter/override-first",
            "openrouter/override-second",
            byo.as_str(),
            local.as_str(),
        ];
        let (input, incoming) = asupersync::channel::mpsc::channel(16);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let mut transcript = Vec::new();
        let editor = async {
            discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                1,
                "initialize",
                json!({"protocolVersion": 1}),
            )
            .await;
            let created = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                2,
                "session/new",
                json!({"cwd": cwd, "mcpServers": []}),
            )
            .await;
            assert_discovery_state(&created["result"], "openrouter/override-first", &choices);
            let id = created["result"]["sessionId"].as_str().unwrap();
            let selected = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                3,
                "session/set_config_option",
                json!({
                    "sessionId": id,
                    "configId": "model",
                    "value": "openrouter/override-second",
                }),
            )
            .await;
            assert_eq!(
                selected["result"]["configOptions"][0]["currentValue"],
                "openrouter/override-second"
            );
            let answer = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                4,
                "session/prompt",
                json!({
                    "sessionId": id,
                    "prompt": [{"type": "text", "text": "Use the provider-bound override"}],
                }),
            )
            .await;
            assert_eq!(answer["result"]["stopReason"], ACP_STOP_REASON_END_TURN);
            let selected = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                5,
                "session/set_model",
                json!({"sessionId": id, "modelId": byo}),
            )
            .await;
            assert!(selected.get("error").is_none(), "{selected}");
            let answer = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                6,
                "session/prompt",
                json!({
                    "sessionId": id,
                    "prompt": [{"type": "text", "text": "Use this model's own credential"}],
                }),
            )
            .await;
            assert_eq!(answer["result"]["stopReason"], ACP_STOP_REASON_END_TURN);
            let resumed = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                7,
                "session/resume",
                json!({"sessionId": id, "cwd": cwd, "mcpServers": []}),
            )
            .await;
            assert_discovery_state(&resumed["result"], &byo, &choices);
            assert_no_private_diagnostics(&transcript);
            input
                .try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string())
                .unwrap();
        };
        let (served, ()) = futures::join!(
            Box::pin(run(options, incoming, AcpOutput::new(output))),
            editor
        );
        served.unwrap();
    }));
    let requests = provider.finish(2);
    assert_eq!(requests[0].1["model"], "override-second");
    assert!(
        requests[0]
            .0
            .to_ascii_lowercase()
            .contains("authorization: bearer launch-fixture-key")
    );
    assert_eq!(requests[1].1["model"], DISCOVERY_BYO_MODEL);
    assert!(
        requests[1]
            .0
            .to_ascii_lowercase()
            .contains("authorization: bearer dalsh-fixture-key")
    );
    assert!(!requests[1].0.contains("launch-fixture-key"));
}

fn user_content(request: &Value) -> &Value {
    &request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
}

async fn bounded_launch_controls<F: Future>(work: F) -> F::Output {
    timeout(wall_now(), Duration::from_secs(60), Box::pin(work))
        .await
        .expect("ACP launch controls watchdog expired")
}

#[test]
#[allow(clippy::too_many_lines)]
fn dispatcher_prompt_keywords_use_authored_text_without_consuming_reference_content() {
    use base64::Engine as _;

    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let sessions_root = root.path().join("sessions");
    let mut provider = ProviderFixture::new(vec![503, 200, 200, 200, 200, 200]);
    let runtime = runtime();
    runtime.block_on(bounded_launch_controls(async {
        let mut options = options(root.path(), &provider.url, runtime.handle(), 1);
        options.session_dir = Some(sessions_root.clone());
        options.launch.no_tools = true;
        options.launch.no_mcp = true;
        options.launch.no_extensions = true;
        options.launch.no_skills = true;
        options.launch.no_context_files = true;
        options.launch.system_prompt = Some("ACP keyword boundary fixture".into());
        let (input, incoming) = asupersync::channel::mpsc::channel(16);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let mut transcript = Vec::new();
        let editor = async {
            discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                1,
                "initialize",
                json!({"protocolVersion": 1}),
            )
            .await;
            let created = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                2,
                "session/new",
                json!({"cwd": cwd, "mcpServers": []}),
            )
            .await;
            let session_id = created["result"]["sessionId"].as_str().unwrap().to_string();
            let encoded = base64::engine::general_purpose::STANDARD
                .encode("REFERENCE-BLOB says chillax and orchestrate in the document.");
            let prompts = [
                json!([
                    {"type": "text", "text": "Analyze the reference without changing the task."},
                    {"type": "resource", "resource": {
                        "uri": "file:///unopened/reference.txt",
                        "mimeType": "text/plain",
                        "text": "REFERENCE-TEXT says chillax and orchestrate in the document.",
                    }},
                ]),
                json!([{"type": "resource", "resource": {
                    "uri": "memory:encoded-reference",
                    "mimeType": "text/plain",
                    "blob": encoded,
                }}]),
                json!([{"type": "resource_link", "uri": "orchestrate", "name": "Reference"}]),
                json!([
                    {"type": "text", "text": "Please"},
                    {"type": "resource", "resource": {
                        "uri": "memory:another-reference",
                        "text": "Reference says chillax.",
                    }},
                    {"type": "text", "text": "orchestrate this task."},
                ]),
                json!([{"type": "text", "text": "Continue normally."}]),
            ];
            for (id, prompt) in (3_u64..).zip(prompts) {
                let response = discovery_request(
                    &input,
                    &outgoing,
                    &mut transcript,
                    id,
                    "session/prompt",
                    json!({"sessionId": session_id, "prompt": prompt}),
                )
                .await;
                assert_eq!(response["result"]["stopReason"], ACP_STOP_REASON_END_TURN);
            }

            // ACP acknowledges only after persistence, including keyword
            // telemetry. Referenced prose must never change pinned effort.
            let index = crate::session_index::SessionIndex::for_sessions_root(&sessions_root);
            index.reindex_all().unwrap();
            let rows = index.list_sessions(None).unwrap();
            let path = &rows.iter().find(|row| row.id == session_id).unwrap().path;
            let saved = Session::open(path).await.unwrap();
            let activations = saved
                .entries_for_current_path()
                .into_iter()
                .filter_map(|entry| match entry {
                    crate::session::SessionEntry::Custom(entry)
                        if entry.custom_type == "magic_keyword" =>
                    {
                        entry.data.as_ref()?.get("word")?.as_str()
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(activations, vec!["orchestrate"]);
            assert_eq!(
                saved.effective_thinking_level_for_current_path().as_deref(),
                Some("high"),
            );
            assert_eq!(
                saved
                    .to_messages_for_current_path()
                    .iter()
                    .filter(|message| matches!(message, Message::User(_)))
                    .count(),
                5,
                "retry must not append the editor input twice",
            );
            input
                .try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string())
                .unwrap();
        };
        let (served, ()) = futures::join!(
            Box::pin(run(options, incoming, AcpOutput::new(output))),
            editor
        );
        served.unwrap();
    }));

    let requests = provider.finish(6);
    assert_eq!(user_content(&requests[0].1), user_content(&requests[1].1));
    for (index, (_, request)) in requests.iter().enumerate() {
        let system = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| matches!(message["role"].as_str(), Some("system" | "developer")))
            .filter_map(|message| message["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            system
                .matches(crate::magic_keywords::ORCHESTRATE_DIRECTIVE)
                .count(),
            usize::from(index == 4),
            "only authored prose activates orchestration on its own turn",
        );
        assert!(!system.contains(crate::magic_keywords::CHILLAX_DIRECTIVE));
    }
    assert!(
        user_content(&requests[0].1)
            .to_string()
            .contains("REFERENCE-TEXT")
    );
    assert!(
        user_content(&requests[2].1)
            .to_string()
            .contains("REFERENCE-BLOB")
    );
    assert_eq!(
        user_content(&requests[3].1),
        &json!([{"type": "text", "text": "orchestrate"}])
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn launch_prewalk_preserves_native_tasks_without_replaying_the_editor_edit() {
    use clap::Parser as _;

    let root = tempfile::tempdir().unwrap();
    let mut server = ProviderFixture::with_replies(
        vec![200, 200, 503, 200],
        vec![
            Some((
                "todo",
                json!({"op":"init", "tasks":["Implement the editor change", "Verify"]}),
            )),
            Some(("write", json!({"path":"prewalk.txt", "content":"one edit"}))),
            None,
            None,
        ],
    );
    let runtime = runtime();
    runtime.block_on(bounded_launch_controls(async {
        let mut options = options(root.path(), &server.url, runtime.handle(), 1);
        let target = format!("{FALLBACK_PROVIDER}/{FALLBACK_MODEL}");
        let cli = crate::cli::Cli::try_parse_from([
            "pi",
            "--acp",
            "--prewalk",
            "--smol",
            &target,
            "--tools",
            "read,write,todo",
        ])
        .unwrap();
        options.launch = AcpLaunchOptions::from_cli(&cli);
        let (id, mut created) = handle_session_new(
            &json!({"cwd": root.path(), "mcpServers": []}),
            &options,
            None,
        )
        .unwrap();
        arm_acp_prewalk(&mut created, &options).await.unwrap();
        let state = Arc::new(Mutex::new(created));
        let observed = events(&state);
        let (_, signal) = AbortHandle::new();
        let (reason, updates) = prompt(&state, &id, text(), signal).await;
        assert_eq!(reason, ACP_STOP_REASON_END_TURN);
        let selected = configuration_updates(&updates);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0][0]["currentValue"], target);
        assert_eq!(selected[0][1]["currentValue"], "off");
        assert!(updates.iter().any(|update| {
            update["params"]["update"]["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("Prewalk switched"))
        }));
        assert_no_private_diagnostics(&updates);
        let plans = updates
            .iter()
            .filter(|update| update["params"]["update"]["sessionUpdate"] == "plan")
            .collect::<Vec<_>>();
        assert_eq!(plans.len(), 1, "recovery must not replay task mutations");
        assert_eq!(plans[0]["params"]["sessionId"], id);
        assert_eq!(plans[0]["params"]["update"]["entries"], json!([
            {"content":"Implement the editor change", "priority":"medium", "status":"in_progress"},
            {"content":"Verify", "priority":"medium", "status":"pending"},
        ]));
        let saved = reopen(&state).await;
        let saved_tasks = crate::todo::latest_from_entries(saved.entries_for_current_path());
        assert_eq!(saved_tasks.current().unwrap().content, "Implement the editor change");
        assert_eq!(saved_tasks.progress(), (0, 2));
        assert_eq!(saved.header.provider.as_deref(), Some(FALLBACK_PROVIDER));
        assert_eq!(saved.header.model_id.as_deref(), Some(FALLBACK_MODEL));
        assert_eq!(
            saved
                .to_messages_for_current_path()
                .iter()
                .filter(|message| matches!(message, Message::User(_)))
                .count(),
            1,
        );
        assert_eq!(
            saved
                .to_messages_for_current_path()
                .iter()
                .filter(|message| matches!(message, Message::ToolResult(_)))
                .count(),
            2,
        );
        let observed = observed.lock().unwrap();
        for event_kind in ["start", "end", "switch", "tool"] {
            assert_eq!(
                observed
                    .iter()
                    .filter(|event| match event_kind {
                        "start" => matches!(event, AgentEvent::AgentStart { .. }),
                        "end" => matches!(event, AgentEvent::AgentEnd { .. }),
                        "switch" => matches!(event, AgentEvent::PrewalkSwitched { .. }),
                        "tool" => matches!(event, AgentEvent::ToolExecutionStart { .. }),
                        _ => unreachable!(),
                    })
                    .count(),
                if event_kind == "tool" { 2 } else { 1 },
                "{event_kind}",
            );
        }
        let guard = state.try_lock().unwrap();
        let handle = guard.agent_session.as_ref().unwrap();
        assert!(handle.prewalk_target().is_none());
        assert_eq!(handle.compaction_settings().context_window_tokens, 32_000);
        assert_eq!(
            handle.session().agent.stream_options().max_tokens,
            Some(512),
        );
    }));
    let requests = server.finish(4);
    assert_eq!(requests[0].1["model"], PRIMARY_MODEL);
    assert_eq!(requests[1].1["model"], PRIMARY_MODEL);
    for (_, request) in &requests[2..] {
        assert_eq!(request["model"], FALLBACK_MODEL);
        assert_eq!(
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "tool")
                .count(),
            2,
        );
        assert!(!request.to_string().contains("## Prewalk"));
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("prewalk.txt")).unwrap(),
        "one edit",
    );
}

#[test]
fn editor_prewalk_requires_explicit_intent_and_validates_workspace_policy() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    runtime.block_on(async {
        let mut options = options(
            root.path(),
            "https://acp-prewalk.invalid/v1",
            runtime.handle(),
            0,
        );
        options.config.prewalk = Some(crate::config::PrewalkSettings {
            enabled: Some(true),
            ..Default::default()
        });
        let params = json!({"cwd": root.path(), "mcpServers": []});
        let (_, mut state) = handle_session_new(&params, &options, None).unwrap();
        arm_acp_prewalk(&mut state, &options).await.unwrap();
        assert!(
            state
                .agent_session
                .as_ref()
                .unwrap()
                .prewalk_target()
                .is_none()
        );
        options.launch.prewalk = true;
        assert!(
            arm_acp_prewalk(&mut state, &options)
                .await
                .unwrap_err()
                .to_string()
                .contains("explicit @smol"),
        );
        options.launch.smol = Some(format!("{FALLBACK_PROVIDER}/{FALLBACK_MODEL}"));
        options.config.model_scope_overrides = Some(vec![crate::config::ModelScopeOverride {
            path: root.path().display().to_string(),
            enabled_models: None,
            disabled_providers: Some(vec![FALLBACK_PROVIDER.to_string()]),
        }]);
        assert!(
            arm_acp_prewalk(&mut state, &options)
                .await
                .unwrap_err()
                .to_string()
                .contains("disabled"),
        );
        assert!(
            state
                .agent_session
                .as_ref()
                .unwrap()
                .prewalk_target()
                .is_none()
        );
        options.launch.no_prewalk = true;
        arm_acp_prewalk(&mut state, &options).await.unwrap();
        assert!(
            state
                .agent_session
                .as_ref()
                .unwrap()
                .prewalk_target()
                .is_none()
        );
        options.launch.no_prewalk = false;
        options.launch.smol = None;
        options.config.model_scope_overrides = None;
        options.config.prewalk.as_mut().unwrap().into =
            Some(format!("{FALLBACK_PROVIDER}/{FALLBACK_MODEL}"));
        arm_acp_prewalk(&mut state, &options).await.unwrap();
        let target = state
            .agent_session
            .as_ref()
            .unwrap()
            .prewalk_target()
            .unwrap();
        assert_eq!(target.provider, FALLBACK_PROVIDER);
        assert_eq!(target.model, FALLBACK_MODEL);
    });
}

#[test]
fn launch_prompt_and_no_tools_controls_reach_the_actual_provider_request() {
    use clap::Parser as _;

    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("CLAUDE.md"),
        "EXCLUDED-ACP-PROJECT-CONTEXT",
    )
    .unwrap();
    std::fs::write(
        root.path().join(".cursorrules"),
        "EXCLUDED-ACP-FOREIGN-CONTEXT",
    )
    .unwrap();
    let mut server = ProviderFixture::new(vec![200]);
    let runtime = runtime();
    runtime.block_on(bounded_launch_controls(async {
        let cli = crate::cli::Cli::try_parse_from([
            "pi",
            "--provider",
            PRIMARY_PROVIDER,
            "--model",
            PRIMARY_MODEL,
            "--tools",
            "read,write,bash",
            "--no-tools",
            "--system-prompt",
            "ACP-HOST-OWNED-PROMPT",
            "--append-system-prompt",
            "ACP-HOST-APPEND-PROMPT",
            "--no-context-files",
            "--hide-cwd-in-prompt",
        ])
        .unwrap();
        let mut options = options(root.path(), &server.url, runtime.handle(), 0);
        options.launch = AcpLaunchOptions::from_cli(&cli);
        options.config.foreign_rules = Some(true);
        options.config.media = Some(crate::media_tools::MediaSettings {
            enable_tts: Some(true),
            ..crate::media_tools::MediaSettings::default()
        });
        options.skills_prompt = Some("EXCLUDED-ACP-SKILLS".to_string());
        let (id, state) = new_state(root.path(), &options);
        {
            let mut guard = state.try_lock().unwrap();
            let context = guard
                .agent_session
                .as_mut()
                .unwrap()
                .session_mut()
                .agent
                .request_context_json();
            assert_eq!(
                context["tools"].as_array().unwrap().len(),
                0,
                "--no-tools removes automatic configured tools too"
            );
        }
        let (_, signal) = AbortHandle::new();
        let (reason, _) = prompt(&state, &id, text(), signal).await;
        assert_eq!(reason, ACP_STOP_REASON_END_TURN);
    }));
    let requests = server.finish(1);
    let body = &requests[0].1;
    assert!(
        body.get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty)),
        "--no-tools must reach the actual provider request"
    );
    let system_prompt = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| matches!(message["role"].as_str(), Some("system" | "developer")))
        .map(|message| message["content"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(system_prompt.contains("ACP-HOST-OWNED-PROMPT"));
    assert!(system_prompt.contains("ACP-HOST-APPEND-PROMPT"));
    assert!(system_prompt.contains("via ACP (Agent Client Protocol)"));
    for excluded in [
        "EXCLUDED-ACP-PROJECT-CONTEXT",
        "EXCLUDED-ACP-FOREIGN-CONTEXT",
        "EXCLUDED-ACP-SKILLS",
        "Current working directory:",
        root.path().to_str().unwrap(),
    ] {
        assert!(
            !system_prompt.contains(excluded),
            "launch controls must exclude {excluded}"
        );
    }
}

#[test]
fn launch_selected_tools_reach_the_live_registry_without_terminal_host_tools() {
    use clap::Parser as _;

    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    runtime.block_on(bounded_launch_controls(async {
        let cli = crate::cli::Cli::try_parse_from([
            "pi",
            "--provider",
            PRIMARY_PROVIDER,
            "--model",
            PRIMARY_MODEL,
            "--tools",
            "read,ask,todo,submit_plan",
        ])
        .unwrap();
        let mut options = options(
            root.path(),
            "https://acp-tools.invalid/v1",
            runtime.handle(),
            0,
        );
        options.launch = AcpLaunchOptions::from_cli(&cli);
        options.skills_prompt = Some("INCLUDED-ACP-SKILLS".to_string());
        let (_, state) = new_state(root.path(), &options);
        let mut guard = state.try_lock().unwrap();
        let context = guard
            .agent_session
            .as_mut()
            .unwrap()
            .session_mut()
            .agent
            .request_context_json();
        let names = context["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"todo"));
        assert!(
            context["systemPrompt"]
                .as_str()
                .unwrap()
                .contains("INCLUDED-ACP-SKILLS")
        );
        for excluded in ["bash", "write", "edit", "ask", "submit_plan"] {
            assert!(
                !names.contains(&excluded),
                "unexpected ACP tool: {excluded}"
            );
        }
    }));
}

#[test]
fn unreadable_explicit_prompt_inputs_reject_session_creation() {
    let root = tempfile::tempdir().unwrap();
    let invalid_prompt = root.path().join("not-a-prompt-file");
    std::fs::create_dir(&invalid_prompt).unwrap();
    let runtime = runtime();
    runtime.block_on(bounded_launch_controls(async {
        for append in [false, true] {
            let mut options = options(
                root.path(),
                "https://acp-prompt.invalid/v1",
                runtime.handle(),
                0,
            );
            options.launch.no_context_files = true;
            options.launch.hide_cwd_in_prompt = true;
            let prompt_input = Some(invalid_prompt.display().to_string());
            if append {
                options.launch.append_system_prompt = prompt_input;
            } else {
                options.launch.system_prompt = prompt_input;
            }
            let error = handle_session_new(
                &json!({ "cwd": root.path(), "mcpServers": [] }),
                &options,
                None,
            )
            .err()
            .expect("an unreadable explicit prompt must reject the session");
            let message = error.to_string();
            assert!(message.contains("Cannot build ACP system prompt"));
            assert!(message.contains("Could not read"));
        }
    }));
}

#[test]
fn launch_zero_time_budget_publishes_the_boundary_marker_only_after_durable_success() {
    use clap::Parser as _;

    for fail_save in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut server = ProviderFixture::new(vec![200]);
        let runtime = runtime();
        runtime.block_on(bounded_launch_controls(async {
            let cli = crate::cli::Cli::try_parse_from([
                "pi",
                "--provider",
                PRIMARY_PROVIDER,
                "--model",
                PRIMARY_MODEL,
                "--max-time",
                "0",
            ])
            .unwrap();
            let mut options = options(root.path(), &server.url, runtime.handle(), 0);
            options.launch = AcpLaunchOptions::from_cli(&cli);
            let (id, state) = new_state(root.path(), &options);
            let fault_triggered = Arc::new(AtomicBool::new(false));
            if fail_save {
                let stored = session_store(&state);
                let invalid = root.path().to_path_buf();
                let triggered = Arc::clone(&fault_triggered);
                state
                    .try_lock()
                    .unwrap()
                    .agent_session
                    .as_ref()
                    .unwrap()
                    .subscribe(move |event| {
                        if let AgentEvent::MessageEnd {
                            message: Message::Assistant(message),
                        } = event
                            && message.api.is_empty()
                            && message.provider.is_empty()
                            && message.model.is_empty()
                            && message.content.iter().any(|block| {
                                matches!(
                                    block,
                                    ContentBlock::Text(text)
                                        if text.text.starts_with("[time cap reached]")
                                )
                            })
                        {
                            stored.try_lock().unwrap().path = Some(invalid.clone());
                            triggered.store(true, Ordering::SeqCst);
                        }
                    });
            }
            let (_, signal) = AbortHandle::new();
            let (reason, updates) = prompt(&state, &id, text(), signal).await;
            let rendered = serde_json::to_string(&updates).unwrap();
            if fail_save {
                assert!(
                    fault_triggered.load(Ordering::SeqCst),
                    "the fault must occur after the synthetic marker is generated"
                );
                assert_eq!(reason, ACP_STOP_REASON_ERROR);
                assert!(rendered.contains("Session persistence failed"));
                assert!(
                    !rendered.contains("[time cap reached]"),
                    "failed persistence must not publish a successful pause"
                );
            } else {
                assert_eq!(reason, ACP_STOP_REASON_END_TURN);
                let marker_updates = updates
                    .iter()
                    .filter(|update| {
                        update["params"]["update"]["content"]["text"]
                            .as_str()
                            .is_some_and(|text| text.starts_with("[time cap reached]"))
                    })
                    .count();
                assert_eq!(marker_updates, 1, "the editor receives one boundary marker");
                let saved = reopen(&state).await;
                let messages = saved.to_messages_for_current_path();
                assert_eq!(
                    messages
                        .iter()
                        .filter(|message| matches!(message, Message::User(_)))
                        .count(),
                    1
                );
                assert!(messages.iter().any(|message| {
                    matches!(
                        message,
                        Message::Assistant(assistant)
                            if assistant.content.iter().any(|block| {
                                matches!(
                                    block,
                                    ContentBlock::Text(text)
                                        if text.text.starts_with("[time cap reached]")
                                )
                            })
                    )
                }));
            }
        }));
        server.finish(0);
    }
}

#[test]
fn launch_iteration_limit_stops_before_executing_the_next_provider_tool_call() {
    use clap::Parser as _;

    let root = tempfile::tempdir().unwrap();
    let mut server = ProviderFixture::with_tool(vec![200, 200, 200], Some("current_time"));
    let runtime = runtime();
    runtime.block_on(bounded_launch_controls(async {
        let cli = crate::cli::Cli::try_parse_from([
            "pi",
            "--provider",
            PRIMARY_PROVIDER,
            "--model",
            PRIMARY_MODEL,
            "--tools",
            "current_time",
            "--max-tool-iterations",
            "1",
        ])
        .unwrap();
        let mut options = options(root.path(), &server.url, runtime.handle(), 0);
        options.launch = AcpLaunchOptions::from_cli(&cli);
        let cx = AgentCx::for_current_or_request();
        let (permission_tx, permission_rx) = std::sync::mpsc::sync_channel::<String>(8);
        let permission_tx = AcpOutput::new(permission_tx);
        let permission_client = AcpPermissionClient {
            pending: Arc::clone(&permission_tx.shared.pending_permissions),
            out_tx: permission_tx,
            request_counter: Arc::new(AtomicU64::new(0)),
            timeout: Duration::from_secs(5),
            cx: cx.clone(),
        };
        let (id, state) = handle_session_new(
            &json!({ "cwd": root.path(), "mcpServers": [] }),
            &options,
            Some(&permission_client),
        )
        .unwrap();
        let state = Arc::new(Mutex::new(state));
        let observed = events(&state);
        let responder = async {
            loop {
                match permission_rx.try_recv() {
                    Ok(line) => {
                        let request: Value = serde_json::from_str(&line).unwrap(); // ubs:ignore[rust.parsing.serde-unwrap] -- Fixture permission requests must parse.
                        assert_eq!(request["method"], "session/request_permission");
                        assert_eq!(request["params"]["sessionId"], id);
                        assert_eq!(request["params"]["toolCall"]["title"], "current_time");
                        assert_eq!(request["params"]["toolCall"]["toolCallId"], "acp-tool-0");
                        assert!(route_permission_response(
                            &json!({
                                "jsonrpc": "2.0",
                                "id": request["id"],
                                "result": {
                                    "outcome": {
                                        "outcome": "selected",
                                        "optionId": ACP_PERMISSION_ALLOW_ONCE,
                                    },
                                },
                            }),
                            &permission_client.pending,
                            &cx,
                        ));
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        cx.time().sleep(Duration::from_millis(2)).await;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        panic!("ACP permission writer disconnected before approval"); // ubs:ignore[rust.ownership.panic-macro] -- Disconnection must fail the fixture.
                    }
                }
            }
        };
        let (_, signal) = AbortHandle::new();
        let ((reason, _), ()) = futures::join!(prompt(&state, &id, text(), signal), responder);
        assert_eq!(reason, ACP_STOP_REASON_END_TURN);
        assert_eq!(
            permission_client.request_counter.load(Ordering::SeqCst),
            1,
            "the blocked second call must not request editor approval"
        );
        // ubs:ignore[rust.async.lock-unwrap] -- The completed fixture must leave no pending permission request.
        assert!(permission_client.pending.lock().unwrap().is_empty());
        assert!(permission_rx.try_recv().is_err());
        // ubs:ignore[rust.async.lock-unwrap] -- Inspect the fixture's completed event trace after both joined futures exit.
        let observed = observed.lock().unwrap();
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event, AgentEvent::ToolExecutionStart { .. }))
                .count(),
            1
        );
        assert!(observed.iter().any(|event| {
            matches!(
                event,
                AgentEvent::ToolExecutionEnd {
                    tool_name,
                    is_error: false,
                    ..
                } if tool_name == "current_time"
            )
        }));
        assert!(observed.iter().any(|event| {
            matches!(
                event,
                AgentEvent::AgentEnd {
                    error: Some(error),
                    ..
                } if error.contains("Maximum tool iterations (1) exceeded")
            )
        }));
    }));
    let requests = server.finish(2);
    assert!(
        requests[1].1["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool"),
        "the approved first tool result must reach the second provider request"
    );
}

#[test]
fn launch_selection_reaches_the_configured_agent_and_session_metadata() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    runtime.block_on(async {
        for (provider, model, models, effort, expected_provider, expected_model, expected_effort) in [
            (Some(PRIMARY_PROVIDER), Some(PRIMARY_MODEL.to_string()), None, "low", PRIMARY_PROVIDER, PRIMARY_MODEL, "low"),
            (Some(FALLBACK_PROVIDER), None, None, "high", FALLBACK_PROVIDER, FALLBACK_MODEL, "off"),
            (None, Some(format!("{PRIMARY_PROVIDER}/{PRIMARY_MODEL}")), None, "high", PRIMARY_PROVIDER, PRIMARY_MODEL, "high"),
            (None, Some(PRIMARY_MODEL.to_string()), None, "off", PRIMARY_PROVIDER, PRIMARY_MODEL, "off"),
            (None, None, Some(format!("{FALLBACK_PROVIDER}/{FALLBACK_MODEL}:high")), "high", FALLBACK_PROVIDER, FALLBACK_MODEL, "off"),
        ] {
            let mut options = options(root.path(), "https://acp-launch.invalid/v1", runtime.handle(), 0);
            // The ready-only catalog is empty: explicit selection and an
            // explicit credential must still reach the registered model.
            options.available_models.clear();
            let mut entries = options.model_registry.models().to_vec();
            for entry in &mut entries {
                entry.api_key = None;
                entry.auth_header = true;
            }
            options.model_registry = ModelRegistry::from_entries_for_tests(entries);
            options.launch = AcpLaunchOptions {
                provider: provider.map(str::to_string), model, models,
                thinking: Some(effort.to_string()),
                api_key: Some("  launch-fixture-key  ".to_string()),
                ..AcpLaunchOptions::default()
            };
            let (id, state) = new_state(root.path(), &options);
            let guard = state.try_lock().unwrap();
            let handle = guard.agent_session.as_ref().unwrap();
            let provider = handle.session().agent.provider();
            assert_eq!((provider.name(), provider.model_id()), (expected_provider, expected_model));
            let stream = handle.session().agent.stream_options();
            assert_eq!(stream.api_key.as_deref(), Some("launch-fixture-key"));
            assert_eq!(stream.session_id.as_deref(), Some(id.as_str()));
            assert_eq!(stream.headers.get("x-acp-model").map(String::as_str), Some(expected_model));
            assert_eq!(stream.thinking_level.unwrap().to_string(), expected_effort);
            assert_eq!(stream.max_tokens, Some(if expected_model == PRIMARY_MODEL { 2_048 } else { 512 }));
            let config = config_options_for_handle(
                handle,
                &options.available_models,
                &options.config,
                root.path(),
            )
            .unwrap();
            assert_eq!(config[0]["currentValue"], format!("{expected_provider}/{expected_model}"));
            assert_eq!(config[1]["currentValue"], expected_effort);
            assert!(!config.to_string().contains("launch-fixture-key"));
            let store = handle.session_store();
            let saved = store.try_lock().unwrap();
            assert_eq!(saved.header.id, id);
            assert_eq!(saved.effective_model_for_current_path(), Some((expected_provider.to_string(), expected_model.to_string())));
            assert_eq!(saved.effective_thinking_level_for_current_path().as_deref(), Some(expected_effort));
        }
    });
}

#[test]
fn launch_scope_uses_the_requested_workspace_and_preserves_scoped_effort() {
    let root = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let runtime = runtime();
    runtime.block_on(async {
        let mut options = options(
            root.path(),
            "https://acp-scope.invalid/v1",
            runtime.handle(),
            0,
        );
        options.config.enabled_models = Some(vec![format!("{FALLBACK_PROVIDER}/{FALLBACK_MODEL}")]);
        options.config.model_scope_overrides = Some(vec![crate::config::ModelScopeOverride {
            path: project.path().display().to_string(),
            enabled_models: Some(vec![format!("{PRIMARY_PROVIDER}/{PRIMARY_MODEL}:low")]),
            disabled_providers: None,
        }]);
        options.launch.api_key = Some("workspace-fixture-key".to_string());
        let (_, state) = new_state(project.path(), &options);
        let guard = state.try_lock().unwrap();
        let agent = &guard.agent_session.as_ref().unwrap().session().agent;
        assert_eq!(agent.provider().model_id(), PRIMARY_MODEL);
        assert_eq!(
            agent.stream_options().thinking_level,
            Some(crate::model::ThinkingLevel::Low)
        );
    });
}

#[test]
fn invalid_launch_selection_and_missing_credentials_fail_before_a_session_is_installed() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    runtime.block_on(async {
        let mut options = options(
            root.path(),
            "https://acp-errors.invalid/v1",
            runtime.handle(),
            0,
        );
        options.launch.provider = Some("unknown-acp-provider".to_string());
        options.launch.model = Some("missing-model".to_string());
        let params = json!({"cwd": root.path(), "mcpServers": []});
        let error = handle_session_new(&params, &options, None)
            .err()
            .expect("reject unknown explicit model");
        assert!(error.to_string().contains("not found"));
        options.launch.provider = Some(PRIMARY_PROVIDER.to_string());
        options.launch.model = Some(PRIMARY_MODEL.to_string());
        options.launch.thinking = Some("invalid-effort".to_string());
        assert!(handle_session_new(&params, &options, None).is_err());
        options.launch.thinking = None;
        let mut entries = options.model_registry.models().to_vec();
        for entry in &mut entries {
            entry.api_key = None;
            entry.auth_header = true;
        }
        options.model_registry = ModelRegistry::from_entries_for_tests(entries);
        options.auth = AuthStorage::empty_at(root.path().join("unreadable-auth"));
        std::fs::create_dir(root.path().join("unreadable-auth")).unwrap();
        options.launch.api_key = Some(" \t ".to_string());
        let error = handle_session_new(&params, &options, None)
            .err()
            .expect("blank override supplies no credential");
        assert!(error.to_string().contains("No API key found"));
        options.launch.api_key = Some("explicit-fixture-key".to_string());
        assert!(
            handle_session_new(&params, &options, None).is_ok(),
            "explicit key does not read the unavailable store"
        );
    });
}

#[test]
fn reopening_preserves_the_selected_branch_over_conflicting_launch_options() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    runtime.block_on(async {
        let mut options = options(
            root.path(),
            "https://acp-restore.invalid/v1",
            runtime.handle(),
            0,
        );
        options.launch = AcpLaunchOptions {
            provider: Some(FALLBACK_PROVIDER.to_string()),
            model: Some(FALLBACK_MODEL.to_string()),
            thinking: Some("high".to_string()),
            api_key: Some("restart-fixture-key".to_string()),
            models: None,
            ..AcpLaunchOptions::default()
        };
        let (mut saved, _) =
            new_acp_session(options.session_dir.as_ref(), &options.config, root.path());
        saved.append_model_change(PRIMARY_PROVIDER.to_string(), PRIMARY_MODEL.to_string());
        saved.append_thinking_level_change("low".to_string());
        let selected = saved.leaf_id.clone().unwrap();
        saved.append_model_change(FALLBACK_PROVIDER.to_string(), FALLBACK_MODEL.to_string());
        saved.append_thinking_level_change("off".to_string());
        assert!(saved.navigate_to(&selected));
        saved.save().await.unwrap();
        let path = saved.path.clone().unwrap();
        let before = std::fs::read(&path).unwrap();
        let reopened = Session::open(path.to_str().unwrap()).await.unwrap();
        let expected_id = reopened.header.id.clone();
        let (id, state) =
            build_acp_session(reopened, true, root.path().to_path_buf(), &options, None).unwrap();
        assert_eq!(id, expected_id);
        let handle = state.agent_session.as_ref().unwrap();
        assert_eq!(handle.session().agent.provider().model_id(), PRIMARY_MODEL);
        assert_eq!(
            handle.session().agent.stream_options().thinking_level,
            Some(crate::model::ThinkingLevel::Low)
        );
        assert_eq!(
            handle.session().agent.stream_options().api_key.as_deref(),
            Some("restart-fixture-key")
        );
        assert_eq!(
            handle
                .session_store()
                .try_lock()
                .unwrap()
                .leaf_id
                .as_deref(),
            Some(selected.as_str())
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "opening a branch sends no provider request or disk write"
        );
    });
}

#[test]
fn selected_ad_hoc_model_remains_registered_for_runtime_switching() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    runtime.block_on(async {
        let mut options = options(
            root.path(),
            "https://acp-ad-hoc.invalid/v1",
            runtime.handle(),
            0,
        );
        options.launch.provider = Some("openai".to_string());
        options.launch.model = Some("editor-ad-hoc-model".to_string());
        options.launch.api_key = Some("ad-hoc-fixture-key".to_string());
        let (_, state) = new_state(root.path(), &options);
        assert!(
            state
                .try_lock()
                .unwrap()
                .agent_session
                .as_ref()
                .unwrap()
                .session()
                .model_registry()
                .unwrap()
                .find("openai", "editor-ad-hoc-model")
                .is_some()
        );
        let cx = AgentCx::for_current_or_request();
        // Both wire shapes resolve through the same path as their dispatchers,
        // including the registry lookup before the durable model transition.
        apply_set_model_request(
            &state,
            &json!({ "provider": FALLBACK_PROVIDER, "model": FALLBACK_MODEL }),
            &cx,
        )
        .await
        .unwrap();
        {
            let guard = state.try_lock().unwrap();
            let config = config_options_for(&guard).unwrap().unwrap();
            assert!(
                config[0]["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| { entry["value"] == "openai/editor-ad-hoc-model" })
            );
        }
        apply_set_model_request(
            &state,
            &json!({ "model": "openai/editor-ad-hoc-model" }),
            &cx,
        )
        .await
        .unwrap();
        let saved = reopen(&state).await;
        assert_eq!(
            saved.effective_model_for_current_path(),
            Some(("openai".to_string(), "editor-ad-hoc-model".to_string()))
        );
        assert!(
            !std::fs::read_to_string(saved.path.unwrap())
                .unwrap()
                .contains("ad-hoc-fixture-key")
        );
    });
}

#[test]
fn startup_refresh_failures_only_block_the_selected_provider_without_an_override() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    runtime.block_on(async {
        let mut options = options(
            root.path(),
            "https://acp-refresh.invalid/v1",
            runtime.handle(),
            0,
        );
        let params = json!({"cwd": root.path(), "mcpServers": []});
        options.oauth_refresh_failures = vec![FALLBACK_PROVIDER.to_string()];
        assert!(handle_session_new(&params, &options, None).is_ok());
        options.oauth_refresh_failures = vec![PRIMARY_PROVIDER.to_uppercase()];
        let error = handle_session_new(&params, &options, None)
            .err()
            .expect("selected refresh failed");
        assert!(error.to_string().contains("OAuth token refresh failed"));
        options.launch.provider = Some(PRIMARY_PROVIDER.to_string());
        options.launch.model = Some(PRIMARY_MODEL.to_string());
        options.launch.api_key = Some("launch-fixture-key".to_string());
        assert!(handle_session_new(&params, &options, None).is_ok());
    });
}

#[test]
fn launch_key_survives_real_retry_failover_and_explicit_runtime_model_switch() {
    let root = tempfile::tempdir().unwrap();
    let mut server = ProviderFixture::new(vec![503, 503, 200, 200]);
    let runtime = runtime();
    runtime.block_on(async {
        let mut options = options(root.path(), &server.url, runtime.handle(), 1);
        options.config.default_provider = Some(FALLBACK_PROVIDER.to_string());
        options.config.default_model = Some(FALLBACK_MODEL.to_string());
        options.launch = AcpLaunchOptions {
            provider: Some(PRIMARY_PROVIDER.to_string()),
            model: Some(PRIMARY_MODEL.to_string()),
            thinking: Some("high".to_string()),
            api_key: Some("  pinned-launch-key  ".to_string()),
            models: None,
            ..AcpLaunchOptions::default()
        };
        options.auth.set(
            PRIMARY_PROVIDER,
            crate::auth::AuthCredential::ApiKey {
                key: "stored-primary-key".to_string(),
            },
        );
        options.auth.set(
            FALLBACK_PROVIDER,
            crate::auth::AuthCredential::ApiKey {
                key: "stored-fallback-key".to_string(),
            },
        );
        let (id, state) = new_state(root.path(), &options);
        let (_, signal) = AbortHandle::new();
        assert_eq!(
            prompt(&state, &id, text(), signal).await.0,
            ACP_STOP_REASON_END_TURN
        );
        apply_set_model(
            &state,
            PRIMARY_PROVIDER,
            PRIMARY_MODEL,
            &AgentCx::for_current_or_request(),
        )
        .await
        .unwrap();
        let (_, signal) = AbortHandle::new();
        assert_eq!(
            prompt(&state, &id, text(), signal).await.0,
            ACP_STOP_REASON_END_TURN
        );
        let saved = reopen(&state).await;
        let durable = std::fs::read_to_string(saved.path.unwrap()).unwrap();
        assert!(!durable.contains("pinned-launch-key"));
        assert!(!durable.contains("stored-primary-key"));
        assert!(!durable.contains("stored-fallback-key"));
    });
    let requests = server.finish(4);
    assert_eq!(
        requests
            .iter()
            .map(|(_, body)| body["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [PRIMARY_MODEL, PRIMARY_MODEL, FALLBACK_MODEL, PRIMARY_MODEL]
    );
    for (index, (headers, body)) in requests.into_iter().enumerate() {
        let expected_bearer = if index == 2 {
            "Bearer stored-fallback-key"
        } else {
            "Bearer pinned-launch-key"
        };
        assert!(headers.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("authorization") && value.trim() == expected_bearer
            })
        }));
        let model = body["model"].as_str().unwrap();
        assert!(headers.contains(&format!("x-acp-model: {model}")));
        assert!(!headers.contains("stored-primary-key"));
        if index == 2 {
            assert!(!headers.contains("pinned-launch-key"));
        } else {
            assert!(!headers.contains("stored-fallback-key"));
        }
    }
}

#[test]
fn acp_audio_video_ingress_reaches_provider_persistence_and_history_replay() {
    use base64::Engine as _;

    let root = tempfile::tempdir().unwrap();
    let mut server = ProviderFixture::new(vec![200]);
    let runtime = runtime();
    // Larger than the native session's inline threshold, so reopening must
    // resolve an integrity-checked attachment rather than an inline JSON copy.
    let audio = base64::engine::general_purpose::STANDARD.encode(vec![42_u8; 65_537]);
    runtime.block_on(bounded_launch_controls(async {
        let mut options = options(root.path(), &server.url, runtime.handle(), 0);
        options.launch.no_tools = true;
        options.launch.no_context_files = true;
        options.config.session_store = Some("jsonl".to_string());
        options.config.media = Some(crate::media_tools::MediaSettings {
            max_bytes: Some(70_000),
            ..crate::media_tools::MediaSettings::default()
        });
        let (id, state) = new_state(root.path(), &options);
        let input = content::extract_prompt_content(
            &[
                json!({ "type": "text", "text": "Compare the recording and clip" }),
                json!({ "type": "audio", "mimeType": "AuDiO/WAV", "data": audio,
                    "_meta": { "secret": "private-attachment-metadata" } }),
                json!({ "type": "image", "mimeType": "image/png", "data": PNG }),
                json!({ "type": "resource", "resource": {
                    "uri": "file:///unopened/editor-clip.mp4", "mimeType": "video/mp4", "blob": "BAUG",
                } }),
                json!({ "type": "text", "text": "Keep their order" }),
            ],
            &options.config,
        )
        .expect("admit ACP native media");
        let expected = serde_json::to_value(UserContent::Blocks(input.clone())).unwrap();
        let (_, signal) = AbortHandle::new();
        let (reason, updates) = prompt(&state, &id, input, signal).await;
        assert_eq!(reason, ACP_STOP_REASON_END_TURN);
        assert_eq!(updates.last().unwrap()["result"]["stopReason"], "end_turn");

        let saved = reopen(&state).await;
        let messages = saved.to_messages_for_current_path();
        let users = messages
            .iter()
            .filter_map(|message| match message {
                Message::User(user) => Some(user),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(users.len(), 1);
        assert_eq!(serde_json::to_value(&users[0].content).unwrap(), expected);
        let durable = std::fs::read_to_string(saved.path.as_ref().unwrap()).unwrap();
        assert!(durable.contains("$piBlob"));
        assert!(!durable.contains(&audio));
        assert!(!durable.contains("private-attachment-metadata"));

        // The restored native input remains usable by Gemini, even though the
        // current provider's transport emits explicit media-omission notices.
        let gemini = serde_json::to_value(
            crate::providers::gemini::convert_user_content_to_parts(&users[0].content),
        )
        .unwrap();
        assert_eq!(gemini[1]["inline_data"]["mimeType"], "audio/wav");
        assert_eq!(gemini[1]["inline_data"]["data"], audio);
        assert_eq!(gemini[2]["inline_data"]["data"], PNG);
        assert_eq!(gemini[4]["inline_data"]["mimeType"], "video/mp4");
        assert_eq!(gemini[4]["inline_data"]["data"], "BAUG");

        let (tx, rx) = std::sync::mpsc::sync_channel(32);
        let tx = AcpOutput::new(tx);
        let cx = AgentCx::for_current_or_request();
        let permission_client = AcpPermissionClient {
            out_tx: tx.clone(),
            pending: Arc::clone(&tx.shared.pending_permissions),
            request_counter: Arc::new(AtomicU64::new(0)),
            timeout: Duration::from_secs(1),
            cx: cx.clone(),
        };
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        history::load(
            &json!({ "sessionId": id, "cwd": root.path(), "mcpServers": [] }),
            &options,
            &permission_client,
            &sessions,
            &cx,
            &tx,
            true,
        )
        .await
        .expect("reload and replay durable ACP media");
        let replay = rx
            .try_iter()
            .map(|line| serde_json::from_str::<Value>(&line).unwrap())
            .filter(|frame| frame["params"]["update"]["sessionUpdate"] == "user_message_chunk")
            .map(|frame| frame["params"]["update"]["content"].clone())
            .collect::<Vec<_>>();
        assert_eq!(replay.len(), 6);
        assert_eq!(replay[0]["text"], "Compare the recording and clip");
        assert_eq!(
            replay[1],
            json!({ "type": "audio", "mimeType": "audio/wav", "data": audio }),
        );
        assert_eq!(replay[2]["type"], "image");
        assert_eq!(replay[2]["data"], PNG);
        assert!(
            replay[3]["text"]
                .as_str()
                .unwrap()
                .contains("file:///unopened/editor-clip.mp4")
        );
        assert_eq!(replay[4]["type"], "resource");
        assert_eq!(replay[4]["resource"]["mimeType"], "video/mp4");
        assert_eq!(replay[4]["resource"]["blob"], "BAUG");
        assert_eq!(replay[5]["text"], "Keep their order");
    }));
    let requests = server.finish(1);
    let wire = user_content(&requests[0].1);
    assert_eq!(wire.as_array().unwrap().len(), 6);
    assert_eq!(wire[0]["text"], "Compare the recording and clip");
    assert_eq!(
        wire[1]["text"],
        "[media omitted: unnamed, audio/wav, 64.0 KB]",
    );
    assert_eq!(
        wire[2]["image_url"]["url"],
        format!("data:image/png;base64,{PNG}"),
    );
    assert_eq!(wire[4]["text"], "[media omitted: unnamed, video/mp4, 3 B]");
    assert_eq!(wire[5]["text"], "Keep their order");
    let wire = wire.to_string();
    assert!(!wire.contains(&audio));
    assert!(!wire.contains("private-attachment-metadata"));
    assert!(!wire.contains("BAUG"));
}

#[test]
fn configured_acp_retry_preserves_ordered_native_input_and_one_durable_turn() {
    let root = tempfile::tempdir().unwrap();
    let mut server = ProviderFixture::new(vec![503, 200]);
    let runtime = runtime();
    runtime.block_on(async {
        let options = options(root.path(), &server.url, runtime.handle(), 1);
        let (id, state) = new_state(root.path(), &options);
        let observed = events(&state);
        let input = native_content();
        let (_, signal) = AbortHandle::new();
        let (reason, updates) = prompt(&state, &id, input.clone(), signal).await;
        assert_eq!(reason, ACP_STOP_REASON_END_TURN);
        assert_eq!(updates.last().unwrap()["result"]["stopReason"], "end_turn");
        assert!(updates.iter().any(|update| {
            update["params"]["update"]["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("attempt 1/1"))
        }));
        assert!(
            configuration_updates(&updates).is_empty(),
            "same-model retry does not alter selectors"
        );
        assert_no_private_diagnostics(&updates);
        let saved = reopen(&state).await;
        let messages = saved.to_messages_for_current_path();
        let users = messages
            .iter()
            .filter_map(|message| match message {
                Message::User(user) => Some(user),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(users.len(), 1, "retry must not append the input twice");
        assert_eq!(
            serde_json::to_value(&users[0].content).unwrap(),
            serde_json::to_value(UserContent::Blocks(input)).unwrap()
        );
        let observed = observed.lock().unwrap();
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event, AgentEvent::AgentStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event, AgentEvent::AgentEnd { error: None, .. }))
                .count(),
            1
        );
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event, AgentEvent::AutoRetryStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event, AgentEvent::AutoRetryEnd { success: true, .. }))
                .count(),
            1
        );
    });
    let requests = server.finish(2);
    assert_eq!(requests[0].1["model"], PRIMARY_MODEL);
    assert_eq!(requests[1].1["model"], PRIMARY_MODEL);
    assert_eq!(user_content(&requests[0].1), user_content(&requests[1].1));
    assert!(
        serde_json::to_string(user_content(&requests[0].1))
            .unwrap()
            .contains(PNG)
    );
    assert!(requests.iter().all(
        |(headers, _)| headers.contains("editor-primary-fixture-key")
            && headers.contains("x-acp-model: editor-primary")
    ));
}

#[test]
fn configured_acp_failover_and_reopen_restore_the_primary_and_editor_selectors() {
    let root = tempfile::tempdir().unwrap();
    let mut server = ProviderFixture::new(vec![503, 200, 200]);
    let runtime = runtime();
    runtime.block_on(async {
        let options = options(root.path(), &server.url, runtime.handle(), 0);
        let (id, state) = new_state(root.path(), &options);
        let observed = events(&state);
        let (_, signal) = AbortHandle::new();
        let (reason, updates) = prompt(&state, &id, native_content(), signal).await;
        assert_eq!(reason, ACP_STOP_REASON_END_TURN);
        let selected = configuration_updates(&updates);
        assert_eq!(
            selected.len(),
            1,
            "a committed fallback publishes one complete selection"
        );
        assert_eq!(
            selected[0][0]["currentValue"],
            format!("{FALLBACK_PROVIDER}/{FALLBACK_MODEL}")
        );
        assert_eq!(selected[0][1]["currentValue"], "off");
        assert_eq!(selected[0][0]["options"].as_array().unwrap().len(), 2);
        assert_no_private_diagnostics(&updates);
        {
            let observed = observed.lock().unwrap();
            assert_eq!(
                observed
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::FailoverStart { .. }))
                    .count(),
                1
            );
            assert_eq!(
                observed
                    .iter()
                    .filter(|event| matches!(
                        event,
                        AgentEvent::FailoverEnd {
                            restored_primary: false,
                            success: true,
                            ..
                        }
                    ))
                    .count(),
                1
            );
            assert!(matches!(
                observed.last(),
                Some(AgentEvent::AgentEnd { error: None, .. })
            ));
        }
        let saved = reopen(&state).await;
        assert!(
            saved
                .active_failover_provenance_for_current_path()
                .is_some()
        );
        let (restored_id, restored) =
            build_acp_session(saved, true, root.path().to_path_buf(), &options, None).unwrap();
        assert_eq!(restored_id, id);
        let restored = Arc::new(Mutex::new(restored));
        let (_, signal) = AbortHandle::new();
        let (reason, updates) = prompt(&restored, &id, text(), signal).await;
        assert_eq!(reason, ACP_STOP_REASON_END_TURN);
        let selected = configuration_updates(&updates);
        assert_eq!(selected.len(), 1);
        assert_eq!(
            selected[0][0]["currentValue"],
            format!("{PRIMARY_PROVIDER}/{PRIMARY_MODEL}")
        );
        assert_eq!(selected[0][1]["currentValue"], "high");
        assert!(updates.iter().any(|update| {
            update["params"]["update"]["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("Restored primary provider"))
        }));
        let saved = reopen(&restored).await;
        assert!(
            saved
                .active_failover_provenance_for_current_path()
                .is_none()
        );
        assert_eq!(
            saved
                .to_messages_for_current_path()
                .iter()
                .filter(|message| matches!(message, Message::User(_)))
                .count(),
            2
        );
    });
    let requests = server.finish(3);
    assert_eq!(
        requests
            .iter()
            .map(|(_, body)| body["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [PRIMARY_MODEL, FALLBACK_MODEL, PRIMARY_MODEL]
    );
    assert_eq!(user_content(&requests[0].1), user_content(&requests[1].1));
    for (index, expected) in [(0, 2_048_u64), (1, 512), (2, 2_048)] {
        let body = &requests[index].1;
        let max_tokens = body
            .get("max_tokens")
            .or_else(|| body.get("max_completion_tokens"))
            .and_then(Value::as_u64)
            .expect("active model output limit");
        assert_eq!(max_tokens, expected);
    }
    assert!(requests[1].0.contains("editor-backup-fixture-key"));
    assert!(requests[1].0.contains("x-acp-model: editor-backup"));
}

#[test]
fn configured_acp_explicitly_choosing_the_fallback_ends_automatic_restoration() {
    let root = tempfile::tempdir().unwrap();
    let mut server = ProviderFixture::new(vec![503, 200, 200]);
    let runtime = runtime();
    runtime.block_on(async {
        let options = options(root.path(), &server.url, runtime.handle(), 0);
        let (id, state) = new_state(root.path(), &options);
        let (_, signal) = AbortHandle::new();
        assert_eq!(
            prompt(&state, &id, text(), signal).await.0,
            ACP_STOP_REASON_END_TURN
        );
        apply_set_model(
            &state,
            FALLBACK_PROVIDER,
            FALLBACK_MODEL,
            &AgentCx::for_current_or_request(),
        )
        .await
        .unwrap();
        assert!(
            reopen(&state)
                .await
                .active_failover_provenance_for_current_path()
                .is_none()
        );
        let (_, signal) = AbortHandle::new();
        let (reason, updates) = prompt(&state, &id, text(), signal).await;
        assert_eq!(reason, ACP_STOP_REASON_END_TURN);
        assert!(configuration_updates(&updates).is_empty());
        assert!(
            !serde_json::to_string(&updates)
                .unwrap()
                .contains("Restored primary")
        );
    });
    let requests = server.finish(3);
    assert_eq!(requests[2].1["model"], FALLBACK_MODEL);
}

#[test]
fn configured_acp_disabled_retries_and_authentication_failures_never_reenter() {
    for (status, enabled) in [(503, false), (401, true)] {
        let root = tempfile::tempdir().unwrap();
        let mut server = ProviderFixture::new(vec![status, 200]);
        let runtime = runtime();
        runtime.block_on(async {
            let mut options = options(root.path(), &server.url, runtime.handle(), 3);
            options.config.retry.as_mut().unwrap().enabled = Some(enabled);
            let (id, state) = new_state(root.path(), &options);
            let (_, signal) = AbortHandle::new();
            let (_, updates) = prompt(&state, &id, text(), signal).await;
            assert_no_private_diagnostics(&updates);
            let rendered = serde_json::to_string(&updates).unwrap();
            assert!(rendered.contains("Error:"));
            assert!(!rendered.contains("Retrying provider request"));
            assert!(!rendered.contains("Provider fallback"));
            assert!(configuration_updates(&updates).is_empty());
        });
        server.finish(1);
    }
}

#[test]
fn configured_acp_abort_during_retry_keeps_the_handle_reusable_without_reentry() {
    let root = tempfile::tempdir().unwrap();
    let mut server = ProviderFixture::new(vec![503, 200]);
    let runtime = runtime();
    runtime.block_on(async {
        let mut options = options(root.path(), &server.url, runtime.handle(), 2);
        let policy = options.config.retry.as_mut().unwrap();
        policy.base_delay_ms = Some(10_000);
        policy.max_delay_ms = Some(10_000);
        let (id, state) = new_state(root.path(), &options);
        let (abort, signal) = AbortHandle::new();
        let retry_started = Arc::new(AtomicBool::new(false));
        let observed_retry = Arc::clone(&retry_started);
        state
            .try_lock()
            .unwrap()
            .agent_session
            .as_ref()
            .unwrap()
            .subscribe(move |event| {
                if matches!(event, AgentEvent::AutoRetryStart { .. }) {
                    observed_retry.store(true, Ordering::SeqCst);
                }
            });
        let mut abort_after_pending = false;
        let pending = prompt(&state, &id, text(), signal);
        let mut pending = std::pin::pin!(pending);
        let (reason, updates) = std::future::poll_fn(|cx| {
            let result = pending.as_mut().poll(cx);
            if result.is_pending() && retry_started.load(Ordering::SeqCst) && !abort_after_pending {
                // AutoRetryStart is synchronous. Only cancel after the turn
                // has yielded in its delay, so this covers pending backoff
                // cancellation without relying on a wall-clock sleep race.
                abort_after_pending = true;
                abort.abort();
                cx.waker().wake_by_ref();
            }
            result
        })
        .await;
        assert!(
            abort_after_pending,
            "the retry delay yielded before cancellation"
        );
        assert_eq!(reason, ACP_STOP_REASON_CANCELLED);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
        assert!(!serde_json::to_string(&updates).unwrap().contains("Error:"));
        assert!(state.try_lock().unwrap().agent_session.is_some());
        let (_, signal) = AbortHandle::new();
        assert_eq!(
            prompt(&state, &id, text(), signal).await.0,
            ACP_STOP_REASON_END_TURN
        );
    });
    server.finish(2);
}

#[test]
fn configured_acp_retry_save_failure_blocks_all_later_provider_work() {
    let root = tempfile::tempdir().unwrap();
    let mut server = ProviderFixture::new(vec![503, 200, 200]);
    let runtime = runtime();
    runtime.block_on(async {
        let options = options(root.path(), &server.url, runtime.handle(), 1);
        let (id, state) = new_state(root.path(), &options);
        let stored = session_store(&state);
        let for_fault = Arc::clone(&stored);
        let original = Arc::new(StdMutex::new(None));
        let saved_path = Arc::clone(&original);
        let invalid = root.path().to_path_buf();
        state.try_lock().unwrap().agent_session.as_ref().unwrap().subscribe(move |event| {
            if matches!(event, AgentEvent::MessageEnd { message: Message::Assistant(ref message) }
                if message.stop_reason == StopReason::Stop)
            {
                let mut session = for_fault.try_lock().unwrap();
                *saved_path.lock().unwrap() = session.path.clone();
                session.path = Some(invalid.clone());
            }
        });
        let (reason, updates) = prompt(&state, &id, native_content(), AbortHandle::new().1).await;
        assert_eq!(reason, ACP_STOP_REASON_ERROR);
        assert!(
            serde_json::to_string(&updates)
                .unwrap()
                .contains("Session persistence failed")
        );
        stored.try_lock().unwrap().path = original.lock().unwrap().clone();
        let before =
            serde_json::to_value(stored.try_lock().unwrap().to_messages_for_current_path())
                .unwrap();
        let (_, updates) = prompt(&state, &id, text(), AbortHandle::new().1).await;
        assert!(
            serde_json::to_string(&updates)
                .unwrap()
                .contains("Session persistence failed")
        );
        assert_eq!(
            before,
            serde_json::to_value(stored.try_lock().unwrap().to_messages_for_current_path())
                .unwrap()
        );
        assert_no_private_diagnostics(&updates);
    });
    server.finish(2);
}

#[test]
fn acp_cli_resolves_default_session_dir_until_no_session_is_set() {
    use clap::Parser as _;

    let default_cli = crate::cli::Cli::parse_from(["pi", "--acp"]);
    assert_eq!(
        resolve_acp_session_dir(&default_cli),
        Some(Config::sessions_dir()),
        "pi --acp must persist per-project sessions by default"
    );

    let custom_cli =
        crate::cli::Cli::parse_from(["pi", "--acp", "--session-dir", "/tmp/acp-custom"]);
    assert_eq!(
        resolve_acp_session_dir(&custom_cli),
        Some(PathBuf::from("/tmp/acp-custom"))
    );

    let ephemeral_cli = crate::cli::Cli::parse_from(["pi", "--acp", "--no-session"]);
    assert_eq!(
        resolve_acp_session_dir(&ephemeral_cli),
        None,
        "pi --acp --no-session must remain in-memory"
    );
}

#[test]
fn acp_boots_workspace_extensions_honoring_trust_and_persists_across_restart() {
    let root = tempfile::tempdir().unwrap();
    let global_dir = root.path().join("global");
    let workspace = root.path().join("workspace");
    let sessions_root = root.path().join("sessions");
    let ext_dir = workspace.join(".pi/extensions");
    std::fs::create_dir_all(&global_dir).unwrap();
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::create_dir_all(&sessions_root).unwrap();

    std::fs::write(
        ext_dir.join("acp_probe.mjs"),
        r#"export default function (pi) {
            pi.registerFlag("acp-probe-tag", {
                type: "string",
                default: "extension-default"
            });
            let startupTag = "startup-not-run";
            let sessionTag = "session-start-not-run";
            pi.on("startup", () => {
                startupTag = pi.getFlag("acp-probe-tag");
            });
            pi.on("session_start", () => {
                sessionTag = pi.getFlag("acp-probe-tag");
            });
            pi.registerCommand("acp-status", {
                description: "Read the extension's startup state without a provider turn",
                handler: args => `ACP_COMMAND:${args}:${startupTag}:${sessionTag}`
            });
            pi.registerTool({
                name: "acp_ext_probe",
                label: "ACP Extension Probe",
                description: "Echoes a marker from an ACP workspace extension",
                parameters: {
                    type: "object",
                    properties: {
                        tag: { type: "string" }
                    },
                    required: ["tag"]
                },
                async execute(_toolCallId, params) {
                    return {
                        content: [{ type: "text", text: `EXT_OK:${params.tag}:${startupTag}:${sessionTag}` }]
                    };
                }
            });
        }"#,
    )
    .unwrap();

    let cwd = workspace.canonicalize().unwrap();
    let mut provider = ProviderFixture::with_replies(
        vec![200, 200],
        vec![
            Some(("acp_ext_probe", json!({ "tag": "zed-turn-1" }))),
            None,
        ],
    );

    let runtime = runtime();

    // 1. Planted negative: untrusted workspace must NOT load `.pi/extensions/acp_probe.mjs`.
    runtime.block_on(bounded_launch_controls(async {
        let mut untrusted_opts = options(&cwd, &provider.url, runtime.handle(), 0);
        untrusted_opts.session_dir = Some(sessions_root.clone());
        untrusted_opts.settings_source = Some(AcpSettingsSource {
            global_dir: global_dir.clone(),
            config_override: None,
            workspace_trust_env: Some("0".to_string()),
            launch_cwd: cwd.clone(),
        });
        let (input, incoming) = asupersync::channel::mpsc::channel(16);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let mut transcript = Vec::new();
        let editor = async {
            let _ = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                1,
                "initialize",
                json!({ "protocolVersion": 1 }),
            )
            .await;
            let created = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                2,
                "session/new",
                json!({ "cwd": cwd, "mcpServers": [] }),
            )
            .await;
            assert!(created.get("error").is_none(), "{created}");
            input
                .try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string())
                .unwrap();
        };
        let (served, ()) = futures::join!(
            Box::pin(run(untrusted_opts, incoming, AcpOutput::new(output))),
            editor
        );
        served.unwrap();
    }));

    // 2. Trusted workspace loads `.pi/extensions/acp_probe.mjs`, executes tool call, and persists session.
    let (session_id, prompt_updates) = runtime.block_on(bounded_launch_controls(async {
        let mut trusted_opts = options(&cwd, &provider.url, runtime.handle(), 0);
        trusted_opts.session_dir = Some(sessions_root.clone());
        trusted_opts.launch.tools = Some("current_time".to_string());
        trusted_opts.launch.no_mcp = true;
        trusted_opts.launch.no_context_files = true;
        trusted_opts.launch.extension_flags = vec![crate::cli::ExtensionCliFlag {
            name: "acp-probe-tag".to_string(),
            value: Some("host-startup-value".to_string()),
        }];
        trusted_opts.settings_source = Some(AcpSettingsSource {
            global_dir: global_dir.clone(),
            config_override: None,
            workspace_trust_env: Some("1".to_string()),
            launch_cwd: cwd.clone(),
        });
        let (input, incoming) = asupersync::channel::mpsc::channel(16);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let mut transcript = Vec::new();
        let editor = async {
            let _ = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                1,
                "initialize",
                json!({ "protocolVersion": 1 }),
            )
            .await;
            let created = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                2,
                "session/new",
                json!({ "cwd": cwd, "mcpServers": [] }),
            )
            .await;
            let id = created["result"]["sessionId"].as_str().unwrap().to_string();
            let command = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                3,
                "session/prompt",
                json!({ "sessionId": id, "prompt": [{ "type": "text", "text": "/acp-status new" }] }),
            )
            .await;
            assert_eq!(command["result"]["stopReason"], "end_turn", "{command}");
            approved_fork_prompt(
                &input,
                &outgoing,
                &mut transcript,
                4,
                &id,
                "Run the extension probe",
            )
            .await;
            input
                .try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string())
                .unwrap();
            (id, transcript)
        };
        let (served, out) = futures::join!(
            Box::pin(run(trusted_opts, incoming, AcpOutput::new(output))),
            editor
        );
        served.unwrap();
        out
    }));

    let wire_log = serde_json::to_string(&prompt_updates).unwrap();
    assert!(
        wire_log.contains("EXT_OK:zed-turn-1:host-startup-value:host-startup-value"),
        "extension flags must reach startup and session_start before ACP tool execution: {wire_log}"
    );
    assert!(wire_log.contains("ACP_COMMAND:new:host-startup-value:host-startup-value"));
    assert!(
        prompt_updates.iter().any(|value| {
            value["params"]["update"]["sessionUpdate"] == "available_commands_update"
                && value["params"]["update"]["availableCommands"]
                    .as_array()
                    .is_some_and(|commands| {
                        commands
                            .iter()
                            .any(|command| command["name"] == "acp-status")
                    })
        }),
        "an extension-only session advertises its actual commands"
    );

    // 3. Restart ACP server and verify `session/load` replays persisted transcript + re-boots extension!
    let reload_updates = runtime.block_on(bounded_launch_controls(async {
        let mut restarted_opts = options(&cwd, &provider.url, runtime.handle(), 0);
        restarted_opts.session_dir = Some(sessions_root.clone());
        restarted_opts.launch.tools = Some("current_time".to_string());
        restarted_opts.launch.no_mcp = true;
        restarted_opts.launch.no_context_files = true;
        restarted_opts.settings_source = Some(AcpSettingsSource {
            global_dir: global_dir.clone(),
            config_override: None,
            workspace_trust_env: Some("1".to_string()),
            launch_cwd: cwd.clone(),
        });
        let (input, incoming) = asupersync::channel::mpsc::channel(16);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let mut transcript = Vec::new();
        let editor = async {
            let _ = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                1,
                "initialize",
                json!({ "protocolVersion": 1 }),
            )
            .await;
            let loaded = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                2,
                "session/load",
                json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] }),
            )
            .await;
            assert!(
                loaded.get("error").is_none(),
                "session/load failed: {loaded}"
            );
            let command = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                3,
                "session/prompt",
                json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "/acp-status restored" }] }),
            )
            .await;
            assert_eq!(command["result"]["stopReason"], "end_turn", "{command}");
            let forked = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                4,
                "session/fork",
                json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] }),
            )
            .await;
            let fork_id = forked["result"]["sessionId"]
                .as_str()
                .expect("forked session");
            let command = discovery_request(
                &input,
                &outgoing,
                &mut transcript,
                5,
                "session/prompt",
                json!({ "sessionId": fork_id, "prompt": [{ "type": "text", "text": "/acp-status forked" }] }),
            )
            .await;
            assert_eq!(command["result"]["stopReason"], "end_turn", "{command}");
            input
                .try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string())
                .unwrap();
            transcript
        };
        let (served, out) = futures::join!(
            Box::pin(run(restarted_opts, incoming, AcpOutput::new(output))),
            editor
        );
        served.unwrap();
        out
    }));
    let reload_wire = serde_json::to_string(&reload_updates).unwrap();
    assert!(
        reload_wire.contains("Run the extension probe")
            && reload_wire.contains("Recovered editor answer"),
        "restarted ACP server must replay persisted transcript: {reload_wire}"
    );
    assert!(reload_wire.contains("ACP_COMMAND:restored:extension-default:extension-default"));
    assert!(reload_wire.contains("ACP_COMMAND:forked:extension-default:extension-default"));
    assert_eq!(
        reload_updates
            .iter()
            .filter(|value| {
                value["params"]["update"]["sessionUpdate"] == "available_commands_update"
                    && value["params"]["update"]["availableCommands"]
                        .as_array()
                        .is_some_and(|commands| {
                            commands
                                .iter()
                                .any(|command| command["name"] == "acp-status")
                        })
            })
            .count(),
        2,
        "load and fork both restore command discovery"
    );

    provider.finish(2);
}


async fn next_planning_frame(output: &std::sync::mpsc::Receiver<String>) -> Value {
    loop {
        match output.try_recv() {
            Ok(line) => return serde_json::from_str(&line).expect("valid ACP planning frame"),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                sleep(wall_now(), Duration::from_millis(1)).await;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("ACP stopped before completing the planning operation");
            }
        }
    }
}

fn answer_planning_permission(
    input: &asupersync::channel::mpsc::Sender<String>,
    frame: &Value,
    option: &str,
) {
    input.try_send(json!({
        "jsonrpc": "2.0", "id": frame["id"],
        "result": {"outcome": {"outcome": "selected", "optionId": option}},
    }).to_string()).unwrap();
}

#[test]
#[allow(clippy::too_many_lines)]
fn dispatcher_plan_review_gates_writes_and_continues_without_a_synthetic_user_message() {
    for extension_command in [false, true] {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let sessions_root = root.path().join("sessions");
    let global_dir = root.path().join("global");
    if extension_command {
        std::fs::create_dir_all(&global_dir).unwrap();
        let extensions = root.path().join(".pi/extensions");
        std::fs::create_dir_all(&extensions).unwrap();
        std::fs::write(extensions.join("plan_command.mjs"), r#"
            export default function(pi) {
                pi.registerCommand("build-plan", { handler: async () => {
                    pi.sendUserMessage("PLAN-REVIEW-USER-INPUT");
                }});
            }
        "#).unwrap();
    }
    let plan = "Create approved.txt with reviewed content, then read it to verify the implementation.";
    let mut provider = ProviderFixture::with_replies(
        vec![200; 5],
        vec![
            Some(("write", json!({"path": "blocked.txt", "content": "must not run"}))),
            Some(("submit_plan", json!({"plan": plan, "files": ["approved.txt"]}))),
            None,
            Some(("write", json!({"path": "approved.txt", "content": "reviewed content"}))),
            None,
        ],
    );
    let runtime = runtime();
    let (session_id, source_path, fork_path) = runtime.block_on(bounded_launch_controls(async {
        let mut options = options(root.path(), &provider.url, runtime.handle(), 0);
        options.session_dir = Some(sessions_root.clone());
        options.launch.tools = Some("read,write".into());
        options.launch.no_mcp = true;
        options.launch.no_context_files = true;
        if extension_command {
            options.settings_source = Some(AcpSettingsSource {
                global_dir: global_dir.clone(),
                config_override: None,
                workspace_trust_env: Some("1".into()),
                launch_cwd: cwd.clone(),
            });
        }
        let (input, incoming) = asupersync::channel::mpsc::channel(16);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let editor = async {
            let mut transcript = Vec::new();
            discovery_request(&input, &outgoing, &mut transcript, 1, "initialize", json!({"protocolVersion": 1})).await;
            let created = discovery_request(&input, &outgoing, &mut transcript, 2, "session/new", json!({"cwd": cwd})).await;
            let session_id = created["result"]["sessionId"].as_str().unwrap().to_string();
            assert_eq!(created["result"]["modes"]["currentModeId"], "code");
            let selected = discovery_request(&input, &outgoing, &mut transcript, 3, "session/set_config_option", json!({
                "sessionId": session_id, "configId": "mode", "value": "plan",
            })).await;
            assert!(selected.get("error").is_none(), "{selected}");
            assert_eq!(
                selected["result"]["configOptions"].as_array().unwrap().iter()
                    .find(|option| option["id"] == "mode").unwrap()["currentValue"],
                "plan",
            );
            assert!(transcript.iter().any(|frame|
                frame["params"]["update"]["sessionUpdate"] == "current_mode_update"
                    && frame["params"]["update"]["currentModeId"] == "plan"
            ));
            input.try_send(json!({
                "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
                "params": {"sessionId": session_id, "prompt": [{
                    "type": "text",
                    "text": if extension_command { "/build-plan" } else { "PLAN-REVIEW-USER-INPUT" },
                }]},
            }).to_string()).unwrap();
            let mut submission_permissions = 0;
            let mut review_permissions = 0;
            let mut write_permissions = 0;
            loop {
                let frame = next_planning_frame(&outgoing).await;
                if frame["method"] == "session/request_permission" {
                    assert_eq!(frame["params"]["sessionId"], session_id);
                    match frame["params"]["toolCall"]["kind"].as_str() {
                        Some("switch_mode") => {
                            review_permissions += 1;
                            assert_eq!(provider.requests.lock().unwrap().len(), 3);
                            assert!(!cwd.join("blocked.txt").exists());
                            assert!(!cwd.join("approved.txt").exists());
                            let shown = frame["params"]["toolCall"]["content"][0]["content"]["text"].as_str().unwrap();
                            assert!(shown.contains(plan), "the entire canonical plan must be reviewed");
                            assert!(shown.contains("approved.txt"), "declared file scopes must be visible");
                            assert_eq!(frame["params"]["options"].as_array().unwrap().len(), 2);
                        }
                        Some("edit") => {
                            write_permissions += 1;
                            assert_eq!(review_permissions, 1, "implementation permission follows plan approval");
                            assert_eq!(frame["params"]["toolCall"]["rawInput"]["path"], "approved.txt");
                            assert!(!cwd.join("approved.txt").exists(), "plan approval is not tool approval");
                        }
                        _ => {
                            submission_permissions += 1;
                            assert_eq!(frame["params"]["toolCall"]["title"], "submit_plan");
                        }
                    }
                    answer_planning_permission(&input, &frame, ACP_PERMISSION_ALLOW_ONCE);
                } else if frame.get("id") == Some(&json!(4)) {
                    assert_eq!(frame["result"]["stopReason"], ACP_STOP_REASON_END_TURN);
                    break;
                }
                transcript.push(frame);
            }
            assert_eq!((submission_permissions, review_permissions, write_permissions), (1, 1, 1));
            assert!(!cwd.join("blocked.txt").exists(), "planning writes must fail before permission is requested");
            assert_eq!(std::fs::read_to_string(cwd.join("approved.txt")).unwrap(), "reviewed content");
            assert!(transcript.iter().any(|frame|
                frame["params"]["update"]["sessionUpdate"] == "current_mode_update"
                    && frame["params"]["update"]["currentModeId"] == "code"
            ));
            let forked = discovery_request(&input, &outgoing, &mut transcript, 5, "session/fork", json!({
                "sessionId": session_id, "cwd": cwd,
            })).await;
            assert!(forked.get("error").is_none(), "{forked}");
            assert_eq!(forked["result"]["modes"]["currentModeId"], "plan", "a fork does not inherit approval");
            let fork_id = forked["result"]["sessionId"].as_str().unwrap();
            let index = crate::session_index::SessionIndex::for_sessions_root(&sessions_root);
            index.reindex_all().unwrap();
            let rows = index.list_sessions(None).unwrap();
            let source_path = rows.iter().find(|row| row.id == session_id).unwrap().path.clone();
            let fork_path = rows.iter().find(|row| row.id == fork_id).unwrap().path.clone();
            let saved = Session::open(&source_path).await.unwrap();
            assert_eq!(crate::plan::replayed_plan_mode(&saved), crate::plan::PlanMode::Approved);
            assert_eq!(saved.to_messages_for_current_path().iter().filter(|message| matches!(message, Message::User(_))).count(), 1);
            let saved_fork = Session::open(&fork_path).await.unwrap();
            assert_eq!(crate::plan::replayed_plan_mode(&saved_fork), crate::plan::PlanMode::PendingApproval);
            input.try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string()).unwrap();
            (session_id, source_path, fork_path)
        };
        let (served, result) = futures::join!(Box::pin(run(options, incoming, AcpOutput::new(output))), editor);
        served.unwrap();
        result
    }));
    let requests = provider.finish(5);
    assert!(requests[0].1["messages"].to_string().contains("ACP session mode: Plan."));
    assert!(requests[3].1["messages"].to_string().contains("Approved Plan (execute this)"));
    assert!(requests[3].1["messages"].to_string().contains(plan));
    assert!(!requests[3].1["messages"].to_string().contains("ACP session mode: Plan."), "approved implementation must not inherit transient planning instructions");
    assert!(requests[0].1["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == "submit_plan"));

    runtime.block_on(bounded_launch_controls(async {
        let mut options = options(root.path(), &provider.url, runtime.handle(), 0);
        options.session_dir = Some(sessions_root.clone());
        options.launch.no_mcp = true;
        options.launch.no_context_files = true;
        let (input, incoming) = asupersync::channel::mpsc::channel(8);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let editor = async {
            let mut transcript = Vec::new();
            discovery_request(&input, &outgoing, &mut transcript, 1, "initialize", json!({"protocolVersion": 1})).await;
            let resumed = discovery_request(&input, &outgoing, &mut transcript, 2, "session/resume", json!({
                "sessionId": session_id, "cwd": cwd,
            })).await;
            assert!(resumed.get("error").is_none(), "{resumed}");
            assert_eq!(resumed["result"]["modes"]["currentModeId"], "plan");
            assert_eq!(crate::plan::replayed_plan_mode(&Session::open(&source_path).await.unwrap()), crate::plan::PlanMode::PendingApproval);
            assert_eq!(crate::plan::replayed_plan_mode(&Session::open(&fork_path).await.unwrap()), crate::plan::PlanMode::PendingApproval);
            input.try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string()).unwrap();
        };
        let (served, ()) = futures::join!(Box::pin(run(options, incoming, AcpOutput::new(output))), editor);
        served.unwrap();
    }));
    }
}

#[test]
fn dispatcher_plan_rejection_unknown_option_and_cancel_never_start_implementation() {
    for choice in [ACP_PERMISSION_REJECT_ONCE, "allow-always", "cancel"] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let mut provider = ProviderFixture::with_replies(
            vec![200; 3],
            vec![
                Some(("submit_plan", json!({"plan": "Create should-not-exist.txt and verify its exact content."}))),
                None,
                Some(("write", json!({"path": "should-not-exist.txt", "content": "forbidden"}))),
            ],
        );
        let runtime = runtime();
        runtime.block_on(bounded_launch_controls(async {
            let mut options = options(root.path(), &provider.url, runtime.handle(), 0);
            options.launch.tools = Some("write".into());
            options.launch.no_mcp = true;
            options.launch.no_context_files = true;
            let (input, incoming) = asupersync::channel::mpsc::channel(16);
            let (output, outgoing) = std::sync::mpsc::sync_channel(128);
            let editor = async {
                let mut transcript = Vec::new();
                discovery_request(&input, &outgoing, &mut transcript, 1, "initialize", json!({"protocolVersion": 1})).await;
                let created = discovery_request(&input, &outgoing, &mut transcript, 2, "session/new", json!({"cwd": cwd})).await;
                let session_id = created["result"]["sessionId"].as_str().unwrap().to_string();
                let selected = discovery_request(&input, &outgoing, &mut transcript, 3, "session/set_mode", json!({
                    "sessionId": session_id, "modeId": "plan",
                })).await;
                assert!(selected.get("error").is_none(), "{selected}");
                input.try_send(json!({
                    "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
                    "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "Propose the implementation"}]},
                }).to_string()).unwrap();
                let mut reviews = 0;
                loop {
                    let frame = next_planning_frame(&outgoing).await;
                    if frame["method"] == "session/request_permission" {
                        if frame["params"]["toolCall"]["kind"] == "switch_mode" {
                            reviews += 1;
                            assert_eq!(provider.requests.lock().unwrap().len(), 2);
                            if choice == "cancel" {
                                input.try_send(json!({
                                    "jsonrpc": "2.0", "method": "session/cancel",
                                    "params": {"sessionId": session_id},
                                }).to_string()).unwrap();
                                // An approval in the input queue after cancellation must be inert.
                                answer_planning_permission(&input, &frame, ACP_PERMISSION_ALLOW_ONCE);
                            } else {
                                answer_planning_permission(&input, &frame, choice);
                            }
                        } else {
                            assert_eq!(frame["params"]["toolCall"]["title"], "submit_plan");
                            answer_planning_permission(&input, &frame, ACP_PERMISSION_ALLOW_ONCE);
                        }
                    } else if frame.get("id") == Some(&json!(4)) {
                        assert_eq!(frame["result"]["stopReason"], if choice == "cancel" {
                            ACP_STOP_REASON_CANCELLED
                        } else {
                            ACP_STOP_REASON_END_TURN
                        });
                        break;
                    }
                }
                assert_eq!(reviews, 1);
                assert!(!cwd.join("should-not-exist.txt").exists());
                let resumed = discovery_request(&input, &outgoing, &mut transcript, 5, "session/resume", json!({
                    "sessionId": session_id, "cwd": cwd,
                })).await;
                assert_eq!(resumed["result"]["modes"]["currentModeId"], "plan");
                let index = crate::session_index::SessionIndex::for_sessions_root(root.path());
                index.reindex_all().unwrap();
                let rows = index.list_sessions(None).unwrap();
                let path = &rows.iter().find(|row| row.id == session_id).unwrap().path;
                let saved = Session::open(path).await.unwrap();
                assert_eq!(crate::plan::replayed_plan_mode(&saved), if choice == ACP_PERMISSION_REJECT_ONCE {
                    crate::plan::PlanMode::Planning
                } else {
                    crate::plan::PlanMode::PendingApproval
                });
                input.try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string()).unwrap();
            };
            let (served, ()) = futures::join!(Box::pin(run(options, incoming, AcpOutput::new(output))), editor);
            served.unwrap();
        }));
        provider.finish(2);
    }
}

#[test]
fn dispatcher_mode_change_cancels_pending_tool_permission_before_acknowledging() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let mut provider = ProviderFixture::with_replies(
        vec![200],
        vec![Some(("write", json!({"path": "cancelled-write.txt", "content": "must not run"})))],
    );
    let runtime = runtime();
    runtime.block_on(bounded_launch_controls(async {
        let mut options = options(root.path(), &provider.url, runtime.handle(), 0);
        options.launch.tools = Some("write".into());
        options.launch.no_mcp = true;
        options.launch.no_context_files = true;
        let (input, incoming) = asupersync::channel::mpsc::channel(16);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let editor = async {
            let mut transcript = Vec::new();
            discovery_request(&input, &outgoing, &mut transcript, 1, "initialize", json!({"protocolVersion": 1})).await;
            let created = discovery_request(&input, &outgoing, &mut transcript, 2, "session/new", json!({"cwd": cwd})).await;
            let session_id = created["result"]["sessionId"].as_str().unwrap().to_string();
            input.try_send(json!({
                "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "Write the file"}]},
            }).to_string()).unwrap();
            let permission = loop {
                let frame = next_planning_frame(&outgoing).await;
                if frame["method"] == "session/request_permission" {
                    break frame;
                }
            };
            assert_eq!(permission["params"]["toolCall"]["title"], "write");
            input.try_send(json!({
                "jsonrpc": "2.0", "id": 4, "method": "session/set_mode",
                "params": {"sessionId": session_id, "modeId": "plan"},
            }).to_string()).unwrap();
            answer_planning_permission(&input, &permission, ACP_PERMISSION_ALLOW_ONCE);
            let mut prompt_settled = false;
            let mut permission_cancelled = false;
            loop {
                let frame = next_planning_frame(&outgoing).await;
                if frame["method"] == "$/cancel_request" && frame["params"]["requestId"] == permission["id"] {
                    permission_cancelled = true;
                } else if frame.get("id") == Some(&json!(3)) {
                    assert_eq!(frame["result"]["stopReason"], ACP_STOP_REASON_CANCELLED);
                    prompt_settled = true;
                } else if frame.get("id") == Some(&json!(4)) {
                    assert!(frame.get("error").is_none(), "{frame}");
                    assert!(prompt_settled, "old turn must retire before mode ACK");
                    assert!(permission_cancelled);
                    break;
                }
            }
            assert!(!cwd.join("cancelled-write.txt").exists());
            let selected = discovery_request(&input, &outgoing, &mut transcript, 5, "session/set_config_option", json!({
                "sessionId": session_id, "configId": "mode", "value": "code",
            })).await;
            assert!(selected.get("error").is_none(), "reservation must retire before mode ACK: {selected}");
            assert_eq!(
                selected["result"]["configOptions"].as_array().unwrap().iter()
                    .find(|option| option["id"] == "mode").unwrap()["currentValue"],
                "code",
            );
            input.try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string()).unwrap();
        };
        let (served, ()) = futures::join!(Box::pin(run(options, incoming, AcpOutput::new(output))), editor);
        served.unwrap();
    }));
    provider.finish(1);
}

#[test]
fn dispatcher_disabled_tools_hide_and_reject_both_mode_selection_surfaces() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let mut provider = ProviderFixture::new(Vec::new());
    let runtime = runtime();
    runtime.block_on(bounded_launch_controls(async {
        let mut options = options(root.path(), &provider.url, runtime.handle(), 0);
        options.launch.no_tools = true;
        options.launch.no_mcp = true;
        options.launch.no_context_files = true;
        let (input, incoming) = asupersync::channel::mpsc::channel(16);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let editor = async {
            let mut transcript = Vec::new();
            discovery_request(&input, &outgoing, &mut transcript, 1, "initialize", json!({"protocolVersion": 1})).await;
            let created = discovery_request(&input, &outgoing, &mut transcript, 2, "session/new", json!({"cwd": cwd})).await;
            let session_id = created["result"]["sessionId"].as_str().unwrap().to_string();
            assert!(created["result"].get("modes").is_none());
            assert!(!created["result"]["configOptions"].as_array().unwrap().iter().any(|option| option["id"] == "mode"));
            let plan = discovery_request(&input, &outgoing, &mut transcript, 3, "session/set_mode", json!({
                "sessionId": session_id, "modeId": "plan",
            })).await;
            assert_eq!(plan["error"]["code"], INVALID_PARAMS);
            let code = discovery_request(&input, &outgoing, &mut transcript, 4, "session/set_config_option", json!({
                "sessionId": session_id, "configId": "mode", "value": "code",
            })).await;
            assert_eq!(code["error"]["code"], INVALID_PARAMS, "even an idempotent hidden mode is unsupported");
            input.try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string()).unwrap();
        };
        let (served, ()) = futures::join!(Box::pin(run(options, incoming, AcpOutput::new(output))), editor);
        served.unwrap();
    }));
    provider.finish(0);
}

fn editor_question_batch() -> Value {
    json!({"questions": [
        {
            "id": "scope", "question": "Which scope should the plan describe?",
            "options": [
                {"label": "Small change", "description": "Keep the internals"},
                {"label": "Full redesign", "description": "Replace the internals"},
            ],
            "recommended": 0,
        },
        {
            "id": "checks", "question": "Which checks are required?", "multi": true,
            "options": [{"label": "Unit tests"}, {"label": "Integration tests"}],
        },
        {
            "id": "constraint", "question": "Which constraint guides the implementation?",
            "options": [{"label": "Latency"}, {"label": "Memory"}],
        },
    ]})
}

fn editor_question_answer() -> Value {
    json!({
        "action": "accept",
        "content": {
            "question_0": "option_1",
            "question_1": ["option_0", "option_1"],
            "question_2": "other",
            "other_2": "Preserve the public API",
        },
    })
}

#[test]
fn native_ask_mount_requires_both_form_capability_and_host_tool_policy() {
    let root = tempfile::tempdir().unwrap();
    let mut provider = ProviderFixture::new(Vec::new());
    let runtime = runtime();
    runtime.block_on(async {
        for (capability, tools, no_tools, expected) in [
            (Value::Null, "ask", false, false),
            (json!({}), "ask", false, false),
            (json!({"url": {}}), "ask", false, false),
            (json!({"form": null}), "ask", false, false),
            (json!({"form": true}), "ask", false, false),
            (json!({"form": {}}), "ask", false, true),
            (json!({"form": {}}), "read", false, false),
            (json!({"form": {}}), "ask", true, false),
        ] {
            let mut options = options(root.path(), &provider.url, runtime.handle(), 0);
            options.launch.tools = Some(tools.into());
            options.launch.no_tools = no_tools;
            options.launch.no_mcp = true;
            options.launch.no_context_files = true;
            let (sender, _receiver) = std::sync::mpsc::sync_channel(8);
            let output = AcpOutput::new(sender);
            output.shared.elicitation.negotiate(&json!({
                "clientCapabilities": {"elicitation": capability},
            }));
            let client = AcpPermissionClient {
                pending: Arc::clone(&output.shared.pending_permissions),
                out_tx: output,
                request_counter: Arc::new(AtomicU64::new(0)),
                timeout: acp_permission_timeout(),
                cx: AgentCx::for_current_or_request(),
            };
            let (_, state) = handle_session_new(
                &json!({"cwd": root.path().canonicalize().unwrap()}),
                &options, Some(&client),
            ).unwrap();
            assert_eq!(
                state.agent_session.as_ref().unwrap().session().agent.has_tool("ask"),
                expected,
                "capability={capability}, tools={tools}, no_tools={no_tools}",
            );
        }
    });
    provider.finish(0);
}

#[test]
#[allow(clippy::too_many_lines)]
fn dispatcher_forms_clarify_readonly_plans_and_forks_with_exact_native_answers() {
    for through_xdev in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let ask = if through_xdev {
            ("xdev", json!({"action": "run", "name": "ask", "args": editor_question_batch()}))
        } else {
            ("ask", editor_question_batch())
        };
        let mut provider = ProviderFixture::with_replies(
            vec![200; 5],
            vec![
                Some(ask.clone()),
                Some(("write", json!({"path": "blocked.txt", "content": "not approved"}))),
                None,
                Some(ask),
                None,
            ],
        );
        let runtime = runtime();
        runtime.block_on(bounded_launch_controls(async {
            let mut options = options(root.path(), &provider.url, runtime.handle(), 0);
            options.launch.no_mcp = true;
            options.launch.no_context_files = true;
            options.config.ask_policy = Some("recommended".into());
            if through_xdev {
                let mut config = serde_json::to_value(&options.config).unwrap();
                config["tools"] = json!({"loadMode": {"ask": "discoverable"}});
                options.config = serde_json::from_value(config).unwrap();
            } else {
                options.launch.tools = Some("ask,write".into());
            }
            let (input, incoming) = asupersync::channel::mpsc::channel(16);
            let (output, outgoing) = std::sync::mpsc::sync_channel(128);
            let editor = async {
                let mut transcript = Vec::new();
                discovery_request(&input, &outgoing, &mut transcript, 1, "initialize", json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {"elicitation": {"form": {}}},
                })).await;
                let created = discovery_request(&input, &outgoing, &mut transcript, 2, "session/new", json!({"cwd": cwd})).await;
                let source = created["result"]["sessionId"].as_str().unwrap().to_string();
                let selected = discovery_request(&input, &outgoing, &mut transcript, 3, "session/set_mode", json!({
                    "sessionId": source, "modeId": "plan",
                })).await;
                assert!(selected.get("error").is_none(), "{selected}");
                let mut active = source.clone();
                let mut original_form_id = None;
                for (prompt_id, expected_requests) in [(4, 1), (6, 4)] {
                    if prompt_id == 6 {
                        let fork = discovery_request(&input, &outgoing, &mut transcript, 5, "session/fork", json!({
                            "sessionId": source, "cwd": cwd,
                        })).await;
                        assert!(fork.get("error").is_none(), "{fork}");
                        assert_eq!(fork["result"]["modes"]["currentModeId"], "plan");
                        active = fork["result"]["sessionId"].as_str().unwrap().to_string();
                        assert_ne!(active, source);
                    }
                    input.try_send(json!({
                        "jsonrpc": "2.0", "id": prompt_id, "method": "session/prompt",
                        "params": {"sessionId": active, "prompt": [{"type": "text", "text": "Clarify the proposed implementation"}]},
                    }).to_string()).unwrap();
                    let mut permissions = Vec::new();
                    let mut forms = 0;
                    loop {
                        let frame = next_planning_frame(&outgoing).await;
                        if frame["method"] == "session/request_permission" {
                            assert_eq!(frame["params"]["sessionId"], active);
                            let name = frame["params"]["toolCall"]["title"].as_str().unwrap();
                            assert!(matches!(name, "ask" | "xdev"), "planning must block write before asking permission");
                            permissions.push(name.to_string());
                            answer_planning_permission(&input, &frame, ACP_PERMISSION_ALLOW_ONCE);
                        } else if frame["method"] == "elicitation/create" {
                            forms += 1;
                            assert_eq!(provider.requests.lock().unwrap().len(), expected_requests);
                            assert_eq!(frame["params"]["sessionId"], active);
                            assert_eq!(frame["params"]["toolCallId"], if prompt_id == 4 { "acp-tool-0" } else { "acp-tool-3" });
                            assert_eq!(frame["params"]["mode"], "form");
                            assert_eq!(permissions, if through_xdev {
                                vec!["xdev".to_string(), "ask".to_string()]
                            } else {
                                vec!["ask".to_string()]
                            }, "the native approval boundary precedes question presentation");
                            let fields = &frame["params"]["requestedSchema"]["properties"];
                            assert_eq!(fields["question_0"]["oneOf"][1]["title"], "Full redesign");
                            assert_eq!(fields["question_1"]["type"], "array");
                            assert_eq!(fields["other_2"]["type"], "string");
                            assert!(!cwd.join("blocked.txt").exists());
                            if let Some(old) = &original_form_id {
                                assert_ne!(&frame["id"], old, "forks get fresh connection-owned requests");
                                input.try_send(json!({"jsonrpc": "2.0", "id": old, "result": {
                                    "action": "accept", "content": {"question_0": "option_0"},
                                }}).to_string()).unwrap();
                            } else {
                                original_form_id = Some(frame["id"].clone());
                            }
                            input.try_send(json!({
                                "jsonrpc": "2.0", "id": frame["id"], "result": editor_question_answer(),
                            }).to_string()).unwrap();
                        } else if frame.get("id") == Some(&json!(prompt_id)) {
                            assert_eq!(frame["result"]["stopReason"], ACP_STOP_REASON_END_TURN);
                            break;
                        }
                        transcript.push(frame);
                    }
                    assert_eq!(forms, 1);
                }
                assert!(!cwd.join("blocked.txt").exists(), "answering a form never approves a write");
                let index = crate::session_index::SessionIndex::for_sessions_root(root.path());
                index.reindex_all().unwrap();
                let rows = index.list_sessions(None).unwrap();
                for (id, expected_users) in [(&source, 1), (&active, 2)] {
                    let path = &rows.iter().find(|row| &row.id == id).unwrap().path;
                    let saved = Session::open(path).await.unwrap();
                    assert_eq!(crate::plan::replayed_plan_mode(&saved), crate::plan::PlanMode::Planning);
                    let messages = saved.to_messages_for_current_path();
                    assert_eq!(messages.iter().filter(|message| matches!(message, Message::User(_))).count(), expected_users);
                    let answered: Vec<_> = messages.iter().filter_map(|message| match message {
                        Message::ToolResult(result) if !result.is_error && matches!(result.tool_name.as_str(), "ask" | "xdev") => Some(result),
                        _ => None,
                    }).collect();
                    assert_eq!(answered.len(), expected_users, "answers are native tool results, not synthetic user prompts");
                    for result in answered {
                        let details = result.details.as_ref().unwrap();
                        assert_eq!(details["autoAnswered"], false);
                        assert_eq!(details["answers"][0]["questionId"], "scope");
                        assert_eq!(details["answers"][0]["selected"], json!(["Full redesign"]));
                        assert_eq!(details["answers"][1]["selected"], json!(["Unit tests", "Integration tests"]));
                        assert_eq!(details["answers"][2]["other"], "Preserve the public API");
                    }
                }
                input.try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string()).unwrap();
            };
            let (served, ()) = futures::join!(
                Box::pin(run(options, incoming, AcpOutput::new(output))), editor,
            );
            served.unwrap();
        }));
        let requests = provider.finish(5);
        let advertised = requests[0].1["tools"].as_array().unwrap();
        assert_eq!(
            advertised.iter().any(|tool| tool["function"]["name"] == "ask"),
            !through_xdev,
        );
        if through_xdev {
            assert!(advertised.iter().any(|tool| tool["function"]["name"] == "xdev"));
        }
        let tool_result = requests[1].1["messages"].as_array().unwrap().iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == "acp-tool-0")
            .unwrap()["content"].as_str().unwrap();
        assert!(tool_result.contains("Full redesign"));
        assert!(tool_result.contains("Unit tests, Integration tests"));
        assert!(tool_result.contains("Other: Preserve the public API"));
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn dispatcher_declined_invalid_denied_and_cancelled_forms_never_invent_answers() {
    for outcome in ["decline", "cancel", "invalid", "permission-denied", "session-cancel"] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let mut provider = ProviderFixture::with_replies(
            vec![200; 2],
            vec![Some(("ask", editor_question_batch())), None],
        );
        let runtime = runtime();
        runtime.block_on(bounded_launch_controls(async {
            let mut options = options(root.path(), &provider.url, runtime.handle(), 0);
            options.launch.tools = Some("ask".into());
            options.launch.no_mcp = true;
            options.launch.no_context_files = true;
            options.config.ask_policy = Some("recommended".into());
            let (input, incoming) = asupersync::channel::mpsc::channel(16);
            let (output, outgoing) = std::sync::mpsc::sync_channel(128);
            let editor = async {
                let mut transcript = Vec::new();
                discovery_request(&input, &outgoing, &mut transcript, 1, "initialize", json!({
                    "protocolVersion": 1, "clientCapabilities": {"elicitation": {"form": {}}},
                })).await;
                let created = discovery_request(&input, &outgoing, &mut transcript, 2, "session/new", json!({"cwd": cwd})).await;
                let session = created["result"]["sessionId"].as_str().unwrap().to_string();
                input.try_send(json!({
                    "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                    "params": {"sessionId": session, "prompt": [{"type": "text", "text": "Ask me before proceeding"}]},
                }).to_string()).unwrap();
                let mut forms = 0;
                let mut cancellation_sent = false;
                loop {
                    let frame = next_planning_frame(&outgoing).await;
                    if frame["method"] == "session/request_permission" {
                        assert_eq!(frame["params"]["toolCall"]["title"], "ask");
                        answer_planning_permission(&input, &frame, if outcome == "permission-denied" {
                            ACP_PERMISSION_REJECT_ONCE
                        } else {
                            ACP_PERMISSION_ALLOW_ONCE
                        });
                    } else if frame["method"] == "elicitation/create" {
                        forms += 1;
                        assert_ne!(outcome, "permission-denied");
                        let result = match outcome {
                            "decline" | "cancel" => json!({
                                "action": outcome, "content": editor_question_answer()["content"],
                            }),
                            "invalid" => json!({
                                "action": "accept", "content": {"question_0": "unoffered-choice"},
                            }),
                            "session-cancel" => {
                                input.try_send(json!({
                                    "jsonrpc": "2.0", "method": "session/cancel",
                                    "params": {"sessionId": session},
                                }).to_string()).unwrap();
                                editor_question_answer()
                            }
                            _ => unreachable!(),
                        };
                        input.try_send(json!({
                            "jsonrpc": "2.0", "id": frame["id"], "result": result,
                        }).to_string()).unwrap();
                    } else if frame["method"] == "$/cancel_request" {
                        cancellation_sent = true;
                    } else if frame.get("id") == Some(&json!(3)) {
                        assert_eq!(frame["result"]["stopReason"], if outcome == "session-cancel" {
                            ACP_STOP_REASON_CANCELLED
                        } else {
                            ACP_STOP_REASON_END_TURN
                        });
                        break;
                    }
                    transcript.push(frame);
                }
                assert_eq!(forms, usize::from(outcome != "permission-denied"));
                assert_eq!(cancellation_sent, outcome == "session-cancel");
                let index = crate::session_index::SessionIndex::for_sessions_root(root.path());
                index.reindex_all().unwrap();
                let path = index.list_sessions(None).unwrap().into_iter()
                    .find(|row| row.id == session).unwrap().path;
                let saved = Session::open(&path).await.unwrap();
                let messages = saved.to_messages_for_current_path();
                let question = messages.iter().find_map(|message| match message {
                    Message::ToolResult(result) if result.tool_name == "ask" => Some(result),
                    _ => None,
                }).unwrap();
                assert!(question.is_error);
                assert!(question.details.as_ref().is_none_or(|details| details.get("answers").is_none()));
                assert!(!serde_json::to_string(question).unwrap().contains("autoAnswered"));
                assert_eq!(messages.iter().filter(|message| matches!(message, Message::User(_))).count(), 1);
                input.try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string()).unwrap();
            };
            let (served, ()) = futures::join!(
                Box::pin(run(options, incoming, AcpOutput::new(output))), editor,
            );
            served.unwrap();
        }));
        provider.finish(if outcome == "session-cancel" { 1 } else { 2 });
    }
}

async fn context_history(root: &Path, cwd: &Path) -> Session {
    use crate::model::{AssistantMessage, ToolCall};
    use crate::session::SessionMessage;

    let mut saved = Session::create_with_dir_and_store(Some(root.into()), SessionStoreKind::Jsonl);
    saved.header.cwd = cwd.display().to_string();
    saved.set_model_header(Some(PRIMARY_PROVIDER.into()), Some(PRIMARY_MODEL.into()), Some("off".into()));
    for index in 0..4 {
        saved.append_message(SessionMessage::User {
            content: UserContent::Text(format!("Original question {index} with important decisions ").repeat(30)),
            timestamp: Some(index),
        });
        saved.append_message(SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: format!("historic-read-{index}"), name: "read".into(),
                    arguments: json!({"path": format!("notes-{index}.txt")}),
                    thought_signature: None,
                })],
                stop_reason: StopReason::ToolUse,
                ..AssistantMessage::default()
            },
        });
        saved.append_message(SessionMessage::ToolResult {
            tool_call_id: format!("historic-read-{index}"), tool_name: "read".into(),
            content: vec![ContentBlock::Text(TextContent::new(format!("bulky-historic-output-{index}\n").repeat(100)))],
            details: None, is_error: false, timestamp: Some(index),
        });
        saved.append_message(SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(format!("Original answer {index}")))],
                stop_reason: StopReason::Stop,
                ..AssistantMessage::default()
            },
        });
    }
    saved.save().await.unwrap();
    saved
}

#[test]
#[allow(clippy::too_many_lines)]
fn dispatcher_native_context_commands_save_before_ack_and_preserve_original_turns() {
    for (command, shake) in [
        ("/compact Keep  migration IDs", false),
        ("/compact aggressive Keep  migration IDs", false),
        ("/shake Keep  migration IDs", true),
        ("/handoff Keep  migration IDs", false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let sessions_root = root.path().join("sessions");
        let expected_calls = if shake { 1 } else { 2 };
        let mut provider = ProviderFixture::new(vec![200; expected_calls]);
        let runtime = runtime();
        runtime.block_on(bounded_launch_controls(async {
            let saved = context_history(&sessions_root, &cwd).await;
            let session_id = saved.header.id.clone();
            let path = saved.path.clone().unwrap();
            let original = saved.entries.clone();
            let original_messages = saved.to_messages_for_current_path();
            let mut options = options(root.path(), &provider.url, runtime.handle(), 0);
            options.session_dir = Some(sessions_root);
            options.config.compaction.as_mut().unwrap().keep_recent_tokens = Some(1);
            options.launch.no_tools = true;
            options.launch.no_mcp = true;
            options.launch.no_context_files = true;
            let (input, incoming) = asupersync::channel::mpsc::channel(16);
            let (output, outgoing) = std::sync::mpsc::sync_channel(128);
            let editor = async {
                let mut transcript = Vec::new();
                discovery_request(&input, &outgoing, &mut transcript, 1, "initialize", json!({"protocolVersion": 1})).await;
                let resumed = discovery_request(&input, &outgoing, &mut transcript, 2, "session/resume", json!({
                    "sessionId": session_id, "cwd": cwd,
                })).await;
                assert!(resumed.get("error").is_none(), "{resumed}");
                let catalog = transcript.iter().find_map(|frame| {
                    (frame["params"]["update"]["sessionUpdate"] == "available_commands_update")
                        .then(|| frame["params"]["update"]["availableCommands"].as_array().unwrap())
                }).unwrap();
                for name in ["compact", "shake", "handoff"] {
                    assert_eq!(catalog.iter().filter(|item| item["name"] == name).count(), 1);
                }
                let before = transcript.len();
                let compacted = discovery_request(&input, &outgoing, &mut transcript, 3, "session/prompt", json!({
                    "sessionId": session_id, "prompt": [{"type": "text", "text": command}],
                })).await;
                assert_eq!(compacted["result"]["stopReason"], ACP_STOP_REASON_END_TURN, "{compacted}");
                assert_eq!(provider.requests.lock().unwrap().len(), usize::from(!shake));
                let reopened = Session::open(path.to_str().unwrap()).await.unwrap();
                assert_eq!(reopened.header.id, session_id, "maintenance stays in the same session");
                assert_eq!(serde_json::to_value(&reopened.entries[..original.len()]).unwrap(), serde_json::to_value(&original).unwrap(), "original branch history is retained");
                let compaction = reopened.entries.iter().rev().find_map(|entry| match entry {
                    crate::session::SessionEntry::Compaction(entry) => Some(entry),
                    _ => None,
                }).expect("native checkpoint must be durable before command acknowledgment");
                let messages = reopened.to_messages_for_current_path();
                assert_eq!(
                    serde_json::to_value(&messages[messages.len() - 4..]).unwrap(),
                    serde_json::to_value(&original_messages[original_messages.len() - 4..]).unwrap(),
                    "latest authored turn and full tool pair survive",
                );
                assert!(!serde_json::to_value(&reopened.entries).unwrap().to_string().contains(command), "command is not a synthetic conversation turn");
                if shake {
                    assert!(compaction.summary.contains("Context Checkpoint (shake)"));
                    assert!(!compaction.summary.contains("bulky-historic-output-0"));
                } else {
                    assert!(compaction.summary.contains("Recovered editor answer"));
                }
                let notices: Vec<_> = transcript[before..].iter()
                    .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str()).collect();
                assert_eq!(notices.len(), 2, "one progress notice and one settled result");
                assert!(!notices[0].contains("saved"));
                assert!(notices[1].contains("compacted"));
                let continued = discovery_request(&input, &outgoing, &mut transcript, 4, "session/prompt", json!({
                    "sessionId": session_id, "prompt": [{"type": "text", "text": "Continue after native maintenance"}],
                })).await;
                assert_eq!(continued["result"]["stopReason"], ACP_STOP_REASON_END_TURN);
                input.try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string()).unwrap();
            };
            let (served, ()) = futures::join!(Box::pin(run(options, incoming, AcpOutput::new(output))), editor);
            served.unwrap();
        }));
        let requests = provider.finish(expected_calls);
        if !shake {
            let generation = requests[0].1["messages"].to_string();
            assert!(generation.contains("Keep  migration IDs"));
            assert!(!generation.contains("/compact"));
            assert!(requests[0].1.get("tools").is_none_or(Value::is_null), "maintenance cannot execute model-requested tools");
        }
        let continued = requests.last().unwrap().1["messages"].to_string();
        assert!(continued.contains("Original question 3"));
        assert!(!continued.contains("bulky-historic-output-0"));
        assert!(continued.contains("Continue after native maintenance"));
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn dispatcher_context_cancellation_retires_only_its_request_and_returns_unconsumed_input() {
    for command in ["/compact keep decisions", "/handoff keep decisions"] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let sessions_root = root.path().join("sessions");
        let mut provider = ProviderFixture::with_paused_replies(
            vec![200], vec![None], Some(Arc::new(AtomicBool::new(false))),
        );
        let runtime = runtime();
        runtime.block_on(bounded_launch_controls(async {
            let saved = context_history(&sessions_root, &cwd).await;
            let session_id = saved.header.id.clone();
            let path = saved.path.clone().unwrap();
            let mut options = options(root.path(), &provider.url, runtime.handle(), 0);
            options.session_dir = Some(sessions_root);
            options.config.compaction.as_mut().unwrap().keep_recent_tokens = Some(1);
            options.launch.no_tools = true;
            options.launch.no_mcp = true;
            options.launch.no_context_files = true;
            let (input, incoming) = asupersync::channel::mpsc::channel(16);
            let (output, outgoing) = std::sync::mpsc::sync_channel(128);
            let editor = async {
                let mut transcript = Vec::new();
                discovery_request(&input, &outgoing, &mut transcript, 1, "initialize", json!({"protocolVersion": 1})).await;
                discovery_request(&input, &outgoing, &mut transcript, 2, "session/resume", json!({
                    "sessionId": session_id, "cwd": cwd,
                })).await;
                let before = std::fs::read(&path).unwrap();
                input.try_send(json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {
                    "sessionId": session_id, "prompt": [{"type": "text", "text": command}],
                }}).to_string()).unwrap();
                while provider.requests.lock().unwrap().is_empty() {
                    sleep(wall_now(), Duration::from_millis(1)).await;
                }
                input.try_send(json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt", "params": {
                    "sessionId": session_id, "prompt": [{"type": "text", "text": "/shake"}],
                }}).to_string()).unwrap();
                input.try_send(json!({"jsonrpc": "2.0", "id": 5, "method": "_session/steering", "params": {
                    "sessionId": session_id, "prompt": [{"type": "text", "text": "Retain this exact unsent follow-up"}],
                    "_meta": {"steering": {"queue": "followUp"}},
                }}).to_string()).unwrap();
                input.try_send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {
                    "sessionId": session_id,
                }}).to_string()).unwrap();
                let mut replies = HashMap::new();
                while replies.len() < 3 {
                    let frame = next_planning_frame(&outgoing).await;
                    if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                        assert!(matches!(id, 3..=5), "unexpected response: {frame}");
                        assert!(replies.insert(id, frame).is_none(), "exactly one reply per request");
                    } else {
                        assert_eq!(frame["method"], "session/update");
                        transcript.push(frame);
                    }
                }
                assert_eq!(replies[&3]["result"]["stopReason"], ACP_STOP_REASON_CANCELLED);
                assert_eq!(replies[&4]["error"]["code"], PROMPT_IN_PROGRESS);
                assert_eq!(replies[&5]["result"]["outcome"], "promptRequired");
                assert_eq!(std::fs::read(&path).unwrap(), before, "cancelled generation cannot install a summary or authored command");
                assert!(transcript.iter().filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
                    .all(|text| !text.contains("Handoff saved") && !text.contains("Conversation compacted;")));
                let next = discovery_request(&input, &outgoing, &mut transcript, 6, "session/prompt", json!({
                    "sessionId": session_id, "prompt": [{"type": "text", "text": "/shake"}],
                })).await;
                assert_eq!(next["result"]["stopReason"], ACP_STOP_REASON_END_TURN, "the session owner is restored after cancellation");
                assert_eq!(provider.requests.lock().unwrap().len(), 1, "no queued input or shake may enter the provider");
                input.try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string()).unwrap();
            };
            let (served, ()) = futures::join!(Box::pin(run(options, incoming, AcpOutput::new(output))), editor);
            served.unwrap();
        }));
        provider.finish(1);
    }
}

#[test]
fn dispatcher_context_commands_reject_attachments_and_handoff_exports_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let sessions_root = root.path().join("sessions");
    let mut provider = ProviderFixture::new(vec![200]);
    let runtime = runtime();
    runtime.block_on(bounded_launch_controls(async {
        let saved = context_history(&sessions_root, &cwd).await;
        let session_id = saved.header.id.clone();
        let path = saved.path.clone().unwrap();
        let mut options = options(root.path(), &provider.url, runtime.handle(), 0);
        options.session_dir = Some(sessions_root);
        options.config.compaction.as_mut().unwrap().keep_recent_tokens = Some(1);
        options.launch.no_tools = true;
        options.launch.no_mcp = true;
        options.launch.no_context_files = true;
        let (input, incoming) = asupersync::channel::mpsc::channel(16);
        let (output, outgoing) = std::sync::mpsc::sync_channel(128);
        let editor = async {
            let mut transcript = Vec::new();
            discovery_request(&input, &outgoing, &mut transcript, 1, "initialize", json!({"protocolVersion": 1})).await;
            discovery_request(&input, &outgoing, &mut transcript, 2, "session/resume", json!({
                "sessionId": session_id, "cwd": cwd,
            })).await;
            let before = std::fs::read(&path).unwrap();
            for (index, blocks) in [
                json!([{"type": "text", "text": "/compact"}, {"type": "image", "data": PNG, "mimeType": "image/png"}]),
                json!([{"type": "text", "text": "/shake"}, {"type": "resource", "resource": {"uri": "file:///notes", "text": "keep me"}}]),
                json!([{"type": "text", "text": "/handoff --export forbidden.json"}]),
            ].into_iter().enumerate() {
                let rejected = discovery_request(&input, &outgoing, &mut transcript, 3 + index as u64, "session/prompt", json!({
                    "sessionId": session_id, "prompt": blocks,
                })).await;
                assert_eq!(rejected["error"]["code"], INVALID_PARAMS);
                assert_eq!(std::fs::read(&path).unwrap(), before);
                assert!(provider.requests.lock().unwrap().is_empty());
            }
            let ordinary = discovery_request(&input, &outgoing, &mut transcript, 6, "session/prompt", json!({
                "sessionId": session_id, "prompt": [{"type": "resource", "resource": {
                    "uri": "file:///notes", "mimeType": "text/plain", "text": "/shake is quoted document content",
                }}],
            })).await;
            assert_eq!(ordinary["result"]["stopReason"], ACP_STOP_REASON_END_TURN);
            let reopened = Session::open(path.to_str().unwrap()).await.unwrap();
            assert!(!reopened.entries.iter().any(|entry| matches!(entry, crate::session::SessionEntry::Compaction(_))), "embedded resource text must never run a command");
            input.try_send(json!({"jsonrpc": "2.0", "method": "exit"}).to_string()).unwrap();
        };
        let (served, ()) = futures::join!(Box::pin(run(options, incoming, AcpOutput::new(output))), editor);
        served.unwrap();
    }));
    assert!(provider.finish(1)[0].1["messages"].to_string().contains("/shake is quoted document content"));
}

#[test]
fn context_command_failed_save_never_claims_completion_or_reenters_the_provider() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let mut provider = ProviderFixture::new(Vec::new());
    let runtime = runtime();
    runtime.block_on(bounded_launch_controls(async {
        let saved = context_history(&root.path().join("sessions"), &cwd).await;
        let original = saved.entries.clone();
        let original_path = saved.path.clone().unwrap();
        let original_bytes = std::fs::read(&original_path).unwrap();
        let mut options = options(root.path(), &provider.url, runtime.handle(), 0);
        options.config.compaction.as_mut().unwrap().keep_recent_tokens = Some(1);
        options.launch.no_tools = true;
        options.launch.no_mcp = true;
        options.launch.no_context_files = true;
        let (id, state) = build_acp_session(saved, true, cwd, &options, None).unwrap();
        let state = Arc::new(Mutex::new(state));
        let stored = session_store(&state);
        // Native compaction creates a candidate, then must acknowledge its
        // durable save. A directory cannot be that candidate's JSONL store.
        let blocked = root.path().join("directory-not-session.jsonl");
        std::fs::create_dir(&blocked).unwrap();
        stored.try_lock().unwrap().path = Some(blocked);
        let command = commands::context_from_prompt(&[json!({"type": "text", "text": "/shake"})]).unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(32);
        let (_, signal) = AbortHandle::new();
        let reason = Box::pin(run_prompt(
            Arc::clone(&state), AcpPrompt::text("/shake"), command, signal,
            AcpOutput::new(tx), id.clone(), AgentCx::for_current_or_request(),
        )).await;
        assert_eq!(reason, ACP_STOP_REASON_ERROR);
        let notices = rx.try_iter().map(|line| serde_json::from_str::<Value>(&line).unwrap()).collect::<Vec<_>>();
        assert!(notices.iter().any(|frame| frame["params"]["update"]["content"]["text"].as_str().is_some_and(|text| text.contains("Session persistence failed"))));
        assert!(!notices.iter().any(|frame| frame["params"]["update"]["content"]["text"].as_str().is_some_and(|text| text.contains("Older tool output dropped;"))));
        assert_eq!(serde_json::to_value(&stored.try_lock().unwrap().entries).unwrap(), serde_json::to_value(&original).unwrap(), "failed installation leaves the original history live");
        assert_eq!(std::fs::read(original_path).unwrap(), original_bytes);
        assert!(state.try_lock().unwrap().agent_session.as_ref().unwrap().session().ensure_provider_reentry_allowed().is_err());
        let (_, signal) = AbortHandle::new();
        let (later, notices) = prompt(&state, &id, text(), signal).await;
        assert_eq!(later, ACP_STOP_REASON_ERROR);
        assert!(notices.iter().any(|frame| frame["params"]["update"]["content"]["text"].as_str().is_some_and(|text| text.contains("Session persistence failed"))));
    }));
    provider.finish(0);
}
