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
        let listener = TcpListener::bind("127.0.0.1:0").expect("provider listener");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let worker = std::thread::spawn(move || {
            for status in statuses {
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
                    let start = json!({
                        "id": "acp-completion", "object": "chat.completion.chunk",
                        "model": model,
                        "choices": [{"index": 0, "delta": {
                            "role": "assistant", "content": "Recovered editor answer"
                        }, "finish_reason": null}],
                    });
                    let end = json!({
                        "id": "acp-completion",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
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
