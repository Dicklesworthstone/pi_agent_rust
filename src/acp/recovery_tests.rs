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
        let listener = TcpListener::bind("127.0.0.1:0").expect("provider listener");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let worker = std::thread::spawn(move || {
            for (reply_index, status) in statuses.into_iter().enumerate() {
                let deadline = Instant::now() + Duration::from_secs(20);
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
                stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
                stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                let (headers, request) = read_request(&mut stream, deadline);
                let model = request["model"].as_str().unwrap().to_string();
                captured.lock().unwrap().push((headers, request));
                let (content_type, body) = if status == 200 {
                    let delta = tool.map_or_else(
                        || {
                            json!({
                                "role": "assistant",
                                "content": "Recovered editor answer",
                            })
                        },
                        |tool| {
                            json!({
                                "role": "assistant",
                                "tool_calls": [{
                                    "index": 0,
                                    "id": format!("acp-tool-{reply_index}"),
                                    "type": "function",
                                    "function": { "name": tool, "arguments": "{}" },
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
                    ("text/event-stream", format!("data: {start}\n\ndata: {end}\n\ndata: [DONE]\n\n"))
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
        Self { url, requests, stopped, worker: Some(worker) }
    }

    fn finish(&mut self, expected: usize) -> Vec<(String, Value)> {
        self.stopped.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().expect("provider fixture completed");
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
            Err(error) if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => continue,
            Err(error) => panic!("provider request read: {error}"),
        }
        assert!(bytes.len() <= LIMIT, "bounded provider request");
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let length = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            }).expect("content-length");
            assert!(end + 4 + length <= LIMIT);
            if bytes.len() >= end + 4 + length {
                assert!(headers.lines().next().unwrap().contains("/chat/completions"));
                return (headers.to_string(), serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap());
            }
        }
    }
}

fn runtime() -> asupersync::runtime::Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("reactor"))
        .build().expect("runtime")
}

fn options(root: &Path, url: &str, runtime: RuntimeHandle, retries: u32) -> AcpOptions {
    let entry = |provider: &str, model: &str, reasoning: bool, max_tokens: u32| {
        let mut entry = crate::models::ad_hoc_model_entry("openai", model).unwrap();
        entry.model.provider = provider.to_string();
        entry.model.api = "openai-completions".to_string();
        entry.model.base_url = url.to_string();
        entry.model.input = vec![crate::provider::InputType::Text, crate::provider::InputType::Image];
        entry.model.reasoning = reasoning;
        entry.model.context_window = if reasoning { 128_000 } else { 32_000 };
        entry.model.max_tokens = max_tokens;
        entry.api_key = Some(format!("{model}-fixture-key"));
        entry.headers.insert("x-acp-model".to_string(), model.to_string());
        entry
    };
    let models = vec![
        entry(PRIMARY_PROVIDER, PRIMARY_MODEL, true, 2_048),
        entry(FALLBACK_PROVIDER, FALLBACK_MODEL, false, 512),
    ];
    AcpOptions {
        launch: AcpLaunchOptions::default(),
        config: Config {
            default_provider: Some(PRIMARY_PROVIDER.to_string()),
            default_model: Some(PRIMARY_MODEL.to_string()),
            default_thinking_level: Some("high".to_string()),
            compaction: Some(crate::config::CompactionSettings {
                enabled: Some(false), ..crate::config::CompactionSettings::default()
            }),
            retry: Some(crate::config::RetrySettings {
                enabled: Some(true), max_retries: Some(retries),
                base_delay_ms: Some(0), max_delay_ms: Some(0),
                fallback_chains: Some(HashMap::from([(
                    "default".to_string(), vec![format!("{FALLBACK_PROVIDER}/{FALLBACK_MODEL}")]
                )])),
                failover_cooldown_secs: Some(0), max_failovers_per_turn: Some(2),
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
    let (id, state) = handle_session_new(&json!({"cwd": root, "mcpServers": []}), options, None).unwrap();
    (id, Arc::new(Mutex::new(state)))
}

fn text() -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new("Keep the editor turn"))]
}

fn native_content() -> Vec<ContentBlock> {
    vec![
        ContentBlock::Text(TextContent::new("  compare\n")),
        ContentBlock::Media(MediaContent {
            data: "YQ==".to_string(), mime_type: "audio/wav".to_string(), name: Some("voice.wav".to_string()),
        }),
        ContentBlock::Image(ImageContent { data: PNG.to_string(), mime_type: "image/png".to_string() }),
        ContentBlock::Text(TextContent::new("this frame")),
    ]
}

async fn prompt(
    state: &Arc<Mutex<AcpSessionState>>, id: &str, content: Vec<ContentBlock>, signal: AbortSignal,
) -> (&'static str, Vec<Value>) {
    let (tx, rx) = std::sync::mpsc::sync_channel(128);
    let reason = run_prompt(Arc::clone(state), content, None, signal, tx.clone(), id.to_string(),
        AgentCx::for_current_or_request()).await;
    // Mirror the dispatcher: the response must follow recovery notices,
    // committed selection updates and the session's final persistence step.
    tx.send(json_rpc_ok(json!(7), json!({"stopReason": reason}))).unwrap();
    (reason, rx.try_iter().map(|line| serde_json::from_str(&line).unwrap()).collect())
}

fn events(state: &Arc<Mutex<AcpSessionState>>) -> Arc<StdMutex<Vec<AgentEvent>>> {
    let recorded = Arc::new(StdMutex::new(Vec::new()));
    let captured = Arc::clone(&recorded);
    state.try_lock().unwrap().agent_session.as_ref().unwrap().subscribe(move |event| {
        captured.lock().unwrap().push(event);
    });
    recorded
}

fn session_store(state: &Arc<Mutex<AcpSessionState>>) -> Arc<Mutex<Session>> {
    state.try_lock().unwrap().agent_session.as_ref().unwrap().session_store()
}

async fn reopen(state: &Arc<Mutex<AcpSessionState>>) -> Session {
    let path = session_store(state).try_lock().unwrap().path.clone().expect("durable session path");
    Session::open(path.to_str().unwrap()).await.unwrap()
}

fn assert_no_private_diagnostics(updates: &[Value]) {
    let rendered = serde_json::to_string(updates).unwrap();
    assert!(!rendered.contains("PRIVATE-PROMPT"));
    assert!(!rendered.contains("PRIVATE-CREDENTIAL"));
    assert!(!rendered.contains("fixture-key"));
}

fn configuration_updates(updates: &[Value]) -> Vec<&Value> {
    updates.iter().filter(|update| update["params"]["update"]["sessionUpdate"] == "config_option_update")
        .map(|update| &update["params"]["update"]["configOptions"]).collect()
}

fn user_content(request: &Value) -> &Value {
    &request["messages"].as_array().unwrap().iter().rev()
        .find(|message| message["role"] == "user").unwrap()["content"]
}

async fn bounded_launch_controls<F: Future>(work: F) -> F::Output {
    timeout(wall_now(), Duration::from_secs(15), Box::pin(work))
        .await
        .expect("ACP launch controls watchdog expired")
}

#[test]
fn launch_prompt_and_no_tools_controls_reach_the_actual_provider_request() {
    use clap::Parser as _;

    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("CLAUDE.md"), "EXCLUDED-ACP-PROJECT-CONTEXT").unwrap();
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
        assert!(
            context["systemPrompt"]
                .as_str()
                .unwrap()
                .contains("INCLUDED-ACP-SKILLS")
        );
        for excluded in ["bash", "write", "edit", "ask", "todo", "submit_plan"] {
            assert!(!names.contains(&excluded), "unexpected ACP tool: {excluded}");
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
        let permission_client = AcpPermissionClient {
            out_tx: permission_tx,
            pending: Arc::new(StdMutex::new(HashMap::new())),
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
            let config = config_options_for_handle(handle, &options.available_models);
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
        let mut options = options(root.path(), "https://acp-scope.invalid/v1", runtime.handle(), 0);
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
        assert_eq!(agent.stream_options().thinking_level, Some(crate::model::ThinkingLevel::Low));
    });
}

#[test]
fn invalid_launch_selection_and_missing_credentials_fail_before_a_session_is_installed() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    runtime.block_on(async {
        let mut options = options(root.path(), "https://acp-errors.invalid/v1", runtime.handle(), 0);
        options.launch.provider = Some("unknown-acp-provider".to_string());
        options.launch.model = Some("missing-model".to_string());
        let params = json!({"cwd": root.path(), "mcpServers": []});
        let error = handle_session_new(&params, &options, None).err().expect("reject unknown explicit model");
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
        let error = handle_session_new(&params, &options, None).err().expect("blank override supplies no credential");
        assert!(error.to_string().contains("No API key found"));
        options.launch.api_key = Some("explicit-fixture-key".to_string());
        assert!(handle_session_new(&params, &options, None).is_ok(), "explicit key does not read the unavailable store");
    });
}

#[test]
fn reopening_preserves_the_selected_branch_over_conflicting_launch_options() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    runtime.block_on(async {
        let mut options = options(root.path(), "https://acp-restore.invalid/v1", runtime.handle(), 0);
        options.launch = AcpLaunchOptions {
            provider: Some(FALLBACK_PROVIDER.to_string()), model: Some(FALLBACK_MODEL.to_string()),
            thinking: Some("high".to_string()), api_key: Some("restart-fixture-key".to_string()),
            models: None,
            ..AcpLaunchOptions::default()
        };
        let (mut saved, _) = new_acp_session(options.session_dir.as_ref(), &options.config, root.path());
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
        let (id, state) = build_acp_session(reopened, true, root.path().to_path_buf(), &options, None).unwrap();
        assert_eq!(id, expected_id);
        let handle = state.agent_session.as_ref().unwrap();
        assert_eq!(handle.session().agent.provider().model_id(), PRIMARY_MODEL);
        assert_eq!(handle.session().agent.stream_options().thinking_level, Some(crate::model::ThinkingLevel::Low));
        assert_eq!(handle.session().agent.stream_options().api_key.as_deref(), Some("restart-fixture-key"));
        assert_eq!(handle.session_store().try_lock().unwrap().leaf_id.as_deref(), Some(selected.as_str()));
        assert_eq!(std::fs::read(&path).unwrap(), before, "opening a branch sends no provider request or disk write");
    });
}

#[test]
fn selected_ad_hoc_model_remains_registered_for_runtime_switching() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    runtime.block_on(async {
        let mut options = options(root.path(), "https://acp-ad-hoc.invalid/v1", runtime.handle(), 0);
        options.launch.provider = Some("openai".to_string());
        options.launch.model = Some("editor-ad-hoc-model".to_string());
        options.launch.api_key = Some("ad-hoc-fixture-key".to_string());
        let (_, state) = new_state(root.path(), &options);
        assert!(state.try_lock().unwrap().agent_session.as_ref().unwrap().session()
            .model_registry().unwrap().find("openai", "editor-ad-hoc-model").is_some());
        let cx = AgentCx::for_current_or_request();
        // Both wire shapes resolve through the same path as their dispatchers,
        // including the registry lookup before the durable model transition.
        apply_set_model_request(&state, &json!({ "provider": FALLBACK_PROVIDER, "model": FALLBACK_MODEL }), &cx).await.unwrap();
        {
            let guard = state.try_lock().unwrap();
            let config = config_options_for(&guard).unwrap();
            assert!(config[0]["options"].as_array().unwrap().iter().any(|entry| {
                entry["value"] == "openai/editor-ad-hoc-model"
            }));
        }
        apply_set_model_request(&state, &json!({ "model": "openai/editor-ad-hoc-model" }), &cx).await.unwrap();
        let saved = reopen(&state).await;
        assert_eq!(saved.effective_model_for_current_path(), Some(("openai".to_string(), "editor-ad-hoc-model".to_string())));
        assert!(!std::fs::read_to_string(saved.path.unwrap()).unwrap().contains("ad-hoc-fixture-key"));
    });
}

#[test]
fn startup_refresh_failures_only_block_the_selected_provider_without_an_override() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    runtime.block_on(async {
        let mut options = options(root.path(), "https://acp-refresh.invalid/v1", runtime.handle(), 0);
        let params = json!({"cwd": root.path(), "mcpServers": []});
        options.oauth_refresh_failures = vec![FALLBACK_PROVIDER.to_string()];
        assert!(handle_session_new(&params, &options, None).is_ok());
        options.oauth_refresh_failures = vec![PRIMARY_PROVIDER.to_uppercase()];
        let error = handle_session_new(&params, &options, None).err().expect("selected refresh failed");
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
            provider: Some(PRIMARY_PROVIDER.to_string()), model: Some(PRIMARY_MODEL.to_string()),
            thinking: Some("high".to_string()), api_key: Some("  pinned-launch-key  ".to_string()),
            models: None,
            ..AcpLaunchOptions::default()
        };
        options.auth.set(PRIMARY_PROVIDER, crate::auth::AuthCredential::ApiKey { key: "stored-primary-key".to_string() });
        options.auth.set(FALLBACK_PROVIDER, crate::auth::AuthCredential::ApiKey { key: "stored-fallback-key".to_string() });
        let (id, state) = new_state(root.path(), &options);
        let (_, signal) = AbortHandle::new();
        assert_eq!(prompt(&state, &id, text(), signal).await.0, ACP_STOP_REASON_END_TURN);
        apply_set_model(&state, PRIMARY_PROVIDER, PRIMARY_MODEL, &AgentCx::for_current_or_request()).await.unwrap();
        let (_, signal) = AbortHandle::new();
        assert_eq!(prompt(&state, &id, text(), signal).await.0, ACP_STOP_REASON_END_TURN);
        let saved = reopen(&state).await;
        let durable = std::fs::read_to_string(saved.path.unwrap()).unwrap();
        assert!(!durable.contains("pinned-launch-key"));
        assert!(!durable.contains("stored-primary-key"));
        assert!(!durable.contains("stored-fallback-key"));
    });
    let requests = server.finish(4);
    assert_eq!(requests.iter().map(|(_, body)| body["model"].as_str().unwrap()).collect::<Vec<_>>(),
        [PRIMARY_MODEL, PRIMARY_MODEL, FALLBACK_MODEL, PRIMARY_MODEL]);
    for (headers, body) in requests {
        assert!(headers.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("authorization") && value.trim() == "Bearer pinned-launch-key"
            })
        }));
        let model = body["model"].as_str().unwrap();
        assert!(headers.contains(&format!("x-acp-model: {model}")));
        assert!(!headers.contains("stored-primary-key"));
        assert!(!headers.contains("stored-fallback-key"));
    }
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
        assert!(updates.iter().any(|update| update["params"]["update"]["content"]["text"]
            .as_str().is_some_and(|text| text.contains("attempt 1/1"))));
        assert!(configuration_updates(&updates).is_empty(), "same-model retry does not alter selectors");
        assert_no_private_diagnostics(&updates);
        let saved = reopen(&state).await;
        let messages = saved.to_messages_for_current_path();
        let users = messages.iter().filter_map(|message| match message { Message::User(user) => Some(user), _ => None }).collect::<Vec<_>>();
        assert_eq!(users.len(), 1, "retry must not append the input twice");
        assert_eq!(serde_json::to_value(&users[0].content).unwrap(), serde_json::to_value(UserContent::Blocks(input)).unwrap());
        let observed = observed.lock().unwrap();
        assert_eq!(observed.iter().filter(|event| matches!(event, AgentEvent::AgentStart { .. })).count(), 1);
        assert_eq!(observed.iter().filter(|event| matches!(event, AgentEvent::AgentEnd { error: None, .. })).count(), 1);
        assert_eq!(observed.iter().filter(|event| matches!(event, AgentEvent::AutoRetryStart { .. })).count(), 1);
        assert_eq!(observed.iter().filter(|event| matches!(event, AgentEvent::AutoRetryEnd { success: true, .. })).count(), 1);
    });
    let requests = server.finish(2);
    assert_eq!(requests[0].1["model"], PRIMARY_MODEL);
    assert_eq!(requests[1].1["model"], PRIMARY_MODEL);
    assert_eq!(user_content(&requests[0].1), user_content(&requests[1].1));
    assert!(serde_json::to_string(user_content(&requests[0].1)).unwrap().contains(PNG));
    assert!(requests.iter().all(|(headers, _)| headers.contains("editor-primary-fixture-key") && headers.contains("x-acp-model: editor-primary")));
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
        assert_eq!(selected.len(), 1, "a committed fallback publishes one complete selection");
        assert_eq!(selected[0][0]["currentValue"], format!("{FALLBACK_PROVIDER}/{FALLBACK_MODEL}"));
        assert_eq!(selected[0][1]["currentValue"], "off");
        assert_eq!(selected[0][0]["options"].as_array().unwrap().len(), 2);
        assert_no_private_diagnostics(&updates);
        {
            let observed = observed.lock().unwrap();
            assert_eq!(observed.iter().filter(|event| matches!(event, AgentEvent::FailoverStart { .. })).count(), 1);
            assert_eq!(observed.iter().filter(|event| matches!(event, AgentEvent::FailoverEnd { restored_primary: false, success: true, .. })).count(), 1);
            assert!(matches!(observed.last(), Some(AgentEvent::AgentEnd { error: None, .. })));
        }
        let saved = reopen(&state).await;
        assert!(saved.active_failover_provenance_for_current_path().is_some());
        let (restored_id, restored) = build_acp_session(saved, true, root.path().to_path_buf(), &options, None).unwrap();
        assert_eq!(restored_id, id);
        let restored = Arc::new(Mutex::new(restored));
        let (_, signal) = AbortHandle::new();
        let (reason, updates) = prompt(&restored, &id, text(), signal).await;
        assert_eq!(reason, ACP_STOP_REASON_END_TURN);
        let selected = configuration_updates(&updates);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0][0]["currentValue"], format!("{PRIMARY_PROVIDER}/{PRIMARY_MODEL}"));
        assert_eq!(selected[0][1]["currentValue"], "high");
        assert!(updates.iter().any(|update| update["params"]["update"]["content"]["text"]
            .as_str().is_some_and(|text| text.contains("Restored primary provider"))));
        let saved = reopen(&restored).await;
        assert!(saved.active_failover_provenance_for_current_path().is_none());
        assert_eq!(saved.to_messages_for_current_path().iter().filter(|message| matches!(message, Message::User(_))).count(), 2);
    });
    let requests = server.finish(3);
    assert_eq!(requests.iter().map(|(_, body)| body["model"].as_str().unwrap()).collect::<Vec<_>>(),
        [PRIMARY_MODEL, FALLBACK_MODEL, PRIMARY_MODEL]);
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
        assert_eq!(prompt(&state, &id, text(), signal).await.0, ACP_STOP_REASON_END_TURN);
        apply_set_model(&state, FALLBACK_PROVIDER, FALLBACK_MODEL, &AgentCx::for_current_or_request()).await.unwrap();
        assert!(reopen(&state).await.active_failover_provenance_for_current_path().is_none());
        let (_, signal) = AbortHandle::new();
        let (reason, updates) = prompt(&state, &id, text(), signal).await;
        assert_eq!(reason, ACP_STOP_REASON_END_TURN);
        assert!(configuration_updates(&updates).is_empty());
        assert!(!serde_json::to_string(&updates).unwrap().contains("Restored primary"));
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
        state.try_lock().unwrap().agent_session.as_ref().unwrap().subscribe(move |event| {
            if matches!(event, AgentEvent::AutoRetryStart { .. }) {
                observed_retry.store(true, Ordering::SeqCst);
            }
        });
        let mut abort_after_pending = false;
        let pending = prompt(&state, &id, text(), signal);
        let mut pending = std::pin::pin!(pending);
        let (reason, updates) = std::future::poll_fn(|cx| {
            let result = pending.as_mut().poll(cx);
            if result.is_pending()
                && retry_started.load(Ordering::SeqCst)
                && !abort_after_pending
            {
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
        assert!(abort_after_pending, "the retry delay yielded before cancellation");
        assert_eq!(reason, ACP_STOP_REASON_CANCELLED);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
        assert!(!serde_json::to_string(&updates).unwrap().contains("Error:"));
        assert!(state.try_lock().unwrap().agent_session.is_some());
        let (_, signal) = AbortHandle::new();
        assert_eq!(prompt(&state, &id, text(), signal).await.0, ACP_STOP_REASON_END_TURN);
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
        assert!(serde_json::to_string(&updates).unwrap().contains("Session persistence failed"));
        stored.try_lock().unwrap().path = original.lock().unwrap().clone();
        let before = serde_json::to_value(stored.try_lock().unwrap().to_messages_for_current_path()).unwrap();
        let (_, updates) = prompt(&state, &id, text(), AbortHandle::new().1).await;
        assert!(serde_json::to_string(&updates).unwrap().contains("Session persistence failed"));
        assert_eq!(before, serde_json::to_value(stored.try_lock().unwrap().to_messages_for_current_path()).unwrap());
        assert_no_private_diagnostics(&updates);
    });
    server.finish(2);
}
