#![allow(clippy::similar_names)]
#![allow(clippy::too_many_lines)]

//! E2E RPC protocol tests — comprehensive command coverage.
//!
//! These tests drive the RPC server in-process via channels, exercising the full
//! JSON-line protocol for commands that are not yet covered by `rpc_mode.rs` or
//! `rpc_protocol.rs`.

mod common;

use common::TestHarness;
use futures::StreamExt;
use pi::agent::{Agent, AgentConfig, AgentSession};
use pi::auth::AuthStorage;
use pi::config::Config;
use pi::extensions::{ExtensionManager, ExtensionRegion, ExtensionUiRequest};
use pi::http::client::Client;
#[cfg(unix)]
use pi::model::Message;
use pi::model::{AssistantMessage, ContentBlock, StopReason, TextContent, Usage, UserContent};
use pi::models::ModelEntry;
use pi::provider::{Context, InputType, Model, ModelCost, Provider, StreamEvent, StreamOptions};
use pi::providers::openai::OpenAIProvider;
use pi::resources::ResourceLoader;
use pi::rpc::{RpcOptions, RpcScopedModel, run};
#[cfg(unix)]
use pi::session::SessionEntry;
use pi::session::{Session, SessionMessage};
use pi::session_index::SessionIndex;
use pi::tools::ToolRegistry;
use pi::vcr::{VcrMode, VcrRecorder};
use serde_json::{Value, json};
use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const RPC_E2E_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const KEYLESS_REPLAY_ABORT_WINDOW: Duration = Duration::from_secs(10);

fn cassette_root() -> PathBuf {
    std::env::var("VCR_CASSETTE_DIR").map_or_else(
        |_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vcr"),
        PathBuf::from,
    )
}

fn test_model(id: &str, reasoning: bool) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "anthropic".to_string(),
        provider: "anthropic".to_string(),
        base_url: "https://api.anthropic.com".to_string(),
        reasoning,
        input: vec![InputType::Text],
        cost: ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        },
        context_window: 200_000,
        max_tokens: 8192,
        headers: HashMap::new(),
    }
}

fn test_entry(id: &str, reasoning: bool) -> ModelEntry {
    ModelEntry {
        model: test_model(id, reasoning),
        api_key: None,
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

fn build_agent_session(session: Session, cassette_dir: &Path) -> AgentSession {
    let model = "gpt-4o-mini".to_string();
    let recorder = VcrRecorder::new_with("e2e_rpc_noop", VcrMode::Playback, cassette_dir);
    let client = Client::new().with_vcr(recorder);
    let provider: Arc<dyn Provider> = Arc::new(OpenAIProvider::new(model).with_client(client));
    let tools = ToolRegistry::new(&[], &std::env::current_dir().unwrap(), None);
    let config = AgentConfig::default();
    let agent = Agent::new(provider, tools, config);
    let session = Arc::new(asupersync::sync::Mutex::new(session));
    AgentSession::new(
        agent,
        session,
        false,
        pi::compaction::ResolvedCompactionSettings::default(),
    )
}

#[derive(Debug)]
struct KeylessReplayProvider {
    model_id: String,
    turn_delay: Duration,
}

impl KeylessReplayProvider {
    fn new(model_id: impl Into<String>, turn_delay: Duration) -> Self {
        Self {
            model_id: model_id.into(),
            turn_delay,
        }
    }

    fn message(&self, text: impl Into<String>, stop_reason: StopReason) -> AssistantMessage {
        let text = text.into();
        AssistantMessage {
            content: if text.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::Text(TextContent::new(text))]
            },
            api: self.api().to_string(),
            provider: self.name().to_string(),
            model: self.model_id.clone(),
            usage: Usage::default(),
            stop_reason,
            stop_details: None,
            error_message: None,
            timestamp: chrono::Utc::now().timestamp_millis(),
        }
    }
}

#[async_trait::async_trait]
impl Provider for KeylessReplayProvider {
    fn name(&self) -> &'static str {
        "keyless-replay"
    }

    fn api(&self) -> &'static str {
        "keyless-replay"
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> pi::error::Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = pi::error::Result<StreamEvent>> + Send>>,
    > {
        let partial = self.message("", StopReason::Stop);
        let done = self.message("keyless replay response", StopReason::Stop);
        let delay = self.turn_delay;
        let start = futures::stream::once(async move { Ok(StreamEvent::Start { partial }) });
        let finish = futures::stream::once(async move {
            asupersync::time::sleep(asupersync::time::wall_now(), delay).await;
            Ok(StreamEvent::Done {
                reason: StopReason::Stop,
                message: done,
            })
        });
        Ok(Box::pin(start.chain(finish)))
    }
}

fn build_persistent_keyless_agent_session(session: Session, cwd: &Path) -> AgentSession {
    let provider: Arc<dyn Provider> = Arc::new(KeylessReplayProvider::new(
        "keyless-rpc-replay",
        KEYLESS_REPLAY_ABORT_WINDOW,
    ));
    let tools = ToolRegistry::new(&[], cwd, None);
    let config = AgentConfig::default();
    let agent = Agent::new(provider, tools, config);
    let session = Arc::new(asupersync::sync::Mutex::new(session));
    AgentSession::new(
        agent,
        session,
        true,
        pi::compaction::ResolvedCompactionSettings::default(),
    )
}

fn data_field<'a>(resp: &'a Value, key: &str) -> Option<&'a Value> {
    resp.get("data")
        .and_then(Value::as_object)
        .and_then(|data| data.get(key))
}

fn require_line(line: Result<String, String>) -> String {
    match line {
        Ok(line) => line,
        Err(err) => {
            assert!(std::hint::black_box(false), "{err}");
            String::new()
        }
    }
}

fn require_send<E>(send_result: Result<(), E>, label: &str) {
    match send_result {
        Ok(()) => {}
        Err(_err) => {
            assert!(std::hint::black_box(false), "send {label}");
        }
    }
}

fn require_response_field_str(resp: &Value, key: &str, label: &str) -> String {
    data_field(resp, key).and_then(Value::as_str).map_or_else(
        || {
            assert!(
                std::hint::black_box(false),
                "{label}: missing string data field {key}"
            );
            String::new()
        },
        str::to_owned,
    )
}

fn require_response_field_u64(resp: &Value, key: &str, label: &str) -> u64 {
    let Some(value) = data_field(resp, key).and_then(Value::as_u64) else {
        assert!(
            std::hint::black_box(false),
            "{label}: missing u64 data field {key}"
        );
        return 0;
    };
    value
}

fn is_response_type(value: &Value) -> bool {
    matches!(value.get("type").and_then(Value::as_str), Some("response"))
}

fn is_agent_end_type(value: &Value) -> bool {
    matches!(value.get("type").and_then(Value::as_str), Some("agent_end"))
}

fn is_response_for_id(value: &Value, expected_id: &str) -> bool {
    is_response_type(value)
        && value
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.eq(expected_id))
}

fn is_aborted_agent_end(value: &Value) -> bool {
    is_agent_end_type(value)
        && value
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| error.eq("Aborted"))
}

fn is_streaming(resp: &Value) -> Option<bool> {
    data_field(resp, "isStreaming").and_then(Value::as_bool)
}

fn pending_message_count(resp: &Value) -> Option<u64> {
    data_field(resp, "pendingMessageCount").and_then(Value::as_u64)
}

fn session_id_field(resp: &Value) -> Option<&str> {
    data_field(resp, "sessionId").and_then(Value::as_str)
}

fn build_options(
    handle: &asupersync::runtime::RuntimeHandle,
    auth_path: PathBuf,
    available_models: Vec<ModelEntry>,
    scoped_models: Vec<RpcScopedModel>,
) -> RpcOptions {
    let auth = AuthStorage::load(auth_path).expect("load auth storage");
    RpcOptions {
        config: Config::default(),
        resources: ResourceLoader::empty(false),
        available_models,
        scoped_models,
        cli_api_key: None,
        auth,
        runtime_handle: handle.clone(),
        ask_tool: None,
    }
}

fn rpc_output_channel() -> (std::sync::mpsc::SyncSender<String>, Receiver<String>) {
    std::sync::mpsc::sync_channel::<String>(1024)
}

async fn recv_line(rx: &Arc<Mutex<Receiver<String>>>, label: &str) -> Result<String, String> {
    let start = Instant::now();
    let mut disconnected = false;

    while start.elapsed() <= RPC_E2E_WAIT_TIMEOUT {
        let recv_result = {
            let rx = rx.lock().expect("lock rpc output receiver");
            rx.try_recv()
        };

        match recv_result {
            Ok(line) => return Ok(line),
            Err(TryRecvError::Disconnected) => {
                disconnected = true;
                break;
            }
            Err(TryRecvError::Empty) => {}
        }

        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5)).await;
    }

    if disconnected {
        Err(format!("{label}: output channel disconnected"))
    } else {
        Err(format!("{label}: timed out waiting for output"))
    }
}

fn parse_response(line: &str) -> Value {
    serde_json::from_str(line.trim()).expect("parse JSON response")
}

async fn recv_response(out_rx: &Arc<Mutex<Receiver<String>>>, label: &str) -> Value {
    let start = Instant::now();

    loop {
        let line = recv_line(out_rx, label)
            .await
            .unwrap_or_else(|err| panic!("{err}"));
        let value = parse_response(&line);

        match value.get("type").and_then(Value::as_str) {
            Some("response") => return value,
            Some("agent_end") => {
                let has_error = value
                    .get("error")
                    .is_some_and(|error| !error.is_null() && error != "");
                assert!(
                    !has_error,
                    "{label}: unexpected agent_end error while waiting for response: {value}"
                );
            }
            _ => {}
        }

        assert!(
            start.elapsed() <= RPC_E2E_WAIT_TIMEOUT,
            "{label}: timed out waiting for RPC response"
        );
    }
}

/// Send a command and get the response.
async fn send_recv(
    in_tx: &asupersync::channel::mpsc::Sender<String>,
    out_rx: &Arc<Mutex<Receiver<String>>>,
    cmd: &str,
    label: &str,
) -> Value {
    let cx = asupersync::Cx::for_testing();
    in_tx
        .send(&cx, cmd.to_string())
        .await
        .unwrap_or_else(|_| panic!("send {label}"));
    recv_response(out_rx, label).await
}

/// Assert that a response indicates success with the expected command.
fn assert_ok(resp: &Value, command: &str) {
    assert_eq!(resp["type"], "response", "response type for {command}");
    assert_eq!(resp["command"], command);
    assert_eq!(resp["success"], true, "success for {command}: {resp}");
}

/// Assert that a response indicates an error with the expected command.
fn assert_err(resp: &Value, command: &str) {
    assert_eq!(resp["type"], "response", "response type for {command}");
    assert_eq!(resp["command"], command);
    assert_eq!(
        resp["success"], false,
        "expected error for {command}: {resp}"
    );
}

async fn recv_response_and_agent_end(
    out_rx: &Arc<Mutex<Receiver<String>>>,
    expected_id: &str,
    label: &str,
) -> (Value, Value) {
    let start = Instant::now();
    let mut response = None;
    let mut agent_end = None;

    while response.is_none() || agent_end.is_none() {
        let line = require_line(recv_line(out_rx, label).await);
        let value = parse_response(&line);
        if is_response_for_id(&value, expected_id) {
            response = Some(value);
        } else if is_agent_end_type(&value) {
            agent_end = Some(value);
        }
        assert!(
            start.elapsed() <= RPC_E2E_WAIT_TIMEOUT,
            "{label}: timed out waiting for response id {expected_id} and agent_end"
        );
    }

    let Some(pair) = response.zip(agent_end) else {
        assert!(
            std::hint::black_box(false),
            "{label}: missing response or agent_end"
        );
        return (Value::Null, Value::Null);
    };
    pair
}

async fn send_recv_after_abort(
    in_tx: &asupersync::channel::mpsc::Sender<String>,
    out_rx: &Arc<Mutex<Receiver<String>>>,
    cmd: &str,
    label: &str,
) -> Value {
    let cx = asupersync::Cx::for_testing();
    require_send(in_tx.send(&cx, cmd.to_string()).await, label);

    let start = Instant::now();
    loop {
        let line = require_line(recv_line(out_rx, label).await);
        let value = parse_response(&line);
        if is_response_type(&value) {
            return value;
        }
        assert!(
            is_agent_end_type(&value),
            "{label}: unexpected event while waiting for response after abort: {value}"
        );
        assert!(
            is_aborted_agent_end(&value),
            "{label}: only already-asserted abort terminal events may be skipped"
        );
        assert!(
            start.elapsed() <= RPC_E2E_WAIT_TIMEOUT,
            "{label}: timed out waiting for RPC response after abort"
        );
    }
}

async fn wait_for_streaming_state(
    in_tx: &asupersync::channel::mpsc::Sender<String>,
    out_rx: &Arc<Mutex<Receiver<String>>>,
    session_idx: usize,
) -> Value {
    let cmd = json!({
        "id": format!("s{session_idx}-streaming"),
        "type": "get_state",
    })
    .to_string();
    for _attempt in 0..100 {
        let resp = send_recv(in_tx, out_rx, &cmd, "get_state(wait streaming)").await;
        assert_ok(&resp, "get_state");
        if matches!(is_streaming(&resp), Some(true)) {
            return resp;
        }
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
    }

    assert!(
        std::hint::black_box(false),
        "session {session_idx}: prompt never entered streaming state"
    );
    Value::Null
}

async fn wait_for_non_streaming_state(
    in_tx: &asupersync::channel::mpsc::Sender<String>,
    out_rx: &Arc<Mutex<Receiver<String>>>,
    command_id: &str,
    label: &str,
) -> Value {
    let start = Instant::now();
    let cmd = json!({
        "id": command_id,
        "type": "get_state",
    })
    .to_string();

    loop {
        let resp = send_recv(in_tx, out_rx, &cmd, label).await;
        assert_ok(&resp, "get_state");
        if matches!(is_streaming(&resp), Some(false)) {
            return resp;
        }

        assert!(
            start.elapsed() <= RPC_E2E_WAIT_TIMEOUT,
            "{label}: RPC state remained streaming: {resp}"
        );
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
    }
}

async fn wait_for_non_streaming_state_after_abort(
    in_tx: &asupersync::channel::mpsc::Sender<String>,
    out_rx: &Arc<Mutex<Receiver<String>>>,
    session_idx: usize,
) -> Value {
    let start = Instant::now();
    let cmd = json!({
        "id": format!("s{session_idx}-final-state"),
        "type": "get_state",
    })
    .to_string();

    loop {
        let resp = send_recv_after_abort(in_tx, out_rx, &cmd, "get_state(final)").await;
        assert_ok(&resp, "get_state");
        if matches!(is_streaming(&resp), Some(false)) {
            return resp;
        }

        assert!(
            start.elapsed() <= RPC_E2E_WAIT_TIMEOUT,
            "session {session_idx}: final state remained streaming after abort: {resp}"
        );
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Debug)]
struct RpcSwarmSessionEvidence {
    session_id: String,
    session_file: PathBuf,
    marker_path: PathBuf,
    message_count: u64,
}

const fn rpc_swarm_command_ids(session_idx: usize) -> &'static str {
    match session_idx {
        0 => "s0-bash,s0-prompt,s0-follow,s0-abort",
        1 => "s1-bash,s1-prompt,s1-follow,s1-abort",
        2 => "s2-bash,s2-prompt,s2-follow,s2-abort",
        _ => "s-extra-bash,s-extra-prompt,s-extra-follow,s-extra-abort",
    }
}

fn log_rpc_swarm_session(
    logger: &common::logging::TestLogger,
    session_idx: usize,
    evidence: &RpcSwarmSessionEvidence,
) {
    let session_id = evidence.session_id.as_str();
    let command_ids = rpc_swarm_command_ids(session_idx);
    let session_file = evidence.session_file.display().to_string();

    logger.info_ctx("rpc-swarm", "session completed", |ctx| {
        ctx.push(("session_id".into(), session_id.to_string()));
        ctx.push(("command_ids".into(), command_ids.into()));
        ctx.push((
            "queue_state".into(),
            "follow_up_pending=1_before_abort".into(),
        ));
        ctx.push(("session_file".into(), session_file));
    });
}

async fn run_rpc_swarm_session(
    handle: asupersync::runtime::RuntimeHandle,
    session_idx: usize,
    project_dir: PathBuf,
    sessions_root: PathBuf,
    auth_path: PathBuf,
    marker_path: PathBuf,
) -> RpcSwarmSessionEvidence {
    let mut session = Session::create_with_dir(Some(sessions_root));
    session.header.cwd = project_dir.display().to_string();
    session.header.provider = Some("keyless-replay".to_string());
    session.header.model_id = Some("keyless-rpc-replay".to_string());
    session.header.thinking_level = Some("off".to_string());
    let session_id = session.header.id.clone();

    let agent_session = build_persistent_keyless_agent_session(session, &project_dir);
    let options = build_options(&handle, auth_path, vec![], vec![]);
    let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
    let (out_tx, out_rx) = rpc_output_channel();
    let out_rx = Arc::new(Mutex::new(out_rx));
    let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

    let marker_content = format!("session-{session_idx}-filesystem-side-effect");
    let bash_cmd = format!(
        "printf {} > {}",
        shell_single_quote(&marker_content),
        shell_single_quote(&marker_path.display().to_string())
    );
    let bash = json!({
        "id": format!("s{session_idx}-bash"),
        "type": "bash",
        "command": bash_cmd,
    })
    .to_string();
    let bash_resp = send_recv(&in_tx, &out_rx, &bash, "bash(filesystem side effect)").await;
    assert_ok(&bash_resp, "bash");
    assert_eq!(
        require_response_field_u64(&bash_resp, "exitCode", "bash(filesystem side effect)"),
        0
    );

    let prompt = json!({
        "id": format!("s{session_idx}-prompt"),
        "type": "prompt",
        "message": format!("swarm prompt {session_idx}"),
    })
    .to_string();
    let prompt_resp = send_recv(&in_tx, &out_rx, &prompt, "prompt(start keyless replay)").await;
    assert_ok(&prompt_resp, "prompt");

    let streaming_state = wait_for_streaming_state(&in_tx, &out_rx, session_idx).await;
    assert!(
        session_id_field(&streaming_state).is_some_and(|id| id.eq(&session_id)),
        "streaming state session ID mismatch: {streaming_state}"
    );
    assert!(
        matches!(pending_message_count(&streaming_state), Some(0)),
        "streaming state should have no pending messages: {streaming_state}"
    );

    let follow_up = json!({
        "id": format!("s{session_idx}-follow"),
        "type": "follow_up",
        "message": format!("queued follow-up {session_idx}"),
    })
    .to_string();
    let follow_resp = send_recv(&in_tx, &out_rx, &follow_up, "follow_up(queue)").await;
    assert_ok(&follow_resp, "follow_up");

    let queued_state_cmd = json!({
        "id": format!("s{session_idx}-queued-state"),
        "type": "get_state",
    })
    .to_string();
    let queued_state = send_recv(&in_tx, &out_rx, &queued_state_cmd, "get_state(queued)").await;
    assert_ok(&queued_state, "get_state");
    assert!(
        matches!(is_streaming(&queued_state), Some(true)),
        "queued state should still be streaming: {queued_state}"
    );
    assert!(
        matches!(pending_message_count(&queued_state), Some(1)),
        "queued state should have one pending message: {queued_state}"
    );

    let abort = json!({
        "id": format!("s{session_idx}-abort"),
        "type": "abort",
    })
    .to_string();
    let cx = asupersync::Cx::for_testing();
    require_send(
        in_tx.send(&cx, abort).await,
        "abort command after queue assertion",
    );
    let (abort_resp, agent_end) = recv_response_and_agent_end(
        &out_rx,
        &format!("s{session_idx}-abort"),
        "abort(streaming)",
    )
    .await;
    assert_ok(&abort_resp, "abort");
    assert!(
        is_aborted_agent_end(&agent_end),
        "abort terminal event: {agent_end}"
    );

    let final_state = wait_for_non_streaming_state_after_abort(&in_tx, &out_rx, session_idx).await;
    let session_file = PathBuf::from(require_response_field_str(
        &final_state,
        "sessionFile",
        "get_state(final)",
    ));
    let message_count =
        require_response_field_u64(&final_state, "messageCount", "get_state(final)");

    drop(in_tx);
    let result = server.await;
    assert!(result.is_ok(), "rpc server error: {result:?}");
    assert_eq!(
        std::fs::read_to_string(&marker_path).expect("read filesystem marker"),
        marker_content
    );

    RpcSwarmSessionEvidence {
        session_id,
        session_file,
        marker_path,
        message_count,
    }
}

#[test]
fn rpc_concurrent_keyless_swarm_e2e_preserves_session_index_and_filesystem_state() {
    let harness = TestHarness::new(
        "rpc_concurrent_keyless_swarm_e2e_preserves_session_index_and_filesystem_state",
    );
    let logger = harness.log();
    let runtime = asupersync::runtime::RuntimeBuilder::multi_thread()
        .blocking_threads(1, 8)
        .enable_parking(false)
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async {
        let project_dir = harness.temp_path("project");
        let sessions_root = harness.temp_path("sessions");
        let auth_root = harness.temp_path("auth");
        let marker_root = harness.temp_path("markers");
        std::fs::create_dir_all(&project_dir).expect("create project dir");
        std::fs::create_dir_all(&sessions_root).expect("create session root");
        std::fs::create_dir_all(&auth_root).expect("create auth root");
        std::fs::create_dir_all(&marker_root).expect("create marker root");

        logger.info_ctx("rpc-swarm", "keyless replay path", |ctx| {
            ctx.push(("provider_path".into(), "keyless-replay/no-network".into()));
            ctx.push((
                "redaction_summary".into(),
                "authorization_context_redacted".into(),
            ));
            ctx.push(("Authorization".into(), "Bearer rpc-swarm-secret".into()));
        });

        let tasks = (0..3)
            .map(|session_idx| {
                let task_handle = handle.clone();
                let project_dir = project_dir.clone();
                let sessions_root = sessions_root.clone();
                let auth_path = auth_root.join(format!("auth-{session_idx}.json"));
                let marker_path = marker_root.join(format!("session-{session_idx}.txt"));
                handle.spawn(async move {
                    run_rpc_swarm_session(
                        task_handle,
                        session_idx,
                        project_dir,
                        sessions_root,
                        auth_path,
                        marker_path,
                    )
                    .await
                })
            })
            .collect::<Vec<_>>();

        let evidences = futures::future::join_all(tasks).await;
        assert_eq!(evidences.len(), 3);

        let index = SessionIndex::for_sessions_root(&sessions_root);
        let summary = index.refresh_incremental().expect("refresh session index");
        assert_eq!(
            summary.failed_files, 0,
            "session index refresh should be clean"
        );
        let sessions = index
            .list_sessions(Some(&project_dir.display().to_string()))
            .expect("list indexed swarm sessions");
        assert!(
            sessions.len() >= evidences.len(),
            "expected all swarm sessions in index, got {sessions:?}"
        );

        for (session_idx, evidence) in evidences.iter().enumerate() {
            let marker_label = match session_idx {
                0 => "rpc-swarm-marker-0",
                1 => "rpc-swarm-marker-1",
                2 => "rpc-swarm-marker-2",
                _ => "rpc-swarm-marker-extra",
            };
            let session_label = match session_idx {
                0 => "rpc-swarm-session-0",
                1 => "rpc-swarm-session-1",
                2 => "rpc-swarm-session-2",
                _ => "rpc-swarm-session-extra",
            };
            harness.record_artifact(marker_label, &evidence.marker_path);
            harness.record_artifact(session_label, &evidence.session_file);
            log_rpc_swarm_session(logger, session_idx, evidence);

            let Some(meta) = sessions
                .iter()
                .find(|meta| meta.id.as_str().eq(evidence.session_id.as_str()))
            else {
                assert!(
                    std::hint::black_box(false),
                    "missing session index row for {evidence:?}"
                );
                continue;
            };
            assert_eq!(PathBuf::from(&meta.path), evidence.session_file);
            assert!(evidence.session_file.exists(), "session file should exist");
            assert!(
                meta.message_count >= 3,
                "bash, prompt, and aborted assistant should be indexed: {meta:?}"
            );
            assert!(
                meta.message_count >= evidence.message_count,
                "indexed message count should not lag final RPC state"
            );
        }

        let log_dump = logger.dump();
        assert!(
            log_dump.contains("session_id"),
            "logs should include session IDs"
        );
        assert!(
            log_dump.contains("queue_state"),
            "logs should include queue state"
        );
        assert!(
            log_dump.contains("command_ids"),
            "logs should include command IDs"
        );
        assert!(
            log_dump.contains("redaction_summary = authorization_context_redacted"),
            "logs should include redaction summary"
        );
        assert!(
            log_dump.contains("Authorization = [REDACTED]"),
            "structured log context should redact sensitive fields"
        );
    });
}

#[cfg(unix)]
const CRASH_INTERRUPT_RECOVERY_HARNESS_SCHEMA: &str = "pi.rpc.crash_interrupt_recovery_soak.v1";
#[cfg(unix)]
const CRASH_INTERRUPT_RECOVERY_WORKER_SCHEMA: &str = "pi.rpc.crash_interrupt_recovery_worker.v1";
#[cfg(unix)]
const CRASH_INTERRUPT_RECOVERY_WORKER_ENV: &str = "PI_CRASH_INTERRUPT_RECOVERY_WORKER";
#[cfg(unix)]
const CRASH_INTERRUPT_RECOVERY_ROLE_ENV: &str = "PI_CRASH_INTERRUPT_RECOVERY_ROLE";
#[cfg(unix)]
const CRASH_INTERRUPT_RECOVERY_CYCLE_ENV: &str = "PI_CRASH_INTERRUPT_RECOVERY_CYCLE";
#[cfg(unix)]
const CRASH_INTERRUPT_RECOVERY_MODE_ENV: &str = "PI_CRASH_INTERRUPT_RECOVERY_MODE";
#[cfg(unix)]
const CRASH_INTERRUPT_RECOVERY_SESSIONS_ENV: &str = "PI_CRASH_INTERRUPT_RECOVERY_SESSIONS_ROOT";
#[cfg(unix)]
const CRASH_INTERRUPT_RECOVERY_ARTIFACT_ENV: &str = "PI_CRASH_INTERRUPT_RECOVERY_ARTIFACT_DIR";
#[cfg(unix)]
const CRASH_INTERRUPT_RECOVERY_PROJECT_ENV: &str = "PI_CRASH_INTERRUPT_RECOVERY_PROJECT_DIR";
#[cfg(unix)]
const CRASH_INTERRUPT_RECOVERY_READY_ENV: &str = "PI_CRASH_INTERRUPT_RECOVERY_READY_PATH";
#[cfg(unix)]
const CRASH_INTERRUPT_RECOVERY_SUMMARY_ENV: &str = "PI_CRASH_INTERRUPT_RECOVERY_SUMMARY_PATH";
#[cfg(unix)]
const CRASH_INTERRUPT_RECOVERY_SESSION_ENV: &str = "PI_CRASH_INTERRUPT_RECOVERY_SESSION_PATH";
#[cfg(unix)]
const CRASH_INTERRUPT_RECOVERY_DEFAULT_TIMEOUT: Duration = Duration::from_secs(45);

#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
enum CrashInterruptRecoveryMode {
    Crash,
    Sigint,
    Sighup,
}

#[cfg(unix)]
impl CrashInterruptRecoveryMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Crash => "crash",
            Self::Sigint => "sigint",
            Self::Sighup => "sighup",
        }
    }

    const fn signal_arg(self) -> &'static str {
        match self {
            Self::Crash => "-KILL",
            Self::Sigint => "-INT",
            Self::Sighup => "-HUP",
        }
    }

    const fn expected_signal(self) -> i32 {
        match self {
            Self::Crash => 9,
            Self::Sigint => 2,
            Self::Sighup => 1,
        }
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct SpawnedCrashInterruptWorker {
    child: Child,
    stdout_handle: Option<JoinHandle<Vec<u8>>>,
    stderr_handle: Option<JoinHandle<Vec<u8>>>,
}

#[cfg(unix)]
impl Drop for SpawnedCrashInterruptWorker {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
        let _ = collect_crash_interrupt_worker_output(self);
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct CrashInterruptWorkerResult {
    exit_code: i32,
    exit_signal: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

#[cfg(unix)]
fn crash_interrupt_recovery_schedule() -> Vec<CrashInterruptRecoveryMode> {
    let base = [
        CrashInterruptRecoveryMode::Crash,
        CrashInterruptRecoveryMode::Sigint,
        CrashInterruptRecoveryMode::Sighup,
    ];
    let configured_cycles = std::env::var("PI_CRASH_INTERRUPT_RECOVERY_CYCLES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0);
    let cycle_count = configured_cycles.unwrap_or_else(|| {
        if std::env::var_os("PI_CRASH_INTERRUPT_RECOVERY_LONG").is_some() {
            base.len() * 3
        } else {
            base.len()
        }
    });
    base.iter().copied().cycle().take(cycle_count).collect()
}

#[cfg(unix)]
fn crash_interrupt_artifact_path(
    artifact_dir: &Path,
    prefix: &str,
    cycle: usize,
    mode: CrashInterruptRecoveryMode,
    extension: &str,
) -> PathBuf {
    artifact_dir.join(format!("{prefix}-{cycle}-{}.{extension}", mode.as_str()))
}

#[cfg(unix)]
fn crash_interrupt_artifact_name(
    prefix: &str,
    cycle: usize,
    mode: CrashInterruptRecoveryMode,
    extension: &str,
) -> String {
    format!("{prefix}-{cycle}-{}.{extension}", mode.as_str())
}

#[cfg(unix)]
fn crash_interrupt_display_path(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(unix)]
fn crash_interrupt_context_field(name: &str, value: impl std::fmt::Display) -> (String, String) {
    (name.to_owned(), value.to_string())
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn spawn_crash_interrupt_recovery_worker(
    current_exe: &Path,
    role: &str,
    cycle: usize,
    mode: CrashInterruptRecoveryMode,
    sessions_root: &Path,
    artifact_dir: &Path,
    project_dir: &Path,
    ready_path: &Path,
    summary_path: &Path,
    session_path: Option<&Path>,
) -> SpawnedCrashInterruptWorker {
    let mut command = Command::new(current_exe);
    command
        .arg("--exact")
        .arg("crash_interrupt_recovery_worker_process_entrypoint")
        .arg("--nocapture")
        .env(CRASH_INTERRUPT_RECOVERY_WORKER_ENV, "1")
        .env(CRASH_INTERRUPT_RECOVERY_ROLE_ENV, role)
        .env(CRASH_INTERRUPT_RECOVERY_CYCLE_ENV, cycle.to_string())
        .env(CRASH_INTERRUPT_RECOVERY_MODE_ENV, mode.as_str())
        .env(CRASH_INTERRUPT_RECOVERY_SESSIONS_ENV, sessions_root)
        .env(CRASH_INTERRUPT_RECOVERY_ARTIFACT_ENV, artifact_dir)
        .env(CRASH_INTERRUPT_RECOVERY_PROJECT_ENV, project_dir)
        .env(CRASH_INTERRUPT_RECOVERY_READY_ENV, ready_path)
        .env(CRASH_INTERRUPT_RECOVERY_SUMMARY_ENV, summary_path)
        .current_dir(project_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(session_path) = session_path {
        command.env(CRASH_INTERRUPT_RECOVERY_SESSION_ENV, session_path);
    }

    let child = command
        .spawn()
        .expect("spawn crash interrupt recovery worker");

    capture_crash_interrupt_worker(child)
}

#[cfg(unix)]
fn capture_crash_interrupt_worker(mut child: Child) -> SpawnedCrashInterruptWorker {
    let mut child_stdout = child.stdout.take().expect("child stdout piped");
    let mut child_stderr = child.stderr.take().expect("child stderr piped");
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut child_stdout, &mut buf);
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut child_stderr, &mut buf);
        buf
    });

    SpawnedCrashInterruptWorker {
        child,
        stdout_handle: Some(stdout_handle),
        stderr_handle: Some(stderr_handle),
    }
}

#[cfg(unix)]
fn collect_crash_interrupt_worker_output(
    spawned: &mut SpawnedCrashInterruptWorker,
) -> (String, String) {
    let stdout = spawned
        .stdout_handle
        .take()
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let stderr = spawned
        .stderr_handle
        .take()
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    (
        String::from_utf8_lossy(&stdout).to_string(),
        String::from_utf8_lossy(&stderr).to_string(),
    )
}

#[cfg(unix)]
fn wait_crash_interrupt_recovery_worker(
    mut spawned: SpawnedCrashInterruptWorker,
    timeout: Duration,
) -> CrashInterruptWorkerResult {
    let started_at = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match spawned.child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => {
                let _ = spawned.child.kill();
                let _ = spawned.child.wait();
                break None;
            }
        }

        if started_at.elapsed() > timeout {
            timed_out = true;
            let _ = spawned.child.kill();
            break spawned.child.wait().ok();
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let (stdout, mut stderr) = collect_crash_interrupt_worker_output(&mut spawned);
    if timed_out {
        stderr = format!("ERROR: timed out after {timeout:?}\n{stderr}");
    }
    let exit_signal = status.and_then(|status| status.signal());
    let exit_code = status.and_then(|status| status.code()).unwrap_or(-1);

    CrashInterruptWorkerResult {
        exit_code,
        exit_signal,
        stdout,
        stderr,
        timed_out,
    }
}

#[cfg(unix)]
fn signal_crash_interrupt_recovery_worker(
    worker: &SpawnedCrashInterruptWorker,
    mode: CrashInterruptRecoveryMode,
) {
    let status = Command::new("kill")
        .arg(mode.signal_arg())
        .arg(worker.child.id().to_string())
        .status()
        .expect("send worker signal");
    assert!(status.success(), "kill command failed with {status:?}");
}

#[cfg(unix)]
fn wait_for_crash_interrupt_checkpoint(path: &Path, timeout: Duration) -> Value {
    let started_at = Instant::now();
    loop {
        if path.exists() {
            let raw = std::fs::read_to_string(path).expect("read worker checkpoint");
            return serde_json::from_str(&raw).unwrap_or_else(|source| {
                exit_crash_interrupt_worker(CrashInterruptCheckpointParseError { path, source })
            });
        }
        assert!(
            started_at.elapsed() <= timeout,
            "timed out waiting for worker checkpoint {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn wait_for_crash_interrupt_worker_checkpoint(
    spawned: &mut SpawnedCrashInterruptWorker,
    path: &Path,
    timeout: Duration,
) -> std::io::Result<Value> {
    let started_at = Instant::now();
    loop {
        if path.exists() {
            let raw = std::fs::read_to_string(path)?;
            let checkpoint = serde_json::from_str(&raw).map_err(|source| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    CrashInterruptCheckpointParseError { path, source }.to_string(),
                )
            })?;
            return Ok(checkpoint);
        }

        if let Some(status) = spawned.child.try_wait()? {
            let (stdout, stderr) = collect_crash_interrupt_worker_output(spawned);
            return Err(std::io::Error::other(format!(
                "worker exited before checkpoint {}: status={status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                path.display()
            )));
        }

        if started_at.elapsed() > timeout {
            let _ = spawned.child.kill();
            let _ = spawned.child.wait();
            let (stdout, stderr) = collect_crash_interrupt_worker_output(spawned);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "timed out waiting for worker checkpoint {} after {timeout:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                    path.display()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
struct CrashInterruptCheckpointParseError<'a> {
    path: &'a Path,
    source: serde_json::Error,
}

#[cfg(unix)]
impl std::fmt::Display for CrashInterruptCheckpointParseError<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "parse worker checkpoint {}: {}",
            self.path.display(),
            self.source
        )
    }
}

#[cfg(unix)]
fn exit_crash_interrupt_worker(message: impl std::fmt::Display) -> ! {
    use std::io::Write as _;

    let _ = writeln!(std::io::stderr(), "{message}");
    std::process::exit(2);
}

#[cfg(unix)]
fn write_json_atomic(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create json artifact parent");
    }
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(
        &tmp_path,
        serde_json::to_string_pretty(value).expect("serialize json artifact"),
    )
    .expect("write json artifact tmp");
    std::fs::rename(&tmp_path, path).expect("publish json artifact");
}

#[cfg(unix)]
fn env_required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        exit_crash_interrupt_worker(format!("missing required env var {name}"));
    })
}

#[cfg(unix)]
fn session_lock_path(session_path: &Path) -> PathBuf {
    let mut lock_path = session_path.as_os_str().to_os_string();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

#[cfg(unix)]
fn assert_lock_released(lock_path: &Path) {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .expect("open lock file");
    if let Err(err) = fs4::FileExt::try_lock(&file) {
        exit_crash_interrupt_worker(format!(
            "lock should be released after interrupted worker: {} ({err})",
            lock_path.display()
        ));
    }
    fs4::FileExt::unlock(&file).expect("unlock test lock probe");
}

#[cfg(unix)]
fn assert_dir_lock_released(lock_path: &Path) {
    assert!(
        !lock_path.exists(),
        "directory lock should be removed after interrupted worker: {}",
        lock_path.display()
    );
}

#[cfg(unix)]
fn count_tempish_files(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            let file_type = entry.file_type().ok();
            if file_type.as_ref().is_some_and(std::fs::FileType::is_dir) {
                return count_tempish_files(&path);
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            usize::from(name.ends_with(".tmp") || name.contains(".tmp."))
        })
        .sum()
}

#[cfg(unix)]
async fn build_persistent_keyless_agent_session_with_js_extension(
    session: Session,
    cwd: &Path,
    extension_entry: PathBuf,
) -> AgentSession {
    let mut agent_session = build_persistent_keyless_agent_session(session, cwd);
    agent_session
        .enable_extensions(&[], cwd, None, &[extension_entry])
        .await
        .expect("enable crash recovery extension");
    agent_session
}

#[cfg(unix)]
#[allow(clippy::too_many_lines)]
fn run_crash_interrupt_recovery_active_worker(
    cycle: usize,
    mode: CrashInterruptRecoveryMode,
    sessions_root: PathBuf,
    artifact_dir: PathBuf,
    project_dir: PathBuf,
    ready_path: PathBuf,
) {
    std::fs::create_dir_all(&sessions_root).expect("create sessions root");
    std::fs::create_dir_all(&artifact_dir).expect("create artifact dir");
    std::fs::create_dir_all(&project_dir).expect("create project dir");

    let runtime = asupersync::runtime::RuntimeBuilder::multi_thread()
        .blocking_threads(1, 4)
        .enable_parking(false)
        .build()
        .expect("build active worker runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let extension_entry = artifact_dir.join(format!("active-{cycle}-{}.mjs", mode.as_str()));
        std::fs::write(&extension_entry, RPC_PROMPT_EXTENSION_COMMAND_EXT)
            .expect("write active worker extension");

        let mut session = Session::create_with_dir(Some(sessions_root.clone()));
        session.header.cwd = project_dir.display().to_string();
        session.header.provider = Some("keyless-replay".to_string());
        session.header.model_id = Some("keyless-rpc-replay".to_string());
        session.header.thinking_level = Some("off".to_string());
        let session_id = session.header.id.clone();

        let agent_session = build_persistent_keyless_agent_session_with_js_extension(
            session,
            &project_dir,
            extension_entry,
        )
        .await;
        let options = build_options(
            &handle,
            artifact_dir.join(format!("active-{cycle}-auth.json")),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));
        let _server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let marker_path = artifact_dir.join(format!("active-{cycle}-{}.txt", mode.as_str()));
        let marker_content = format!("active-cycle-{cycle}-{}", mode.as_str());
        let bash = json!({
            "id": format!("active-{cycle}-bash"),
            "type": "bash",
            "command": format!(
                "printf {} > {}",
                shell_single_quote(&marker_content),
                shell_single_quote(&marker_path.display().to_string())
            ),
        })
        .to_string();
        let bash_resp = send_recv(&in_tx, &out_rx, &bash, "active bash").await;
        assert_ok(&bash_resp, "bash");

        let extension_prompt = json!({
            "id": format!("active-{cycle}-extension"),
            "type": "prompt",
            "message": "/emit-now",
        })
        .to_string();
        let extension_resp = send_recv(
            &in_tx,
            &out_rx,
            &extension_prompt,
            "active extension command",
        )
        .await;
        assert_ok(&extension_resp, "prompt");
        let _messages = wait_for_custom_message(
            &in_tx,
            &out_rx,
            "rpc-note",
            "rpc-message",
            "active extension custom message",
        )
        .await;
        let idle_state_id = format!("active-{cycle}-extension-idle");
        let idle_state = wait_for_non_streaming_state(
            &in_tx,
            &out_rx,
            &idle_state_id,
            "active extension completion",
        )
        .await;
        let idle_message_count =
            require_response_field_u64(&idle_state, "messageCount", "active idle get_state");

        let prompt = json!({
            "id": format!("active-{cycle}-prompt"),
            "type": "prompt",
            "message": format!("interrupt recovery streaming prompt {cycle}"),
        })
        .to_string();
        let prompt_resp = send_recv(&in_tx, &out_rx, &prompt, "active streaming prompt").await;
        assert_ok(&prompt_resp, "prompt");
        let streaming_state = wait_for_streaming_state(&in_tx, &out_rx, cycle).await;
        let session_file = require_response_field_str(
            &streaming_state,
            "sessionFile",
            "active streaming get_state",
        );
        // The streaming user prompt is appended to the session just after the
        // streaming state flips on, so poll until the count includes it —
        // otherwise the recorded count races the persisted session file.
        let expected_count = idle_message_count + 1;
        let mut message_count =
            require_response_field_u64(&streaming_state, "messageCount", "active get_state");
        for attempt in 0..200 {
            if message_count >= expected_count {
                break;
            }
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
            let recheck_cmd = json!({
                "id": format!("active-{cycle}-count-{attempt}"),
                "type": "get_state",
            })
            .to_string();
            let recheck = send_recv(&in_tx, &out_rx, &recheck_cmd, "active count get_state").await;
            assert_ok(&recheck, "get_state");
            message_count =
                require_response_field_u64(&recheck, "messageCount", "active count get_state");
        }
        assert_eq!(
            message_count, expected_count,
            "streaming prompt should be persisted exactly once before the interrupt"
        );

        write_json_atomic(
            &ready_path,
            &json!({
                "schema": CRASH_INTERRUPT_RECOVERY_WORKER_SCHEMA,
                "role": "active",
                "cycle": cycle,
                "interrupt": mode.as_str(),
                "sessionId": session_id,
                "sessionFile": session_file,
                "messageCountBeforeInterrupt": message_count,
                "markerPath": marker_path.display().to_string(),
                "extensionCommandObserved": true,
                "streamingBeforeInterrupt": is_streaming(&streaming_state),
            }),
        );

        loop {
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_secs(60)).await;
        }
    });
}

#[cfg(unix)]
#[allow(clippy::too_many_lines)]
fn run_crash_interrupt_recovery_restart_worker(
    cycle: usize,
    mode: CrashInterruptRecoveryMode,
    sessions_root: PathBuf,
    artifact_dir: PathBuf,
    project_dir: PathBuf,
    session_path: PathBuf,
    summary_path: PathBuf,
) {
    std::fs::create_dir_all(&sessions_root).expect("create sessions root");
    std::fs::create_dir_all(&artifact_dir).expect("create artifact dir");
    std::fs::create_dir_all(&project_dir).expect("create project dir");

    let runtime = asupersync::runtime::RuntimeBuilder::multi_thread()
        .blocking_threads(1, 4)
        .enable_parking(false)
        .build()
        .expect("build restart worker runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (loaded, diagnostics) =
            Session::open_with_diagnostics(session_path.to_string_lossy().as_ref())
                .await
                .expect("restart worker opens interrupted session");
        assert!(
            diagnostics.skipped_entries.is_empty(),
            "restart should not skip interrupted session entries: {diagnostics:?}"
        );
        let loaded_messages = loaded.to_messages_for_current_path();
        let message_count_before = loaded
            .entries_for_current_path()
            .iter()
            .filter(|entry| matches!(entry, SessionEntry::Message(_)))
            .count();
        let recovered_session_id = loaded.header.id.clone();
        let pre_interrupt_prompt = format!("interrupt recovery streaming prompt {cycle}");
        let pre_interrupt_prompt_preserved = loaded_messages.iter().any(|message| {
            matches!(
                message,
                Message::User(user)
                    if matches!(
                        &user.content,
                        UserContent::Text(text) if text == &pre_interrupt_prompt
                    )
            )
        });
        assert!(
            pre_interrupt_prompt_preserved,
            "restart should preserve the pre-interrupt prompt {pre_interrupt_prompt:?}"
        );

        let extension_entry = artifact_dir.join(format!("restart-{cycle}-{}.mjs", mode.as_str()));
        std::fs::write(&extension_entry, RPC_PROMPT_EXTENSION_COMMAND_EXT)
            .expect("write restart worker extension");
        let agent_session = build_persistent_keyless_agent_session_with_js_extension(
            loaded,
            &project_dir,
            extension_entry,
        )
        .await;
        let options = build_options(
            &handle,
            artifact_dir.join(format!("restart-{cycle}-auth.json")),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));
        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let marker_path = artifact_dir.join(format!("restart-{cycle}-{}.txt", mode.as_str()));
        let marker_content = format!("restart-cycle-{cycle}-{}", mode.as_str());
        let bash = json!({
            "id": format!("restart-{cycle}-bash"),
            "type": "bash",
            "command": format!(
                "printf {} > {}",
                shell_single_quote(&marker_content),
                shell_single_quote(&marker_path.display().to_string())
            ),
        })
        .to_string();
        let bash_resp = send_recv(&in_tx, &out_rx, &bash, "restart bash").await;
        assert_ok(&bash_resp, "bash");

        let extension_prompt = json!({
            "id": format!("restart-{cycle}-extension"),
            "type": "prompt",
            "message": "/emit-now",
        })
        .to_string();
        let extension_resp = send_recv(
            &in_tx,
            &out_rx,
            &extension_prompt,
            "restart extension command",
        )
        .await;
        assert_ok(&extension_resp, "prompt");
        let _messages = wait_for_custom_message(
            &in_tx,
            &out_rx,
            "rpc-note",
            "rpc-message",
            "restart extension custom message",
        )
        .await;

        let state_id = format!("restart-{cycle}-state");
        let state_after = wait_for_non_streaming_state(
            &in_tx,
            &out_rx,
            &state_id,
            "restart extension completion",
        )
        .await;
        let rpc_message_count_after =
            require_response_field_u64(&state_after, "messageCount", "restart get_state");

        drop(in_tx);
        let server_result = server.await;
        assert!(
            server_result.is_ok(),
            "restart rpc server error: {server_result:?}"
        );

        let (persisted_after_restart, restart_diagnostics) =
            Session::open_with_diagnostics(session_path.to_string_lossy().as_ref())
                .await
                .expect("restart worker reopens session after RPC shutdown");
        assert!(
            restart_diagnostics.skipped_entries.is_empty(),
            "restart shutdown should leave a clean session: {restart_diagnostics:?}"
        );
        assert_eq!(
            persisted_after_restart.header.id, recovered_session_id,
            "restart shutdown must preserve the recovered session ID"
        );
        let message_count_after = persisted_after_restart
            .entries_for_current_path()
            .iter()
            .filter(|entry| matches!(entry, SessionEntry::Message(_)))
            .count();
        assert!(
            message_count_after > message_count_before,
            "restart work should persistently grow the recovered session: before={message_count_before}, after={message_count_after}"
        );

        let index = SessionIndex::for_sessions_root(&sessions_root);
        let index_summary = index
            .refresh_incremental()
            .expect("restart worker refresh session index");
        let indexed = index
            .list_sessions(Some(&project_dir.display().to_string()))
            .expect("restart worker list sessions");

        write_json_atomic(
            &summary_path,
            &json!({
                "schema": CRASH_INTERRUPT_RECOVERY_WORKER_SCHEMA,
                "role": "restart",
                "cycle": cycle,
                "interrupt": mode.as_str(),
                "sessionPath": session_path.display().to_string(),
                "sessionIdBeforeRestart": recovered_session_id,
                "messageCountBeforeRestart": message_count_before,
                "messageCountAfterRestart": message_count_after,
                "rpcMessageCountAfterRestart": rpc_message_count_after,
                "preInterruptPromptPreserved": pre_interrupt_prompt_preserved,
                "indexFailedFiles": index_summary.failed_files,
                "indexedSessionCount": indexed.len(),
                "markerPath": marker_path.display().to_string(),
                "extensionCommandObserved": true,
            }),
        );
    });
}

#[cfg(unix)]
fn run_crash_interrupt_recovery_worker_from_env() {
    let role = env_required(CRASH_INTERRUPT_RECOVERY_ROLE_ENV);
    let cycle_raw = env_required(CRASH_INTERRUPT_RECOVERY_CYCLE_ENV);
    let cycle = cycle_raw.parse::<usize>().unwrap_or_else(|err| {
        exit_crash_interrupt_worker(format!("parse recovery cycle {cycle_raw}: {err}"))
    });
    let mode = match env_required(CRASH_INTERRUPT_RECOVERY_MODE_ENV).as_str() {
        "crash" => CrashInterruptRecoveryMode::Crash,
        "sigint" => CrashInterruptRecoveryMode::Sigint,
        "sighup" => CrashInterruptRecoveryMode::Sighup,
        other => {
            exit_crash_interrupt_worker(format!("unknown crash interrupt recovery mode {other}"))
        }
    };
    install_crash_interrupt_recovery_signal_default(mode);
    let sessions_root = PathBuf::from(env_required(CRASH_INTERRUPT_RECOVERY_SESSIONS_ENV));
    let artifact_dir = PathBuf::from(env_required(CRASH_INTERRUPT_RECOVERY_ARTIFACT_ENV));
    let project_dir = PathBuf::from(env_required(CRASH_INTERRUPT_RECOVERY_PROJECT_ENV));
    match role.as_str() {
        "active" => run_crash_interrupt_recovery_active_worker(
            cycle,
            mode,
            sessions_root,
            artifact_dir,
            project_dir,
            PathBuf::from(env_required(CRASH_INTERRUPT_RECOVERY_READY_ENV)),
        ),
        "restart" => run_crash_interrupt_recovery_restart_worker(
            cycle,
            mode,
            sessions_root,
            artifact_dir,
            project_dir,
            PathBuf::from(env_required(CRASH_INTERRUPT_RECOVERY_SESSION_ENV)),
            PathBuf::from(env_required(CRASH_INTERRUPT_RECOVERY_SUMMARY_ENV)),
        ),
        other => exit_crash_interrupt_worker(format!(
            "unknown crash interrupt recovery worker role {other}"
        )),
    }
}

#[cfg(unix)]
fn install_crash_interrupt_recovery_signal_default(mode: CrashInterruptRecoveryMode) {
    use std::sync::atomic::AtomicBool;

    let signal = match mode {
        CrashInterruptRecoveryMode::Crash => return,
        CrashInterruptRecoveryMode::Sigint => signal_hook::consts::SIGINT,
        CrashInterruptRecoveryMode::Sighup => signal_hook::consts::SIGHUP,
    };
    signal_hook::flag::register_conditional_default(signal, Arc::new(AtomicBool::new(true)))
        .unwrap_or_else(|error| {
            exit_crash_interrupt_worker(format!(
                "install default {} handler: {error}",
                mode.as_str()
            ));
        });
}

#[cfg(unix)]
#[test]
fn crash_interrupt_recovery_worker_process_entrypoint() {
    if std::env::var_os(CRASH_INTERRUPT_RECOVERY_WORKER_ENV).is_none() {
        return;
    }
    run_crash_interrupt_recovery_worker_from_env();
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn crash_interrupt_recovery_soak_harness_survives_signals_and_restarts() -> std::io::Result<()> {
    let harness = TestHarness::new("crash_interrupt_recovery_soak_harness");
    let logger = harness.log();
    let current_exe = std::env::current_exe().expect("current test binary path");
    let project_dir = harness.temp_path("project");
    let sessions_root = harness.temp_path("sessions");
    let artifact_dir = harness.temp_path("crash-interrupt-artifacts");
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    std::fs::create_dir_all(&sessions_root).expect("create sessions root");
    std::fs::create_dir_all(&artifact_dir).expect("create artifact dir");

    let schedule = crash_interrupt_recovery_schedule();
    let mut cycle_summaries = Vec::new();
    for (cycle, mode) in schedule.iter().copied().enumerate() {
        let ready_path = crash_interrupt_artifact_path(&artifact_dir, "ready", cycle, mode, "json");
        let active_summary_path =
            crash_interrupt_artifact_path(&artifact_dir, "active", cycle, mode, "json");
        let restart_summary_path =
            crash_interrupt_artifact_path(&artifact_dir, "restart", cycle, mode, "json");
        let mut active_worker = spawn_crash_interrupt_recovery_worker(
            &current_exe,
            "active",
            cycle,
            mode,
            &sessions_root,
            &artifact_dir,
            &project_dir,
            &ready_path,
            &active_summary_path,
            None,
        );
        let ready = wait_for_crash_interrupt_worker_checkpoint(
            &mut active_worker,
            &ready_path,
            CRASH_INTERRUPT_RECOVERY_DEFAULT_TIMEOUT,
        )?;
        signal_crash_interrupt_recovery_worker(&active_worker, mode);
        let active_result = wait_crash_interrupt_recovery_worker(
            active_worker,
            CRASH_INTERRUPT_RECOVERY_DEFAULT_TIMEOUT,
        );
        assert!(
            !active_result.timed_out,
            "active worker timed out: stderr={}",
            active_result.stderr
        );
        assert_eq!(
            active_result.exit_signal,
            Some(mode.expected_signal()),
            "active worker should terminate by {}. stdout:\n{}\nstderr:\n{}",
            mode.as_str(),
            active_result.stdout,
            active_result.stderr
        );

        let session_path = PathBuf::from(
            ready
                .get("sessionFile")
                .and_then(Value::as_str)
                .expect("ready sessionFile"),
        );
        assert!(session_path.exists(), "session file should survive signal");
        let marker_path = PathBuf::from(
            ready
                .get("markerPath")
                .and_then(Value::as_str)
                .expect("ready markerPath"),
        );
        assert!(
            marker_path.exists(),
            "pre-interrupt bash marker should survive"
        );
        assert_lock_released(&session_lock_path(&session_path));

        let restart_worker = spawn_crash_interrupt_recovery_worker(
            &current_exe,
            "restart",
            cycle,
            mode,
            &sessions_root,
            &artifact_dir,
            &project_dir,
            &ready_path,
            &restart_summary_path,
            Some(&session_path),
        );
        let restart_result = wait_crash_interrupt_recovery_worker(
            restart_worker,
            CRASH_INTERRUPT_RECOVERY_DEFAULT_TIMEOUT,
        );
        assert!(
            !restart_result.timed_out,
            "restart worker timed out: stderr={}",
            restart_result.stderr
        );
        assert_eq!(
            restart_result.exit_code, 0,
            "restart worker failed\nstdout:\n{}\nstderr:\n{}",
            restart_result.stdout, restart_result.stderr
        );
        let restart_summary = wait_for_crash_interrupt_checkpoint(
            &restart_summary_path,
            CRASH_INTERRUPT_RECOVERY_DEFAULT_TIMEOUT,
        );
        assert_eq!(
            restart_summary
                .get("indexFailedFiles")
                .and_then(Value::as_u64),
            Some(0),
            "restart worker index refresh should be clean: {restart_summary}"
        );
        let ready_session_id = ready
            .get("sessionId")
            .and_then(Value::as_str)
            .expect("ready session ID");
        let recovered_session_id = restart_summary
            .get("sessionIdBeforeRestart")
            .and_then(Value::as_str)
            .expect("recovered session ID");
        assert_eq!(
            recovered_session_id, ready_session_id,
            "restart must continue the exact interrupted session"
        );
        let message_count_before_interrupt = ready
            .get("messageCountBeforeInterrupt")
            .and_then(Value::as_u64)
            .expect("ready message count");
        let message_count_before_restart = restart_summary
            .get("messageCountBeforeRestart")
            .and_then(Value::as_u64)
            .expect("recovered message count");
        assert_eq!(
            message_count_before_restart, message_count_before_interrupt,
            "restart must preserve the exact pre-interrupt message count"
        );
        assert_eq!(
            restart_summary
                .get("preInterruptPromptPreserved")
                .and_then(Value::as_bool),
            Some(true),
            "restart must preserve the pre-interrupt prompt"
        );
        assert_lock_released(&session_lock_path(&session_path));
        assert_dir_lock_released(&sessions_root.join("session-index.lock"));

        let parent_session_path_display = crash_interrupt_display_path(&session_path);
        let parent_session_path_display_for_log = crash_interrupt_display_path(&session_path);
        let parent_session_path = session_path;
        let (reopened, diagnostics) = common::run_async(async move {
            Session::open_with_diagnostics(parent_session_path.to_string_lossy().as_ref()).await
        })
        .expect("parent reopens recovered session");
        assert!(
            diagnostics.skipped_entries.is_empty(),
            "parent reopen should not skip entries after {}: {diagnostics:?}",
            mode.as_str()
        );
        let parent_message_count = reopened
            .entries_for_current_path()
            .iter()
            .filter(|entry| matches!(entry, SessionEntry::Message(_)))
            .count();
        let restart_message_count = restart_summary
            .get("messageCountAfterRestart")
            .and_then(Value::as_u64)
            .expect("restart message count");
        assert!(
            restart_message_count > message_count_before_restart,
            "restart should append messages after the recovered baseline"
        );
        assert_eq!(
            u64::try_from(parent_message_count).unwrap_or(u64::MAX),
            restart_message_count,
            "parent reopen must observe the exact post-restart message count"
        );

        harness.record_artifact(
            crash_interrupt_artifact_name("ready", cycle, mode, "json"),
            &ready_path,
        );
        harness.record_artifact(
            crash_interrupt_artifact_name("restart", cycle, mode, "json"),
            &restart_summary_path,
        );
        logger.info_ctx("crash-interrupt-recovery", "cycle recovered", |ctx| {
            ctx.push(crash_interrupt_context_field("cycle", cycle));
            ctx.push(crash_interrupt_context_field("interrupt", mode.as_str()));
            ctx.push(("session_path".into(), parent_session_path_display_for_log));
            ctx.push(crash_interrupt_context_field(
                "parent_message_count",
                parent_message_count,
            ));
        });

        cycle_summaries.push(json!({
            "cycle": cycle,
            "interrupt": mode.as_str(),
            "activeExitSignal": active_result.exit_signal,
            "sessionPath": parent_session_path_display,
            "parentMessageCount": parent_message_count,
            "restart": restart_summary,
            "ready": ready,
        }));
    }

    let index = SessionIndex::for_sessions_root(&sessions_root);
    let index_summary = index
        .refresh_incremental()
        .expect("parent final refresh session index");
    assert_eq!(
        index_summary.failed_files, 0,
        "final reindex should not report failed files"
    );
    let indexed = index
        .list_sessions(Some(&project_dir.display().to_string()))
        .expect("parent list recovered sessions");
    assert!(
        indexed.len() >= schedule.len(),
        "session index should include every interrupted cycle"
    );
    let tempish_files = count_tempish_files(&sessions_root) + count_tempish_files(&artifact_dir);
    assert!(
        tempish_files <= schedule.len(),
        "temp artifact growth exceeded one file per cycle: tempish_files={tempish_files}, cycles={}",
        schedule.len()
    );
    assert_dir_lock_released(&sessions_root.join("session-index.lock"));

    let summary_path = harness.temp_path("crash-interrupt-recovery-soak-summary.json");
    let summary = json!({
        "schema": CRASH_INTERRUPT_RECOVERY_HARNESS_SCHEMA,
        "mode": if std::env::var_os("PI_CRASH_INTERRUPT_RECOVERY_LONG").is_some() {
            "long"
        } else {
            "ci"
        },
        "cycles": schedule.len(),
        "interrupts": schedule.iter().map(|mode| mode.as_str()).collect::<Vec<_>>(),
        "sessionsRoot": sessions_root.display().to_string(),
        "indexedSessionCount": indexed.len(),
        "indexFailedFiles": index_summary.failed_files,
        "tempishFileCount": tempish_files,
        "cyclesRecovered": cycle_summaries,
    });
    write_json_atomic(&summary_path, &summary);
    harness.record_artifact("crash-interrupt-recovery-soak-summary.json", &summary_path);
    Ok(())
}

#[cfg(unix)]
#[test]
fn rpc_binary_sigint_exits_orderly_and_preserves_session() -> std::io::Result<()> {
    let harness = TestHarness::new("rpc_binary_sigint_orderly_shutdown");
    let project_dir = harness.temp_path("project");
    let sessions_root = harness.temp_path("sessions");
    let agent_dir = harness.temp_path("agent");
    let package_dir = harness.temp_path("packages");
    let config_path = harness.temp_path("settings.json");
    let marker_path = harness.temp_path("sigint-marker.txt");
    std::fs::create_dir_all(&project_dir)?;
    std::fs::create_dir_all(&sessions_root)?;
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::create_dir_all(&package_dir)?;

    let mut command = Command::new(env!("CARGO_BIN_EXE_pi")); // ubs:ignore Cargo-provided test binary path, not user input.
    command
        .args([
            "--rpc",
            "--provider",
            "ollama",
            "--model",
            "qwen2.5:0.5b",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-migrations",
            "--session-dir",
        ])
        .arg(&sessions_root)
        .env("PI_CODING_AGENT_DIR", &agent_dir)
        .env("PI_CONFIG_PATH", &config_path)
        .env("PI_SESSIONS_DIR", &sessions_root)
        .env("PI_PACKAGE_DIR", &package_dir)
        .env("PI_TEST_MODE", "1")
        .current_dir(&project_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn()?;
    let mut worker = capture_crash_interrupt_worker(child);

    let marker_content = "real-rpc-sigint-persistence";
    let bash_command = format!(
        "printf {} > {}",
        shell_single_quote(marker_content),
        shell_single_quote(&marker_path.display().to_string())
    );
    let bash_request = json!({
        "id": "real-rpc-sigint-bash",
        "type": "bash",
        "command": bash_command,
    });
    {
        use std::io::Write as _;

        let stdin = worker.child.stdin.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pi RPC stdin was not piped")
        })?;
        writeln!(stdin, "{bash_request}")?;
        stdin.flush()?;
    }

    let index = SessionIndex::for_sessions_root(&sessions_root);
    let cwd = project_dir.display().to_string();
    let started_at = Instant::now();
    let persisted_meta = loop {
        if marker_path.exists()
            && let Ok(indexed) = index.list_sessions(Some(&cwd))
            && let Some(meta) = indexed.into_iter().find(|meta| meta.message_count >= 1)
        {
            break meta;
        }

        if let Some(status) = worker.child.try_wait()? {
            let (stdout, stderr) = collect_crash_interrupt_worker_output(&mut worker);
            return Err(std::io::Error::other(format!(
                "pi --rpc exited before persisting the SIGINT fixture: status={status}\nstdout:\n{stdout}\nstderr:\n{stderr}"
            )));
        }
        if started_at.elapsed() > CRASH_INTERRUPT_RECOVERY_DEFAULT_TIMEOUT {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for pi --rpc to persist the SIGINT fixture",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    // The soak worker intentionally restores default signal death. This sends
    // SIGINT to the real binary so `run_rpc_mode` takes its orderly Ctrl-C path.
    signal_crash_interrupt_recovery_worker(&worker, CrashInterruptRecoveryMode::Sigint);
    let result =
        wait_crash_interrupt_recovery_worker(worker, CRASH_INTERRUPT_RECOVERY_DEFAULT_TIMEOUT);
    assert!(
        !result.timed_out,
        "real pi --rpc SIGINT shutdown timed out: {}",
        result.stderr
    );
    assert_eq!(
        result.exit_signal, None,
        "real pi --rpc must handle SIGINT instead of dying by signal: stdout={} stderr={}",
        result.stdout, result.stderr
    );
    assert_eq!(
        result.exit_code, 0,
        "real pi --rpc SIGINT shutdown should succeed: stdout={} stderr={}",
        result.stdout, result.stderr
    );

    let session_path = PathBuf::from(&persisted_meta.path);
    let (reopened, diagnostics) = common::run_async(async move {
        Session::open_with_diagnostics(session_path.to_string_lossy().as_ref()).await
    })
    .expect("reopen session after orderly RPC SIGINT");
    assert!(
        diagnostics.skipped_entries.is_empty(),
        "orderly RPC SIGINT should leave a clean session: {diagnostics:?}"
    );
    assert_eq!(
        reopened.header.id, persisted_meta.id,
        "orderly RPC SIGINT must preserve the indexed session identity"
    );
    assert_eq!(
        u64::try_from(reopened.to_messages_for_current_path().len()).unwrap_or(u64::MAX),
        persisted_meta.message_count,
        "orderly RPC SIGINT must preserve the indexed message count"
    );
    assert!(
        reopened
            .entries_for_current_path()
            .iter()
            .copied()
            .any(|entry| {
                matches!(
                    entry,
                    SessionEntry::Message(message)
                        if matches!(
                            &message.message,
                            SessionMessage::BashExecution { command, .. }
                                if command == &bash_command
                        )
                )
            }),
        "orderly RPC SIGINT must preserve the completed bash message"
    );
    assert_eq!(std::fs::read_to_string(marker_path)?, marker_content);

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests: Configuration commands
// ---------------------------------------------------------------------------

#[test]
fn rpc_set_steering_mode_valid() {
    let harness = TestHarness::new("rpc_set_steering_mode_valid");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Set to "all"
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_steering_mode","mode":"all"}"#,
            "set_steering_mode(all)",
        )
        .await;
        assert_ok(&resp, "set_steering_mode");
        assert_eq!(resp["id"], "1");

        // Set to "one-at-a-time"
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_steering_mode","mode":"one-at-a-time"}"#,
            "set_steering_mode(one-at-a-time)",
        )
        .await;
        assert_ok(&resp, "set_steering_mode");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_steering_mode_invalid() {
    let harness = TestHarness::new("rpc_set_steering_mode_invalid");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Missing mode
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_steering_mode"}"#,
            "set_steering_mode(missing)",
        )
        .await;
        assert_err(&resp, "set_steering_mode");
        assert_eq!(resp["error"], "Missing mode");

        // Invalid mode
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_steering_mode","mode":"bogus"}"#,
            "set_steering_mode(bogus)",
        )
        .await;
        assert_err(&resp, "set_steering_mode");
        assert_eq!(resp["error"], "Invalid steering mode");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_follow_up_mode_valid_and_invalid() {
    let harness = TestHarness::new("rpc_set_follow_up_mode_valid_and_invalid");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Valid
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_follow_up_mode","mode":"all"}"#,
            "set_follow_up_mode(all)",
        )
        .await;
        assert_ok(&resp, "set_follow_up_mode");

        // Missing mode
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_follow_up_mode"}"#,
            "set_follow_up_mode(missing)",
        )
        .await;
        assert_err(&resp, "set_follow_up_mode");
        assert_eq!(resp["error"], "Missing mode");

        // Invalid mode
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"3","type":"set_follow_up_mode","mode":"nope"}"#,
            "set_follow_up_mode(nope)",
        )
        .await;
        assert_err(&resp, "set_follow_up_mode");
        assert_eq!(resp["error"], "Invalid follow-up mode");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_auto_compaction_and_retry() {
    let harness = TestHarness::new("rpc_set_auto_compaction_and_retry");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // set_auto_compaction true
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_auto_compaction","enabled":true}"#,
            "set_auto_compaction(true)",
        )
        .await;
        assert_ok(&resp, "set_auto_compaction");

        // set_auto_compaction missing enabled
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_auto_compaction"}"#,
            "set_auto_compaction(missing)",
        )
        .await;
        assert_err(&resp, "set_auto_compaction");
        assert_eq!(resp["error"], "Missing enabled");

        // set_auto_retry false
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"3","type":"set_auto_retry","enabled":false}"#,
            "set_auto_retry(false)",
        )
        .await;
        assert_ok(&resp, "set_auto_retry");

        // set_auto_retry missing enabled
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"4","type":"set_auto_retry"}"#,
            "set_auto_retry(missing)",
        )
        .await;
        assert_err(&resp, "set_auto_retry");
        assert_eq!(resp["error"], "Missing enabled");

        // abort_retry (always succeeds)
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"5","type":"abort_retry"}"#,
            "abort_retry",
        )
        .await;
        assert_ok(&resp, "abort_retry");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Model / Thinking Level commands
// ---------------------------------------------------------------------------

#[test]
fn rpc_get_available_models_empty() {
    let harness = TestHarness::new("rpc_get_available_models_empty");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_available_models"}"#,
            "get_available_models",
        )
        .await;
        assert_ok(&resp, "get_available_models");
        let models = resp["data"]["models"].as_array().unwrap();
        assert!(models.is_empty(), "expected empty model list");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_available_models_populated() {
    let harness = TestHarness::new("rpc_get_available_models_populated");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let models = vec![
            test_entry("claude-opus-4-6", true),
            test_entry("gpt-4o", false),
        ];
        let options = build_options(&handle, harness.temp_path("auth.json"), models, vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_available_models"}"#,
            "get_available_models",
        )
        .await;
        assert_ok(&resp, "get_available_models");
        let models = resp["data"]["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "claude-opus-4-6");
        assert_eq!(models[0]["reasoning"], true);
        assert_eq!(models[1]["id"], "gpt-4o");
        assert_eq!(models[1]["reasoning"], false);

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_thinking_level_success() {
    let harness = TestHarness::new("rpc_set_thinking_level_success");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Set to high
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_thinking_level","level":"high"}"#,
            "set_thinking_level(high)",
        )
        .await;
        assert_ok(&resp, "set_thinking_level");

        // Set to off
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_thinking_level","level":"off"}"#,
            "set_thinking_level(off)",
        )
        .await;
        assert_ok(&resp, "set_thinking_level");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_thinking_level_errors() {
    let harness = TestHarness::new("rpc_set_thinking_level_errors");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Missing level
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_thinking_level"}"#,
            "set_thinking_level(missing)",
        )
        .await;
        assert_err(&resp, "set_thinking_level");
        assert_eq!(resp["error"], "Missing level");

        // Invalid level
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_thinking_level","level":"impossible"}"#,
            "set_thinking_level(impossible)",
        )
        .await;
        assert_err(&resp, "set_thinking_level");
        assert!(
            resp["error"].as_str().is_some_and(|s| !s.is_empty()),
            "expected error message"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Session data commands
// ---------------------------------------------------------------------------

#[test]
fn rpc_get_messages_empty_session() {
    let harness = TestHarness::new("rpc_get_messages_empty_session");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_messages"}"#,
            "get_messages(empty)",
        )
        .await;
        assert_ok(&resp, "get_messages");
        let messages = resp["data"]["messages"].as_array().unwrap();
        assert!(messages.is_empty(), "expected empty messages");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_messages_with_user_messages() {
    let harness = TestHarness::new("rpc_get_messages_with_user_messages");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let now = 1_700_000_000_000i64;
        let mut session = Session::in_memory();
        session.append_message(SessionMessage::User {
            content: UserContent::Text("hello".to_string()),
            timestamp: Some(now),
        });
        session.append_message(SessionMessage::User {
            content: UserContent::Text("world".to_string()),
            timestamp: Some(now + 1000),
        });

        let agent_session = build_agent_session(session, &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_messages"}"#,
            "get_messages(with_users)",
        )
        .await;
        assert_ok(&resp, "get_messages");
        let messages = resp["data"]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "user");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_last_assistant_text_empty() {
    let harness = TestHarness::new("rpc_get_last_assistant_text_empty");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_last_assistant_text"}"#,
            "get_last_assistant_text(empty)",
        )
        .await;
        assert_ok(&resp, "get_last_assistant_text");
        assert!(resp["data"]["text"].is_null(), "expected null text");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_last_assistant_text_with_assistant() {
    let harness = TestHarness::new("rpc_get_last_assistant_text_with_assistant");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let now = 1_700_000_000_000i64;
        let mut session = Session::in_memory();
        session.append_message(SessionMessage::User {
            content: UserContent::Text("hello".to_string()),
            timestamp: Some(now),
        });
        session.append_message(SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("Hi there!"))],
                api: "test".to_string(),
                provider: "test".to_string(),
                model: "test-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: now,
            },
        });

        let agent_session = build_agent_session(session, &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_last_assistant_text"}"#,
            "get_last_assistant_text(with_assistant)",
        )
        .await;
        assert_ok(&resp, "get_last_assistant_text");
        assert_eq!(resp["data"]["text"], "Hi there!");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_commands_empty() {
    let harness = TestHarness::new("rpc_get_commands_empty");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_commands"}"#,
            "get_commands",
        )
        .await;
        assert_ok(&resp, "get_commands");
        assert!(
            resp["data"]["commands"].is_array(),
            "commands should be an array"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Session management commands
// ---------------------------------------------------------------------------

#[test]
fn rpc_set_session_name_success() {
    let harness = TestHarness::new("rpc_set_session_name_success");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_session_name","name":"Test Session"}"#,
            "set_session_name",
        )
        .await;
        assert_ok(&resp, "set_session_name");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_session_name_missing_name() {
    let harness = TestHarness::new("rpc_set_session_name_missing_name");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_session_name"}"#,
            "set_session_name(missing)",
        )
        .await;
        assert_err(&resp, "set_session_name");
        assert_eq!(resp["error"], "Missing name");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Bash command
// ---------------------------------------------------------------------------

#[test]
fn rpc_bash_echo() {
    let harness = TestHarness::new("rpc_bash_echo");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"bash","command":"echo hello_rpc"}"#,
            "bash(echo)",
        )
        .await;
        assert_ok(&resp, "bash");
        assert_eq!(resp["data"]["exitCode"], 0);
        let output = resp["data"]["output"].as_str().unwrap_or("");
        assert!(
            output.contains("hello_rpc"),
            "bash output should contain hello_rpc, got: {output}"
        );
        assert_eq!(resp["data"]["persisted"], false);
        assert_eq!(
            resp["data"]["persistenceStatus"]["event"],
            "session.persistence.disabled"
        );
        assert!(
            resp["data"]["persistenceStatus"]["pendingMessageCount"]
                .as_u64()
                .is_some_and(|count| count > 0),
            "the in-memory BashExecution mutation must remain observable: {resp}"
        );
        assert!(
            resp["data"]["persistenceWarning"].is_null(),
            "disabled persistence is an expected mode, not a save failure"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_bash_persistence_failure_reports_real_backlog_without_retryable_error() {
    let harness = TestHarness::new("rpc_bash_persistence_failure_reports_real_backlog");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let blocked_session_dir = harness.create_file("not-a-session-directory", b"blocked");
        let session = Session::create_with_dir(Some(blocked_session_dir));
        let metrics_before = session.autosave_metrics();
        let provider: Arc<dyn Provider> = Arc::new(KeylessReplayProvider::new(
            "keyless-rpc-replay",
            KEYLESS_REPLAY_ABORT_WINDOW,
        ));
        let tools = ToolRegistry::new(&[], &harness.temp_path("worktree"), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(asupersync::sync::Mutex::new(session));
        let agent_session = AgentSession::new(
            agent,
            Arc::clone(&session),
            true,
            pi::compaction::ResolvedCompactionSettings::default(),
        );
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"bash","command":"echo persistence_failure_probe"}"#,
            "bash(persistence failure)",
        )
        .await;
        assert_ok(&resp, "bash");
        assert_eq!(resp["data"]["exitCode"], 0);
        assert!(
            resp["data"]["output"]
                .as_str()
                .is_some_and(|output| output.contains("persistence_failure_probe")),
            "the command result must survive the independent save failure: {resp}"
        );
        assert_eq!(resp["data"]["persisted"], false);
        assert_eq!(
            resp["data"]["persistenceStatus"]["event"],
            "session.persistence.backlog"
        );
        let metrics_cx = asupersync::Cx::for_testing();
        let metrics_after = session
            .lock(&metrics_cx)
            .await
            .expect("lock failed-persistence session")
            .autosave_metrics();
        let live_pending_mutations = metrics_after.pending_mutations as u64;
        assert!(
            live_pending_mutations > 0,
            "the failed flush must retain the appended BashExecution mutation"
        );
        assert_eq!(
            resp["data"]["persistenceStatus"]["pendingMessageCount"],
            live_pending_mutations,
            "the response must expose the retained autosave backlog: {resp}"
        );
        assert!(
            resp["data"]["persistenceWarning"].is_string(),
            "the successful command response must carry a non-retryable save warning: {resp}"
        );
        assert_eq!(
            metrics_after.coalesced_mutations,
            metrics_before.coalesced_mutations,
            "the first pending BashExecution mutation must not be reported as coalesced"
        );
        assert_eq!(
            metrics_after.pending_mutations,
            metrics_before
                .pending_mutations
                .saturating_add(1)
                .min(metrics_before.max_pending_mutations),
            "the failed flush must retain the appended mutation"
        );
        assert_eq!(
            metrics_after.flush_started,
            metrics_before.flush_started + 1,
            "the backlog response must follow a real flush attempt"
        );
        assert_eq!(
            metrics_after.flush_failed,
            metrics_before.flush_failed + 1,
            "the real flush attempt must record its failure"
        );
        assert_eq!(
            metrics_after.flush_succeeded, metrics_before.flush_succeeded,
            "the failed flush must not claim durable success"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_bash_missing_command() {
    let harness = TestHarness::new("rpc_bash_missing_command");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"bash"}"#,
            "bash(missing)",
        )
        .await;
        assert_err(&resp, "bash");
        assert_eq!(resp["error"], "Missing command");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_bash_nonzero_exit() {
    let harness = TestHarness::new("rpc_bash_nonzero_exit");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"bash","command":"exit 42"}"#,
            "bash(exit 42)",
        )
        .await;
        assert_ok(&resp, "bash");
        assert_eq!(resp["data"]["exitCode"], 42);

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[cfg(target_os = "linux")]
#[test]
fn rpc_abort_bash_kills_background_children() {
    let harness = TestHarness::new("rpc_abort_bash_kills_background_children");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let marker = harness.temp_path("rpc_bash_survived.txt");
        let marker_str = marker.to_string_lossy();
        let command = format!(
            r#"{{"id":"1","type":"bash","command":"(sleep 3; echo leaked > '{marker_str}') & sleep 30"}}"#
        );
        let cx = asupersync::Cx::for_testing();
        in_tx
            .send(&cx, command)
            .await
            .expect("send long-running bash");

        asupersync::time::sleep(
            asupersync::time::wall_now(),
            Duration::from_millis(100),
        )
        .await;

        in_tx
            .send(&cx, r#"{"id":"2","type":"abort_bash"}"#.to_string())
            .await
            .expect("send abort_bash");

        let mut abort_resp = None;
        let mut bash_resp = None;
        for label in ["rpc_abort_bash:first", "rpc_abort_bash:second"] {
            let resp = parse_response(
                &recv_line(&out_rx, label)
                    .await
                    .unwrap_or_else(|err| panic!("{err}")),
            );
            match (resp["command"].as_str(), resp["id"].as_str()) {
                (Some("abort_bash"), Some("2")) => abort_resp = Some(resp),
                (Some("bash"), Some("1")) => bash_resp = Some(resp),
                other => panic!("unexpected response ordering/content: {other:?}"),
            }
        }

        let abort_resp = abort_resp.expect("missing abort_bash response");
        assert_ok(&abort_resp, "abort_bash");
        let bash_resp = bash_resp.expect("missing bash response");
        assert_ok(&bash_resp, "bash");
        assert_eq!(bash_resp["id"], "1");
        assert_eq!(bash_resp["data"]["cancelled"], true);

        std::thread::sleep(Duration::from_secs(4));
        assert!(
            !marker.exists(),
            "background child survived rpc abort"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Request ID handling
// ---------------------------------------------------------------------------

#[test]
fn rpc_request_id_preserved() {
    let harness = TestHarness::new("rpc_request_id_preserved");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // With string ID
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"custom-id-123","type":"get_state"}"#,
            "get_state(with id)",
        )
        .await;
        assert_ok(&resp, "get_state");
        assert_eq!(resp["id"], "custom-id-123");

        // With numeric ID (RPC server uses as_str(), so numeric IDs are treated as absent)
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":42,"type":"get_state"}"#,
            "get_state(numeric id)",
        )
        .await;
        assert_ok(&resp, "get_state");
        assert!(
            resp.get("id").is_none() || resp["id"].is_null(),
            "numeric IDs should be treated as absent (parsed via as_str)"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_request_without_id() {
    let harness = TestHarness::new("rpc_request_without_id");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Request without id field
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"type":"get_state"}"#,
            "get_state(no id)",
        )
        .await;
        assert_ok(&resp, "get_state");
        // id should be null or absent
        assert!(
            resp.get("id").is_none() || resp["id"].is_null(),
            "expected no id or null id"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Multiple rapid commands
// ---------------------------------------------------------------------------

#[test]
fn rpc_rapid_sequence_of_sync_commands() {
    let harness = TestHarness::new("rpc_rapid_sequence_of_sync_commands");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let models = vec![test_entry("test-model", false)];
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), models, vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(32);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let cx = asupersync::Cx::for_testing();

        // Fire 8 commands rapidly.
        let commands = [
            (r#"{"id":"1","type":"get_state"}"#, "get_state"),
            (
                r#"{"id":"2","type":"get_available_models"}"#,
                "get_available_models",
            ),
            (r#"{"id":"3","type":"get_messages"}"#, "get_messages"),
            (
                r#"{"id":"4","type":"get_session_stats"}"#,
                "get_session_stats",
            ),
            (r#"{"id":"5","type":"get_commands"}"#, "get_commands"),
            (
                r#"{"id":"6","type":"get_last_assistant_text"}"#,
                "get_last_assistant_text",
            ),
            (
                r#"{"id":"7","type":"set_auto_compaction","enabled":true}"#,
                "set_auto_compaction",
            ),
            (
                r#"{"id":"8","type":"set_auto_retry","enabled":false}"#,
                "set_auto_retry",
            ),
        ];

        for (cmd, _label) in &commands {
            in_tx
                .send(&cx, cmd.to_string())
                .await
                .expect("send rapid command");
        }

        // Collect all 8 responses.
        let mut responses = Vec::new();
        for (_, label) in &commands {
            let line = recv_line(&out_rx, label)
                .await
                .unwrap_or_else(|err| panic!("{err}"));
            responses.push(parse_response(&line));
        }

        // Verify each response matches its command.
        for (i, (_, expected_cmd)) in commands.iter().enumerate() {
            assert_ok(&responses[i], expected_cmd);
            assert_eq!(
                responses[i]["id"],
                serde_json::Value::String((i + 1).to_string())
            );
        }

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: State reflection after mutations
// ---------------------------------------------------------------------------

#[test]
fn rpc_get_state_reflects_session_stats() {
    let harness = TestHarness::new("rpc_get_state_reflects_session_stats");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let now = 1_700_000_000_000i64;
        let mut session = Session::in_memory();
        session.header.provider = Some("anthropic".to_string());
        session.header.model_id = Some("claude-opus-4-6".to_string());
        session.append_message(SessionMessage::User {
            content: UserContent::Text("hello".to_string()),
            timestamp: Some(now),
        });
        session.append_message(SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("world"))],
                api: "test".to_string(),
                provider: "anthropic".to_string(),
                model: "claude-opus-4-6".to_string(),
                usage: Usage {
                    input: 10,
                    output: 5,
                    total_tokens: 15,
                    ..Usage::default()
                },
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: now,
            },
        });

        let agent_session = build_agent_session(session, &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // get_session_stats should reflect pre-populated messages
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_session_stats"}"#,
            "get_session_stats",
        )
        .await;
        assert_ok(&resp, "get_session_stats");
        assert_eq!(resp["data"]["userMessages"], 1);
        assert_eq!(resp["data"]["assistantMessages"], 1);
        assert_eq!(resp["data"]["totalMessages"], 2);
        assert_eq!(resp["data"]["tokens"]["input"], 10);
        assert_eq!(resp["data"]["tokens"]["output"], 5);
        assert_eq!(resp["data"]["tokens"]["total"], 15);
        assert_eq!(
            resp["data"]["persistenceStatus"]["event"],
            "session.persistence.disabled"
        );
        assert_eq!(
            resp["data"]["persistenceStatus"]["pendingMessageCount"],
            2,
            "in-memory mutations remain observable without being mislabeled as a durable backlog"
        );

        // get_messages should return the 2 messages
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"get_messages"}"#,
            "get_messages",
        )
        .await;
        assert_ok(&resp, "get_messages");
        let msgs = resp["data"]["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");

        // get_last_assistant_text should return "world"
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"3","type":"get_last_assistant_text"}"#,
            "get_last_assistant_text",
        )
        .await;
        assert_ok(&resp, "get_last_assistant_text");
        assert_eq!(resp["data"]["text"], "world");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Error path coverage
// ---------------------------------------------------------------------------

#[test]
fn rpc_prompt_missing_message() {
    let harness = TestHarness::new("rpc_prompt_missing_message");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"prompt"}"#,
            "prompt(missing message)",
        )
        .await;
        assert_err(&resp, "prompt");
        assert_eq!(resp["error"], "Missing message");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_prompt_dispatches_registered_extension_command() {
    let harness = TestHarness::new("rpc_prompt_dispatches_registered_extension_command");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, _manager) = build_agent_session_with_js_extension(
            Session::in_memory(),
            &cassette_dir,
            &harness,
            RPC_PROMPT_EXTENSION_COMMAND_EXT,
        )
        .await;
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"prompt","message":"/emit-now"}"#,
            "prompt(extension command)",
        )
        .await;
        assert_ok(&resp, "prompt");

        let messages = wait_for_custom_message(
            &in_tx,
            &out_rx,
            "rpc-note",
            "rpc-message",
            "get_messages(after extension command)",
        )
        .await;
        let messages = messages["data"]["messages"]
            .as_array()
            .expect("messages array");
        assert!(
            messages.iter().any(|message| {
                message["role"] == "custom"
                    && message["customType"] == "rpc-note"
                    && message["content"] == "rpc-message"
            }),
            "expected RPC prompt command to append custom message, got {messages:?}"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_steer_missing_message() {
    let harness = TestHarness::new("rpc_steer_missing_message");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"steer"}"#,
            "steer(missing message)",
        )
        .await;
        assert_err(&resp, "steer");
        assert_eq!(resp["error"], "Missing message");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_follow_up_missing_message() {
    let harness = TestHarness::new("rpc_follow_up_missing_message");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"follow_up"}"#,
            "follow_up(missing message)",
        )
        .await;
        assert_err(&resp, "follow_up");
        assert_eq!(resp["error"], "Missing message");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_model_missing_model_id() {
    let harness = TestHarness::new("rpc_set_model_missing_model_id");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Missing provider
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_model","modelId":"x"}"#,
            "set_model(missing provider)",
        )
        .await;
        assert_err(&resp, "set_model");
        assert_eq!(resp["error"], "Missing provider");

        // Missing modelId
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_model","provider":"anthropic"}"#,
            "set_model(missing modelId)",
        )
        .await;
        assert_err(&resp, "set_model");
        assert_eq!(resp["error"], "Missing modelId");

        // Model not found
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"3","type":"set_model","provider":"anthropic","modelId":"nonexistent"}"#,
            "set_model(not found)",
        )
        .await;
        assert_err(&resp, "set_model");
        assert!(
            resp["error"]
                .as_str()
                .is_some_and(|s| s.contains("Model not found")),
            "expected model not found error: {resp}"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_fork_missing_entry_id() {
    let harness = TestHarness::new("rpc_fork_missing_entry_id");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"fork"}"#,
            "fork(missing entryId)",
        )
        .await;
        assert_err(&resp, "fork");
        assert_eq!(resp["error"], "Missing entryId");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_export_html_empty_session() {
    let harness = TestHarness::new("rpc_export_html_empty_session");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let output = harness.temp_path("export.html");
        let cmd = serde_json::json!({
            "id": "1",
            "type": "export_html",
            "outputPath": output.display().to_string()
        })
        .to_string();
        let resp = send_recv(&in_tx, &out_rx, &cmd, "export_html").await;
        assert_ok(&resp, "export_html");
        assert!(resp["data"]["path"].is_string(), "should return path");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: abort (when not streaming)
// ---------------------------------------------------------------------------

#[test]
fn rpc_abort_when_idle() {
    let harness = TestHarness::new("rpc_abort_when_idle");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Abort when nothing is streaming should still succeed.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"abort"}"#,
            "abort(idle)",
        )
        .await;
        assert_ok(&resp, "abort");

        // abort_bash when nothing is running should also succeed.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"abort_bash"}"#,
            "abort_bash(idle)",
        )
        .await;
        assert_ok(&resp, "abort_bash");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Helpers: Extension UI roundtrip
// ---------------------------------------------------------------------------

/// Build an `AgentSession` with an `ExtensionManager` attached so the RPC
/// server sets up the extension UI channel.  Returns both the session (to
/// hand to `run()`) and a cloned `ExtensionManager` the test can use to
/// call `request_ui()` / `respond_ui()`.
fn build_agent_session_with_extensions(
    session: Session,
    cassette_dir: &Path,
) -> (AgentSession, ExtensionManager) {
    let manager = ExtensionManager::default();
    let region = ExtensionRegion::new(manager.clone());
    let mut agent_session = build_agent_session(session, cassette_dir);
    agent_session.extensions = Some(region);
    (agent_session, manager)
}

async fn build_agent_session_with_js_extension(
    session: Session,
    cassette_dir: &Path,
    harness: &TestHarness,
    source: &str,
) -> (AgentSession, ExtensionManager) {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/ext.mjs", source.as_bytes());
    let mut agent_session = build_agent_session(session, cassette_dir);
    agent_session
        .enable_extensions(&[], &cwd, None, &[ext_entry_path])
        .await
        .expect("enable extensions");
    let manager = agent_session
        .extensions
        .as_ref()
        .expect("extension region")
        .manager()
        .clone();
    (agent_session, manager)
}

const SESSION_SWITCH_CANCEL_EXT: &str = r#"
export default function init(pi) {
    pi.on("session_before_switch", () => ({ cancelled: true }));
}
"#;

const SESSION_SWITCH_RECORD_EXT: &str = r#"
export default function init(pi) {
    const events = [];

    pi.on("session_before_switch", () => ({ cancelled: false }));
    pi.on("session_switch", (event) => {
        events.push(event);
        return null;
    });

    pi.registerCommand("get-events", {
        description: "Return recorded session switch events",
        handler: async () => JSON.stringify(events),
    });
}
"#;

const RPC_PROMPT_EXTENSION_COMMAND_EXT: &str = r#"
export default function init(pi) {
    pi.registerCommand("emit-now", {
        description: "Record a custom message through RPC prompt dispatch",
        handler: async () => {
            await pi.events("sendMessage", {
                message: {
                    customType: "rpc-note",
                    content: "rpc-message",
                    display: true
                },
                options: { triggerTurn: false }
            });
            return "queued";
        }
    });
}
"#;

/// Wait for an `extension_ui_request` event on the RPC output channel.
/// Skips any non-event / non-ui-request lines.
async fn recv_ui_request(out_rx: &Arc<Mutex<Receiver<String>>>, label: &str) -> Value {
    let start = Instant::now();
    loop {
        let recv_result = {
            let rx = out_rx.lock().expect("lock rpc output receiver");
            rx.try_recv()
        };

        match recv_result {
            Ok(line) => {
                if let Ok(val) = serde_json::from_str::<Value>(&line)
                    && val.get("type").and_then(Value::as_str) == Some("extension_ui_request")
                {
                    return val;
                }
                // Not our event — keep waiting.
            }
            Err(TryRecvError::Disconnected) => {
                panic!(
                    "{label}: output channel disconnected while waiting for extension_ui_request"
                );
            }
            Err(TryRecvError::Empty) => {}
        }

        assert!(
            start.elapsed() <= RPC_E2E_WAIT_TIMEOUT,
            "{label}: timed out waiting for extension_ui_request"
        );
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5)).await;
    }
}

async fn wait_for_custom_message(
    in_tx: &asupersync::channel::mpsc::Sender<String>,
    out_rx: &Arc<Mutex<Receiver<String>>>,
    custom_type: &str,
    content: &str,
    label: &str,
) -> Value {
    let start = Instant::now();
    let mut attempt = 0usize;

    loop {
        attempt = attempt.saturating_add(1);
        let cmd = json!({
            "id": format!("wait-{attempt}"),
            "type": "get_messages",
        })
        .to_string();
        let resp = send_recv(in_tx, out_rx, &cmd, label).await;
        assert_ok(&resp, "get_messages");

        if resp["data"]["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|message| {
                message["role"] == "custom"
                    && message["customType"] == custom_type
                    && message["content"] == content
            })
        }) {
            return resp;
        }

        assert!(
            start.elapsed() <= RPC_E2E_WAIT_TIMEOUT,
            "{label}: timed out waiting for custom message {custom_type}/{content}"
        );
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests: RPC session switch hooks
// ---------------------------------------------------------------------------

#[test]
fn rpc_new_session_can_be_cancelled_by_extension() {
    let harness = TestHarness::new("rpc_new_session_can_be_cancelled_by_extension");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let session = Session::in_memory();
        let original_session_id = session.header.id.clone();
        let (agent_session, _manager) = build_agent_session_with_js_extension(
            session,
            &cassette_dir,
            &harness,
            SESSION_SWITCH_CANCEL_EXT,
        )
        .await;
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"new_session"}"#,
            "new_session(cancelled)",
        )
        .await;
        assert_ok(&resp, "new_session");
        assert_eq!(resp["data"]["cancelled"], true);

        let state = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"get_state"}"#,
            "get_state(after cancelled new_session)",
        )
        .await;
        assert_ok(&state, "get_state");
        assert_eq!(state["data"]["sessionId"], original_session_id);

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_switch_session_can_be_cancelled_by_extension() {
    let harness = TestHarness::new("rpc_switch_session_can_be_cancelled_by_extension");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let session = Session::in_memory();
        let original_session_id = session.header.id.clone();

        let target_root = tempfile::tempdir().expect("target session dir");
        let mut target = Session::create_with_dir(Some(target_root.path().to_path_buf()));
        target.append_message(SessionMessage::User {
            content: UserContent::Text("target".to_string()),
            timestamp: Some(1_700_000_000_000),
        });
        target.save().await.expect("save target session");
        let target_path = target
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .expect("target session path");

        let (agent_session, _manager) = build_agent_session_with_js_extension(
            session,
            &cassette_dir,
            &harness,
            SESSION_SWITCH_CANCEL_EXT,
        )
        .await;
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let cmd = json!({
            "id": "1",
            "type": "switch_session",
            "sessionPath": target_path,
        })
        .to_string();
        let resp = send_recv(&in_tx, &out_rx, &cmd, "switch_session(cancelled)").await;
        assert_ok(&resp, "switch_session");
        assert_eq!(resp["data"]["cancelled"], true);

        let state = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"get_state"}"#,
            "get_state(after cancelled switch_session)",
        )
        .await;
        assert_ok(&state, "get_state");
        assert_eq!(state["data"]["sessionId"], original_session_id);

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_session_switch_events_are_emitted_for_new_and_resume() {
    let harness = TestHarness::new("rpc_session_switch_events_are_emitted_for_new_and_resume");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let current_root = tempfile::tempdir().expect("current session dir");
        let mut current = Session::create_with_dir(Some(current_root.path().to_path_buf()));
        current.append_message(SessionMessage::User {
            content: UserContent::Text("current".to_string()),
            timestamp: Some(1_700_000_000_000),
        });
        current.save().await.expect("save current session");
        let current_path = current
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .expect("current session path");

        let target_root = tempfile::tempdir().expect("target session dir");
        let mut target = Session::create_with_dir(Some(target_root.path().to_path_buf()));
        target.append_message(SessionMessage::User {
            content: UserContent::Text("target".to_string()),
            timestamp: Some(1_700_000_000_100),
        });
        target.save().await.expect("save target session");
        let target_path = target
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .expect("target session path");

        let (agent_session, manager) = build_agent_session_with_js_extension(
            current,
            &cassette_dir,
            &harness,
            SESSION_SWITCH_RECORD_EXT,
        )
        .await;
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let new_resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"new_session"}"#,
            "new_session(recorded)",
        )
        .await;
        assert_ok(&new_resp, "new_session");
        assert_eq!(new_resp["data"]["cancelled"], false);

        let switch_cmd = json!({
            "id": "2",
            "type": "switch_session",
            "sessionPath": target_path.clone(),
        })
        .to_string();
        let switch_resp = send_recv(&in_tx, &out_rx, &switch_cmd, "switch_session(recorded)").await;
        assert_ok(&switch_resp, "switch_session");
        assert_eq!(switch_resp["data"]["cancelled"], false);

        let events_json = manager
            .execute_command("get-events", "", 5000)
            .await
            .expect("get recorded events");
        let events: Vec<Value> = serde_json::from_str(events_json.as_str().expect("events string"))
            .expect("parse events");

        assert_eq!(
            events.len(),
            2,
            "expected new + resume events, got {events:?}"
        );
        assert_eq!(events[0]["reason"], "new");
        assert_eq!(events[0]["previousSessionFile"], current_path);
        assert!(
            events[0]["sessionId"]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "expected new-session event to include sessionId: {:?}",
            events[0]
        );

        assert_eq!(events[1]["reason"], "resume");
        assert_eq!(events[1]["previousSessionFile"], Value::Null);
        assert_eq!(events[1]["targetSessionFile"], target_path);
        assert!(
            events[1]["sessionId"]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "expected resume event to include sessionId: {:?}",
            events[1]
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI roundtrip — confirm
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_confirm_roundtrip() {
    let _harness = TestHarness::new("rpc_extension_ui_confirm_roundtrip");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_confirm.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Give the server a moment to set up the UI channel.
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        // Spawn a request_ui call from the extension side.
        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-confirm-1".to_string(),
                method: "confirm".to_string(),
                payload: json!({
                    "title": "Delete file?",
                    "message": "This cannot be undone.",
                    "timeout": 5000
                }),
                timeout_ms: Some(5000),
                extension_id: Some("test-ext".to_string()),
            };
            mgr.request_ui(request).await
        });

        // Capture the emitted extension_ui_request event.
        let ui_event = recv_ui_request(&out_rx, "confirm").await;
        assert_eq!(ui_event["type"], "extension_ui_request");
        assert_eq!(ui_event["id"], "req-confirm-1");
        assert_eq!(ui_event["method"], "confirm");
        assert_eq!(ui_event["title"], "Delete file?");

        // Respond with confirmed = true.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-1","type":"extension_ui_response","requestId":"req-confirm-1","confirmed":true}"#,
            "confirm_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");
        assert_eq!(resp["data"]["resolved"], true);

        // Verify the request_ui future resolved with the correct value.
        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        let response = response.expect("should have a response");
        assert!(!response.cancelled);
        assert_eq!(response.value, Some(json!(true)));

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_extension_ui_confirm_denied() {
    let _harness = TestHarness::new("rpc_extension_ui_confirm_denied");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_deny.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-deny-1".to_string(),
                method: "confirm".to_string(),
                payload: json!({ "title": "Do risky thing?", "timeout": 5000 }),
                timeout_ms: Some(5000),
                extension_id: None,
            };
            mgr.request_ui(request).await
        });

        let ui_event = recv_ui_request(&out_rx, "confirm_denied").await;
        assert_eq!(ui_event["id"], "req-deny-1");

        // Respond with confirmed = false.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-2","type":"extension_ui_response","requestId":"req-deny-1","value":false}"#,
            "confirm_denied_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        let response = response.expect("should have a response");
        assert!(!response.cancelled);
        assert_eq!(response.value, Some(json!(false)));

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI roundtrip — select
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_select_roundtrip() {
    let _harness = TestHarness::new("rpc_extension_ui_select_roundtrip");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_select.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-select-1".to_string(),
                method: "select".to_string(),
                payload: json!({
                    "title": "Pick a model",
                    "options": ["claude-sonnet", "gpt-4o", "gemini-pro"],
                    "timeout": 5000
                }),
                timeout_ms: Some(5000),
                extension_id: Some("model-picker-ext".to_string()),
            };
            mgr.request_ui(request).await
        });

        let ui_event = recv_ui_request(&out_rx, "select").await;
        assert_eq!(ui_event["id"], "req-select-1");
        assert_eq!(ui_event["method"], "select");
        assert_eq!(ui_event["title"], "Pick a model");
        assert!(ui_event["options"].is_array());

        // Select the second option.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-3","type":"extension_ui_response","requestId":"req-select-1","value":"gpt-4o"}"#,
            "select_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        let response = response.expect("should have a response");
        assert!(!response.cancelled);
        assert_eq!(response.value, Some(json!("gpt-4o")));

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI roundtrip — input
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_input_roundtrip() {
    let _harness = TestHarness::new("rpc_extension_ui_input_roundtrip");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_input.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-input-1".to_string(),
                method: "input".to_string(),
                payload: json!({
                    "title": "Enter API key",
                    "message": "Paste your key below",
                    "timeout": 5000
                }),
                timeout_ms: Some(5000),
                extension_id: Some("api-key-ext".to_string()),
            };
            mgr.request_ui(request).await
        });

        let ui_event = recv_ui_request(&out_rx, "input").await;
        assert_eq!(ui_event["id"], "req-input-1");
        assert_eq!(ui_event["method"], "input");

        // Respond with typed text.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-4","type":"extension_ui_response","requestId":"req-input-1","value":"sk-test-12345"}"#,
            "input_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        let response = response.expect("should have a response");
        assert!(!response.cancelled);
        assert_eq!(response.value, Some(json!("sk-test-12345")));

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI roundtrip — editor
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_editor_roundtrip() {
    let _harness = TestHarness::new("rpc_extension_ui_editor_roundtrip");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_editor.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-editor-1".to_string(),
                method: "editor".to_string(),
                payload: json!({
                    "title": "Edit config",
                    "message": "Modify the YAML below",
                    "timeout": 5000
                }),
                timeout_ms: Some(5000),
                extension_id: None,
            };
            mgr.request_ui(request).await
        });

        let ui_event = recv_ui_request(&out_rx, "editor").await;
        assert_eq!(ui_event["id"], "req-editor-1");
        assert_eq!(ui_event["method"], "editor");

        // Respond with edited text.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-5","type":"extension_ui_response","requestId":"req-editor-1","value":"key: new_value"}"#,
            "editor_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        let response = response.expect("should have a response");
        assert!(!response.cancelled);
        assert_eq!(response.value, Some(json!("key: new_value")));

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — cancellation
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_cancel_response() {
    let _harness = TestHarness::new("rpc_extension_ui_cancel_response");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_cancel.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-cancel-1".to_string(),
                method: "confirm".to_string(),
                payload: json!({ "title": "Proceed?", "timeout": 5000 }),
                timeout_ms: Some(5000),
                extension_id: None,
            };
            mgr.request_ui(request).await
        });

        let ui_event = recv_ui_request(&out_rx, "cancel").await;
        assert_eq!(ui_event["id"], "req-cancel-1");

        // Respond with cancelled: true.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-6","type":"extension_ui_response","requestId":"req-cancel-1","cancelled":true}"#,
            "cancel_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        let response = response.expect("should have a response");
        assert!(response.cancelled);

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — no extensions configured (noop fallback)
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_response_without_extensions() {
    let _harness = TestHarness::new("rpc_extension_ui_response_without_extensions");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        // No extensions — build_agent_session leaves extensions = None.
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_noext.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Sending extension_ui_response when no extensions are configured
        // should return a success noop (no data).
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-7","type":"extension_ui_response","requestId":"req-x","confirmed":true}"#,
            "no_extensions",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");
        assert!(resp.get("data").is_none() || resp["data"].is_null());

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — mismatched request ID
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_mismatched_request_id() {
    let _harness = TestHarness::new("rpc_extension_ui_mismatched_request_id");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_mismatch.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        let mgr = manager.clone();
        let _ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-real-1".to_string(),
                method: "confirm".to_string(),
                payload: json!({ "title": "Do it?", "timeout": 5000 }),
                timeout_ms: Some(5000),
                extension_id: None,
            };
            mgr.request_ui(request).await
        });

        let ui_event = recv_ui_request(&out_rx, "mismatch").await;
        assert_eq!(ui_event["id"], "req-real-1");

        // Send response with WRONG request ID.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-8","type":"extension_ui_response","requestId":"req-WRONG","confirmed":true}"#,
            "wrong_id_response",
        )
        .await;
        assert_err(&resp, "extension_ui_response");
        let error_msg = resp["error"].as_str().unwrap_or("");
        assert!(
            error_msg.contains("Unexpected requestId"),
            "error should mention unexpected requestId: {error_msg}"
        );

        // Now send the correct one to clean up.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-9","type":"extension_ui_response","requestId":"req-real-1","confirmed":true}"#,
            "correct_id_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — missing requestId
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_missing_request_id() {
    let _harness = TestHarness::new("rpc_extension_ui_missing_request_id");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, _manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_noid.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Send extension_ui_response without requestId OR id — the parser
        // falls back to "id" as an alias, so we must omit both to trigger
        // the "Missing requestId" error.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"type":"extension_ui_response","confirmed":true}"#,
            "missing_id",
        )
        .await;
        assert_err(&resp, "extension_ui_response");
        let error_msg = resp["error"].as_str().unwrap_or("");
        assert!(
            error_msg.contains("Missing requestId"),
            "error should mention missing requestId: {error_msg}"
        );

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — legacy id alias accepted
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_id_alias_roundtrip() {
    let _harness = TestHarness::new("rpc_extension_ui_id_alias_roundtrip");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_id_alias.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-legacy-1".to_string(),
                method: "confirm".to_string(),
                payload: json!({ "title": "Legacy id alias?", "timeout": 5000 }),
                timeout_ms: Some(5000),
                extension_id: None,
            };
            mgr.request_ui(request).await
        });

        let ui_event = recv_ui_request(&out_rx, "id_alias").await;
        assert_eq!(ui_event["id"], "req-legacy-1");
        assert_eq!(ui_event["method"], "confirm");

        // Upstream accepts top-level "id" as a requestId alias for
        // extension_ui_response.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"req-legacy-1","type":"extension_ui_response","confirmed":true}"#,
            "id_alias_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");
        assert_eq!(resp["id"], "req-legacy-1");
        assert_eq!(resp["data"]["resolved"], true);

        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        let response = response.expect("should have a response");
        assert_eq!(response.value, Some(json!(true)));
        assert!(!response.cancelled);

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — sequential ordering (one at a time)
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_sequential_ordering() {
    let _harness = TestHarness::new("rpc_extension_ui_sequential_ordering");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_seq.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        // Fire two requests concurrently.
        let mgr1 = manager.clone();
        let ui_task_1 = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-seq-1".to_string(),
                method: "confirm".to_string(),
                payload: json!({ "title": "First?", "timeout": 5000 }),
                timeout_ms: Some(5000),
                extension_id: Some("ext-a".to_string()),
            };
            mgr1.request_ui(request).await
        });

        // Wait for the first to be emitted before sending the second.
        let first_event = recv_ui_request(&out_rx, "seq_first").await;
        assert_eq!(first_event["id"], "req-seq-1");

        let mgr2 = manager.clone();
        let ui_task_2 = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-seq-2".to_string(),
                method: "input".to_string(),
                payload: json!({ "title": "Second?", "timeout": 5000 }),
                timeout_ms: Some(5000),
                extension_id: Some("ext-b".to_string()),
            };
            mgr2.request_ui(request).await
        });

        // Give the second request time to enter the queue.
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(100)).await;

        // Respond to the first — this should dequeue the second.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-11","type":"extension_ui_response","requestId":"req-seq-1","confirmed":true}"#,
            "seq_first_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        let r1 = ui_task_1
            .await
            .expect("first request_ui")
            .expect("has response");
        assert_eq!(r1.value, Some(json!(true)));

        // Now the second request should be emitted.
        let second_event = recv_ui_request(&out_rx, "seq_second").await;
        assert_eq!(second_event["id"], "req-seq-2");
        assert_eq!(second_event["method"], "input");

        // Respond to the second.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-12","type":"extension_ui_response","requestId":"req-seq-2","value":"hello"}"#,
            "seq_second_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        let r2 = ui_task_2
            .await
            .expect("second request_ui")
            .expect("has response");
        assert_eq!(r2.value, Some(json!("hello")));

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — no active request error
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_no_active_request() {
    let _harness = TestHarness::new("rpc_extension_ui_no_active_request");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, _manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_noactive.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Send a response when no UI request is active.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-13","type":"extension_ui_response","requestId":"req-ghost","confirmed":true}"#,
            "no_active",
        )
        .await;
        assert_err(&resp, "extension_ui_response");
        let error_msg = resp["error"].as_str().unwrap_or("");
        assert!(
            error_msg.contains("No active extension UI request"),
            "error should mention no active request: {error_msg}"
        );

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — fire-and-forget (notify) emitted but not queued
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_notify_fire_and_forget() {
    let _harness = TestHarness::new("rpc_extension_ui_notify_fire_and_forget");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_notify.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        // Send a "notify" request — fire-and-forget, no response expected.
        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-notify-1".to_string(),
                method: "notify".to_string(),
                payload: json!({
                    "title": "Heads up!",
                    "message": "Something happened"
                }),
                timeout_ms: None,
                extension_id: Some("notifier-ext".to_string()),
            };
            mgr.request_ui(request).await
        });

        // The event should still be emitted to the RPC output.
        let ui_event = recv_ui_request(&out_rx, "notify").await;
        assert_eq!(ui_event["type"], "extension_ui_request");
        assert_eq!(ui_event["id"], "req-notify-1");
        assert_eq!(ui_event["method"], "notify");

        // request_ui should return Ok(None) for fire-and-forget.
        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        assert!(response.is_none(), "notify should not return a response");

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Checkpoint / rewind / fresh / retry RPC commands (bd-cv653.3.7)
// ---------------------------------------------------------------------------

#[test]
fn rpc_checkpoint_rewind_fresh_retry_cycle() {
    let harness = TestHarness::new("rpc_checkpoint_rewind_fresh_retry_cycle");
    let runtime = asupersync::runtime::RuntimeBuilder::multi_thread()
        .blocking_threads(1, 8)
        .enable_parking(false)
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let project_dir = harness.temp_path("project");
        let sessions_root = harness.temp_path("sessions");
        let auth_path = harness.temp_path("auth.json");
        std::fs::create_dir_all(&project_dir).expect("create project dir");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");

        let mut session = Session::create_with_dir(Some(sessions_root));
        session.header.cwd = project_dir.display().to_string();
        session.header.provider = Some("keyless-replay".to_string());
        session.header.model_id = Some("keyless-rpc-replay".to_string());
        session.header.thinking_level = Some("off".to_string());
        let agent_session = build_persistent_keyless_agent_session(session, &project_dir);
        let options = build_options(&handle, auth_path, vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = rpc_output_channel();
        let out_rx = Arc::new(Mutex::new(out_rx));
        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // A turn so there is something to checkpoint around.
        let prompt = json!({
            "id": "cp-prompt",
            "type": "prompt",
            "message": "first turn content",
        })
        .to_string();
        let prompt_resp = send_recv(&in_tx, &out_rx, &prompt, "prompt(turn)").await;
        assert_ok(&prompt_resp, "prompt");
        let _idle = wait_for_non_streaming_state(&in_tx, &out_rx, "cp-idle-1", "idle after turn").await;

        // Checkpoint at the current leaf.
        let checkpoint = json!({
            "id": "cp-mark",
            "type": "checkpoint",
            "name": "alpha",
            "note": "before more work",
        })
        .to_string();
        let checkpoint_resp = send_recv(&in_tx, &out_rx, &checkpoint, "checkpoint(mark)").await;
        assert_ok(&checkpoint_resp, "checkpoint");
        assert_eq!(
            data_field(&checkpoint_resp, "name").and_then(Value::as_str),
            Some("alpha")
        );
        let marked_count =
            require_response_field_u64(&checkpoint_resp, "messageCount", "checkpoint mark");

        // Another turn past the checkpoint.
        let prompt2 = json!({
            "id": "cp-prompt-2",
            "type": "prompt",
            "message": "second turn content after the checkpoint",
        })
        .to_string();
        let prompt2_resp = send_recv(&in_tx, &out_rx, &prompt2, "prompt(turn 2)").await;
        assert_ok(&prompt2_resp, "prompt");
        let _idle2 = wait_for_non_streaming_state(&in_tx, &out_rx, "cp-idle-2", "idle after turn 2").await;

        // Rewind to the checkpoint: the second turn collapses into a report.
        let rewind = json!({
            "id": "cp-rewind",
            "type": "rewind",
            "name": "alpha",
        })
        .to_string();
        let rewind_resp = send_recv(&in_tx, &out_rx, &rewind, "rewind(to alpha)").await;
        assert_ok(&rewind_resp, "rewind");
        let collapsed =
            require_response_field_u64(&rewind_resp, "collapsedMessages", "rewind");
        assert!(
            collapsed > 0,
            "rewind must collapse the post-checkpoint span (marked at {marked_count}): {rewind_resp}"
        );
        assert_eq!(
            data_field(&rewind_resp, "treePreserved").and_then(Value::as_bool),
            Some(true)
        );

        // Fresh: provider stream state rotates; transcript untouched.
        let fresh = json!({ "id": "cp-fresh", "type": "fresh" }).to_string();
        let fresh_resp = send_recv(&in_tx, &out_rx, &fresh, "fresh").await;
        assert_ok(&fresh_resp, "fresh");
        let new_id = require_response_field_str(&fresh_resp, "newSessionId", "fresh");
        assert!(new_id.starts_with("fresh-"), "{new_id}");

        // Retry: re-EXECUTES the last user turn immediately (a queued-only
        // retry silently waited for, then polluted, the next prompt).
        let retry = json!({ "id": "cp-retry", "type": "retry" }).to_string();
        let retry_resp = send_recv(&in_tx, &out_rx, &retry, "retry").await;
        assert_ok(&retry_resp, "retry");
        assert_eq!(
            data_field(&retry_resp, "rerunning").and_then(Value::as_bool),
            Some(true)
        );
        // The retried turn is a real run: wait for it to finish so shutdown
        // doesn't race an in-flight stream.
        let _idle3 =
            wait_for_non_streaming_state(&in_tx, &out_rx, "cp-idle-3", "idle after retry").await;

        drop(in_tx);
        let _ = server.await;
    });
}
