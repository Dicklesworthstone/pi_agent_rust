//! RPC mode: headless JSON protocol over stdin/stdout.
//!
//! This implements a compatibility subset of pi-mono's RPC protocol
//! (see legacy `docs/rpc.md` in `legacy_pi_mono_code`).

#![allow(clippy::significant_drop_tightening)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::ignored_unit_patterns)]
#![allow(clippy::needless_pass_by_value)]

use crate::agent::{AbortHandle, AgentEvent, AgentSession, InputSource, QueueMode};
use crate::agent_cx::AgentCx;
use crate::auth::AuthStorage;
use crate::compaction::{
    ResolvedCompactionSettings, compact, compaction_details_to_value, prepare_compaction,
};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::error_hints;
use crate::extensions::{
    EXTENSION_EVENT_TIMEOUT_MS, ExtensionEventName, ExtensionManager, ExtensionUiRequest,
    ExtensionUiResponse,
};
use crate::model::{
    ContentBlock, ImageContent, Message, StopReason, TextContent, UserContent, UserMessage,
};
use crate::models::{ModelEntry, model_requires_configured_credential, normalize_api_key_opt};
use crate::provider_metadata::provider_ids_match;
use crate::providers;
use crate::resources::ResourceLoader;
use crate::session::SessionMessage;
use crate::tools::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};
use asupersync::channel::{mpsc, oneshot};
use asupersync::runtime::RuntimeHandle;
use asupersync::sync::{Mutex, OwnedMutexGuard};
use asupersync::time::{sleep, wall_now};
use memchr::memchr_iter;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::future::Future;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Clone)]
pub struct RpcOptions {
    pub config: Config,
    pub resources: ResourceLoader,
    pub available_models: Vec<ModelEntry>,
    pub scoped_models: Vec<RpcScopedModel>,
    pub cli_api_key: Option<String>,
    pub auth: AuthStorage,
    pub runtime_handle: RuntimeHandle,
}

#[derive(Debug, Clone)]
pub struct RpcScopedModel {
    pub model: ModelEntry,
    pub thinking_level: Option<crate::model::ThinkingLevel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamingBehavior {
    Steer,
    FollowUp,
}

#[derive(Debug, Clone)]
struct RpcStateSnapshot {
    steering_count: usize,
    follow_up_count: usize,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    auto_compaction_enabled: bool,
    auto_retry_enabled: bool,
}

impl From<&RpcSharedState> for RpcStateSnapshot {
    fn from(state: &RpcSharedState) -> Self {
        Self {
            steering_count: state.steering.len(),
            follow_up_count: state.follow_up.len(),
            steering_mode: state.steering_mode,
            follow_up_mode: state.follow_up_mode,
            auto_compaction_enabled: state.auto_compaction_enabled,
            auto_retry_enabled: state.auto_retry_enabled,
        }
    }
}

impl RpcStateSnapshot {
    const fn pending_count(&self) -> usize {
        self.steering_count + self.follow_up_count
    }
}

use crate::config::parse_queue_mode;

fn streaming_behavior_value(parsed: &Value) -> Option<&Value> {
    parsed
        .get("streamingBehavior")
        .or_else(|| parsed.get("streaming_behavior"))
}

fn parse_streaming_behavior(value: Option<&Value>) -> Result<Option<StreamingBehavior>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(s) = value.as_str() else {
        return Err(Error::validation("streamingBehavior must be a string"));
    };
    match s {
        "steer" => Ok(Some(StreamingBehavior::Steer)),
        "follow-up" | "followUp" | "follow_up" => Ok(Some(StreamingBehavior::FollowUp)),
        _ => Err(Error::validation(format!("Invalid streamingBehavior: {s}"))),
    }
}

fn parse_optional_u32_field(parsed: &Value, field: &str) -> Result<Option<u32>> {
    let Some(value) = parsed.get(field) else {
        return Ok(None);
    };
    let number = value
        .as_u64()
        .ok_or_else(|| Error::Validation(format!("{field} must be a non-negative integer")))?;
    u32::try_from(number)
        .map(Some)
        .map_err(|_| Error::Validation(format!("{field} exceeds the maximum supported value")))
}

fn future_with_current_cx<F>(
    current_cx: asupersync::Cx,
    future: F,
) -> impl Future<Output = F::Output> + Send + 'static
where
    F: Future + Send + 'static,
{
    let mut future = Box::pin(future);
    std::future::poll_fn(move |poll_cx| {
        let _guard = asupersync::Cx::set_current(Some(current_cx.clone()));
        future.as_mut().poll(poll_cx)
    })
}

fn normalize_command_type(command_type: &str) -> &str {
    match command_type {
        "follow-up" | "followUp" | "queue-follow-up" | "queueFollowUp" => "follow_up",
        "get-state" | "getState" => "get_state",
        "set-model" | "setModel" => "set_model",
        "set-steering-mode" | "setSteeringMode" => "set_steering_mode",
        "set-follow-up-mode" | "setFollowUpMode" => "set_follow_up_mode",
        "set-auto-compaction" | "setAutoCompaction" => "set_auto_compaction",
        "set-auto-retry" | "setAutoRetry" => "set_auto_retry",
        _ => command_type,
    }
}

fn build_user_message(text: &str, images: &[ImageContent]) -> Message {
    let timestamp = chrono::Utc::now().timestamp_millis();
    if images.is_empty() {
        return Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp,
        });
    }
    let blocks = build_prompt_content_blocks(text, images);
    Message::User(UserMessage {
        content: UserContent::Blocks(blocks),
        timestamp,
    })
}

fn build_prompt_content_blocks(text: &str, images: &[ImageContent]) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    if !text.trim().is_empty() {
        blocks.push(ContentBlock::Text(TextContent::new(text.to_string())));
    }
    for image in images {
        blocks.push(ContentBlock::Image(image.clone()));
    }
    blocks
}

fn parse_extension_command_line(message: &str) -> Option<(String, String)> {
    let trimmed = message.trim_start();
    let stripped = trimmed.strip_prefix('/')?;
    let (command, args) = stripped
        .split_once(char::is_whitespace)
        .unwrap_or((stripped, ""));
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    Some((command.to_string(), args.trim_start().to_string()))
}

fn resolve_extension_command(
    message: &str,
    manager: Option<&ExtensionManager>,
) -> Option<(String, String)> {
    if !message.trim_start().starts_with('/') {
        return None;
    }

    let manager = manager?;
    let (command_name, args) = parse_extension_command_line(message)?;
    manager
        .has_command(&command_name)
        .then_some((command_name, args))
}

fn rpc_agent_event_handler(
    out_tx: std::sync::mpsc::SyncSender<String>,
    runtime_handle: RuntimeHandle,
    extensions: Option<ExtensionManager>,
) -> impl Fn(AgentEvent) + Send + Sync + 'static {
    let coalescer = extensions.map(crate::extensions::EventCoalescer::new);
    let output_pressure = Arc::new(std::sync::Mutex::new(RpcOutputPressureState::default()));

    move |event: AgentEvent| {
        let serialized = if let AgentEvent::AgentEnd {
            messages, error, ..
        } = &event
        {
            json!({
                "type": "agent_end",
                "messages": messages,
                "error": error,
            })
            .to_string()
        } else {
            serde_json::to_string(&event).unwrap_or_else(|err| {
                json!({
                    "type": "event_serialize_error",
                    "error": err.to_string(),
                })
                .to_string()
            })
        };
        output_pressure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send_agent_event(&out_tx, &event, serialized);
        if let Some(coalescer) = &coalescer {
            coalescer.dispatch_agent_event_lazy(&event, &runtime_handle);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcOutputPressureClass {
    Semantic,
    MessageDelta,
    ToolUpdate,
}

#[derive(Debug)]
struct PendingRpcPressureEvent {
    class: RpcOutputPressureClass,
    serialized: String,
}

#[derive(Debug, Default)]
struct RpcOutputPressureState {
    pending: Vec<PendingRpcPressureEvent>,
    coalesced_message_delta_count: u64,
    coalesced_tool_update_count: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RpcOutputPressureSnapshot {
    pending: usize,
    message_deltas_coalesced: u64,
    tool_updates_coalesced: u64,
}

impl RpcOutputPressureState {
    fn send_agent_event(
        &mut self,
        tx: &std::sync::mpsc::SyncSender<String>,
        event: &AgentEvent,
        serialized: String,
    ) {
        match rpc_output_pressure_class(event) {
            RpcOutputPressureClass::Semantic => {
                self.flush_pending(tx);
                let _ = tx.send(serialized);
            }
            class => self.try_send_or_coalesce(tx, class, serialized),
        }
    }

    fn try_send_or_coalesce(
        &mut self,
        tx: &std::sync::mpsc::SyncSender<String>,
        class: RpcOutputPressureClass,
        serialized: String,
    ) {
        match tx.try_send(serialized) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(serialized)) => {
                self.coalesce(class, serialized);
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.pending.clear();
            }
        }
    }

    fn coalesce(&mut self, class: RpcOutputPressureClass, serialized: String) {
        match class {
            RpcOutputPressureClass::MessageDelta => {
                self.coalesced_message_delta_count =
                    self.coalesced_message_delta_count.saturating_add(1);
            }
            RpcOutputPressureClass::ToolUpdate => {
                self.coalesced_tool_update_count =
                    self.coalesced_tool_update_count.saturating_add(1);
            }
            RpcOutputPressureClass::Semantic => {}
        }

        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| pending.class == class)
        {
            pending.serialized = serialized;
        } else {
            self.pending
                .push(PendingRpcPressureEvent { class, serialized });
        }
    }

    fn flush_pending(&mut self, tx: &std::sync::mpsc::SyncSender<String>) {
        let pending = std::mem::take(&mut self.pending);
        for event in pending {
            if tx.send(event.serialized).is_err() {
                break;
            }
        }
    }

    #[cfg(test)]
    fn snapshot(&self) -> RpcOutputPressureSnapshot {
        RpcOutputPressureSnapshot {
            pending: self.pending.len(),
            message_deltas_coalesced: self.coalesced_message_delta_count,
            tool_updates_coalesced: self.coalesced_tool_update_count,
        }
    }
}

const fn rpc_output_pressure_class(event: &AgentEvent) -> RpcOutputPressureClass {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event:
                crate::model::AssistantMessageEvent::TextDelta { .. }
                | crate::model::AssistantMessageEvent::ThinkingDelta { .. }
                | crate::model::AssistantMessageEvent::ToolCallDelta { .. },
            ..
        } => RpcOutputPressureClass::MessageDelta,
        AgentEvent::ToolExecutionUpdate { .. } => RpcOutputPressureClass::ToolUpdate,
        _ => RpcOutputPressureClass::Semantic,
    }
}

async fn rpc_dispatch_session_before_switch(
    manager: Option<ExtensionManager>,
    reason: &str,
    target_session_file: Option<&str>,
) -> bool {
    let Some(manager) = manager else {
        return false;
    };

    let payload = target_session_file.map_or_else(
        || json!({ "reason": reason }),
        |target_session_file| json!({ "reason": reason, "targetSessionFile": target_session_file }),
    );

    manager
        .dispatch_cancellable_event(
            ExtensionEventName::SessionBeforeSwitch,
            Some(payload),
            EXTENSION_EVENT_TIMEOUT_MS,
        )
        .await
        .unwrap_or(false)
}

async fn rpc_dispatch_session_switch_event(manager: Option<ExtensionManager>, payload: Value) {
    let Some(manager) = manager else {
        return;
    };

    let _ = manager
        .dispatch_event(ExtensionEventName::SessionSwitch, Some(payload))
        .await;
}

fn try_send_line_with_backpressure(tx: &mpsc::Sender<String>, mut line: String) -> bool {
    loop {
        match tx.try_send(line) {
            Ok(()) => return true,
            Err(mpsc::SendError::Full(unsent)) => {
                line = unsent;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(mpsc::SendError::Disconnected(_) | mpsc::SendError::Cancelled(_)) => {
                return false;
            }
        }
    }
}

#[derive(Debug)]
struct RpcSharedState {
    steering: VecDeque<Message>,
    follow_up: VecDeque<Message>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    auto_compaction_enabled: bool,
    auto_retry_enabled: bool,
}

const MAX_RPC_PENDING_MESSAGES: usize = 128;

/// In-band shutdown marker for the stdout writer thread. Never a valid JSON
/// event line (leading NUL), so it cannot collide with real output. Sent by
/// `run_stdio` after `run` returns so the writer exits deterministically even
/// if a background task still holds an `out_tx` clone (gh #137).
const RPC_WRITER_SHUTDOWN_SENTINEL: &str = "\u{0}__pi_rpc_writer_shutdown__";

/// Clears an activity flag when dropped. Held by the turn/command/compaction
/// tasks so a panic (isolated by the runtime) or task cancellation cannot
/// leak `is_streaming`/`is_compacting` as stuck-true and pin the stdin-EOF
/// drain loop forever (gh #137). Normal completion paths still clear the
/// flags explicitly at their intended points; this is a safety net.
struct ClearFlagOnDrop(Arc<AtomicBool>);

impl Drop for ClearFlagOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl RpcSharedState {
    fn new(config: &Config) -> Self {
        Self {
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            steering_mode: config.steering_queue_mode(),
            follow_up_mode: config.follow_up_queue_mode(),
            auto_compaction_enabled: config.compaction_enabled(),
            auto_retry_enabled: config.retry_enabled(),
        }
    }

    fn pending_count(&self) -> usize {
        self.steering.len() + self.follow_up.len()
    }

    fn push_steering(&mut self, message: Message) -> Result<()> {
        if self.pending_count() >= MAX_RPC_PENDING_MESSAGES {
            return Err(Error::session(
                "Steering queue is full (Do you have too many pending commands?)",
            ));
        }
        self.steering.push_back(message);
        Ok(())
    }

    fn push_follow_up(&mut self, message: Message) -> Result<()> {
        if self.pending_count() >= MAX_RPC_PENDING_MESSAGES {
            return Err(Error::session("Follow-up queue is full"));
        }
        self.follow_up.push_back(message);
        Ok(())
    }

    fn pop_steering(&mut self) -> Vec<Message> {
        match self.steering_mode {
            QueueMode::All => self.steering.drain(..).collect(),
            QueueMode::OneAtATime => self.steering.pop_front().into_iter().collect(),
        }
    }

    fn pop_follow_up(&mut self) -> Vec<Message> {
        match self.follow_up_mode {
            QueueMode::All => self.follow_up.drain(..).collect(),
            QueueMode::OneAtATime => self.follow_up.pop_front().into_iter().collect(),
        }
    }
}

/// Tracks a running bash command so it can be aborted.
struct RunningBash {
    id: String,
    abort_tx: oneshot::Sender<()>,
}

#[derive(Debug, Default)]
struct RpcUiBridgeState {
    active: Option<ExtensionUiRequest>,
    queue: VecDeque<ExtensionUiRequest>,
}

pub async fn run_stdio(mut session: AgentSession, options: RpcOptions) -> Result<()> {
    session.set_input_source(InputSource::Rpc);
    let (in_tx, in_rx) = mpsc::channel::<String>(1024);
    let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);

    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut reader = io::BufReader::new(stdin.lock());
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line_to_send = line.clone();
                    // Retry loop to handle backpressure (channel full) without dropping input.
                    // Stop when the receiver side has closed so this thread does not spin forever.
                    if !try_send_line_with_backpressure(&in_tx, line_to_send) {
                        break;
                    }
                }
            }
        }
    });

    let writer_handle = std::thread::spawn(move || {
        let stdout = io::stdout();
        let mut writer = io::BufWriter::new(stdout.lock());
        for line in out_rx {
            if line == RPC_WRITER_SHUTDOWN_SENTINEL {
                break;
            }
            if writer.write_all(line.as_bytes()).is_err() {
                break;
            }
            if writer.write_all(b"\n").is_err() {
                break;
            }
            if writer.flush().is_err() {
                break;
            }
        }
    });

    let out_tx_shutdown = out_tx.clone();
    let result = run(session, options, in_rx, out_tx).await;

    // `run` has drained any in-flight turn, so everything queued ahead of the
    // sentinel is the complete event stream. The sentinel (rather than a bare
    // join) makes writer shutdown deterministic even if a background task
    // still holds an `out_tx` clone (gh #137). try_send, not send: if the
    // channel is full the client has stopped reading stdout, and a blocking
    // send here would stall the runtime thread (blocking even Ctrl+C
    // handling); in that case skip the join and let process exit tear the
    // writer down.
    if out_tx_shutdown
        .try_send(RPC_WRITER_SHUTDOWN_SENTINEL.to_string())
        .is_ok()
    {
        drop(out_tx_shutdown);
        let _ = writer_handle.join();
    }

    result
}

#[allow(clippy::too_many_lines)]
#[allow(
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee
)]
pub async fn run(
    session: AgentSession,
    options: RpcOptions,
    mut in_rx: mpsc::Receiver<String>,
    out_tx: std::sync::mpsc::SyncSender<String>,
) -> Result<()> {
    let cx = AgentCx::for_current_or_request();
    let session_handle = Arc::clone(&session.session);
    let session = Arc::new(Mutex::new(session));
    let shared_state = Arc::new(Mutex::new(RpcSharedState::new(&options.config)));
    let is_streaming = Arc::new(AtomicBool::new(false));
    let is_compacting = Arc::new(AtomicBool::new(false));
    let abort_handle: Arc<Mutex<Option<AbortHandle>>> = Arc::new(Mutex::new(None));
    let bash_state: Arc<Mutex<Option<RunningBash>>> = Arc::new(Mutex::new(None));
    let retry_abort = Arc::new(AtomicBool::new(false));

    {
        use futures::future::BoxFuture;
        let steering_state = Arc::clone(&shared_state);
        let follow_state = Arc::clone(&shared_state);
        let steering_cx = cx.clone();
        let follow_cx = cx.clone();
        let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
            .await
            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
        guard.set_queue_modes(
            options.config.steering_queue_mode(),
            options.config.follow_up_queue_mode(),
        );
        let steering_fetcher = move || -> BoxFuture<'static, Vec<Message>> {
            let steering_state = Arc::clone(&steering_state);
            let steering_cx = steering_cx.clone();
            Box::pin(async move {
                steering_state
                    .lock(&steering_cx)
                    .await
                    .map_or_else(|_| Vec::new(), |mut state| state.pop_steering())
            })
        };
        let follow_fetcher = move || -> BoxFuture<'static, Vec<Message>> {
            let follow_state = Arc::clone(&follow_state);
            let follow_cx = follow_cx.clone();
            Box::pin(async move {
                follow_state
                    .lock(&follow_cx)
                    .await
                    .map_or_else(|_| Vec::new(), |mut state| state.pop_follow_up())
            })
        };
        guard.agent.register_message_fetchers(
            Some(Arc::new(steering_fetcher)),
            Some(Arc::new(follow_fetcher)),
        );
    }

    // Set up extension UI channel for RPC mode.
    // When extensions request UI (capability prompts, etc.), we emit them as
    // JSON notifications so the RPC client can respond programmatically.
    let rpc_extension_manager = {
        let cx_ui = cx.clone();
        let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx_ui)
            .await
            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
        guard
            .extensions
            .as_ref()
            .map(crate::extensions::ExtensionRegion::manager)
            .cloned()
    };

    let rpc_ui_state: Option<Arc<Mutex<RpcUiBridgeState>>> = rpc_extension_manager
        .as_ref()
        .map(|_| Arc::new(Mutex::new(RpcUiBridgeState::default())));

    if let Some(ref manager) = rpc_extension_manager {
        let (extension_ui_tx, mut extension_ui_rx) =
            asupersync::channel::mpsc::channel::<ExtensionUiRequest>(64);
        manager.set_ui_sender(extension_ui_tx);

        let out_tx_ui = out_tx.clone();
        let ui_state = rpc_ui_state
            .as_ref()
            .map(Arc::clone)
            .expect("rpc ui state should exist when extension manager exists");
        let manager_ui = (*manager).clone();
        let runtime_handle_ui = options.runtime_handle.clone();
        options.runtime_handle.spawn(async move {
            const MAX_UI_PENDING_REQUESTS: usize = 64;
            let cx = AgentCx::for_request();
            while let Ok(request) = extension_ui_rx.recv(&cx).await {
                if request.expects_response() {
                    let emit_now = {
                        let Ok(mut guard) = ui_state.lock(&cx).await else {
                            return;
                        };
                        if guard.active.is_none() {
                            guard.active = Some(request.clone());
                            true
                        } else if guard.queue.len() < MAX_UI_PENDING_REQUESTS {
                            guard.queue.push_back(request.clone());
                            false
                        } else {
                            drop(guard);
                            let _ = manager_ui.respond_ui(ExtensionUiResponse {
                                id: request.id.clone(),
                                value: None,
                                cancelled: true,
                            });
                            false
                        }
                    };

                    if emit_now {
                        rpc_emit_extension_ui_request(
                            &runtime_handle_ui,
                            Arc::clone(&ui_state),
                            manager_ui.clone(),
                            out_tx_ui.clone(),
                            request,
                        );
                    }
                } else {
                    // Fire-and-forget UI updates should not be queued.
                    let rpc_event = request.to_rpc_event();
                    let _ = out_tx_ui.send(event(&rpc_event));
                }
            }
        });
    }

    while let Ok(line) = in_rx.recv(&cx).await {
        if line.trim().is_empty() {
            continue;
        }

        let parsed: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(err) => {
                let resp = response_error(None, "parse", format!("Failed to parse command: {err}"));
                let _ = out_tx.send(resp);
                continue;
            }
        };

        let Some(command_type_raw) = parsed.get("type").and_then(Value::as_str) else {
            let resp = response_error(None, "parse", "Missing command type".to_string());
            let _ = out_tx.send(resp);
            continue;
        };
        let command_type = normalize_command_type(command_type_raw);

        let id = parsed.get("id").and_then(Value::as_str).map(str::to_string);

        match command_type {
            "prompt" => {
                let Some(message) = parsed
                    .get("message")
                    .and_then(Value::as_str)
                    .map(String::from)
                else {
                    let resp = response_error(id, "prompt", "Missing message".to_string());
                    let _ = out_tx.send(resp);
                    continue;
                };

                let images = match parse_prompt_images(parsed.get("images")) {
                    Ok(images) => images,
                    Err(err) => {
                        let resp = response_error_with_hints(id, "prompt", &err);
                        let _ = out_tx.send(resp);
                        continue;
                    }
                };

                let streaming_behavior =
                    match parse_streaming_behavior(streaming_behavior_value(&parsed)) {
                        Ok(value) => value,
                        Err(err) => {
                            let resp = response_error_with_hints(id, "prompt", &err);
                            let _ = out_tx.send(resp);
                            continue;
                        }
                    };

                let extension_command =
                    resolve_extension_command(&message, rpc_extension_manager.as_ref());

                if is_streaming.load(Ordering::SeqCst) {
                    if extension_command.is_some() {
                        let resp = response_error(
                            id,
                            "prompt",
                            "Extension commands are not allowed while agent is streaming"
                                .to_string(),
                        );
                        let _ = out_tx.send(resp);
                        continue;
                    }

                    if streaming_behavior.is_none() {
                        let resp = response_error(
                            id,
                            "prompt",
                            "Agent is currently streaming; specify streamingBehavior".to_string(),
                        );
                        let _ = out_tx.send(resp);
                        continue;
                    }

                    let expanded = options.resources.expand_input(&message);
                    let queued_result = {
                        let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                            .await
                            .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                        match streaming_behavior {
                            Some(StreamingBehavior::Steer) => {
                                state.push_steering(build_user_message(&expanded, &images))
                            }
                            Some(StreamingBehavior::FollowUp) => {
                                state.push_follow_up(build_user_message(&expanded, &images))
                            }
                            None => Ok(()), // Unreachable due to check above
                        }
                    };

                    match queued_result {
                        Ok(()) => {
                            let _ = out_tx.send(response_ok(id, "prompt", None));
                        }
                        Err(err) => {
                            let resp = response_error_with_hints(id, "prompt", &err);
                            let _ = out_tx.send(resp);
                        }
                    }
                    continue;
                }

                // Ack immediately.
                let _ = out_tx.send(response_ok(id, "prompt", None));

                is_streaming.store(true, Ordering::SeqCst);

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let shared_state = Arc::clone(&shared_state);
                let is_streaming = Arc::clone(&is_streaming);
                let is_compacting = Arc::clone(&is_compacting);
                let abort_handle_slot = Arc::clone(&abort_handle);
                let runtime_handle = options.runtime_handle.clone();
                if let Some((command_name, args)) = extension_command {
                    let command_runtime = runtime_handle.clone();
                    let command_cx = cx.clone();
                    runtime_handle.spawn(future_with_current_cx(
                        command_cx.cx().clone(),
                        async move {
                            run_extension_command(
                                session,
                                is_streaming,
                                abort_handle_slot,
                                out_tx,
                                command_runtime,
                                command_name,
                                args,
                                command_cx,
                            )
                            .await;
                        },
                    ));
                } else {
                    let retry_abort = retry_abort.clone();
                    let options = options.clone();
                    let expanded = options.resources.expand_input(&message);
                    let prompt_cx = cx.clone();
                    runtime_handle.spawn(future_with_current_cx(
                        prompt_cx.cx().clone(),
                        async move {
                            run_prompt_with_retry(
                                session,
                                shared_state,
                                is_streaming,
                                is_compacting,
                                abort_handle_slot,
                                out_tx,
                                retry_abort,
                                options,
                                expanded,
                                images,
                                prompt_cx,
                            )
                            .await;
                        },
                    ));
                }
            }

            "steer" => {
                let Some(message) = parsed
                    .get("message")
                    .and_then(Value::as_str)
                    .map(String::from)
                else {
                    let resp = response_error(id, "steer", "Missing message".to_string());
                    let _ = out_tx.send(resp);
                    continue;
                };

                if resolve_extension_command(&message, rpc_extension_manager.as_ref()).is_some() {
                    let resp = response_error(
                        id,
                        "steer",
                        "Extension commands are not allowed with steer".to_string(),
                    );
                    let _ = out_tx.send(resp);
                    continue;
                }

                let expanded = options.resources.expand_input(&message);
                if is_streaming.load(Ordering::SeqCst) {
                    let result = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?
                        .push_steering(build_user_message(&expanded, &[]));

                    match result {
                        Ok(()) => {
                            let _ = out_tx.send(response_ok(id, "steer", None));
                        }
                        Err(err) => {
                            let _ = out_tx.send(response_error_with_hints(id, "steer", &err));
                        }
                    }
                    continue;
                }

                let _ = out_tx.send(response_ok(id, "steer", None));

                is_streaming.store(true, Ordering::SeqCst);

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let shared_state = Arc::clone(&shared_state);
                let is_streaming = Arc::clone(&is_streaming);
                let is_compacting = Arc::clone(&is_compacting);
                let abort_handle_slot = Arc::clone(&abort_handle);
                let retry_abort = retry_abort.clone();
                let options = options.clone();
                let expanded = expanded.clone();
                let runtime_handle = options.runtime_handle.clone();
                let prompt_cx = cx.clone();
                runtime_handle.spawn(future_with_current_cx(prompt_cx.cx().clone(), async move {
                    run_prompt_with_retry(
                        session,
                        shared_state,
                        is_streaming,
                        is_compacting,
                        abort_handle_slot,
                        out_tx,
                        retry_abort,
                        options,
                        expanded,
                        Vec::new(),
                        prompt_cx,
                    )
                    .await;
                }));
            }

            "follow_up" => {
                let Some(message) = parsed
                    .get("message")
                    .and_then(Value::as_str)
                    .map(String::from)
                else {
                    let resp = response_error(id, "follow_up", "Missing message".to_string());
                    let _ = out_tx.send(resp);
                    continue;
                };

                if resolve_extension_command(&message, rpc_extension_manager.as_ref()).is_some() {
                    let resp = response_error(
                        id,
                        "follow_up",
                        "Extension commands are not allowed with follow_up".to_string(),
                    );
                    let _ = out_tx.send(resp);
                    continue;
                }

                let expanded = options.resources.expand_input(&message);
                if is_streaming.load(Ordering::SeqCst) {
                    let result = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?
                        .push_follow_up(build_user_message(&expanded, &[]));

                    match result {
                        Ok(()) => {
                            let _ = out_tx.send(response_ok(id, "follow_up", None));
                        }
                        Err(err) => {
                            let _ = out_tx.send(response_error_with_hints(id, "follow_up", &err));
                        }
                    }
                    continue;
                }

                let _ = out_tx.send(response_ok(id, "follow_up", None));

                is_streaming.store(true, Ordering::SeqCst);

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let shared_state = Arc::clone(&shared_state);
                let is_streaming = Arc::clone(&is_streaming);
                let is_compacting = Arc::clone(&is_compacting);
                let abort_handle_slot = Arc::clone(&abort_handle);
                let retry_abort = retry_abort.clone();
                let options = options.clone();
                let expanded = expanded.clone();
                let runtime_handle = options.runtime_handle.clone();
                let prompt_cx = cx.clone();
                runtime_handle.spawn(future_with_current_cx(prompt_cx.cx().clone(), async move {
                    run_prompt_with_retry(
                        session,
                        shared_state,
                        is_streaming,
                        is_compacting,
                        abort_handle_slot,
                        out_tx,
                        retry_abort,
                        options,
                        expanded,
                        Vec::new(),
                        prompt_cx,
                    )
                    .await;
                }));
            }

            "abort" => {
                let handle = abort_handle
                    .lock(&cx)
                    .await
                    .map_err(|err| Error::session(format!("abort lock failed: {err}")))?
                    .clone();
                if let Some(handle) = handle {
                    handle.abort();
                }
                let _ = out_tx.send(response_ok(id, "abort", None));
            }

            "get_state" => {
                let snapshot = {
                    let state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                    RpcStateSnapshot::from(&*state)
                };
                let data = {
                    let inner_session = OwnedMutexGuard::lock(Arc::clone(&session_handle), &cx)
                        .await
                        .map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                    session_state(
                        &inner_session,
                        &options,
                        &snapshot,
                        is_streaming.load(Ordering::SeqCst),
                        is_compacting.load(Ordering::SeqCst),
                    )
                };
                let _ = out_tx.send(response_ok(id, "get_state", Some(data)));
            }

            "get_session_stats" => {
                let data = {
                    let inner_session = OwnedMutexGuard::lock(Arc::clone(&session_handle), &cx)
                        .await
                        .map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                    session_stats(&inner_session)
                };
                let _ = out_tx.send(response_ok(id, "get_session_stats", Some(data)));
            }

            "get_messages" => {
                let messages = {
                    let inner_session = OwnedMutexGuard::lock(Arc::clone(&session_handle), &cx)
                        .await
                        .map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                    inner_session
                        .entries_for_current_path()
                        .iter()
                        .filter_map(|entry| match entry {
                            crate::session::SessionEntry::Message(msg) => match msg.message {
                                SessionMessage::User { .. }
                                | SessionMessage::Assistant { .. }
                                | SessionMessage::ToolResult { .. }
                                | SessionMessage::BashExecution { .. }
                                | SessionMessage::Custom { .. } => Some(msg.message.clone()),
                                _ => None,
                            },
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                };
                let messages = messages
                    .into_iter()
                    .map(rpc_session_message_value)
                    .collect::<Vec<_>>();
                let _ = out_tx.send(response_ok(
                    id,
                    "get_messages",
                    Some(json!({ "messages": messages })),
                ));
            }

            "get_available_models" => {
                let models = options
                    .available_models
                    .iter()
                    .map(rpc_model_from_entry)
                    .collect::<Vec<_>>();
                let _ = out_tx.send(response_ok(
                    id,
                    "get_available_models",
                    Some(json!({ "models": models })),
                ));
            }

            "set_model" => {
                let Some(provider) = parsed.get("provider").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_model",
                        "Missing provider".to_string(),
                    ));
                    continue;
                };
                let Some(model_id) = parsed.get("modelId").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_model",
                        "Missing modelId".to_string(),
                    ));
                    continue;
                };

                let Some(entry) = options
                    .available_models
                    .iter()
                    .find(|m| {
                        provider_ids_match(&m.model.provider, provider)
                            && m.model.id.eq_ignore_ascii_case(model_id)
                    })
                    .cloned()
                else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_model",
                        format!("Model not found: {provider}/{model_id}"),
                    ));
                    continue;
                };

                let key = resolve_model_key(options.cli_api_key.as_deref(), &options.auth, &entry);
                if model_requires_configured_credential(&entry) && key.is_none() {
                    let err = Error::auth(format!(
                        "Missing credentials for {}/{}",
                        entry.model.provider, entry.model.id
                    ));
                    let _ = out_tx.send(response_error_with_hints(id, "set_model", &err));
                    continue;
                }

                let result: Result<()> = async {
                    let clamped_level = {
                        let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                            .await
                            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                        let provider_impl = providers::create_provider(
                            &entry,
                            guard
                                .extensions
                                .as_ref()
                                .map(crate::extensions::ExtensionRegion::manager),
                        )?;
                        guard.agent.set_provider(provider_impl);
                        guard.agent.stream_options_mut().api_key.clone_from(&key);
                        guard
                            .agent
                            .stream_options_mut()
                            .headers
                            .clone_from(&entry.headers);

                        apply_model_change(&mut guard, &entry).await?;

                        let current_thinking = guard
                            .agent
                            .stream_options()
                            .thinking_level
                            .unwrap_or_default();
                        entry.clamp_thinking_level(current_thinking)
                    }; // Drop guard here

                    // Apply thinking level without holding lock across await
                    apply_thinking_level(Arc::clone(&session), clamped_level).await?;
                    Ok(())
                }
                .await;

                match result {
                    Ok(()) => {
                        let _ = out_tx.send(response_ok(
                            id,
                            "set_model",
                            Some(rpc_model_from_entry(&entry)),
                        ));
                    }
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "set_model", &err));
                    }
                }
            }

            "cycle_model" => {
                let result = async {
                    let cycle_result = {
                        let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                            .await
                            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                        cycle_model_for_rpc(&mut guard, &options).await?
                    };

                    if let Some((entry, thinking_level, is_scoped)) = cycle_result {
                        // Apply thinking level after dropping lock
                        apply_thinking_level_for_session(session.clone(), thinking_level, &cx)
                            .await?;
                        Ok(Some((entry, thinking_level, is_scoped)))
                    } else {
                        Ok(None)
                    }
                }
                .await;

                match result {
                    Ok(Some((entry, thinking_level, is_scoped))) => {
                        let _ = out_tx.send(response_ok(
                            id,
                            "cycle_model",
                            Some(json!({
                                "model": rpc_model_from_entry(&entry),
                                "thinkingLevel": thinking_level.to_string(),
                                "isScoped": is_scoped,
                            })),
                        ));
                    }
                    Ok(None) => {
                        let _ =
                            out_tx.send(response_ok(id.clone(), "cycle_model", Some(Value::Null)));
                    }
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "cycle_model", &err));
                    }
                }
            }

            "set_thinking_level" => {
                let Some(level) = parsed.get("level").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_thinking_level",
                        "Missing level".to_string(),
                    ));
                    continue;
                };
                let level = match parse_thinking_level(level) {
                    Ok(level) => level,
                    Err(err) => {
                        let _ =
                            out_tx.send(response_error_with_hints(id, "set_thinking_level", &err));
                        continue;
                    }
                };

                // Get the properly clamped level first
                let clamped_level = {
                    let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let runtime_provider = guard.agent.provider().name().to_string();
                    let runtime_model_id = guard.agent.provider().model_id().to_string();
                    let inner_session = guard.session.lock(&cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    current_or_runtime_model_entry(
                        &inner_session,
                        &runtime_provider,
                        &runtime_model_id,
                        &options,
                    )
                    .map_or(level, |entry| entry.clamp_thinking_level(level))
                };

                // Apply the thinking level without holding the lock across await
                let result = {
                    let session_clone = Arc::clone(&session);
                    apply_thinking_level_for_session(session_clone, clamped_level, &cx).await
                };

                if let Err(err) = result {
                    let _ = out_tx.send(response_error_with_hints(
                        id.clone(),
                        "set_thinking_level",
                        &err,
                    ));
                    continue;
                }
                let _ = out_tx.send(response_ok(id, "set_thinking_level", None));
            }

            "cycle_thinking_level" => {
                // Calculate next thinking level without holding lock across apply_thinking_level await
                let next = {
                    let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let runtime_provider = guard.agent.provider().name().to_string();
                    let runtime_model_id = guard.agent.provider().model_id().to_string();
                    let entry = {
                        let inner_session = guard.session.lock(&cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        current_or_runtime_model_entry(
                            &inner_session,
                            &runtime_provider,
                            &runtime_model_id,
                            &options,
                        )
                        .cloned()
                    };
                    let Some(entry) = entry else {
                        let _ =
                            out_tx.send(response_ok(id, "cycle_thinking_level", Some(Value::Null)));
                        continue;
                    };
                    if !entry.model.reasoning {
                        let _ =
                            out_tx.send(response_ok(id, "cycle_thinking_level", Some(Value::Null)));
                        continue;
                    }

                    let levels = available_thinking_levels(&entry);
                    let current = guard
                        .agent
                        .stream_options()
                        .thinking_level
                        .unwrap_or_default();
                    let current_index = levels
                        .iter()
                        .position(|level| *level == current)
                        .unwrap_or(0);
                    levels[(current_index + 1) % levels.len()]
                }; // Drop guard here

                // Apply thinking level without holding lock across await
                if let Err(err) = apply_thinking_level(Arc::clone(&session), next).await {
                    let _ = out_tx.send(response_error_with_hints(
                        id.clone(),
                        "cycle_thinking_level",
                        &err,
                    ));
                    continue;
                }
                let _ = out_tx.send(response_ok(
                    id,
                    "cycle_thinking_level",
                    Some(json!({ "level": next.to_string() })),
                ));
            }

            "set_steering_mode" => {
                let Some(mode) = parsed.get("mode").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_steering_mode",
                        "Missing mode".to_string(),
                    ));
                    continue;
                };
                let Some(mode) = parse_queue_mode(Some(mode)) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_steering_mode",
                        "Invalid steering mode".to_string(),
                    ));
                    continue;
                };
                let follow_up_mode = {
                    let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                    state.steering_mode = mode;
                    state.follow_up_mode
                };
                let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                    .await
                    .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                guard.set_queue_modes(mode, follow_up_mode);
                drop(guard);
                let _ = out_tx.send(response_ok(id, "set_steering_mode", None));
            }

            "set_follow_up_mode" => {
                let Some(mode) = parsed.get("mode").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_follow_up_mode",
                        "Missing mode".to_string(),
                    ));
                    continue;
                };
                let Some(mode) = parse_queue_mode(Some(mode)) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_follow_up_mode",
                        "Invalid follow-up mode".to_string(),
                    ));
                    continue;
                };
                let steering_mode = {
                    let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                    state.follow_up_mode = mode;
                    state.steering_mode
                };
                let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                    .await
                    .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                guard.set_queue_modes(steering_mode, mode);
                drop(guard);
                let _ = out_tx.send(response_ok(id, "set_follow_up_mode", None));
            }

            "set_auto_compaction" => {
                let Some(enabled) = parsed.get("enabled").and_then(Value::as_bool) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_auto_compaction",
                        "Missing enabled".to_string(),
                    ));
                    continue;
                };
                let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                    .await
                    .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                state.auto_compaction_enabled = enabled;
                drop(state);
                let _ = out_tx.send(response_ok(id, "set_auto_compaction", None));
            }

            "set_auto_retry" => {
                let Some(enabled) = parsed.get("enabled").and_then(Value::as_bool) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_auto_retry",
                        "Missing enabled".to_string(),
                    ));
                    continue;
                };
                let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                    .await
                    .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                state.auto_retry_enabled = enabled;
                drop(state);
                let _ = out_tx.send(response_ok(id, "set_auto_retry", None));
            }

            "abort_retry" => {
                retry_abort.store(true, Ordering::SeqCst);
                let _ = out_tx.send(response_ok(id, "abort_retry", None));
            }

            "set_session_name" => {
                let Some(name) = parsed.get("name").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_session_name",
                        "Missing name".to_string(),
                    ));
                    continue;
                };
                let result: Result<()> = async {
                    // Apply session info changes without holding lock across persist_session await
                    {
                        let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                            .await
                            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                        let mut inner_session = guard.session.lock(&cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        inner_session.append_session_info(Some(name.to_string()));
                    } // Drop guard here

                    // Re-acquire guard just for persist_session
                    let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    guard.persist_session().await?;
                    Ok(())
                }
                .await;

                match result {
                    Ok(()) => {
                        let _ = out_tx.send(response_ok(id, "set_session_name", None));
                    }
                    Err(err) => {
                        let _ =
                            out_tx.send(response_error_with_hints(id, "set_session_name", &err));
                    }
                }
            }

            "get_last_assistant_text" => {
                let text = {
                    let inner_session = OwnedMutexGuard::lock(Arc::clone(&session_handle), &cx)
                        .await
                        .map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                    last_assistant_text(&inner_session)
                };
                let _ = out_tx.send(response_ok(
                    id,
                    "get_last_assistant_text",
                    Some(json!({ "text": text })),
                ));
            }

            "export_html" => {
                let output_path = parsed
                    .get("outputPath")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                // Capture a lightweight snapshot under lock, then release immediately.
                // This avoids cloning the full Session (caches, autosave queue, etc.)
                // and allows the HTML rendering + file I/O to proceed without holding
                // any session lock.
                let snapshot = {
                    let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let inner = guard.session.lock(&cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    inner.export_snapshot()
                };
                match export_html_snapshot(&snapshot, output_path.as_deref()).await {
                    Ok(path) => {
                        let _ = out_tx.send(response_ok(
                            id,
                            "export_html",
                            Some(json!({ "path": path })),
                        ));
                    }
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "export_html", &err));
                    }
                }
            }

            "bash" => {
                let Some(command) = parsed.get("command").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(id, "bash", "Missing command".to_string()));
                    continue;
                };

                let mut running = OwnedMutexGuard::lock(Arc::clone(&bash_state), &cx)
                    .await
                    .map_err(|err| Error::session(format!("bash state lock failed: {err}")))?;
                if running.is_some() {
                    let _ = out_tx.send(response_error(
                        id,
                        "bash",
                        "Bash command already running".to_string(),
                    ));
                    continue;
                }

                let run_id = uuid::Uuid::new_v4().to_string();
                let (abort_tx, abort_rx) = oneshot::channel();
                *running = Some(RunningBash {
                    id: run_id.clone(),
                    abort_tx,
                });

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let bash_state = Arc::clone(&bash_state);
                let command = command.to_string();
                let id_clone = id.clone();
                let runtime_handle = options.runtime_handle.clone();
                let bash_cx = cx.clone();

                runtime_handle.spawn(async move {
                    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                    let result = run_bash_rpc(&cwd, &command, abort_rx).await;

                    let response = match result {
                        Ok(result) => {
                            // Append bash execution message without holding lock across persist_session await.
                            //
                            // The outer `AgentSession` guard must be owned: it is
                            // held across the inner `guard.session.lock(..).await`,
                            // and `MutexGuard` is `!Send` (asupersync 0.3.9) while
                            // this future is handed to `RuntimeHandle::spawn`.
                            // Acquisition order is unchanged (outer `AgentSession`
                            // then inner `Session`), and the inner guard is still
                            // released before the persist step below.
                            let should_persist = if let Ok(guard) =
                                OwnedMutexGuard::lock(Arc::clone(&session), &bash_cx).await
                            {
                                if let Ok(mut inner_session) = guard.session.lock(&bash_cx).await {
                                    inner_session.append_message(SessionMessage::BashExecution {
                                        command: command.clone(),
                                        output: result.output.clone(),
                                        exit_code: result.exit_code,
                                        cancelled: Some(result.cancelled),
                                        truncated: Some(result.truncated),
                                        full_output_path: result.full_output_path.clone(),
                                        timestamp: Some(chrono::Utc::now().timestamp_millis()),
                                        extra: std::collections::HashMap::default(),
                                    });
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            };

                            // Persist session outside the guard scope
                            if should_persist
                                && let Ok(mut guard) =
                                    OwnedMutexGuard::lock(Arc::clone(&session), &bash_cx).await
                            {
                                let _ = guard.persist_session().await;
                            }

                            response_ok(
                                id_clone,
                                "bash",
                                Some(json!({
                                    "output": result.output,
                                    "exitCode": result.exit_code,
                                    "cancelled": result.cancelled,
                                    "truncated": result.truncated,
                                    "fullOutputPath": result.full_output_path,
                                })),
                            )
                        }
                        Err(err) => response_error_with_hints(id_clone, "bash", &err),
                    };

                    let _ = out_tx.send(response);
                    if let Ok(mut running) = bash_state.lock(&bash_cx).await
                        && running.as_ref().is_some_and(|r| r.id == run_id)
                    {
                        *running = None;
                    }
                });
            }

            "abort_bash" => {
                let mut running = OwnedMutexGuard::lock(Arc::clone(&bash_state), &cx)
                    .await
                    .map_err(|err| Error::session(format!("bash state lock failed: {err}")))?;
                if let Some(running_bash) = running.take() {
                    let _ = running_bash.abort_tx.send(&cx, ());
                }
                let _ = out_tx.send(response_ok(id, "abort_bash", None));
            }

            "compact" => {
                let custom_instructions = parsed
                    .get("customInstructions")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let reserve_tokens_override =
                    match parse_optional_u32_field(&parsed, "reserveTokens") {
                        Ok(value) => value,
                        Err(err) => {
                            let _ =
                                out_tx.send(response_error_with_hints(id.clone(), "compact", &err));
                            continue;
                        }
                    };
                let keep_recent_tokens_override =
                    match parse_optional_u32_field(&parsed, "keepRecentTokens") {
                        Ok(value) => value,
                        Err(err) => {
                            let _ =
                                out_tx.send(response_error_with_hints(id.clone(), "compact", &err));
                            continue;
                        }
                    };

                let result: Result<Value> = async {
                    let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let path_entries = {
                        let mut inner_session = guard.session.lock(&cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        inner_session.ensure_entry_ids();
                        inner_session
                            .entries_for_current_path()
                            .into_iter()
                            .cloned()
                            .collect::<Vec<_>>()
                    };

                    let key = guard
                        .agent
                        .stream_options()
                        .api_key
                        .as_deref()
                        .ok_or_else(|| Error::auth("Missing API key for compaction"))?;

                    let provider = guard.agent.provider();

                    let settings = ResolvedCompactionSettings {
                        enabled: options.config.compaction_enabled(),
                        reserve_tokens: reserve_tokens_override
                            .unwrap_or_else(|| options.config.compaction_reserve_tokens()),
                        keep_recent_tokens: keep_recent_tokens_override
                            .unwrap_or_else(|| options.config.compaction_keep_recent_tokens()),
                        ..Default::default()
                    };

                    let prep = prepare_compaction(&path_entries, settings).ok_or_else(|| {
                        Error::session(
                            "Compaction not available (already compacted or missing IDs)",
                        )
                    })?;

                    is_compacting.store(true, Ordering::SeqCst);
                    let compact_res =
                        compact(prep, provider, key, custom_instructions.as_deref()).await;
                    is_compacting.store(false, Ordering::SeqCst);
                    let result_data = compact_res?;

                    let details_value = compaction_details_to_value(&result_data.details)?;

                    let messages = {
                        let mut inner_session = guard.session.lock(&cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        inner_session.append_compaction(
                            result_data.summary.clone(),
                            result_data.first_kept_entry_id.clone(),
                            result_data.tokens_before,
                            Some(details_value.clone()),
                            None,
                        );
                        inner_session.to_messages_for_current_path()
                    };
                    guard.persist_session().await?;
                    guard.agent.replace_messages(messages);

                    Ok(json!({
                        "summary": result_data.summary,
                        "firstKeptEntryId": result_data.first_kept_entry_id,
                        "tokensBefore": result_data.tokens_before,
                        "details": details_value,
                    }))
                }
                .await;

                match result {
                    Ok(data) => {
                        let _ = out_tx.send(response_ok(id, "compact", Some(data)));
                    }
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "compact", &err));
                    }
                }
            }

            "new_session" => {
                if rpc_dispatch_session_before_switch(rpc_extension_manager.clone(), "new", None)
                    .await
                {
                    let _ = out_tx.send(response_ok(
                        id,
                        "new_session",
                        Some(json!({ "cancelled": true })),
                    ));
                    continue;
                }

                let parent = parsed
                    .get("parentSession")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let (session_id, previous_session_file) = {
                    let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let (session_dir, provider, model_id, thinking_level, previous_session_file) = {
                        let inner_session = guard.session.lock(&cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        (
                            inner_session.session_dir.clone(),
                            inner_session.header.provider.clone(),
                            inner_session.header.model_id.clone(),
                            inner_session.header.thinking_level.clone(),
                            inner_session.path.as_ref().map(|p| p.display().to_string()),
                        )
                    };
                    let mut new_session = if guard.save_enabled() {
                        crate::session::Session::create_with_dir(session_dir)
                    } else {
                        crate::session::Session::in_memory()
                    };
                    new_session.header.parent_session = parent;
                    // Keep model fields in header for clients.
                    new_session.header.provider.clone_from(&provider);
                    new_session.header.model_id.clone_from(&model_id);
                    new_session
                        .header
                        .thinking_level
                        .clone_from(&thinking_level);

                    let session_id = new_session.header.id.clone();
                    {
                        let mut inner_session = guard.session.lock(&cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        *inner_session = new_session;
                    }
                    guard.agent.clear_messages();
                    guard.agent.stream_options_mut().session_id = Some(session_id.clone());

                    (session_id, previous_session_file)
                };
                {
                    let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                    state.steering.clear();
                    state.follow_up.clear();
                }
                rpc_dispatch_session_switch_event(
                    rpc_extension_manager.clone(),
                    json!({
                        "reason": "new",
                        "previousSessionFile": previous_session_file,
                        "sessionId": session_id,
                    }),
                )
                .await;
                let _ = out_tx.send(response_ok(
                    id,
                    "new_session",
                    Some(json!({ "cancelled": false })),
                ));
            }

            "switch_session" => {
                let Some(session_path) = parsed.get("sessionPath").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "switch_session",
                        "Missing sessionPath".to_string(),
                    ));
                    continue;
                };

                if rpc_dispatch_session_before_switch(
                    rpc_extension_manager.clone(),
                    "resume",
                    Some(session_path),
                )
                .await
                {
                    let _ = out_tx.send(response_ok(
                        id,
                        "switch_session",
                        Some(json!({ "cancelled": true })),
                    ));
                    continue;
                }

                // Validate relative paths against the sessions directory to prevent traversal.
                let session_path_buf = std::path::PathBuf::from(session_path);
                let sessions_dir = crate::config::Config::sessions_dir();
                let resolved_path = if session_path_buf.is_relative() {
                    sessions_dir.join(&session_path_buf)
                } else {
                    session_path_buf.clone()
                };
                if session_path_buf.is_relative() {
                    let canonical_session = crate::extensions::safe_canonicalize(&resolved_path);
                    let canonical_sessions_dir =
                        crate::extensions::safe_canonicalize(&sessions_dir);
                    if !canonical_session.starts_with(&canonical_sessions_dir) {
                        let _ = out_tx.send(response_error(
                            id,
                            "switch_session",
                            "Session path is outside the sessions directory".to_string(),
                        ));
                        continue;
                    }
                }

                let loaded =
                    crate::session::Session::open(resolved_path.to_string_lossy().as_ref()).await;
                match loaded {
                    Ok(new_session) => {
                        let target_session_file = new_session.path.as_ref().map_or_else(
                            || resolved_path.display().to_string(),
                            |p| p.display().to_string(),
                        );
                        let messages = new_session.to_messages_for_current_path();
                        let session_id = new_session.header.id.clone();
                        let previous_session_file;
                        let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                            .await
                            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                        {
                            let mut inner_session =
                                guard.session.lock(&cx).await.map_err(|err| {
                                    Error::session(format!("inner session lock failed: {err}"))
                                })?;
                            previous_session_file =
                                inner_session.path.as_ref().map(|p| p.display().to_string());
                            *inner_session = new_session;
                        }
                        guard.agent.replace_messages(messages);
                        guard.agent.stream_options_mut().session_id = Some(session_id.clone());
                        let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                            .await
                            .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                        state.steering.clear();
                        state.follow_up.clear();
                        drop(state);
                        drop(guard);

                        rpc_dispatch_session_switch_event(
                            rpc_extension_manager.clone(),
                            json!({
                                "reason": "resume",
                                "previousSessionFile": previous_session_file,
                                "targetSessionFile": target_session_file,
                                "sessionId": session_id,
                            }),
                        )
                        .await;

                        let _ = out_tx.send(response_ok(
                            id,
                            "switch_session",
                            Some(json!({ "cancelled": false })),
                        ));
                    }
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "switch_session", &err));
                    }
                }
            }

            "fork" => {
                let Some(entry_id) = parsed.get("entryId").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(id, "fork", "Missing entryId".to_string()));
                    continue;
                };

                let result: Result<String> = async {
                    // Phase 1: Snapshot — brief lock to compute ForkPlan + extract metadata.
                    let (fork_plan, parent_path, session_dir, save_enabled, header_snapshot) = {
                        let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                            .await
                            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                        let inner = guard.session.lock(&cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        let plan = inner.plan_fork_from_user_message(entry_id)?;
                        let parent_path = inner.path.as_ref().map(|p| p.display().to_string());
                        let session_dir = inner.session_dir.clone();
                        let header = inner.header.clone();
                        (plan, parent_path, session_dir, guard.save_enabled(), header)
                        // Both locks released here.
                    };

                    // Phase 2: Build new session without holding any lock.
                    let selected_text = fork_plan.selected_text.clone();

                    let mut new_session = if save_enabled {
                        crate::session::Session::create_with_dir(session_dir)
                    } else {
                        crate::session::Session::in_memory()
                    };
                    new_session.header.parent_session = parent_path;
                    new_session
                        .header
                        .provider
                        .clone_from(&header_snapshot.provider);
                    new_session
                        .header
                        .model_id
                        .clone_from(&header_snapshot.model_id);
                    new_session
                        .header
                        .thinking_level
                        .clone_from(&header_snapshot.thinking_level);
                    new_session.init_from_fork_plan(fork_plan);

                    let messages = new_session.to_messages_for_current_path();
                    let session_id = new_session.header.id.clone();

                    // Phase 3: Swap — brief lock to install the new session.
                    {
                        let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                            .await
                            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                        let mut inner = guard.session.lock(&cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        *inner = new_session;
                        drop(inner);
                        guard.agent.replace_messages(messages);
                        guard.agent.stream_options_mut().session_id = Some(session_id);
                    }

                    {
                        let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                            .await
                            .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                        state.steering.clear();
                        state.follow_up.clear();
                    }

                    Ok(selected_text)
                }
                .await;

                match result {
                    Ok(selected_text) => {
                        let _ = out_tx.send(response_ok(
                            id,
                            "fork",
                            Some(json!({ "text": selected_text, "cancelled": false })),
                        ));
                    }
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "fork", &err));
                    }
                }
            }

            "get_fork_messages" => {
                // Snapshot entries under brief lock, compute messages outside.
                let path_entries = {
                    let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let inner_session = guard.session.lock(&cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    inner_session
                        .entries_for_current_path()
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>()
                };
                let messages = fork_messages_from_entries(&path_entries);
                let _ = out_tx.send(response_ok(
                    id,
                    "get_fork_messages",
                    Some(json!({ "messages": messages })),
                ));
            }

            "get_commands" => {
                let commands = options.resources.list_commands();
                let _ = out_tx.send(response_ok(
                    id,
                    "get_commands",
                    Some(json!({ "commands": commands })),
                ));
            }

            "extension_ui_response" => {
                if let (Some(manager), Some(ui_state)) =
                    (rpc_extension_manager.as_ref(), rpc_ui_state.as_ref())
                {
                    let Some(request_id) = rpc_parse_extension_ui_response_id(&parsed) else {
                        let _ = out_tx.send(response_error(
                            id,
                            "extension_ui_response",
                            "Missing requestId (or id) field",
                        ));
                        continue;
                    };

                    let (response, next_request) = {
                        let Ok(mut guard) = ui_state.lock(&cx).await else {
                            let _ = out_tx.send(response_error(
                                id,
                                "extension_ui_response",
                                "Extension UI bridge unavailable",
                            ));
                            continue;
                        };

                        let Some(active) = guard.active.clone() else {
                            let _ = out_tx.send(response_error(
                                id,
                                "extension_ui_response",
                                "No active extension UI request",
                            ));
                            continue;
                        };

                        if active.id != request_id {
                            let _ = out_tx.send(response_error(
                                id,
                                "extension_ui_response",
                                format!(
                                    "Unexpected requestId: {request_id} (active: {})",
                                    active.id
                                ),
                            ));
                            continue;
                        }

                        let response = match rpc_parse_extension_ui_response(&parsed, &active) {
                            Ok(response) => response,
                            Err(message) => {
                                let _ = out_tx.send(response_error(
                                    id,
                                    "extension_ui_response",
                                    message,
                                ));
                                continue;
                            }
                        };

                        guard.active = None;
                        let next = guard.queue.pop_front();
                        if let Some(ref next) = next {
                            guard.active = Some(next.clone());
                        }
                        (response, next)
                    };

                    let resolved = manager.respond_ui(response);
                    let _ = out_tx.send(response_ok(
                        id,
                        "extension_ui_response",
                        Some(json!({ "resolved": resolved })),
                    ));

                    if let Some(next) = next_request {
                        rpc_emit_extension_ui_request(
                            &options.runtime_handle,
                            Arc::clone(ui_state),
                            (*manager).clone(),
                            out_tx.clone(),
                            next,
                        );
                    }
                } else {
                    let _ = out_tx.send(response_ok(id, "extension_ui_response", None));
                }
            }

            _ => {
                let _ = out_tx.send(response_error(
                    id,
                    command_type_raw,
                    format!("Unknown command: {command_type_raw}"),
                ));
            }
        }
    }

    // stdin has closed. Drain any in-flight work (streaming turn, extension
    // command, auto-compaction, background bash) before tearing down so a
    // client that pipes a single command and closes stdin
    // (`printf '{"type":"prompt",...}' | pi --mode rpc`) still receives the
    // full event stream through `agent_end` (gh #137). Without this the
    // process shuts down while the spawned task is still starting or
    // streaming, and the work is silently dropped. The Ctrl+C abort path in
    // `run_rpc_mode` still provides an escape hatch if a provider never
    // completes; a client that stops reading stdout while work is in flight
    // gets backpressure-blocked here by design.
    loop {
        let bash_running = OwnedMutexGuard::lock(Arc::clone(&bash_state), &cx)
            .await
            .is_ok_and(|running| running.is_some());
        if !is_streaming.load(Ordering::SeqCst)
            && !is_compacting.load(Ordering::SeqCst)
            && !bash_running
        {
            break;
        }
        // Extension UI requests can never be answered once stdin is closed;
        // cancel them (including any that arrive mid-drain) so an extension
        // command blocked on UI input finishes instead of pinning the drain.
        if let (Some(ui_state), Some(manager)) = (&rpc_ui_state, &rpc_extension_manager) {
            let pending = ui_state.lock(&cx).await.map_or_else(
                |_| Vec::new(),
                |mut guard| {
                    let mut pending: Vec<ExtensionUiRequest> =
                        guard.active.take().into_iter().collect();
                    pending.extend(std::mem::take(&mut guard.queue));
                    pending
                },
            );
            for request in pending {
                let _ = manager.respond_ui(ExtensionUiResponse {
                    id: request.id,
                    value: None,
                    cancelled: true,
                });
            }
        }
        let now = cx
            .cx()
            .timer_driver()
            .map_or_else(wall_now, |timer| timer.now());
        sleep(now, Duration::from_millis(25)).await;
    }

    // Explicitly shut down extension runtimes before the session drops.
    // Move the region out under lock, then await shutdown after releasing
    // the lock so we don't hold the session mutex across an async wait.
    let extension_region = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
        .await
        .ok()
        .and_then(|mut guard| guard.extensions.take());
    if let Some(ext) = extension_region {
        ext.shutdown().await;
    }

    Ok(())
}

// =============================================================================
// Prompt Execution
// =============================================================================

#[allow(clippy::too_many_lines)]
async fn run_prompt_with_retry(
    session: Arc<Mutex<AgentSession>>,
    shared_state: Arc<Mutex<RpcSharedState>>,
    is_streaming: Arc<AtomicBool>,
    is_compacting: Arc<AtomicBool>,
    abort_handle_slot: Arc<Mutex<Option<AbortHandle>>>,
    out_tx: std::sync::mpsc::SyncSender<String>,
    retry_abort: Arc<AtomicBool>,
    options: RpcOptions,
    message: String,
    images: Vec<ImageContent>,
    cx: AgentCx,
) {
    retry_abort.store(false, Ordering::SeqCst);
    is_streaming.store(true, Ordering::SeqCst);
    let _streaming_guard = ClearFlagOnDrop(Arc::clone(&is_streaming));

    let max_retries = options.config.retry_max_retries();
    let mut retry_count: u32 = 0;
    let mut success = false;
    let mut final_error: Option<String> = None;
    let mut final_error_hints: Option<Value> = None;

    loop {
        if retry_count > 0 && cx.checkpoint().is_err() {
            final_error = Some("Retry aborted".to_string());
            final_error_hints = None;
            break;
        }

        let (abort_handle, abort_signal) = AbortHandle::new();
        if let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(&abort_handle_slot), &cx).await {
            *guard = Some(abort_handle);
        } else {
            is_streaming.store(false, Ordering::SeqCst);
            return;
        }

        let runtime_for_events = options.runtime_handle.clone();

        let result = {
            let mut guard = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
                Ok(guard) => guard,
                Err(err) => {
                    final_error = Some(format!("session lock failed: {err}"));
                    final_error_hints = None;
                    break;
                }
            };
            let extensions = guard.extensions.as_ref().map(|r| r.manager().clone());
            let event_handler =
                rpc_agent_event_handler(out_tx.clone(), runtime_for_events, extensions);

            if retry_count == 0 {
                // First attempt: add the user message and run the turn.
                if images.is_empty() {
                    guard
                        .run_text_with_abort(message.clone(), Some(abort_signal), event_handler)
                        .await
                } else {
                    let blocks = build_prompt_content_blocks(&message, &images);
                    guard
                        .run_with_content_with_abort(blocks, Some(abort_signal), event_handler)
                        .await
                }
            } else {
                // Retry: resume the turn from the last completed state instead of
                // replaying it from the user message. The incomplete output of the
                // failed request was stripped via `revert_incomplete_response`
                // below; every completed tool cycle stays on the path, so the
                // retry re-issues only the failed provider request — no tool
                // re-execution, no re-billing of prior work (pi_agent_rust#125).
                guard
                    .run_continue_with_abort(Some(abort_signal), event_handler)
                    .await
            }
        };

        if let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(&abort_handle_slot), &cx).await {
            *guard = None;
        }

        match result {
            Ok(message) => {
                if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                    final_error = message
                        .error_message
                        .clone()
                        .or_else(|| Some("Request error".to_string()));
                    final_error_hints = None;
                    if message.stop_reason == StopReason::Aborted {
                        break;
                    }
                    // Check if this error is retryable. Context overflow and
                    // auth failures should NOT be retried.
                    if let Some(ref err_msg) = final_error {
                        let context_window = if let Ok(guard) =
                            OwnedMutexGuard::lock(Arc::clone(&session), &cx).await
                        {
                            let runtime_provider = guard.agent.provider().name().to_string();
                            let runtime_model_id = guard.agent.provider().model_id().to_string();
                            guard.session.lock(&cx).await.map_or(None, |inner| {
                                current_or_runtime_model_entry(
                                    &inner,
                                    &runtime_provider,
                                    &runtime_model_id,
                                    &options,
                                )
                                .map(|e| e.model.context_window)
                            })
                        } else {
                            None
                        };
                        if !crate::error::is_retryable_error(
                            err_msg,
                            Some(message.usage.input),
                            context_window,
                        ) {
                            break;
                        }
                    }
                } else {
                    success = true;
                    break;
                }
            }
            Err(err) => {
                let err_str = err.to_string();
                // Classify from the TYPED error first — `is_transient` walks the
                // source chain for a transient `io::ErrorKind` (connection
                // reset/abort/EOF/broken pipe/timeout) without depending on the
                // flattened message text. Fall back to message-text matching for
                // prose-only errors (pi_agent_rust#118). No usage/context_window
                // from an `Err` (no response received), so pass None for both.
                if !err.is_transient() && !crate::error::is_retryable_error(&err_str, None, None) {
                    final_error = Some(err_str);
                    final_error_hints = Some(error_hints_value(&err));
                    break;
                }
                final_error = Some(err_str);
                final_error_hints = Some(error_hints_value(&err));
            }
        }

        let retry_enabled = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
            .await
            .is_ok_and(|state| state.auto_retry_enabled);
        if !retry_enabled || retry_count >= max_retries {
            break;
        }

        retry_count += 1;
        let delay_ms = retry_delay_ms(&options.config, retry_count);
        let error_message = final_error
            .clone()
            .unwrap_or_else(|| "Request error".to_string());
        let _ = out_tx.send(agent_event(AgentEvent::AutoRetryStart {
            attempt: retry_count,
            max_attempts: max_retries,
            delay_ms: u64::from(delay_ms),
            error_message,
        }));

        let delay = Duration::from_millis(delay_ms as u64);
        let start = std::time::Instant::now();
        let mut retry_cancelled = false;
        while start.elapsed() < delay {
            if retry_abort.load(Ordering::SeqCst) {
                retry_cancelled = true;
                break;
            }
            if cx.checkpoint().is_err() {
                retry_cancelled = true;
                break;
            }
            let now = cx
                .cx()
                .timer_driver()
                .map_or_else(wall_now, |timer| timer.now());
            sleep(now, Duration::from_millis(50)).await;
        }

        if retry_cancelled || retry_abort.load(Ordering::SeqCst) {
            final_error = Some("Retry aborted".to_string());
            break;
        }

        // Strip only the failed request's incomplete output before the resume;
        // the user prompt and every completed tool cycle stay on the session
        // path so the retry re-issues only the failed provider request rather
        // than replaying the whole turn (pi_agent_rust#125).
        if let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
            let _ = guard.revert_incomplete_response().await;
        }
    }

    if retry_count > 0 {
        let _ = out_tx.send(agent_event(AgentEvent::AutoRetryEnd {
            success,
            attempt: retry_count,
            final_error: if success { None } else { final_error.clone() },
        }));
    }

    if !success {
        // Emit the terminal event BEFORE clearing is_streaming: the stdin-EOF
        // drain only guarantees flush-before-shutdown for events queued while
        // a flag is still set (gh #137).
        if let Some(err) = final_error {
            let mut payload = json!({
                "type": "agent_end",
                "messages": [],
                "error": err
            });
            if let Some(hints) = final_error_hints {
                payload["errorHints"] = hints;
            }
            let _ = out_tx.send(event(&payload));
        }
        is_streaming.store(false, Ordering::SeqCst);
        return;
    }

    let auto_compaction_enabled = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
        .await
        .is_ok_and(|state| state.auto_compaction_enabled);
    if auto_compaction_enabled {
        // Pre-claim the compaction flag before releasing is_streaming so the
        // stdin-EOF drain never samples both flags false during the handoff
        // (maybe_auto_compact only claims it after several awaits).
        is_compacting.store(true, Ordering::SeqCst);
    }
    is_streaming.store(false, Ordering::SeqCst);
    if auto_compaction_enabled {
        maybe_auto_compact(session, options, Arc::clone(&is_compacting), out_tx).await;
        // maybe_auto_compact clears the flag on the paths where it claims it
        // itself; this covers its early returns before that claim.
        is_compacting.store(false, Ordering::SeqCst);
    }
}

async fn run_extension_command(
    session: Arc<Mutex<AgentSession>>,
    is_streaming: Arc<AtomicBool>,
    abort_handle_slot: Arc<Mutex<Option<AbortHandle>>>,
    out_tx: std::sync::mpsc::SyncSender<String>,
    runtime_handle: RuntimeHandle,
    command_name: String,
    args: String,
    cx: AgentCx,
) {
    is_streaming.store(true, Ordering::SeqCst);
    let _streaming_guard = ClearFlagOnDrop(Arc::clone(&is_streaming));

    let (abort_handle, abort_signal) = AbortHandle::new();
    if let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(&abort_handle_slot), &cx).await {
        *guard = Some(abort_handle);
    } else {
        is_streaming.store(false, Ordering::SeqCst);
        return;
    }

    let result = {
        let mut guard = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
            Ok(guard) => guard,
            Err(err) => {
                let err = Error::session(format!("session lock failed: {err}"));
                let mut payload = json!({
                    "type": "agent_end",
                    "messages": [],
                    "error": err.to_string(),
                });
                payload["errorHints"] = error_hints_value(&err);
                let _ = out_tx.send(event(&payload));
                is_streaming.store(false, Ordering::SeqCst);
                return;
            }
        };
        let extensions = guard
            .extensions
            .as_ref()
            .map(|region| region.manager().clone());
        let event_handler = rpc_agent_event_handler(out_tx.clone(), runtime_handle, extensions);
        guard
            .execute_extension_command_with_abort(
                &command_name,
                &args,
                EXTENSION_EVENT_TIMEOUT_MS,
                Some(abort_signal),
                event_handler,
            )
            .await
    };

    if let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(&abort_handle_slot), &cx).await {
        *guard = None;
    }

    // Emit the terminal event BEFORE clearing is_streaming so the stdin-EOF
    // drain flushes it before shutdown (gh #137).
    if let Err(err) = result {
        let mut payload = json!({
            "type": "agent_end",
            "messages": [],
            "error": err.to_string(),
        });
        payload["errorHints"] = error_hints_value(&err);
        let _ = out_tx.send(event(&payload));
    }
    is_streaming.store(false, Ordering::SeqCst);
}

// =============================================================================
// Helpers
// =============================================================================

fn response_ok(id: Option<String>, command: &str, data: Option<Value>) -> String {
    let mut resp = json!({
        "type": "response",
        "command": command,
        "success": true,
    });
    if let Some(id) = id {
        resp["id"] = Value::String(id);
    }
    if let Some(data) = data {
        resp["data"] = data;
    }
    resp.to_string()
}

fn response_error(id: Option<String>, command: &str, error: impl Into<String>) -> String {
    let mut resp = json!({
        "type": "response",
        "command": command,
        "success": false,
        "error": error.into(),
    });
    if let Some(id) = id {
        resp["id"] = Value::String(id);
    }
    resp.to_string()
}

fn response_error_with_hints(id: Option<String>, command: &str, error: &Error) -> String {
    let mut resp = json!({
        "type": "response",
        "command": command,
        "success": false,
        "error": error.to_string(),
        "errorHints": error_hints_value(error),
    });
    if let Some(id) = id {
        resp["id"] = Value::String(id);
    }
    resp.to_string()
}

fn event(value: &Value) -> String {
    value.to_string()
}

fn agent_event(event: AgentEvent) -> String {
    serde_json::to_string(&event).unwrap_or_else(|err| {
        json!({
            "type": "event_serialize_error",
            "error": err.to_string(),
        })
        .to_string()
    })
}

fn rpc_emit_extension_ui_request(
    runtime_handle: &RuntimeHandle,
    ui_state: Arc<Mutex<RpcUiBridgeState>>,
    manager: ExtensionManager,
    out_tx_ui: std::sync::mpsc::SyncSender<String>,
    request: ExtensionUiRequest,
) {
    // Emit the UI request as a JSON notification to the client.
    let rpc_event = request.to_rpc_event();
    let _ = out_tx_ui.send(event(&rpc_event));

    if !request.expects_response() {
        return;
    }

    // For dialog methods, enforce deterministic ordering (one active request at a time) by
    // auto-resolving timeouts as cancellation defaults (per bd-2hz.1).
    let Some(timeout_ms) = request.effective_timeout_ms() else {
        return;
    };

    // Fire a little early so ExtensionManager::request_ui doesn't hit its own timeout first.
    let fire_ms = timeout_ms.saturating_sub(10).max(1);
    let request_id = request.id;
    let ui_state_timeout = Arc::clone(&ui_state);
    let manager_timeout = manager;
    let out_tx_timeout = out_tx_ui;
    let runtime_handle_inner = runtime_handle.clone();

    runtime_handle.spawn(async move {
        sleep(wall_now(), Duration::from_millis(fire_ms)).await;
        let cx = AgentCx::for_request();

        let next = {
            let Ok(mut guard) = ui_state_timeout.lock(cx.cx()).await else {
                return;
            };

            let Some(active) = guard.active.as_ref() else {
                return;
            };

            // No-op if the active request has already advanced.
            if active.id != request_id {
                return;
            }

            // Resolve with cancellation defaults (downstream maps method -> default return value).
            let _ = manager_timeout.respond_ui(ExtensionUiResponse {
                id: request_id,
                value: None,
                cancelled: true,
            });

            guard.active = None;
            let next = guard.queue.pop_front();
            if let Some(ref next) = next {
                guard.active = Some(next.clone());
            }
            next
        };

        if let Some(next) = next {
            rpc_emit_extension_ui_request(
                &runtime_handle_inner,
                ui_state_timeout,
                manager_timeout,
                out_tx_timeout,
                next,
            );
        }
    });
}

fn rpc_parse_extension_ui_response_id(parsed: &Value) -> Option<String> {
    let request_id = parsed
        .get("requestId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from);

    request_id.or_else(|| {
        parsed
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
    })
}

fn rpc_parse_extension_ui_response(
    parsed: &Value,
    active: &ExtensionUiRequest,
) -> std::result::Result<ExtensionUiResponse, String> {
    let cancelled = parsed
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if cancelled && active.method != "custom" {
        return Ok(ExtensionUiResponse {
            id: active.id.clone(),
            value: None,
            cancelled: true,
        });
    }

    match active.method.as_str() {
        "confirm" => {
            let value = parsed
                .get("confirmed")
                .and_then(Value::as_bool)
                .or_else(|| parsed.get("value").and_then(Value::as_bool))
                .ok_or_else(|| "confirm requires boolean `confirmed` (or `value`)".to_string())?;
            Ok(ExtensionUiResponse {
                id: active.id.clone(),
                value: Some(Value::Bool(value)),
                cancelled: false,
            })
        }
        "select" => {
            let Some(value) = parsed.get("value") else {
                return Err("select requires `value` field".to_string());
            };

            let options = active
                .payload
                .get("options")
                .and_then(Value::as_array)
                .ok_or_else(|| "select request missing `options` array".to_string())?;

            let mut allowed = Vec::with_capacity(options.len());
            for opt in options {
                match opt {
                    Value::String(s) => allowed.push(Value::String(s.clone())),
                    Value::Object(map) => {
                        let label = map
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim();
                        if label.is_empty() {
                            continue;
                        }
                        if let Some(v) = map.get("value") {
                            allowed.push(v.clone());
                        } else {
                            allowed.push(Value::String(label.to_string()));
                        }
                    }
                    _ => {}
                }
            }

            if !allowed.iter().any(|candidate| candidate == value) {
                return Err("select response value did not match any option".to_string());
            }

            Ok(ExtensionUiResponse {
                id: active.id.clone(),
                value: Some(value.clone()),
                cancelled: false,
            })
        }
        "input" | "editor" => {
            let Some(value) = parsed.get("value") else {
                return Err(format!("{} requires `value` field", active.method));
            };
            if !value.is_string() {
                return Err(format!("{} requires string `value`", active.method));
            }
            Ok(ExtensionUiResponse {
                id: active.id.clone(),
                value: Some(value.clone()),
                cancelled: false,
            })
        }
        "custom" => {
            if let Some(value) = parsed.get("value").filter(|value| !value.is_null()) {
                return Ok(ExtensionUiResponse {
                    id: active.id.clone(),
                    value: Some(value.clone()),
                    cancelled: false,
                });
            }

            let mut payload = serde_json::Map::new();
            if let Some(key) = parsed.get("key").and_then(Value::as_str) {
                payload.insert("key".to_string(), Value::String(key.to_string()));
            }
            if let Some(width) = parsed.get("width").and_then(Value::as_u64) {
                payload.insert("width".to_string(), Value::from(width));
            }
            if let Some(close) = parsed
                .get("cancelled")
                .or_else(|| parsed.get("closed"))
                .and_then(Value::as_bool)
            {
                payload.insert("closed".to_string(), Value::Bool(close));
            }
            if payload.is_empty() {
                return Err("custom requires `value`, `key`, `width`, or `cancelled`".to_string());
            }
            Ok(ExtensionUiResponse {
                id: active.id.clone(),
                value: Some(Value::Object(payload)),
                cancelled: false,
            })
        }
        "notify" => Ok(ExtensionUiResponse {
            id: active.id.clone(),
            value: None,
            cancelled: false,
        }),
        other => Err(format!("Unsupported extension UI method: {other}")),
    }
}

#[cfg(test)]
mod ui_bridge_tests {
    use super::*;

    #[test]
    fn parse_extension_ui_response_id_prefers_request_id() {
        let value = json!({"type":"extension_ui_response","id":"legacy","requestId":"canonical"});
        assert_eq!(
            rpc_parse_extension_ui_response_id(&value),
            Some("canonical".to_string())
        );
    }

    #[test]
    fn parse_extension_ui_response_id_accepts_id_alias() {
        let value = json!({"type":"extension_ui_response","id":"legacy"});
        assert_eq!(
            rpc_parse_extension_ui_response_id(&value),
            Some("legacy".to_string())
        );
    }

    #[test]
    fn parse_confirm_response_accepts_confirmed_alias() {
        let active = ExtensionUiRequest::new("req-1", "confirm", json!({"title":"t"}));
        let value = json!({"type":"extension_ui_response","requestId":"req-1","confirmed":true});
        let resp = rpc_parse_extension_ui_response(&value, &active).expect("parse confirm");
        assert!(!resp.cancelled);
        assert_eq!(resp.value, Some(json!(true)));
    }

    #[test]
    fn parse_confirm_response_accepts_value_bool() {
        let active = ExtensionUiRequest::new("req-1", "confirm", json!({"title":"t"}));
        let value = json!({"type":"extension_ui_response","requestId":"req-1","value":false});
        let resp = rpc_parse_extension_ui_response(&value, &active).expect("parse confirm");
        assert!(!resp.cancelled);
        assert_eq!(resp.value, Some(json!(false)));
    }

    #[test]
    fn parse_cancelled_response_wins_over_value() {
        let active = ExtensionUiRequest::new("req-1", "confirm", json!({"title":"t"}));
        let value = json!({"type":"extension_ui_response","requestId":"req-1","cancelled":true,"value":true});
        let resp = rpc_parse_extension_ui_response(&value, &active).expect("parse cancel");
        assert!(resp.cancelled);
        assert_eq!(resp.value, None);
    }

    #[test]
    fn parse_select_response_validates_against_options() {
        let active = ExtensionUiRequest::new(
            "req-1",
            "select",
            json!({"title":"pick","options":["A","B"]}),
        );
        let ok_value = json!({"type":"extension_ui_response","requestId":"req-1","value":"B"});
        let ok = rpc_parse_extension_ui_response(&ok_value, &active).expect("parse select ok");
        assert_eq!(ok.value, Some(json!("B")));

        let bad_value = json!({"type":"extension_ui_response","requestId":"req-1","value":"C"});
        assert!(
            rpc_parse_extension_ui_response(&bad_value, &active).is_err(),
            "invalid selection should error"
        );
    }

    #[test]
    fn parse_input_requires_string_value() {
        let active = ExtensionUiRequest::new("req-1", "input", json!({"title":"t"}));
        let ok_value = json!({"type":"extension_ui_response","requestId":"req-1","value":"hi"});
        let ok = rpc_parse_extension_ui_response(&ok_value, &active).expect("parse input ok");
        assert_eq!(ok.value, Some(json!("hi")));

        let bad_value = json!({"type":"extension_ui_response","requestId":"req-1","value":123});
        assert!(
            rpc_parse_extension_ui_response(&bad_value, &active).is_err(),
            "non-string input should error"
        );
    }

    #[test]
    fn parse_editor_requires_string_value() {
        let active = ExtensionUiRequest::new("req-1", "editor", json!({"title":"t"}));
        let ok = json!({"requestId":"req-1","value":"multi\nline"});
        let resp = rpc_parse_extension_ui_response(&ok, &active).expect("editor ok");
        assert_eq!(resp.value, Some(json!("multi\nline")));

        let bad = json!({"requestId":"req-1","value":42});
        assert!(
            rpc_parse_extension_ui_response(&bad, &active).is_err(),
            "editor needs string"
        );
    }

    #[test]
    fn parse_notify_returns_no_value() {
        let active = ExtensionUiRequest::new("req-1", "notify", json!({"title":"t"}));
        let val = json!({"requestId":"req-1"});
        let resp = rpc_parse_extension_ui_response(&val, &active).expect("notify ok");
        assert!(!resp.cancelled);
        assert!(resp.value.is_none());
    }

    #[test]
    fn parse_custom_accepts_value_passthrough() {
        let active = ExtensionUiRequest::new("req-1", "custom", json!({}));
        let val = json!({"requestId":"req-1","value":{"key":"w","width":88}});
        let resp = rpc_parse_extension_ui_response(&val, &active).expect("custom value");
        assert_eq!(resp.value, Some(json!({"key":"w","width":88})));
        assert!(!resp.cancelled);
    }

    #[test]
    fn parse_custom_accepts_key_width_fields() {
        let active = ExtensionUiRequest::new("req-1", "custom", json!({}));
        let val = json!({"requestId":"req-1","key":"q","width":120});
        let resp = rpc_parse_extension_ui_response(&val, &active).expect("custom key+width");
        assert_eq!(resp.value, Some(json!({"key":"q","width":120})));
        assert!(!resp.cancelled);
    }

    #[test]
    fn parse_custom_preserves_cancelled_and_width_as_payload() {
        let active = ExtensionUiRequest::new("req-1", "custom", json!({}));
        let val = json!({"requestId":"req-1","width":120,"cancelled":true});
        let resp = rpc_parse_extension_ui_response(&val, &active).expect("custom cancelled+width");
        assert_eq!(resp.value, Some(json!({"width":120,"closed":true})));
        assert!(!resp.cancelled);
    }

    #[test]
    fn parse_custom_treats_null_value_as_absent_for_close_payloads() {
        let active = ExtensionUiRequest::new("req-1", "custom", json!({}));
        let val = json!({"requestId":"req-1","value":null,"cancelled":true});
        let resp = rpc_parse_extension_ui_response(&val, &active).expect("custom null+cancelled");
        assert_eq!(resp.value, Some(json!({"closed":true})));
        assert!(!resp.cancelled);
    }

    #[test]
    fn parse_unsupported_method_errors() {
        let active = ExtensionUiRequest::new("req-1", "custom_method", json!({}));
        let val = json!({"requestId":"req-1","value":"x"});
        let err = rpc_parse_extension_ui_response(&val, &active).unwrap_err();
        assert!(err.contains("Unsupported"), "err={err}");
    }

    #[test]
    fn parse_select_missing_value_field() {
        let active =
            ExtensionUiRequest::new("req-1", "select", json!({"title":"pick","options":["A"]}));
        let val = json!({"requestId":"req-1"});
        let err = rpc_parse_extension_ui_response(&val, &active).unwrap_err();
        assert!(err.contains("value"), "err={err}");
    }

    #[test]
    fn parse_confirm_missing_value_errors() {
        let active = ExtensionUiRequest::new("req-1", "confirm", json!({"title":"t"}));
        let val = json!({"requestId":"req-1"});
        let err = rpc_parse_extension_ui_response(&val, &active).unwrap_err();
        assert!(err.contains("confirm"), "err={err}");
    }

    #[test]
    fn parse_select_with_label_value_objects() {
        let active = ExtensionUiRequest::new(
            "req-1",
            "select",
            json!({
                "title": "pick",
                "options": [
                    {"label": "Alpha", "value": "a"},
                    {"label": "Beta", "value": "b"},
                ]
            }),
        );
        let val = json!({"requestId":"req-1","value":"a"});
        let resp = rpc_parse_extension_ui_response(&val, &active).expect("select by value");
        assert_eq!(resp.value, Some(json!("a")));
    }

    #[test]
    fn parse_id_rejects_empty_and_whitespace() {
        let val = json!({"requestId":"  ","id":""});
        assert!(rpc_parse_extension_ui_response_id(&val).is_none());
    }

    #[test]
    fn bridge_state_default_is_empty() {
        let state = RpcUiBridgeState::default();
        assert!(state.active.is_none());
        assert!(state.queue.is_empty());
    }
}

fn error_hints_value(error: &Error) -> Value {
    let hint = error_hints::hints_for_error(error);
    json!({
        "summary": hint.summary,
        "hints": hint.hints,
        "contextFields": hint.context_fields,
    })
}

fn rpc_session_message_value(message: SessionMessage) -> Value {
    let mut value = match serde_json::to_value(message) {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("Failed to serialize SessionMessage: {err}");
            return serde_json::json!({"error": format!("serialization error: {err}")});
        }
    };
    rpc_flatten_content_blocks(&mut value);
    value
}

fn rpc_flatten_content_blocks(value: &mut Value) {
    let Value::Object(message_obj) = value else {
        return;
    };
    let Some(content) = message_obj.get_mut("content") else {
        return;
    };
    let Value::Array(blocks) = content else {
        return;
    };

    for block in blocks {
        let Value::Object(block_obj) = block else {
            continue;
        };
        let Some(inner) = block_obj.remove("0") else {
            continue;
        };
        let Value::Object(inner_obj) = inner else {
            block_obj.insert("0".to_string(), inner);
            continue;
        };
        for (key, value) in inner_obj {
            block_obj.entry(key).or_insert(value);
        }
    }
}

fn retry_delay_ms(config: &Config, attempt: u32) -> u32 {
    let base = u64::from(config.retry_base_delay_ms());
    let max = u64::from(config.retry_max_delay_ms());
    let shift = attempt.saturating_sub(1);
    let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let delay = base.saturating_mul(multiplier).min(max);
    u32::try_from(delay).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::agent::{Agent, AgentConfig, AgentSession};
    use crate::model::{AssistantMessage, Usage};
    use crate::provider::Provider;
    use crate::resources::ResourceLoader;
    use crate::session::Session;
    use crate::tools::ToolRegistry;
    use async_trait::async_trait;
    use futures::stream;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct FlakyProvider {
        calls: AtomicUsize,
    }

    impl FlakyProvider {
        const fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for FlakyProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &crate::provider::Context<'_>,
            _options: &crate::provider::StreamOptions,
        ) -> crate::error::Result<
            Pin<
                Box<
                    dyn futures::Stream<Item = crate::error::Result<crate::model::StreamEvent>>
                        + Send,
                >,
            >,
        > {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);

            let mut partial = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };

            let events = if call == 0 {
                // First call fails with an explicit error event.
                partial.stop_reason = StopReason::Error;
                partial.error_message = Some("server error".to_string());
                vec![
                    Ok(crate::model::StreamEvent::Start {
                        partial: partial.clone(),
                    }),
                    Ok(crate::model::StreamEvent::Error {
                        reason: StopReason::Error,
                        error: partial,
                    }),
                ]
            } else {
                // Second call succeeds.
                vec![
                    Ok(crate::model::StreamEvent::Start {
                        partial: partial.clone(),
                    }),
                    Ok(crate::model::StreamEvent::Done {
                        reason: StopReason::Stop,
                        message: partial,
                    }),
                ]
            };

            Ok(Box::pin(stream::iter(events)))
        }
    }

    #[derive(Debug)]
    struct AlwaysErrorProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for AlwaysErrorProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &crate::provider::Context<'_>,
            _options: &crate::provider::StreamOptions,
        ) -> crate::error::Result<
            Pin<
                Box<
                    dyn futures::Stream<Item = crate::error::Result<crate::model::StreamEvent>>
                        + Send,
                >,
            >,
        > {
            let mut partial = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Error,
                stop_details: None,
                error_message: Some("server error".to_string()),
                timestamp: 0,
            };

            let events = vec![
                Ok(crate::model::StreamEvent::Start {
                    partial: partial.clone(),
                }),
                Ok(crate::model::StreamEvent::Error {
                    reason: StopReason::Error,
                    error: {
                        partial.stop_reason = StopReason::Error;
                        partial
                    },
                }),
            ];

            Ok(Box::pin(stream::iter(events)))
        }
    }

    #[test]
    fn rpc_auto_retry_retries_then_succeeds() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 8)
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(FlakyProvider::new());
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let inner_session = Arc::new(Mutex::new(Session::in_memory()));
            let agent_session = AgentSession::new(
                agent,
                inner_session,
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );

            let session = Arc::new(Mutex::new(agent_session));

            let mut config = Config::default();
            config.retry = Some(crate::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(1),
                base_delay_ms: Some(1),
                max_delay_ms: Some(1),
            });

            let mut shared = RpcSharedState::new(&config);
            shared.auto_compaction_enabled = false;
            let shared_state = Arc::new(Mutex::new(shared));

            let is_streaming = Arc::new(AtomicBool::new(false));
            let is_compacting = Arc::new(AtomicBool::new(false));
            let abort_handle_slot: Arc<Mutex<Option<AbortHandle>>> = Arc::new(Mutex::new(None));
            let retry_abort = Arc::new(AtomicBool::new(false));
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);

            let auth_path = tempfile::tempdir()
                .expect("tempdir")
                .path()
                .join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("auth load");

            let options = RpcOptions {
                config,
                resources: ResourceLoader::empty(false),
                available_models: Vec::new(),
                scoped_models: Vec::new(),
                cli_api_key: None,
                auth,
                runtime_handle,
            };

            run_prompt_with_retry(
                session,
                shared_state,
                is_streaming,
                is_compacting,
                abort_handle_slot,
                out_tx,
                retry_abort,
                options,
                "hello".to_string(),
                Vec::new(),
                AgentCx::for_request(),
            )
            .await;

            let mut saw_retry_start = false;
            let mut saw_retry_end_success = false;
            let mut retry_end_value = None;

            for line in out_rx.try_iter() {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(kind) = value.get("type").and_then(Value::as_str) else {
                    continue;
                };
                match kind {
                    "auto_retry_start" => {
                        saw_retry_start = true;
                    }
                    "auto_retry_end"
                        if value.get("success").and_then(Value::as_bool) == Some(true) =>
                    {
                        retry_end_value = Some(value.clone());
                        saw_retry_end_success = true;
                    }
                    _ => {}
                }
            }

            assert!(saw_retry_start, "missing auto_retry_start event");
            assert!(
                saw_retry_end_success,
                "missing successful auto_retry_end event"
            );
            let retry_end_value = retry_end_value.expect("retry end event value");
            assert!(
                retry_end_value.get("finalError").is_none(),
                "successful auto_retry_end must omit absent finalError: {retry_end_value}"
            );
        });
    }

    #[test]
    fn rpc_abort_retry_emits_ordered_retry_timeline() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 8)
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(AlwaysErrorProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let inner_session = Arc::new(Mutex::new(Session::in_memory()));
            let agent_session = AgentSession::new(
                agent,
                inner_session,
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );

            let session = Arc::new(Mutex::new(agent_session));

            let mut config = Config::default();
            config.retry = Some(crate::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(3),
                base_delay_ms: Some(1000),
                max_delay_ms: Some(1000),
            });

            let mut shared = RpcSharedState::new(&config);
            shared.auto_compaction_enabled = false;
            let shared_state = Arc::new(Mutex::new(shared));

            let is_streaming = Arc::new(AtomicBool::new(false));
            let is_compacting = Arc::new(AtomicBool::new(false));
            let abort_handle_slot: Arc<Mutex<Option<AbortHandle>>> = Arc::new(Mutex::new(None));
            let retry_abort = Arc::new(AtomicBool::new(false));
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);

            let auth_path = tempfile::tempdir()
                .expect("tempdir")
                .path()
                .join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("auth load");

            let options = RpcOptions {
                config,
                resources: ResourceLoader::empty(false),
                available_models: Vec::new(),
                scoped_models: Vec::new(),
                cli_api_key: None,
                auth,
                runtime_handle,
            };

            let retry_abort_for_thread = Arc::clone(&retry_abort);
            let abort_thread = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(100));
                retry_abort_for_thread.store(true, Ordering::SeqCst);
            });

            run_prompt_with_retry(
                session,
                shared_state,
                is_streaming,
                is_compacting,
                abort_handle_slot,
                out_tx,
                retry_abort,
                options,
                "hello".to_string(),
                Vec::new(),
                AgentCx::for_request(),
            )
            .await;
            abort_thread.join().expect("abort thread join");

            let mut timeline = Vec::new();
            let mut last_agent_end_error = None::<String>;

            for line in out_rx.try_iter() {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(kind) = value.get("type").and_then(Value::as_str) else {
                    continue;
                };
                timeline.push(kind.to_string());
                if kind == "agent_end" {
                    last_agent_end_error = value
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }

            let retry_start_idx = timeline
                .iter()
                .position(|kind| kind == "auto_retry_start")
                .expect("missing auto_retry_start");
            let retry_end_idx = timeline
                .iter()
                .position(|kind| kind == "auto_retry_end")
                .expect("missing auto_retry_end");
            let agent_end_idx = timeline
                .iter()
                .rposition(|kind| kind == "agent_end")
                .expect("missing agent_end");

            assert!(
                retry_start_idx < retry_end_idx && retry_end_idx < agent_end_idx,
                "unexpected retry timeline ordering: {timeline:?}"
            );
            assert_eq!(
                last_agent_end_error.as_deref(),
                Some("Retry aborted"),
                "expected retry-abort terminal error, timeline: {timeline:?}"
            );
        });
    }

    #[test]
    fn rpc_cancelled_agent_cx_aborts_retry_timeline() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 8)
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(AlwaysErrorProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let inner_session = Arc::new(Mutex::new(Session::in_memory()));
            let agent_session = AgentSession::new(
                agent,
                inner_session,
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );

            let session = Arc::new(Mutex::new(agent_session));

            let mut config = Config::default();
            config.retry = Some(crate::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(3),
                base_delay_ms: Some(1000),
                max_delay_ms: Some(1000),
            });

            let mut shared = RpcSharedState::new(&config);
            shared.auto_compaction_enabled = false;
            let shared_state = Arc::new(Mutex::new(shared));

            let is_streaming = Arc::new(AtomicBool::new(false));
            let is_compacting = Arc::new(AtomicBool::new(false));
            let abort_handle_slot: Arc<Mutex<Option<AbortHandle>>> = Arc::new(Mutex::new(None));
            let retry_abort = Arc::new(AtomicBool::new(false));
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);

            let auth_path = tempfile::tempdir()
                .expect("tempdir")
                .path()
                .join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("auth load");

            let options = RpcOptions {
                config,
                resources: ResourceLoader::empty(false),
                available_models: Vec::new(),
                scoped_models: Vec::new(),
                cli_api_key: None,
                auth,
                runtime_handle,
            };

            let retry_cx = asupersync::Cx::for_testing();
            let cancel_cx = retry_cx.clone();
            let cancel_thread = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(100));
                cancel_cx.set_cancel_requested(true);
            });

            run_prompt_with_retry(
                session,
                shared_state,
                is_streaming,
                is_compacting,
                abort_handle_slot,
                out_tx,
                retry_abort,
                options,
                "hello".to_string(),
                Vec::new(),
                AgentCx::from_cx(retry_cx),
            )
            .await;
            cancel_thread.join().expect("cancel thread join");

            let mut timeline = Vec::new();
            let mut last_agent_end_error = None::<String>;

            for line in out_rx.try_iter() {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(kind) = value.get("type").and_then(Value::as_str) else {
                    continue;
                };
                timeline.push(kind.to_string());
                if kind == "agent_end" {
                    last_agent_end_error = value
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }

            let retry_start_idx = timeline
                .iter()
                .position(|kind| kind == "auto_retry_start")
                .expect("missing auto_retry_start");
            let retry_end_idx = timeline
                .iter()
                .position(|kind| kind == "auto_retry_end")
                .expect("missing auto_retry_end");
            let agent_end_idx = timeline
                .iter()
                .rposition(|kind| kind == "agent_end")
                .expect("missing agent_end");

            assert!(
                retry_start_idx < retry_end_idx && retry_end_idx < agent_end_idx,
                "unexpected retry timeline ordering: {timeline:?}"
            );
            assert_eq!(
                last_agent_end_error.as_deref(),
                Some("Retry aborted"),
                "expected retry-abort terminal error, timeline: {timeline:?}"
            );
        });
    }

    #[test]
    fn rpc_prompt_command_inherits_cancelled_context_from_run() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(AlwaysErrorProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let agent_session = AgentSession::new(
                agent,
                Arc::new(asupersync::sync::Mutex::new(Session::in_memory())),
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );

            let mut config = Config::default();
            config.retry = Some(crate::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(10),
                base_delay_ms: Some(1000),
                max_delay_ms: Some(1000),
            });

            let auth_path = tempfile::tempdir()
                .expect("tempdir")
                .path()
                .join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("auth load");
            let options = RpcOptions {
                config,
                resources: ResourceLoader::empty(false),
                available_models: Vec::new(),
                scoped_models: Vec::new(),
                cli_api_key: None,
                auth,
                runtime_handle,
            };

            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(std::sync::Mutex::new(out_rx));

            let ambient_cx = asupersync::Cx::for_testing();
            let cancel_cx = ambient_cx.clone();
            let _current = asupersync::Cx::set_current(Some(ambient_cx));

            let client_out_rx = Arc::clone(&out_rx);
            let client = async move {
                let send_cx = asupersync::Cx::for_testing();
                in_tx
                    .send(
                        &send_cx,
                        r#"{"id":"1","type":"prompt","message":"hello"}"#.to_string(),
                    )
                    .await
                    .expect("send prompt command");

                let ack_wait = async {
                    loop {
                        let recv_result = {
                            let rx = client_out_rx.lock().expect("lock rpc output receiver");
                            rx.try_recv()
                        };

                        match recv_result {
                            Ok(line) => {
                                let value: Value =
                                    serde_json::from_str(&line).expect("parse rpc output");
                                if value.get("type").and_then(Value::as_str) == Some("response") {
                                    break value;
                                }
                            }
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                tracing::warn!(
                                    "prompt(cancel-inherit): output channel disconnected"
                                );
                                break Value::Object(serde_json::Map::new());
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => {
                                asupersync::time::sleep(
                                    asupersync::time::wall_now(),
                                    Duration::from_millis(5),
                                )
                                .await;
                            }
                        }
                    }
                };
                futures::pin_mut!(ack_wait);
                let ack = asupersync::time::timeout(
                    asupersync::time::wall_now(),
                    Duration::from_secs(5),
                    ack_wait,
                )
                .await;
                let ack = ack.expect("prompt acknowledgement");
                assert_eq!(ack["command"], "prompt");
                assert_eq!(ack["success"], true, "prompt should be accepted: {ack}");

                let cancel_thread = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(100));
                    cancel_cx.set_cancel_requested(true);
                });

                let retry_abort_wait = async {
                    let mut timeline = Vec::new();
                    loop {
                        let recv_result = {
                            let rx = client_out_rx.lock().expect("lock rpc output receiver");
                            rx.try_recv()
                        };

                        match recv_result {
                            Ok(line) => {
                                let value: Value =
                                    serde_json::from_str(&line).expect("parse rpc output");
                                let Some(kind) = value.get("type").and_then(Value::as_str) else {
                                    continue;
                                };
                                timeline.push(kind.to_string());
                                if kind == "agent_end" {
                                    let agent_end_error = value
                                        .get("error")
                                        .and_then(Value::as_str)
                                        .map(str::to_string);
                                    if agent_end_error.as_deref() == Some("Retry aborted") {
                                        break (timeline, agent_end_error);
                                    }
                                }
                            }
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                tracing::warn!(
                                    "prompt(cancel-inherit): output channel disconnected"
                                );
                                break (timeline, None);
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => {
                                asupersync::time::sleep(
                                    asupersync::time::wall_now(),
                                    Duration::from_millis(5),
                                )
                                .await;
                            }
                        }
                    }
                };
                futures::pin_mut!(retry_abort_wait);
                let (timeline, last_agent_end_error) = asupersync::time::timeout(
                    asupersync::time::wall_now(),
                    Duration::from_secs(5),
                    retry_abort_wait,
                )
                .await
                .expect("cancelled prompt should finish before timeout");

                cancel_thread.join().expect("cancel thread join");
                let retry_start_idx = timeline
                    .iter()
                    .position(|kind| kind == "auto_retry_start")
                    .expect("missing auto_retry_start");
                let retry_end_idx = timeline
                    .iter()
                    .position(|kind| kind == "auto_retry_end")
                    .expect("missing auto_retry_end");
                let agent_end_idx = timeline
                    .iter()
                    .rposition(|kind| kind == "agent_end")
                    .expect("missing agent_end");
                assert!(
                    retry_start_idx < retry_end_idx && retry_end_idx < agent_end_idx,
                    "unexpected retry timeline ordering: {timeline:?}"
                );
                assert_eq!(
                    last_agent_end_error.as_deref(),
                    Some("Retry aborted"),
                    "expected retry-abort terminal error, timeline: {timeline:?}"
                );

                drop(in_tx);
            };

            let (server_result, ()) =
                futures::future::join(run(agent_session, options, in_rx, out_tx), client).await;
            assert!(server_result.is_ok(), "rpc server error: {server_result:?}");
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn run_bash_rpc_cancelled_context_kills_process_tree() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().expect("tempdir");
            let marker = tmp.path().join("leaked_child.txt");

            let ambient_cx = asupersync::Cx::for_testing();
            let cancel_cx = ambient_cx.clone();
            let _current = asupersync::Cx::set_current(Some(ambient_cx));

            let cancel_thread = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(100));
                cancel_cx.set_cancel_requested(true);
            });

            let (_abort_tx, abort_rx) = oneshot::channel();
            let result = run_bash_rpc(
                tmp.path(),
                "(sleep 3; echo leaked > leaked_child.txt) & sleep 10",
                abort_rx,
            )
            .await
            .expect("cancelled rpc bash should return a result");

            cancel_thread.join().expect("cancel thread");

            assert!(
                result.cancelled,
                "expected cancelled rpc bash result: {result:?}"
            );

            std::thread::sleep(std::time::Duration::from_secs(4));
            assert!(
                !marker.exists(),
                "background child was not terminated on RPC cancellation"
            );
        });
    }

    struct RpcFailingReader {
        responses: std::collections::VecDeque<std::io::Result<Vec<u8>>>,
    }

    impl RpcFailingReader {
        fn new(responses: impl IntoIterator<Item = std::io::Result<Vec<u8>>>) -> Self {
            Self {
                responses: responses.into_iter().collect(),
            }
        }
    }

    impl std::io::Read for RpcFailingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.responses.pop_front().unwrap_or_else(|| Ok(Vec::new())) {
                Ok(bytes) => {
                    assert!(
                        bytes.len() <= buf.len(),
                        "test reader only supports single-chunk reads"
                    );
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                Err(err) => Err(err),
            }
        }
    }

    #[test]
    fn run_bash_rpc_large_output_completes_without_deadlock() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().expect("tempdir");
            let (_abort_tx, abort_rx) = oneshot::channel();
            let run = run_bash_rpc(tmp.path(), "yes x | head -c 1200000", abort_rx);

            let result = asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_secs(15),
                Box::pin(run),
            )
            .await
            .expect("rpc bash timed out; possible stdout/stderr reader deadlock")
            .expect("rpc bash should succeed");

            assert_eq!(result.exit_code, 0, "expected successful shell exit");
            assert!(
                result.truncated,
                "large RPC bash output should truncate instead of blocking"
            );
        });
    }

    #[test]
    fn run_bash_rpc_pump_stream_emits_io_error_frame_after_partial_output() {
        let reader = RpcFailingReader::new([
            Ok(b"partial stdout".to_vec()),
            Err(std::io::Error::other("simulated stdout failure")),
        ]);
        let (tx, rx) = std::sync::mpsc::sync_channel::<BashRpcStreamFrame>(4);

        pump_bash_rpc_stream(reader, tx, "stdout");

        match rx.recv().expect("partial chunk") {
            BashRpcStreamFrame::Chunk(chunk) => assert_eq!(chunk, b"partial stdout"),
            BashRpcStreamFrame::Error(message) => {
                unreachable!("expected output chunk before error, got error frame: {message}")
            }
        }

        match rx.recv().expect("io error frame") {
            BashRpcStreamFrame::Chunk(chunk) => {
                unreachable!("expected io error after partial chunk, got chunk: {chunk:?}")
            }
            BashRpcStreamFrame::Error(message) => {
                assert!(message.contains("Failed to read bash stdout"));
                assert!(message.contains("simulated stdout failure"));
            }
        }

        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty | std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn run_bash_rpc_ingest_frame_returns_pipe_read_error() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().expect("tempdir");
            let spill_path = tmp.path().join("partial-rpc-bash.log");
            std::fs::write(&spill_path, b"partial output").expect("write spill file");

            let mut chunks = VecDeque::new();
            let mut chunks_bytes = 0usize;
            let mut total_bytes = 0usize;
            let mut total_lines = 0usize;
            let mut last_byte_was_newline = false;
            let mut temp_file = None;
            let mut temp_file_path = Some(spill_path.clone());
            let mut spill_failed = false;

            ingest_bash_rpc_frame(
                BashRpcStreamFrame::Chunk(b"partial stderr".to_vec()),
                &mut chunks,
                &mut chunks_bytes,
                &mut total_bytes,
                &mut total_lines,
                &mut last_byte_was_newline,
                &mut temp_file,
                &mut temp_file_path,
                &mut spill_failed,
                DEFAULT_MAX_BYTES,
            )
            .await
            .expect("partial output should ingest");

            let err = ingest_bash_rpc_frame(
                BashRpcStreamFrame::Error(
                    "Failed to read bash stderr: simulated stderr failure".to_string(),
                ),
                &mut chunks,
                &mut chunks_bytes,
                &mut total_bytes,
                &mut total_lines,
                &mut last_byte_was_newline,
                &mut temp_file,
                &mut temp_file_path,
                &mut spill_failed,
                DEFAULT_MAX_BYTES,
            )
            .await
            .expect_err("pipe read failures must surface as errors");

            let message = err.to_string();
            assert!(message.contains("Failed to read bash stderr"));
            assert!(message.contains("simulated stderr failure"));
            assert!(message.contains("Partial output before failure"));
            assert!(message.contains("partial stderr"));
            assert!(spill_failed);
            assert!(temp_file.is_none());
            assert!(temp_file_path.is_none());
            assert_eq!(total_bytes, "partial stderr".len());
            assert!(
                !spill_path.exists(),
                "errored RPC spill files should be discarded"
            );
        });
    }

    #[test]
    fn rpc_spill_file_abandon_clears_path_and_unlinks_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let spill_path = tmp.path().join("partial-rpc-bash.log");
        std::fs::write(&spill_path, b"partial output").expect("write spill file");

        let mut temp_file = None;
        let mut temp_file_path = Some(spill_path.clone());
        let mut spill_failed = false;

        abandon_bash_rpc_spill_file(&mut temp_file, &mut temp_file_path, &mut spill_failed);

        assert!(spill_failed);
        assert!(temp_file.is_none());
        assert!(temp_file_path.is_none());
        assert!(
            !spill_path.exists(),
            "abandoned RPC spill files should not be left behind"
        );
    }

    #[test]
    fn rpc_spill_file_hard_limit_abandons_partial_spill_file() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().expect("tempdir");
            let spill_path = tmp.path().join("hard-limit-rpc-bash.log");
            std::fs::write(&spill_path, b"partial output").expect("write spill file");

            let spill_file = asupersync::fs::OpenOptions::new()
                .append(true)
                .open(&spill_path)
                .await
                .expect("open spill file");

            let mut chunks = VecDeque::new();
            let mut chunks_bytes = 0usize;
            let mut total_bytes = crate::tools::BASH_FILE_LIMIT_BYTES;
            let mut total_lines = 0usize;
            let mut last_byte_was_newline = false;
            let mut temp_file = Some(spill_file);
            let mut temp_file_path = Some(spill_path.clone());
            let mut spill_failed = false;

            ingest_bash_rpc_chunk(
                vec![b'x'],
                &mut chunks,
                &mut chunks_bytes,
                &mut total_bytes,
                &mut total_lines,
                &mut last_byte_was_newline,
                &mut temp_file,
                &mut temp_file_path,
                &mut spill_failed,
                DEFAULT_MAX_BYTES,
            )
            .await;

            assert!(spill_failed);
            assert!(temp_file.is_none());
            assert!(temp_file_path.is_none());
            assert!(
                !spill_path.exists(),
                "hard-limit RPC spill files must be discarded"
            );
        });
    }
}

fn should_auto_compact(tokens_before: u64, context_window: u32, reserve_tokens: u32) -> bool {
    let reserve = u64::from(reserve_tokens);
    let window = u64::from(context_window);
    tokens_before > window.saturating_sub(reserve)
}

#[allow(clippy::too_many_lines)]
async fn maybe_auto_compact(
    session: Arc<Mutex<AgentSession>>,
    options: RpcOptions,
    is_compacting: Arc<AtomicBool>,
    out_tx: std::sync::mpsc::SyncSender<String>,
) {
    // Safety net for panics/cancellation: never leak is_compacting stuck-true
    // (it would pin the stdin-EOF drain; gh #137). The caller may have
    // pre-claimed the flag before handing off, so clearing on drop is the
    // correct terminal state on every exit.
    let _compacting_guard = ClearFlagOnDrop(Arc::clone(&is_compacting));
    let cx = AgentCx::for_current_or_request();
    let (path_entries, context_window, reserve_tokens, settings) = {
        let Ok(guard) = OwnedMutexGuard::lock(Arc::clone(&session), cx.cx()).await else {
            return;
        };
        let (path_entries, context_window) = {
            let runtime_provider = guard.agent.provider().name().to_string();
            let runtime_model_id = guard.agent.provider().model_id().to_string();
            let Ok(mut inner_session) = guard.session.lock(cx.cx()).await else {
                return;
            };
            inner_session.ensure_entry_ids();
            let Some(entry) = current_or_runtime_model_entry(
                &inner_session,
                &runtime_provider,
                &runtime_model_id,
                &options,
            ) else {
                return;
            };
            let path_entries = inner_session
                .entries_for_current_path()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            (path_entries, entry.model.context_window)
        };

        let reserve_tokens = options.config.compaction_reserve_tokens();
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens,
            keep_recent_tokens: options.config.compaction_keep_recent_tokens(),
            ..Default::default()
        };

        (path_entries, context_window, reserve_tokens, settings)
    };

    let Some(prep) = prepare_compaction(&path_entries, settings) else {
        return;
    };
    if !should_auto_compact(prep.tokens_before, context_window, reserve_tokens) {
        return;
    }

    let _ = out_tx.send(agent_event(AgentEvent::AutoCompactionStart {
        reason: "threshold".to_string(),
    }));
    is_compacting.store(true, Ordering::SeqCst);

    // No interior `is_compacting.store(false)` on the exit paths below: the
    // ClearFlagOnDrop guard clears the flag at function exit, strictly AFTER
    // every event send, so the stdin-EOF drain cannot shut the writer down
    // between a clear and a trailing AutoCompactionEnd emission (gh #137).
    let (provider, key) = {
        let Ok(guard) = session.lock(cx.cx()).await else {
            return;
        };
        let Some(key) = guard.agent.stream_options().api_key.clone() else {
            let _ = out_tx.send(agent_event(AgentEvent::AutoCompactionEnd {
                result: None,
                aborted: false,
                will_retry: false,
                error_message: Some("Missing API key for compaction".to_string()),
            }));
            return;
        };
        (guard.agent.provider(), key)
    };

    let result = compact(prep, provider, &key, None).await;

    match result {
        Ok(result) => {
            let details_value = match compaction_details_to_value(&result.details) {
                Ok(value) => value,
                Err(err) => {
                    let _ = out_tx.send(agent_event(AgentEvent::AutoCompactionEnd {
                        result: None,
                        aborted: false,
                        will_retry: false,
                        error_message: Some(err.to_string()),
                    }));
                    return;
                }
            };

            let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(&session), cx.cx()).await else {
                return;
            };
            let messages = {
                let Ok(mut inner_session) = guard.session.lock(cx.cx()).await else {
                    return;
                };
                inner_session.append_compaction(
                    result.summary.clone(),
                    result.first_kept_entry_id.clone(),
                    result.tokens_before,
                    Some(details_value.clone()),
                    None,
                );
                inner_session.to_messages_for_current_path()
            };
            let _ = guard.persist_session().await;
            guard.agent.replace_messages(messages);
            drop(guard);

            let _ = out_tx.send(agent_event(AgentEvent::AutoCompactionEnd {
                result: Some(json!({
                    "summary": result.summary,
                    "firstKeptEntryId": result.first_kept_entry_id,
                    "tokensBefore": result.tokens_before,
                    "details": details_value,
                })),
                aborted: false,
                will_retry: false,
                error_message: None,
            }));
        }
        Err(err) => {
            let _ = out_tx.send(agent_event(AgentEvent::AutoCompactionEnd {
                result: None,
                aborted: false,
                will_retry: false,
                error_message: Some(err.to_string()),
            }));
        }
    }
}

fn rpc_model_from_entry(entry: &ModelEntry) -> Value {
    let input = entry
        .model
        .input
        .iter()
        .map(|t| match t {
            crate::provider::InputType::Text => "text",
            crate::provider::InputType::Image => "image",
        })
        .collect::<Vec<_>>();

    json!({
        "id": entry.model.id,
        "name": entry.model.name,
        "api": entry.model.api,
        "provider": entry.model.provider,
        "baseUrl": entry.model.base_url,
        "reasoning": entry.model.reasoning,
        "input": input,
        "contextWindow": entry.model.context_window,
        "maxTokens": entry.model.max_tokens,
        "cost": entry.model.cost,
    })
}

fn session_state(
    session: &crate::session::Session,
    options: &RpcOptions,
    snapshot: &RpcStateSnapshot,
    is_streaming: bool,
    is_compacting: bool,
) -> Value {
    let model = session
        .header
        .provider
        .as_deref()
        .zip(session.header.model_id.as_deref())
        .and_then(|(provider, model_id)| {
            options.available_models.iter().find(|m| {
                provider_ids_match(&m.model.provider, provider)
                    && m.model.id.eq_ignore_ascii_case(model_id)
            })
        })
        .map(rpc_model_from_entry);

    let message_count = session
        .entries_for_current_path()
        .iter()
        .filter(|entry| matches!(entry, crate::session::SessionEntry::Message(_)))
        .count();

    let session_name = session
        .entries_for_current_path()
        .iter()
        .rev()
        .find_map(|entry| {
            let crate::session::SessionEntry::SessionInfo(info) = entry else {
                return None;
            };
            info.name.clone()
        });

    let mut state = serde_json::Map::new();
    state.insert("model".to_string(), model.unwrap_or(Value::Null));
    state.insert(
        "thinkingLevel".to_string(),
        Value::String(
            session
                .header
                .thinking_level
                .clone()
                .unwrap_or_else(|| "off".to_string()),
        ),
    );
    state.insert("isStreaming".to_string(), Value::Bool(is_streaming));
    state.insert("isCompacting".to_string(), Value::Bool(is_compacting));
    state.insert(
        "steeringMode".to_string(),
        Value::String(snapshot.steering_mode.as_str().to_string()),
    );
    state.insert(
        "followUpMode".to_string(),
        Value::String(snapshot.follow_up_mode.as_str().to_string()),
    );
    state.insert(
        "sessionFile".to_string(),
        session
            .path
            .as_ref()
            .map_or(Value::Null, |p| Value::String(p.display().to_string())),
    );
    state.insert(
        "sessionId".to_string(),
        Value::String(session.header.id.clone()),
    );
    state.insert(
        "sessionName".to_string(),
        session_name.map_or(Value::Null, Value::String),
    );
    state.insert(
        "autoCompactionEnabled".to_string(),
        Value::Bool(snapshot.auto_compaction_enabled),
    );
    state.insert(
        "autoRetryEnabled".to_string(),
        Value::Bool(snapshot.auto_retry_enabled),
    );
    state.insert(
        "messageCount".to_string(),
        Value::Number(message_count.into()),
    );
    state.insert(
        "pendingMessageCount".to_string(),
        Value::Number(snapshot.pending_count().into()),
    );
    state.insert(
        "durabilityMode".to_string(),
        Value::String(session.autosave_durability_mode().as_str().to_string()),
    );
    Value::Object(state)
}

fn session_stats(session: &crate::session::Session) -> Value {
    let mut user_messages: u64 = 0;
    let mut assistant_messages: u64 = 0;
    let mut tool_results: u64 = 0;
    let mut tool_calls: u64 = 0;

    let mut total_input: u64 = 0;
    let mut total_output: u64 = 0;
    let mut total_cache_read: u64 = 0;
    let mut total_cache_write: u64 = 0;
    let mut total_cost: f64 = 0.0;

    let messages = session.to_messages_for_current_path();

    for message in &messages {
        match message {
            Message::User(_) | Message::Custom(_) => user_messages += 1,
            Message::Assistant(message) => {
                assistant_messages += 1;
                tool_calls += message
                    .content
                    .iter()
                    .filter(|block| matches!(block, ContentBlock::ToolCall(_)))
                    .count() as u64;
                total_input += message.usage.input;
                total_output += message.usage.output;
                total_cache_read += message.usage.cache_read;
                total_cache_write += message.usage.cache_write;
                total_cost += message.usage.cost.total;
            }
            Message::ToolResult(_) => tool_results += 1,
        }
    }

    let total_messages = messages.len() as u64;

    let total_tokens = total_input + total_output + total_cache_read + total_cache_write;
    let autosave = session.autosave_metrics();
    let pending_message_count = autosave.pending_mutations as u64;
    let durability_mode = session.autosave_durability_mode();
    let durability_mode_label = match durability_mode {
        crate::session::AutosaveDurabilityMode::Strict => "strict",
        crate::session::AutosaveDurabilityMode::Balanced => "balanced",
        crate::session::AutosaveDurabilityMode::Throughput => "throughput",
    };
    let (status_event, status_severity, status_summary, status_action, status_sli_ids) =
        if pending_message_count == 0 {
            (
                "session.persistence.healthy",
                "ok",
                "Persistence queue is clear.",
                "No action required.",
                vec!["sli_resume_ready_p95_ms"],
            )
        } else {
            let summary = match durability_mode {
                crate::session::AutosaveDurabilityMode::Strict => {
                    "Pending persistence backlog under strict durability mode."
                }
                crate::session::AutosaveDurabilityMode::Balanced => {
                    "Pending persistence backlog under balanced durability mode."
                }
                crate::session::AutosaveDurabilityMode::Throughput => {
                    "Pending persistence backlog under throughput durability mode."
                }
            };
            let action = match durability_mode {
                crate::session::AutosaveDurabilityMode::Throughput => {
                    "Expect deferred writes; trigger manual save before critical transitions."
                }
                _ => "Allow autosave flush to complete or trigger manual save before exit.",
            };
            (
                "session.persistence.backlog",
                "warning",
                summary,
                action,
                vec![
                    "sli_resume_ready_p95_ms",
                    "sli_failure_recovery_success_rate",
                ],
            )
        };

    let mut data = serde_json::Map::new();
    data.insert(
        "sessionFile".to_string(),
        session
            .path
            .as_ref()
            .map_or(Value::Null, |p| Value::String(p.display().to_string())),
    );
    data.insert(
        "sessionId".to_string(),
        Value::String(session.header.id.clone()),
    );
    data.insert(
        "userMessages".to_string(),
        Value::Number(user_messages.into()),
    );
    data.insert(
        "assistantMessages".to_string(),
        Value::Number(assistant_messages.into()),
    );
    data.insert("toolCalls".to_string(), Value::Number(tool_calls.into()));
    data.insert(
        "toolResults".to_string(),
        Value::Number(tool_results.into()),
    );
    data.insert(
        "totalMessages".to_string(),
        Value::Number(total_messages.into()),
    );
    data.insert(
        "durabilityMode".to_string(),
        Value::String(durability_mode_label.to_string()),
    );
    data.insert(
        "pendingMessageCount".to_string(),
        Value::Number(pending_message_count.into()),
    );
    data.insert(
        "tokens".to_string(),
        json!({
            "input": total_input,
            "output": total_output,
            "cacheRead": total_cache_read,
            "cacheWrite": total_cache_write,
            "total": total_tokens,
        }),
    );
    data.insert(
        "persistenceStatus".to_string(),
        json!({
            "event": status_event,
            "severity": status_severity,
            "summary": status_summary,
            "action": status_action,
            "sliIds": status_sli_ids,
            "pendingMessageCount": pending_message_count,
            "flushCounters": {
                "started": autosave.flush_started,
                "succeeded": autosave.flush_succeeded,
                "failed": autosave.flush_failed,
            },
        }),
    );
    data.insert(
        "uxEventMarkers".to_string(),
        json!([
            {
                "event": status_event,
                "severity": status_severity,
                "durabilityMode": durability_mode_label,
                "pendingMessageCount": pending_message_count,
                "sliIds": status_sli_ids,
            }
        ]),
    );
    data.insert("cost".to_string(), Value::from(total_cost));
    Value::Object(data)
}

fn last_assistant_text(session: &crate::session::Session) -> Option<String> {
    let entries = session.entries_for_current_path();
    for entry in entries.into_iter().rev() {
        let crate::session::SessionEntry::Message(msg_entry) = entry else {
            continue;
        };
        let SessionMessage::Assistant { message } = &msg_entry.message else {
            continue;
        };
        let mut text = String::new();
        for block in &message.content {
            if let ContentBlock::Text(t) = block {
                text.push_str(&t.text);
            }
        }
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

/// Export HTML from a lightweight `ExportSnapshot` (non-blocking path).
///
/// The snapshot is captured under a brief lock, so the HTML rendering and
/// file I/O happen entirely outside any session lock.
async fn export_html_snapshot(
    snapshot: &crate::session::ExportSnapshot,
    output_path: Option<&str>,
) -> Result<String> {
    let html = snapshot.to_html();

    let path = output_path.map_or_else(
        || {
            snapshot.path.as_ref().map_or_else(
                || {
                    let ts = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S%.3fZ");
                    PathBuf::from(format!("pi-session-{ts}.html"))
                },
                |session_path| {
                    let basename = session_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("session");
                    PathBuf::from(format!("pi-session-{basename}.html"))
                },
            )
        },
        PathBuf::from,
    );

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        asupersync::fs::create_dir_all(parent).await?;
    }
    asupersync::fs::write(&path, html).await?;
    Ok(path.display().to_string())
}

#[derive(Debug, Clone)]
struct BashRpcResult {
    output: String,
    exit_code: i32,
    cancelled: bool,
    truncated: bool,
    full_output_path: Option<String>,
}

enum BashRpcStreamFrame {
    Chunk(Vec<u8>),
    Error(String),
}

fn pump_bash_rpc_stream(
    mut reader: impl std::io::Read,
    tx: std::sync::mpsc::SyncSender<BashRpcStreamFrame>,
    stream_name: &'static str,
) {
    let mut buf = [0u8; 8192];
    loop {
        let read = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                let _ = tx.send(BashRpcStreamFrame::Error(format!(
                    "Failed to read bash {stream_name}: {err}"
                )));
                break;
            }
        };
        if tx
            .send(BashRpcStreamFrame::Chunk(buf[..read].to_vec()))
            .is_err()
        {
            break;
        }
    }
}

fn abandon_bash_rpc_spill_file(
    temp_file: &mut Option<asupersync::fs::File>,
    temp_file_path: &mut Option<PathBuf>,
    spill_failed: &mut bool,
) {
    *spill_failed = true;
    *temp_file = None;
    if let Some(path) = temp_file_path.take()
        && let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(
            "Failed to remove incomplete RPC bash spill file {}: {}",
            path.display(),
            e
        );
    }
}

const fn line_count_from_newline_count(
    total_bytes: usize,
    newline_count: usize,
    last_byte_was_newline: bool,
) -> usize {
    if total_bytes == 0 {
        0
    } else if last_byte_was_newline {
        newline_count
    } else {
        newline_count.saturating_add(1)
    }
}

async fn ingest_bash_rpc_chunk(
    bytes: Vec<u8>,
    chunks: &mut VecDeque<Vec<u8>>,
    chunks_bytes: &mut usize,
    total_bytes: &mut usize,
    total_lines: &mut usize,
    last_byte_was_newline: &mut bool,
    temp_file: &mut Option<asupersync::fs::File>,
    temp_file_path: &mut Option<PathBuf>,
    spill_failed: &mut bool,
    max_chunks_bytes: usize,
) {
    if bytes.is_empty() {
        return;
    }

    *last_byte_was_newline = bytes.last().is_some_and(|byte| *byte == b'\n');
    *total_bytes = total_bytes.saturating_add(bytes.len());
    *total_lines = total_lines.saturating_add(memchr_iter(b'\n', &bytes).count());

    // Spill to temp file if we exceed the limit
    if *total_bytes > DEFAULT_MAX_BYTES && temp_file.is_none() && !*spill_failed {
        let id_full = uuid::Uuid::new_v4().simple().to_string();
        let id = &id_full[..16];
        let path = std::env::temp_dir().join(format!("pi-rpc-bash-{id}.log"));

        // Secure synchronous creation
        let path_clone = path.clone();
        let expected_inode: Option<u64> =
            asupersync::runtime::spawn_blocking_io(move || -> std::io::Result<Option<u64>> {
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }

                match options.open(&path_clone) {
                    Ok(file) => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::MetadataExt;
                            Ok(file.metadata().ok().map(|m| m.ino()))
                        }
                        #[cfg(not(unix))]
                        {
                            drop(file);
                            Ok(None)
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to create bash temp file: {e}");
                        Ok(None)
                    }
                }
            })
            .await
            .unwrap_or(None);

        if expected_inode.is_some() || !cfg!(unix) {
            // Re-open async for writing
            match asupersync::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .await
            {
                Ok(mut file) => {
                    // Validate identity to prevent TOCTOU/symlink attacks
                    #[cfg_attr(not(unix), allow(unused_mut))]
                    let mut identity_match = true;
                    #[cfg(unix)]
                    if let Some(expected) = expected_inode {
                        use std::os::unix::fs::MetadataExt;
                        // asupersync 0.3.6's fs::Metadata no longer exposes the
                        // inode; re-stat the path with std symlink_metadata
                        // (does not follow symlinks) for the TOCTOU guard.
                        match std::fs::symlink_metadata(&path) {
                            Ok(meta) => {
                                if meta.ino() != expected {
                                    tracing::warn!(
                                        "Temp file identity mismatch (possible TOCTOU attack)"
                                    );
                                    identity_match = false;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to stat temp file: {e}");
                                identity_match = false;
                            }
                        }
                    }

                    if identity_match {
                        // Flush existing chunks to the new file
                        let mut failed_flush = false;
                        for existing in chunks.iter() {
                            use asupersync::io::AsyncWriteExt;
                            if let Err(e) = file.write_all(existing).await {
                                tracing::warn!("Failed to flush bash chunk to temp file: {e}");
                                failed_flush = true;
                                break;
                            }
                        }
                        *temp_file_path = Some(path);
                        if failed_flush {
                            abandon_bash_rpc_spill_file(temp_file, temp_file_path, spill_failed);
                        } else {
                            *temp_file = Some(file);
                        }
                    } else {
                        *temp_file_path = Some(path);
                        abandon_bash_rpc_spill_file(temp_file, temp_file_path, spill_failed);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to reopen bash temp file async: {e}");
                    *temp_file_path = Some(path);
                    abandon_bash_rpc_spill_file(temp_file, temp_file_path, spill_failed);
                }
            }
        } else {
            *spill_failed = true;
        }
    }

    // Write new chunk to file if we have one
    let mut abandon_spill_file = false;
    let mut close_spill_file = false;
    if let Some(file) = temp_file.as_mut() {
        if *total_bytes <= crate::tools::BASH_FILE_LIMIT_BYTES {
            use asupersync::io::AsyncWriteExt;
            if let Err(e) = file.write_all(&bytes).await {
                tracing::warn!("Failed to write bash chunk to temp file: {e}");
                abandon_spill_file = true;
            }
        } else {
            // Hard limit reached. Stop writing and close the file to release the FD.
            if !*spill_failed {
                tracing::warn!("Bash output exceeded hard limit; stopping file log");
                close_spill_file = true;
                *spill_failed = true;
            }
        }
    }
    if abandon_spill_file || close_spill_file {
        abandon_bash_rpc_spill_file(temp_file, temp_file_path, spill_failed);
    }

    // Update memory buffer
    *chunks_bytes = chunks_bytes.saturating_add(bytes.len());
    chunks.push_back(bytes);
    while *chunks_bytes > max_chunks_bytes && chunks.len() > 1 {
        if let Some(front) = chunks.pop_front() {
            *chunks_bytes = chunks_bytes.saturating_sub(front.len());
        }
    }
}

async fn ingest_bash_rpc_frame(
    frame: BashRpcStreamFrame,
    chunks: &mut VecDeque<Vec<u8>>,
    chunks_bytes: &mut usize,
    total_bytes: &mut usize,
    total_lines: &mut usize,
    last_byte_was_newline: &mut bool,
    temp_file: &mut Option<asupersync::fs::File>,
    temp_file_path: &mut Option<PathBuf>,
    spill_failed: &mut bool,
    max_chunks_bytes: usize,
) -> Result<()> {
    match frame {
        BashRpcStreamFrame::Chunk(bytes) => {
            ingest_bash_rpc_chunk(
                bytes,
                chunks,
                chunks_bytes,
                total_bytes,
                total_lines,
                last_byte_was_newline,
                temp_file,
                temp_file_path,
                spill_failed,
                max_chunks_bytes,
            )
            .await;
            Ok(())
        }
        BashRpcStreamFrame::Error(message) => {
            let error_message =
                bash_rpc_capture_error_message(&message, chunks, *total_bytes, *chunks_bytes);
            abandon_bash_rpc_spill_file(temp_file, temp_file_path, spill_failed);
            Err(Error::tool("bash", error_message))
        }
    }
}

fn bash_rpc_capture_error_message(
    message: &str,
    chunks: &VecDeque<Vec<u8>>,
    total_bytes: usize,
    chunks_bytes: usize,
) -> String {
    let mut raw = Vec::with_capacity(chunks_bytes);
    for chunk in chunks {
        raw.extend_from_slice(chunk);
    }
    if raw.is_empty() {
        return message.to_string();
    }

    let full_text = String::from_utf8_lossy(&raw).into_owned();
    let truncation = truncate_tail(full_text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut error_message = message.to_string();
    let partial_output = if truncation.content.is_empty() {
        "(no output)".to_string()
    } else {
        truncation.content
    };
    error_message.push_str("\n\nPartial output before failure:\n");
    error_message.push_str(&partial_output);
    if truncation.truncated || total_bytes > chunks_bytes {
        error_message.push_str("\n\n[Partial output truncated before failure]");
    }
    error_message
}

async fn run_bash_rpc(
    cwd: &std::path::Path,
    command: &str,
    mut abort_rx: oneshot::Receiver<()>,
) -> Result<BashRpcResult> {
    let shell = ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .unwrap_or("sh");

    let command = format!("trap 'code=$?; wait; exit $code' EXIT\n{command}");

    let mut child = std::process::Command::new(shell);
    child
        .arg("-c")
        .arg(&command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::tools::isolate_command_process_group(&mut child);
    let mut child = child
        .spawn()
        .map_err(|e| Error::tool("bash", format!("Failed to spawn shell: {e}")))?;

    let Some(stdout) = child.stdout.take() else {
        return Err(Error::tool("bash", "Missing stdout".to_string()));
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(Error::tool("bash", "Missing stderr".to_string()));
    };

    let mut guard =
        crate::tools::ProcessGuard::new(child, crate::tools::ProcessCleanupMode::ProcessGroupTree);

    // We use a bounded channel to provide backpressure. If the child process
    // produces output faster than the async loop can drain it (and spill to disk),
    // the pump threads will block on send(), which stops them from reading from the OS pipe.
    // The OS pipe buffer will fill up, causing the child's `write()` calls to block.
    // This correctly pauses the child until we catch up, preventing unbounded memory growth (OOM).
    let (tx, rx) = std::sync::mpsc::sync_channel::<BashRpcStreamFrame>(1024);
    let tx_stdout = tx.clone();
    let _stdout_handle =
        std::thread::spawn(move || pump_bash_rpc_stream(stdout, tx_stdout, "stdout"));
    let _stderr_handle = std::thread::spawn(move || pump_bash_rpc_stream(stderr, tx, "stderr"));

    let tick = Duration::from_millis(10);
    let cx = asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request);

    // Bounded buffer state (same logic as BashTool)
    let mut chunks: VecDeque<Vec<u8>> = VecDeque::new();
    let mut chunks_bytes = 0usize;
    let mut total_bytes = 0usize;
    let mut total_lines = 0usize;
    let mut last_byte_was_newline = false;
    let mut temp_file: Option<asupersync::fs::File> = None;
    let mut temp_file_path: Option<PathBuf> = None;
    let max_chunks_bytes = DEFAULT_MAX_BYTES * 2;

    let mut cancelled = false;
    let mut spill_failed = false;

    let exit_code = loop {
        while let Ok(frame) = rx.try_recv() {
            if let Err(err) = ingest_bash_rpc_frame(
                frame,
                &mut chunks,
                &mut chunks_bytes,
                &mut total_bytes,
                &mut total_lines,
                &mut last_byte_was_newline,
                &mut temp_file,
                &mut temp_file_path,
                &mut spill_failed,
                max_chunks_bytes,
            )
            .await
            {
                let _ = guard.kill();
                return Err(err);
            }
        }

        if !cancelled && abort_rx.try_recv().is_ok() {
            cancelled = true;
            let status_code = guard
                .kill()
                .map_or(-1, |status| status.code().unwrap_or(-1));
            break status_code;
        }

        match guard.try_wait_child() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => {}
            Err(err) => {
                return Err(Error::tool(
                    "bash",
                    format!("Failed to wait for process: {err}"),
                ));
            }
        }

        if cx.checkpoint().is_err() {
            cancelled = true;
            let _ = guard.kill();
            let status_code = -1;
            break status_code;
        }

        let now = cx.timer_driver().map_or_else(wall_now, |timer| timer.now());
        sleep(now, tick).await;
    };

    // Drain remaining output
    let now_drain = cx.timer_driver().map_or_else(wall_now, |timer| timer.now());
    let drain_deadline = now_drain + std::time::Duration::from_secs(2);
    let mut drain_timed_out = false;
    loop {
        match rx.try_recv() {
            Ok(frame) => {
                if let Err(err) = ingest_bash_rpc_frame(
                    frame,
                    &mut chunks,
                    &mut chunks_bytes,
                    &mut total_bytes,
                    &mut total_lines,
                    &mut last_byte_was_newline,
                    &mut temp_file,
                    &mut temp_file_path,
                    &mut spill_failed,
                    max_chunks_bytes,
                )
                .await
                {
                    let _ = guard.kill();
                    return Err(err);
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                let now = cx.timer_driver().map_or_else(wall_now, |timer| timer.now());
                if now >= drain_deadline {
                    drain_timed_out = true;
                    break;
                }
                if cx.checkpoint().is_err() {
                    cancelled = true;
                    break;
                }
                sleep(now, tick).await;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }

    // Drop the receiver to close the channel.
    // This ensures that any `tx.send()` calls in the pump threads return an error (Disconnected)
    // instead of blocking if the channel is full.
    // We intentionally do NOT join() the pump threads because if a background child process
    // inherits stdout/stderr, the pipe remains open and `read()` blocks indefinitely,
    // which would cause `join()` to hang the entire agent.
    drop(rx);

    // Explicitly reap the child to prevent zombie retention on macOS and other
    // platforms where the polling path can observe process exit before the
    // shell is fully reaped from its isolated process group.
    let _ = guard.wait();

    // Explicitly drop the temp file handle to ensure any buffered data is flushed to disk
    // before we potentially return the path to the caller.
    drop(temp_file);

    // Construct final output from memory buffer
    let mut combined = Vec::with_capacity(chunks_bytes);
    for chunk in chunks {
        combined.extend_from_slice(&chunk);
    }
    let tail_output = String::from_utf8_lossy(&combined).to_string();

    let mut truncation = truncate_tail(tail_output, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    if total_bytes > chunks_bytes {
        truncation.truncated = true;
        truncation.truncated_by = Some(crate::tools::TruncatedBy::Bytes);
        truncation.total_bytes = total_bytes;
        truncation.total_lines =
            line_count_from_newline_count(total_bytes, total_lines, last_byte_was_newline);
    } else if drain_timed_out {
        truncation.truncated = true;
        truncation.truncated_by = Some(crate::tools::TruncatedBy::Bytes);
    }
    let will_truncate = truncation.truncated;

    let mut output_text = if truncation.content.is_empty() {
        "(no output)".to_string()
    } else {
        truncation.content
    };

    if drain_timed_out {
        output_text.push_str("\n... [Output truncated: drain timeout]");
    }

    Ok(BashRpcResult {
        output: output_text,
        exit_code,
        cancelled,
        truncated: will_truncate,
        full_output_path: temp_file_path.map(|p| p.display().to_string()),
    })
}

fn parse_prompt_images(value: Option<&Value>) -> Result<Vec<ImageContent>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(arr) = value.as_array() else {
        return Err(Error::validation("images must be an array"));
    };

    let mut images = Vec::new();
    for item in arr {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let item_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        if item_type != "image" {
            continue;
        }
        let Some(source) = obj.get("source").and_then(Value::as_object) else {
            continue;
        };
        let source_type = source.get("type").and_then(Value::as_str).unwrap_or("");
        if source_type != "base64" {
            continue;
        }
        let Some(media_type) = source.get("mediaType").and_then(Value::as_str) else {
            continue;
        };
        let Some(data) = source.get("data").and_then(Value::as_str) else {
            continue;
        };
        images.push(ImageContent {
            data: data.to_string(),
            mime_type: media_type.to_string(),
        });
    }
    Ok(images)
}

fn resolve_model_key(
    cli_api_key: Option<&str>,
    auth: &AuthStorage,
    entry: &ModelEntry,
) -> Option<String> {
    cli_api_key
        .and_then(|key| {
            let trimmed = key.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .or_else(|| normalize_api_key_opt(auth.resolve_api_key(&entry.model.provider, None)))
        .or_else(|| normalize_api_key_opt(entry.api_key.clone()))
}

fn parse_thinking_level(level: &str) -> Result<crate::model::ThinkingLevel> {
    level.parse().map_err(|err: String| Error::validation(err))
}

fn session_thinking_level(
    session: &crate::session::Session,
) -> Option<crate::model::ThinkingLevel> {
    session
        .effective_thinking_level_for_current_path()
        .as_deref()
        .and_then(|raw| {
            raw.parse::<crate::model::ThinkingLevel>().map_or_else(
                |_| {
                    tracing::warn!("Ignoring invalid session thinking level in RPC state: {raw}");
                    None
                },
                Some,
            )
        })
}

fn current_model_entry<'a>(
    session: &crate::session::Session,
    options: &'a RpcOptions,
) -> Option<&'a ModelEntry> {
    let (provider, model_id) = session.effective_model_for_current_path()?;
    model_entry_for_provider_and_id(&provider, &model_id, options)
}

fn current_or_runtime_model_entry<'a>(
    session: &crate::session::Session,
    runtime_provider: &str,
    runtime_model_id: &str,
    options: &'a RpcOptions,
) -> Option<&'a ModelEntry> {
    current_model_entry(session, options)
        .or_else(|| model_entry_for_provider_and_id(runtime_provider, runtime_model_id, options))
}

fn model_entry_for_provider_and_id<'a>(
    provider: &str,
    model_id: &str,
    options: &'a RpcOptions,
) -> Option<&'a ModelEntry> {
    options.available_models.iter().find(|m| {
        provider_ids_match(&m.model.provider, provider) && m.model.id.eq_ignore_ascii_case(model_id)
    })
}

async fn apply_thinking_level(
    session: Arc<asupersync::sync::Mutex<AgentSession>>,
    level: crate::model::ThinkingLevel,
) -> Result<()> {
    let cx = AgentCx::for_current_or_request();
    let level_str = level.to_string();

    // Apply thinking level changes without holding lock across persist_session await
    {
        let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
            .await
            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;

        {
            let mut inner_session = guard
                .session
                .lock(cx.cx())
                .await
                .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
            let previous = session_thinking_level(&inner_session);
            inner_session.header.thinking_level = Some(level_str.clone());
            if previous != Some(level) {
                inner_session.append_thinking_level_change(level_str);
            }
        }
        guard.agent.stream_options_mut().thinking_level = Some(level);
    } // Drop guard here

    // Re-acquire guard just for persist_session
    let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    guard.persist_session().await
}

async fn apply_thinking_level_for_session(
    session: Arc<asupersync::sync::Mutex<AgentSession>>,
    level: crate::model::ThinkingLevel,
    cx: &AgentCx,
) -> Result<()> {
    // Apply thinking level changes without holding lock across persist_session await
    {
        let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), cx)
            .await
            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;

        let level_str = level.to_string();
        {
            let mut inner_session = guard
                .session
                .lock(cx.cx())
                .await
                .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
            let previous = session_thinking_level(&inner_session);
            inner_session.header.thinking_level = Some(level_str.clone());
            if previous != Some(level) {
                inner_session.append_thinking_level_change(level_str);
            }
        }
        guard.agent.stream_options_mut().thinking_level = Some(level);
    } // Drop guard here

    // Re-acquire guard just for persist_session to avoid holding lock across await
    let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    guard.persist_session().await
}

async fn apply_model_change(guard: &mut AgentSession, entry: &ModelEntry) -> Result<()> {
    let cx = AgentCx::for_current_or_request();
    {
        let mut inner_session = guard
            .session
            .lock(cx.cx())
            .await
            .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
        inner_session.header.provider = Some(entry.model.provider.clone());
        inner_session.header.model_id = Some(entry.model.id.clone());
        inner_session.append_model_change(entry.model.provider.clone(), entry.model.id.clone());
    }
    guard.persist_session().await
}

/// Extract user messages from a pre-captured list of session entries.
///
/// Used by the non-blocking `get_fork_messages` path where entries are
/// captured under a brief lock and messages are computed outside the lock.
fn fork_messages_from_entries(entries: &[crate::session::SessionEntry]) -> Vec<Value> {
    let mut result = Vec::new();

    for entry in entries {
        let crate::session::SessionEntry::Message(m) = entry else {
            continue;
        };
        let SessionMessage::User { content, .. } = &m.message else {
            continue;
        };
        let entry_id = m.base.id.clone().unwrap_or_default();
        let text = extract_user_text(content);
        result.push(json!({
            "entryId": entry_id,
            "text": text,
        }));
    }

    result
}

fn extract_user_text(content: &crate::model::UserContent) -> Option<String> {
    match content {
        crate::model::UserContent::Text(text) => Some(text.clone()),
        crate::model::UserContent::Blocks(blocks) => blocks.iter().find_map(|b| {
            if let ContentBlock::Text(t) = b {
                Some(t.text.clone())
            } else {
                None
            }
        }),
    }
}

/// Returns the available thinking levels for a model.
/// For reasoning models, returns the full range; for non-reasoning, returns only Off.
fn available_thinking_levels(entry: &ModelEntry) -> Vec<crate::model::ThinkingLevel> {
    use crate::model::ThinkingLevel;
    if entry.model.reasoning {
        let mut levels = vec![
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ];
        if entry.supports_xhigh() {
            levels.push(ThinkingLevel::XHigh);
        }
        if entry.supports_max() {
            levels.push(ThinkingLevel::Max);
        }
        levels
    } else {
        vec![ThinkingLevel::Off]
    }
}

/// Cycles through scoped models (if any) and returns the next model.
/// Returns (ModelEntry, ThinkingLevel, is_from_scoped_models).
async fn cycle_model_for_rpc(
    guard: &mut AgentSession,
    options: &RpcOptions,
) -> Result<Option<(ModelEntry, crate::model::ThinkingLevel, bool)>> {
    let (candidates, is_scoped) = if options.scoped_models.is_empty() {
        (options.available_models.clone(), false)
    } else {
        (
            options
                .scoped_models
                .iter()
                .map(|sm| sm.model.clone())
                .collect::<Vec<_>>(),
            true,
        )
    };

    if candidates.len() <= 1 {
        return Ok(None);
    }

    let cx = AgentCx::for_current_or_request();
    let runtime_provider = guard.agent.provider().name().to_string();
    let runtime_model_id = guard.agent.provider().model_id().to_string();
    let (current_provider, current_model_id) = {
        let inner_session = guard
            .session
            .lock(cx.cx())
            .await
            .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
        current_or_runtime_model_entry(
            &inner_session,
            &runtime_provider,
            &runtime_model_id,
            options,
        )
        .map_or_else(
            || {
                (
                    inner_session.header.provider.clone(),
                    inner_session.header.model_id.clone(),
                )
            },
            |entry| {
                (
                    Some(entry.model.provider.clone()),
                    Some(entry.model.id.clone()),
                )
            },
        )
    };

    let current_index = candidates.iter().position(|entry| {
        current_provider
            .as_deref()
            .is_some_and(|provider| provider_ids_match(provider, &entry.model.provider))
            && current_model_id
                .as_deref()
                .is_some_and(|model_id| model_id.eq_ignore_ascii_case(&entry.model.id))
    });

    let next_index = current_index.map_or(0, |idx| (idx + 1) % candidates.len());

    let next_entry = candidates[next_index].clone();
    let key = resolve_model_key(options.cli_api_key.as_deref(), &options.auth, &next_entry);
    if model_requires_configured_credential(&next_entry) && key.is_none() {
        return Err(Error::auth(format!(
            "Missing credentials for {}/{}",
            next_entry.model.provider, next_entry.model.id
        )));
    }

    let provider_impl = crate::providers::create_provider(
        &next_entry,
        guard
            .extensions
            .as_ref()
            .map(crate::extensions::ExtensionRegion::manager),
    )?;
    guard.agent.set_provider(provider_impl);

    guard.agent.stream_options_mut().api_key.clone_from(&key);
    guard
        .agent
        .stream_options_mut()
        .headers
        .clone_from(&next_entry.headers);

    apply_model_change(guard, &next_entry).await?;

    let desired_thinking = if is_scoped {
        options.scoped_models[next_index]
            .thinking_level
            .unwrap_or(crate::model::ThinkingLevel::Off)
    } else {
        guard
            .agent
            .stream_options()
            .thinking_level
            .unwrap_or_default()
    };

    let next_thinking = next_entry.clamp_thinking_level(desired_thinking);

    Ok(Some((next_entry, next_thinking, is_scoped)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentConfig};
    use crate::auth::AuthCredential;
    use crate::model::{
        AssistantMessage, AssistantMessageEvent, ContentBlock, ImageContent, Message, StopReason,
        TextContent, ThinkingLevel, ToolCall, Usage, UserContent, UserMessage,
    };
    use crate::package_manager::PackageManager;
    use crate::provider::{InputType, Model, ModelCost, Provider};
    use crate::resources::{ResourceCliOptions, ResourceLoader};
    use crate::session::Session;
    use crate::tools::ToolRegistry;
    use async_trait::async_trait;
    use futures::stream;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::Path;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::mpsc::{Receiver, TryRecvError};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const RPC_OUTPUT_PRESSURE_SCHEMA_V1: &str = "pi.rpc_output_pressure.v1";
    const RPC_OUTPUT_PRESSURE_DEBUG_BUDGET_US: u64 = 50_000;

    // -----------------------------------------------------------------------
    // Helper builders
    // -----------------------------------------------------------------------

    fn dummy_model(id: &str, reasoning: bool) -> Model {
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

    fn dummy_entry(id: &str, reasoning: bool) -> ModelEntry {
        ModelEntry {
            model: dummy_model(id, reasoning),
            api_key: None,
            headers: HashMap::new(),
            auth_header: false,
            compat: None,
            oauth_config: None,
        }
    }

    fn rpc_options_with_models(available_models: Vec<ModelEntry>) -> RpcOptions {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 1)
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        let auth_path = tempfile::tempdir()
            .expect("tempdir")
            .path()
            .join("auth.json");
        let auth = AuthStorage::load(auth_path).expect("auth load");

        RpcOptions {
            config: Config::default(),
            resources: ResourceLoader::empty(false),
            available_models,
            scoped_models: Vec::new(),
            cli_api_key: None,
            auth,
            runtime_handle,
        }
    }

    #[derive(Debug)]
    struct NoopProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for NoopProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &crate::provider::Context<'_>,
            _options: &crate::provider::StreamOptions,
        ) -> crate::error::Result<
            Pin<
                Box<
                    dyn futures::Stream<Item = crate::error::Result<crate::model::StreamEvent>>
                        + Send,
                >,
            >,
        > {
            let message = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            Ok(Box::pin(stream::iter(vec![
                Ok(crate::model::StreamEvent::Start {
                    partial: message.clone(),
                }),
                Ok(crate::model::StreamEvent::Done {
                    reason: StopReason::Stop,
                    message,
                }),
            ])))
        }
    }

    #[derive(Default)]
    struct RpcDeadlineProbeState {
        calls: std::sync::atomic::AtomicUsize,
        observed_deadlines: Mutex<Vec<Option<asupersync::Time>>>,
    }

    struct RpcDeadlineProbeProvider {
        state: Arc<RpcDeadlineProbeState>,
    }

    impl RpcDeadlineProbeProvider {
        fn assistant_message(&self) -> AssistantMessage {
            AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            }
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for RpcDeadlineProbeProvider {
        fn name(&self) -> &str {
            "deadline-probe"
        }

        fn api(&self) -> &str {
            "deadline-probe"
        }

        fn model_id(&self) -> &str {
            "deadline-probe-model"
        }

        async fn stream(
            &self,
            _context: &crate::provider::Context<'_>,
            _options: &crate::provider::StreamOptions,
        ) -> crate::error::Result<
            Pin<
                Box<
                    dyn futures::Stream<Item = crate::error::Result<crate::model::StreamEvent>>
                        + Send,
                >,
            >,
        > {
            self.state.calls.fetch_add(1, Ordering::SeqCst);
            let deadline = asupersync::Cx::current().and_then(|cx| cx.budget().deadline);
            self.state
                .observed_deadlines
                .lock()
                .expect("lock rpc deadline probe")
                .push(deadline);

            let message = self.assistant_message();
            Ok(Box::pin(stream::iter(vec![
                Ok(crate::model::StreamEvent::Start {
                    partial: message.clone(),
                }),
                Ok(crate::model::StreamEvent::Done {
                    reason: StopReason::Stop,
                    message,
                }),
            ])))
        }
    }

    fn build_test_agent_session_with_provider(
        session: Session,
        provider: Arc<dyn Provider>,
    ) -> AgentSession {
        let tools = ToolRegistry::new(&[], &std::env::current_dir().expect("current dir"), None);
        let agent = crate::agent::Agent::new(provider, tools, crate::agent::AgentConfig::default());
        let session = Arc::new(asupersync::sync::Mutex::new(session));
        AgentSession::new(
            agent,
            session,
            false,
            crate::compaction::ResolvedCompactionSettings::default(),
        )
    }

    fn build_test_agent_session(session: Session) -> AgentSession {
        let provider: Arc<dyn Provider> = Arc::new(NoopProvider);
        build_test_agent_session_with_provider(session, provider)
    }

    fn rpc_test_assistant_message(text: &str) -> Arc<AssistantMessage> {
        Arc::new(AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text.to_string()))],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "test-model".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        })
    }

    fn rpc_text_delta_event(full_text: &str, delta: &str) -> AgentEvent {
        let partial = rpc_test_assistant_message(full_text);
        AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: delta.to_string(),
                partial,
            },
        }
    }

    fn rpc_thinking_delta_event(full_text: &str, delta: &str) -> AgentEvent {
        let partial = rpc_test_assistant_message(full_text);
        AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: delta.to_string(),
                partial,
            },
        }
    }

    fn rpc_tool_call_delta_event(full_text: &str, delta: &str) -> AgentEvent {
        let partial = rpc_test_assistant_message(full_text);
        AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: delta.to_string(),
                partial,
            },
        }
    }

    fn rpc_text_start_event(full_text: &str) -> AgentEvent {
        let partial = rpc_test_assistant_message(full_text);
        AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::TextStart {
                content_index: 0,
                partial,
            },
        }
    }

    fn rpc_text_end_event(full_text: &str) -> AgentEvent {
        let partial = rpc_test_assistant_message(full_text);
        AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: full_text.to_string(),
                partial,
            },
        }
    }

    fn rpc_tool_call_end_event(full_text: &str) -> AgentEvent {
        let partial = rpc_test_assistant_message(full_text);
        AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: AssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call: ToolCall {
                    id: "tool-1".to_string(),
                    name: "bash".to_string(),
                    arguments: json!({ "cmd": "printf hi" }),
                    thought_signature: None,
                },
                partial,
            },
        }
    }

    fn rpc_tool_update_event(output: &str) -> AgentEvent {
        AgentEvent::ToolExecutionUpdate {
            tool_call_id: "tool-1".to_string(),
            tool_name: "bash".to_string(),
            args: json!({ "cmd": "printf hi" }),
            partial_result: crate::tools::ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(output.to_string()))],
                details: None,
                is_error: false,
            },
        }
    }

    fn rpc_tool_start_event() -> AgentEvent {
        AgentEvent::ToolExecutionStart {
            tool_call_id: "tool-1".to_string(),
            tool_name: "bash".to_string(),
            args: json!({ "cmd": "printf hi" }),
        }
    }

    fn rpc_tool_end_event(output: &str) -> AgentEvent {
        AgentEvent::ToolExecutionEnd {
            tool_call_id: "tool-1".to_string(),
            tool_name: "bash".to_string(),
            result: crate::tools::ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(output.to_string()))],
                details: None,
                is_error: false,
            },
            is_error: false,
        }
    }

    fn rpc_agent_end_event(final_text: &str) -> AgentEvent {
        let message = rpc_test_assistant_message(final_text);
        AgentEvent::AgentEnd {
            session_id: Arc::from("rpc-pressure-session"),
            messages: vec![Message::Assistant(message)],
            error: None,
        }
    }

    fn write_rpc_pressure_evidence(entry: &Value) {
        let path = std::env::var_os("PI_RPC_OUTPUT_PRESSURE_EVIDENCE")
            .filter(|path| !path.as_os_str().is_empty())
            .map_or_else(
                || {
                    let base = std::env::var_os("CARGO_TARGET_DIR")
                        .map(PathBuf::from)
                        .filter(|path| !path.as_os_str().is_empty())
                        .unwrap_or_else(|| {
                            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target")
                        });
                    base.join("perf").join("rpc_output_pressure.jsonl")
                },
                |path| {
                    let path = PathBuf::from(path);
                    if path.is_absolute() {
                        path
                    } else {
                        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
                    }
                },
            );

        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("create rpc pressure evidence dir");
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("rpc_output_pressure.jsonl");
        let temp_id = uuid::Uuid::new_v4();
        let temp_path = path.with_file_name(format!(".{file_name}.{temp_id}.tmp"));
        let serialized = format!(
            "{}\n",
            serde_json::to_string(entry).expect("serialize evidence")
        );
        let mut file =
            std::fs::File::create_new(&temp_path).expect("create rpc pressure evidence temp file");
        std::io::Write::write_all(&mut file, serialized.as_bytes())
            .expect("write rpc pressure evidence temp file");
        file.sync_all()
            .expect("sync rpc pressure evidence temp file");
        drop(file);
        std::fs::rename(&temp_path, &path).expect("persist rpc pressure evidence");
    }

    fn rpc_percentile_index(len: usize, numerator: usize, denominator: usize) -> usize {
        if len == 0 {
            return 0;
        }
        let rank = len
            .saturating_mul(numerator)
            .saturating_add(denominator.saturating_sub(1))
            / denominator.max(1);
        rank.saturating_sub(1).min(len - 1)
    }

    fn build_test_rpc_options(
        handle: &asupersync::runtime::RuntimeHandle,
        auth_path: PathBuf,
    ) -> RpcOptions {
        let auth = AuthStorage::load(auth_path).expect("load auth storage");
        RpcOptions {
            config: Config::default(),
            resources: ResourceLoader::empty(false),
            available_models: Vec::new(),
            scoped_models: Vec::new(),
            cli_api_key: None,
            auth,
            runtime_handle: handle.clone(),
        }
    }

    async fn load_test_prompt_template_resources(
        cwd: &Path,
        template_name: &str,
        content: &str,
    ) -> ResourceLoader {
        let prompt_path = cwd.join(format!("{template_name}.md"));
        std::fs::write(&prompt_path, content).expect("write prompt template");

        let manager = PackageManager::new(cwd.to_path_buf());
        let config = crate::config::Config::default();
        let cli = ResourceCliOptions {
            no_skills: true,
            no_prompt_templates: false,
            no_extensions: true,
            no_themes: true,
            skill_paths: Vec::new(),
            prompt_paths: vec![prompt_path.to_string_lossy().to_string()],
            extension_paths: Vec::new(),
            theme_paths: Vec::new(),
        };

        ResourceLoader::load(&manager, cwd, &config, &cli)
            .await
            .expect("load prompt template resources")
    }

    async fn build_queue_state_rpc_fixture(
        handle: &asupersync::runtime::RuntimeHandle,
        cwd: &Path,
    ) -> (AgentSession, RpcOptions) {
        let ext_entry_path = cwd.join("queue-state-ext.mjs");
        std::fs::write(&ext_entry_path, RPC_QUEUE_STATE_EXTENSION_EXT)
            .expect("write extension source");

        let mut agent_session = build_test_agent_session(Session::in_memory());
        agent_session
            .enable_extensions(&[], cwd, None, &[ext_entry_path])
            .await
            .expect("enable extensions");

        let mut options = build_test_rpc_options(handle, cwd.join("auth.json"));
        options.resources = load_test_prompt_template_resources(
            cwd,
            "report-queue-state",
            "Prompt template shadow that should not win.\n",
        )
        .await;

        (agent_session, options)
    }

    async fn recv_line(
        rx: &Arc<Mutex<Receiver<String>>>,
        label: &str,
    ) -> std::result::Result<String, String> {
        let start = Instant::now();
        loop {
            let recv_result = {
                let rx = rx.lock().expect("lock rpc output receiver");
                rx.try_recv()
            };

            match recv_result {
                Ok(line) => return Ok(line),
                Err(TryRecvError::Disconnected) => {
                    return Err(format!("{label}: output channel disconnected"));
                }
                Err(TryRecvError::Empty) => {}
            }

            if start.elapsed() > Duration::from_secs(10) {
                return Err(format!("{label}: timed out waiting for output"));
            }

            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5)).await;
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
                .unwrap_or_else(|err| unreachable!("{err}"));
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
                start.elapsed() <= Duration::from_secs(10),
                "{label}: timed out waiting for RPC response"
            );
        }
    }

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
            .unwrap_or_else(|_| unreachable!("send {label}"));
        recv_response(out_rx, label).await
    }

    fn assert_ok(resp: &Value, command: &str) {
        assert_eq!(resp["type"], "response", "response type for {command}");
        assert_eq!(resp["command"], command);
        assert_eq!(resp["success"], true, "success for {command}: {resp}");
    }

    fn assert_err(resp: &Value, command: &str) {
        assert_eq!(resp["type"], "response", "response type for {command}");
        assert_eq!(resp["command"], command);
        assert_eq!(
            resp["success"], false,
            "expected error for {command}: {resp}"
        );
    }

    async fn recv_ui_request(out_rx: &Arc<Mutex<Receiver<String>>>, label: &str) -> Value {
        let start = Instant::now();
        loop {
            let recv_result = {
                let rx = out_rx.lock().expect("lock rpc output receiver");
                rx.try_recv()
            };

            match recv_result {
                Ok(line) => {
                    if let Ok(val) = serde_json::from_str::<Value>(&line) {
                        if val.get("type").and_then(Value::as_str) == Some("extension_ui_request") {
                            return val;
                        }
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    unreachable!(
                        "{label}: output channel disconnected while waiting for extension_ui_request"
                    );
                }
                Err(TryRecvError::Empty) => {}
            }

            assert!(
                start.elapsed() <= Duration::from_secs(10),
                "{label}: timed out waiting for extension_ui_request"
            );
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5)).await;
        }
    }

    async fn wait_for_custom_message(
        in_tx: &asupersync::channel::mpsc::Sender<String>,
        out_rx: &Arc<Mutex<Receiver<String>>>,
        custom_type: &str,
        label: &str,
    ) -> Value {
        let start = Instant::now();
        let mut attempt = 0usize;

        loop {
            let response = send_recv(
                in_tx,
                out_rx,
                &format!(r#"{{"id":"poll-{attempt}","type":"get_messages"}}"#),
                label,
            )
            .await;
            let messages = response["data"]["messages"]
                .as_array()
                .expect("messages array");
            if let Some(message) = messages
                .iter()
                .find(|message| message["role"] == "custom" && message["customType"] == custom_type)
            {
                return message.clone();
            }

            assert!(
                start.elapsed() <= Duration::from_secs(10),
                "{label}: timed out waiting for custom message"
            );
            attempt = attempt.saturating_add(1);
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
        }
    }

    const RPC_BUSY_EXTENSION_COMMAND_EXT: &str = r#"
export default function init(pi) {
    pi.registerCommand("wait-confirm", {
        description: "Block until RPC confirms",
        handler: async () => {
            const confirmed = await pi.ui("confirm", {
                title: "Wait",
                message: "Hold the command open"
            });
            return confirmed ? "confirmed" : "cancelled";
        }
    });
}
"#;

    /// Like `wait-confirm`, but reports the UI outcome through a custom
    /// message event so tests can observe completion on the output stream
    /// even after stdin has closed (gh #137).
    const RPC_EOF_DRAIN_EXTENSION_EXT: &str = r#"
export default function init(pi) {
    pi.registerCommand("wait-confirm-report", {
        description: "Block until UI resolves, then report the outcome",
        handler: async () => {
            const confirmed = await pi.ui("confirm", {
                title: "Wait",
                message: "Hold the command open"
            });
            await pi.events("sendMessage", {
                message: {
                    customType: "eof-drain-outcome",
                    content: confirmed ? "confirmed" : "cancelled",
                    display: false
                },
                options: {
                    triggerTurn: false
                }
            });
            return "reported";
        }
    });
}
"#;

    const RPC_QUEUE_STATE_EXTENSION_EXT: &str = r#"
export default function init(pi) {
    pi.registerCommand("report-queue-state", {
        description: "Report queue modes visible to extensions",
        handler: async () => {
            const state = await pi.session("getState", {});
            await pi.events("sendMessage", {
                message: {
                    customType: "queue-state",
                    content: JSON.stringify({
                        steeringMode: state.steeringMode,
                        followUpMode: state.followUpMode
                    }),
                    display: false
                },
                options: {
                    triggerTurn: false
                }
            });
            return "reported";
        }
    });
}
"#;

    #[test]
    fn line_count_from_newline_count_matches_trailing_newline_semantics() {
        assert_eq!(line_count_from_newline_count(0, 0, false), 0);
        assert_eq!(line_count_from_newline_count(2, 1, true), 1);
        assert_eq!(line_count_from_newline_count(1, 0, false), 1);
        assert_eq!(line_count_from_newline_count(3, 1, false), 2);
    }

    // -----------------------------------------------------------------------
    // parse_queue_mode
    // -----------------------------------------------------------------------

    #[test]
    fn parse_queue_mode_all() {
        assert_eq!(parse_queue_mode(Some("all")), Some(QueueMode::All));
    }

    #[test]
    fn parse_queue_mode_one_at_a_time() {
        assert_eq!(
            parse_queue_mode(Some("one-at-a-time")),
            Some(QueueMode::OneAtATime)
        );
    }

    #[test]
    fn parse_queue_mode_none_value() {
        assert_eq!(parse_queue_mode(None), None);
    }

    #[test]
    fn parse_queue_mode_unknown_returns_none() {
        assert_eq!(parse_queue_mode(Some("batch")), None);
        assert_eq!(parse_queue_mode(Some("")), None);
    }

    #[test]
    fn parse_queue_mode_trims_whitespace() {
        assert_eq!(parse_queue_mode(Some("  all  ")), Some(QueueMode::All));
    }

    #[test]
    fn provider_ids_match_accepts_aliases() {
        assert!(provider_ids_match("openrouter", "open-router"));
        assert!(provider_ids_match("google-gemini-cli", "gemini-cli"));
        assert!(!provider_ids_match("openai", "anthropic"));
    }

    #[test]
    fn resolve_model_key_prefers_stored_auth_key_over_inline_entry_key() {
        let mut entry = dummy_entry("gpt-4o-mini", true);
        entry.model.provider = "openai".to_string();
        entry.auth_header = true;
        entry.api_key = Some("dummy-test-key-12345".to_string());

        let auth_path = tempfile::tempdir()
            .expect("tempdir")
            .path()
            .join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("auth load");
        auth.set(
            "openai".to_string(),
            AuthCredential::ApiKey {
                key: "stored-auth-key".to_string(),
            },
        );

        assert_eq!(
            resolve_model_key(None, &auth, &entry).as_deref(),
            Some("stored-auth-key")
        );
    }

    #[test]
    fn resolve_model_key_ignores_blank_inline_key_and_falls_back_to_auth_storage() {
        let mut entry = dummy_entry("gpt-4o-mini", true);
        entry.model.provider = "openai".to_string();
        entry.auth_header = true;
        entry.api_key = Some("   ".to_string()); // intentional blank space

        let auth_path = tempfile::tempdir()
            .expect("tempdir")
            .path()
            .join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("auth load");
        auth.set(
            "openai".to_string(),
            AuthCredential::ApiKey {
                key: "stored-auth-key".to_string(),
            },
        );

        assert_eq!(
            resolve_model_key(None, &auth, &entry).as_deref(),
            Some("stored-auth-key")
        );
    }

    #[test]
    fn resolve_model_key_prefers_cli_override_over_stored_and_inline_keys() {
        let mut entry = dummy_entry("gpt-4o-mini", true);
        entry.model.provider = "openai".to_string();
        entry.auth_header = true;
        entry.api_key = Some("inline-key".to_string());

        let temp = tempfile::tempdir().expect("tempdir");
        let auth_path = temp.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("auth load");
        auth.set(
            "openai".to_string(),
            AuthCredential::ApiKey {
                key: "stored-auth-key".to_string(),
            },
        );

        assert_eq!(
            resolve_model_key(Some("cli-override-key"), &auth, &entry).as_deref(),
            Some("cli-override-key")
        );
    }

    #[test]
    fn unknown_keyless_model_does_not_require_credentials() {
        let mut entry = dummy_entry("dev-model", false);
        entry.model.provider = "acme-local".to_string();
        entry.auth_header = false;
        entry.oauth_config = None;

        assert!(!model_requires_configured_credential(&entry));
    }

    #[test]
    fn anthropic_model_requires_credentials_even_without_auth_header() {
        let mut entry = dummy_entry("claude-sonnet-4-6", true);
        entry.model.provider = "anthropic".to_string();
        entry.auth_header = false;
        entry.oauth_config = None;

        assert!(model_requires_configured_credential(&entry));
    }

    // -----------------------------------------------------------------------
    // parse_streaming_behavior
    // -----------------------------------------------------------------------

    #[test]
    fn parse_streaming_behavior_steer() {
        let val = json!("steer");
        let result = parse_streaming_behavior(Some(&val)).unwrap();
        assert_eq!(result, Some(StreamingBehavior::Steer));
    }

    #[test]
    fn parse_streaming_behavior_follow_up_hyphenated() {
        let val = json!("follow-up");
        let result = parse_streaming_behavior(Some(&val)).unwrap();
        assert_eq!(result, Some(StreamingBehavior::FollowUp));
    }

    #[test]
    fn parse_streaming_behavior_follow_up_camel() {
        let val = json!("followUp");
        let result = parse_streaming_behavior(Some(&val)).unwrap();
        assert_eq!(result, Some(StreamingBehavior::FollowUp));
    }

    #[test]
    fn parse_streaming_behavior_follow_up_snake() {
        let val = json!("follow_up");
        let result = parse_streaming_behavior(Some(&val)).unwrap();
        assert_eq!(result, Some(StreamingBehavior::FollowUp));
    }

    #[test]
    fn parse_streaming_behavior_none() {
        let result = parse_streaming_behavior(None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn parse_streaming_behavior_invalid_string() {
        let val = json!("invalid");
        assert!(parse_streaming_behavior(Some(&val)).is_err());
    }

    #[test]
    fn parse_streaming_behavior_non_string_errors() {
        let val = json!(42);
        assert!(parse_streaming_behavior(Some(&val)).is_err());
    }

    #[test]
    fn streaming_behavior_field_accepts_snake_case_key() {
        let payload = json!({ "streaming_behavior": "follow_up" });
        let value = streaming_behavior_value(&payload).expect("streaming behavior value");
        let result = parse_streaming_behavior(Some(value)).unwrap();
        assert_eq!(result, Some(StreamingBehavior::FollowUp));
    }

    // -----------------------------------------------------------------------
    // parse_optional_u32_field
    // -----------------------------------------------------------------------

    #[test]
    fn parse_optional_u32_field_none() {
        let payload = json!({ "type": "compact" });
        let parsed = parse_optional_u32_field(&payload, "reserveTokens").unwrap();
        assert_eq!(parsed, None);
    }

    #[test]
    fn parse_optional_u32_field_valid() {
        let payload = json!({ "reserveTokens": 8192 });
        let parsed = parse_optional_u32_field(&payload, "reserveTokens").unwrap();
        assert_eq!(parsed, Some(8192));
    }

    #[test]
    fn parse_optional_u32_field_invalid_type() {
        let payload = json!({ "reserveTokens": "8192" });
        assert!(parse_optional_u32_field(&payload, "reserveTokens").is_err());
    }

    #[test]
    fn parse_optional_u32_field_too_large() {
        let payload = json!({ "reserveTokens": u64::from(u32::MAX) + 1 });
        assert!(parse_optional_u32_field(&payload, "reserveTokens").is_err());
    }

    // -----------------------------------------------------------------------
    // normalize_command_type
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_command_type_passthrough() {
        assert_eq!(normalize_command_type("prompt"), "prompt");
        assert_eq!(normalize_command_type("compact"), "compact");
    }

    #[test]
    fn normalize_command_type_follow_up_aliases() {
        assert_eq!(normalize_command_type("follow-up"), "follow_up");
        assert_eq!(normalize_command_type("followUp"), "follow_up");
        assert_eq!(normalize_command_type("queue-follow-up"), "follow_up");
        assert_eq!(normalize_command_type("queueFollowUp"), "follow_up");
    }

    #[test]
    fn normalize_command_type_kebab_and_camel_aliases() {
        assert_eq!(normalize_command_type("get-state"), "get_state");
        assert_eq!(normalize_command_type("getState"), "get_state");
        assert_eq!(normalize_command_type("set-model"), "set_model");
        assert_eq!(normalize_command_type("setModel"), "set_model");
        assert_eq!(
            normalize_command_type("set-steering-mode"),
            "set_steering_mode"
        );
        assert_eq!(
            normalize_command_type("setSteeringMode"),
            "set_steering_mode"
        );
        assert_eq!(
            normalize_command_type("set-follow-up-mode"),
            "set_follow_up_mode"
        );
        assert_eq!(
            normalize_command_type("setFollowUpMode"),
            "set_follow_up_mode"
        );
        assert_eq!(
            normalize_command_type("set-auto-compaction"),
            "set_auto_compaction"
        );
        assert_eq!(
            normalize_command_type("setAutoCompaction"),
            "set_auto_compaction"
        );
        assert_eq!(normalize_command_type("set-auto-retry"), "set_auto_retry");
        assert_eq!(normalize_command_type("setAutoRetry"), "set_auto_retry");
    }

    // -----------------------------------------------------------------------
    // build_user_message
    // -----------------------------------------------------------------------

    #[test]
    fn build_user_message_text_only() {
        let msg = build_user_message("hello", &[]);
        match msg {
            Message::User(UserMessage {
                content: UserContent::Text(text),
                ..
            }) => assert_eq!(text, "hello"),
            other => unreachable!("expected different match, got: {other:?}"),
        }
    }

    #[test]
    fn build_user_message_with_images() {
        let images = vec![ImageContent {
            data: "base64data".to_string(),
            mime_type: "image/png".to_string(),
        }];
        let msg = build_user_message("look at this", &images);
        match msg {
            Message::User(UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            }) => {
                assert_eq!(blocks.len(), 2);
                assert!(matches!(&blocks[0], ContentBlock::Text(_)));
                assert!(matches!(&blocks[1], ContentBlock::Image(_)));
            }
            other => unreachable!("expected different match, got: {other:?}"),
        }
    }

    #[test]
    fn build_user_message_image_only_omits_empty_text_block() {
        let images = vec![ImageContent {
            data: "base64data".to_string(),
            mime_type: "image/png".to_string(),
        }];
        let msg = build_user_message("", &images);
        match msg {
            Message::User(UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            }) => {
                assert_eq!(blocks.len(), 1);
                assert!(matches!(&blocks[0], ContentBlock::Image(_)));
            }
            other => unreachable!("expected different match, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // parse_extension_command_line
    // -----------------------------------------------------------------------

    #[test]
    fn parse_extension_command_line_parses_simple_command() {
        assert_eq!(
            parse_extension_command_line("/mycommand"),
            Some(("mycommand".to_string(), String::new()))
        );
    }

    #[test]
    fn parse_extension_command_line_preserves_arguments() {
        assert_eq!(
            parse_extension_command_line("/mycommand alpha beta"),
            Some(("mycommand".to_string(), "alpha beta".to_string()))
        );
    }

    #[test]
    fn parse_extension_command_line_requires_leading_slash() {
        assert_eq!(parse_extension_command_line("hello"), None);
    }

    #[test]
    fn parse_extension_command_line_accepts_leading_whitespace() {
        assert_eq!(
            parse_extension_command_line("  /cmd\targ"),
            Some(("cmd".to_string(), "arg".to_string()))
        );
    }

    #[test]
    fn parse_extension_command_line_rejects_blank_command_name() {
        assert_eq!(parse_extension_command_line("/   "), None);
    }

    #[test]
    fn rpc_busy_extension_command_rejects_follow_on_extension_prompt_without_blocking() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let cwd = temp.path().to_path_buf();
            let ext_entry_path = cwd.join("busy-ext.mjs");
            std::fs::write(&ext_entry_path, RPC_BUSY_EXTENSION_COMMAND_EXT)
                .expect("write extension source");

            let mut agent_session = build_test_agent_session(Session::in_memory());
            agent_session
                .enable_extensions(&[], &cwd, None, &[ext_entry_path])
                .await
                .expect("enable extensions");

            let options = build_test_rpc_options(&handle, cwd.join("auth.json"));
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(Mutex::new(out_rx));

            let server =
                handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

            let first = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"1","type":"prompt","message":"/wait-confirm"}"#,
                "prompt(wait-confirm:first)",
            )
            .await;
            assert_ok(&first, "prompt");

            let ui_event = recv_ui_request(&out_rx, "wait-confirm ui").await;
            assert_eq!(ui_event["method"], "confirm");
            let request_id = ui_event["id"]
                .as_str()
                .expect("ui request id should be a string")
                .to_string();

            let second = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"2","type":"prompt","message":"/wait-confirm"}"#,
                "prompt(wait-confirm:busy)",
            )
            .await;
            assert_err(&second, "prompt");
            assert_eq!(
                second["error"],
                "Extension commands are not allowed while agent is streaming"
            );

            let response = json!({
                "id": "3",
                "type": "extension_ui_response",
                "requestId": request_id,
                "confirmed": true,
            })
            .to_string();
            let ui_resp = send_recv(&in_tx, &out_rx, &response, "wait-confirm response").await;
            assert_ok(&ui_resp, "extension_ui_response");

            drop(in_tx);
            let result = server.await;
            assert!(result.is_ok(), "rpc server error: {result:?}");
        });
    }

    /// Closing stdin while a turn is in flight must drain the turn instead of
    /// tearing down and silently dropping it (gh #137: `printf '...' | pi
    /// --mode rpc` lost the entire event stream after the prompt ack). An
    /// extension command blocked on a UI request is the hardest variant: the
    /// UI answer can never arrive once stdin is closed, so the drain has to
    /// cancel the pending request for the command to complete at all.
    #[test]
    fn rpc_stdin_eof_drains_in_flight_turn_and_cancels_pending_ui() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let cwd = temp.path().to_path_buf();
            let ext_entry_path = cwd.join("eof-drain-ext.mjs");
            std::fs::write(&ext_entry_path, RPC_EOF_DRAIN_EXTENSION_EXT)
                .expect("write extension source");

            let mut agent_session = build_test_agent_session(Session::in_memory());
            agent_session
                .enable_extensions(&[], &cwd, None, &[ext_entry_path])
                .await
                .expect("enable extensions");
            let session_handle = Arc::clone(&agent_session.session);

            let options = build_test_rpc_options(&handle, cwd.join("auth.json"));
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(Mutex::new(out_rx));

            let server =
                handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

            let ack = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"1","type":"prompt","message":"/wait-confirm-report"}"#,
                "prompt(wait-confirm-report:eof)",
            )
            .await;
            assert_ok(&ack, "prompt");

            // The command is now mid-flight, blocked on the UI confirm.
            let ui_event = recv_ui_request(&out_rx, "wait-confirm-report ui before eof").await;
            assert_eq!(ui_event["method"], "confirm");

            // Simulate `printf ... | pi --mode rpc`: stdin closes immediately.
            drop(in_tx);

            // The server must drain the in-flight command (cancelling the
            // unanswerable UI request) rather than dropping the turn.
            let result = server.await;
            assert!(result.is_ok(), "rpc server error: {result:?}");

            // The handler resumed after the UI cancel and committed its
            // outcome report via `sendMessage` before returning, so the drain
            // guarantees the message is in the session by the time `run`
            // returns. Before the fix the command was dropped mid-flight and
            // no such message exists.
            let cx = AgentCx::for_request();
            let inner = OwnedMutexGuard::lock(session_handle, &cx)
                .await
                .expect("session lock");
            let saw_completion = inner.entries_for_current_path().iter().any(|entry| {
                matches!(
                    entry,
                    crate::session::SessionEntry::Message(msg)
                        if matches!(
                            &msg.message,
                            SessionMessage::Custom { custom_type, content, .. }
                                if custom_type.as_str() == "eof-drain-outcome"
                                    && content.as_str() == "cancelled"
                        )
                )
            });
            assert!(
                saw_completion,
                "extension command completion report should be committed before shutdown"
            );
        });
    }

    #[test]
    fn rpc_queue_mode_updates_reach_extension_session_state() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let cwd = temp.path().to_path_buf();
            let ext_entry_path = cwd.join("queue-state-ext.mjs");
            std::fs::write(&ext_entry_path, RPC_QUEUE_STATE_EXTENSION_EXT)
                .expect("write extension source");

            let mut agent_session = build_test_agent_session(Session::in_memory());
            agent_session
                .enable_extensions(&[], &cwd, None, &[ext_entry_path])
                .await
                .expect("enable extensions");

            let options = build_test_rpc_options(&handle, cwd.join("auth.json"));
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(Mutex::new(out_rx));

            let server =
                handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

            let steering = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"1","type":"set_steering_mode","mode":"all"}"#,
                "set_steering_mode(queue-state)",
            )
            .await;
            assert_ok(&steering, "set_steering_mode");

            let follow_up = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"2","type":"setFollowUpMode","mode":"all"}"#,
                "setFollowUpMode(queue-state)",
            )
            .await;
            assert_ok(&follow_up, "set_follow_up_mode");

            let prompt = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"3","type":"prompt","message":"/report-queue-state"}"#,
                "prompt(report-queue-state)",
            )
            .await;
            assert_ok(&prompt, "prompt");

            let message =
                wait_for_custom_message(&in_tx, &out_rx, "queue-state", "queue-state message")
                    .await;
            let reported_state: Value = serde_json::from_str(
                message["content"]
                    .as_str()
                    .expect("queue-state content should be string"),
            )
            .expect("queue-state content should be json");
            assert_eq!(reported_state["steeringMode"], "all");
            assert_eq!(reported_state["followUpMode"], "all");

            drop(in_tx);
            let result = server.await;
            assert!(result.is_ok(), "rpc server error: {result:?}");
        });
    }

    #[test]
    fn rpc_prompt_prefers_extension_command_over_prompt_template_name_collision() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let cwd = temp.path().to_path_buf();
            let (agent_session, options) = build_queue_state_rpc_fixture(&handle, &cwd).await;
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(Mutex::new(out_rx));

            let server =
                handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

            let prompt = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"1","type":"prompt","message":"/report-queue-state"}"#,
                "prompt(report-queue-state:shadowed)",
            )
            .await;
            assert_ok(&prompt, "prompt");

            let message =
                wait_for_custom_message(&in_tx, &out_rx, "queue-state", "queue-state shadowed")
                    .await;
            let reported_state: Value = serde_json::from_str(
                message["content"]
                    .as_str()
                    .expect("queue-state content should be string"),
            )
            .expect("queue-state content should be json");
            assert!(
                reported_state["steeringMode"].is_string(),
                "extension command should report steeringMode"
            );
            assert!(
                reported_state["followUpMode"].is_string(),
                "extension command should report followUpMode"
            );

            drop(in_tx);
            let result = server.await;
            assert!(result.is_ok(), "rpc server error: {result:?}");
        });
    }

    #[test]
    fn rpc_steer_rejects_extension_command_even_when_prompt_template_name_matches() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let cwd = temp.path().to_path_buf();
            let (agent_session, options) = build_queue_state_rpc_fixture(&handle, &cwd).await;
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(Mutex::new(out_rx));

            let server =
                handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

            let response = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"1","type":"steer","message":"/report-queue-state"}"#,
                "steer(report-queue-state:shadowed)",
            )
            .await;
            assert_err(&response, "steer");
            assert_eq!(
                response["error"],
                "Extension commands are not allowed with steer"
            );

            drop(in_tx);
            let result = server.await;
            assert!(result.is_ok(), "rpc server error: {result:?}");
        });
    }

    #[test]
    fn rpc_follow_up_rejects_extension_command_even_when_prompt_template_name_matches() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let cwd = temp.path().to_path_buf();
            let (agent_session, options) = build_queue_state_rpc_fixture(&handle, &cwd).await;
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(Mutex::new(out_rx));

            let server =
                handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

            let response = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"1","type":"follow_up","message":"/report-queue-state"}"#,
                "follow_up(report-queue-state:shadowed)",
            )
            .await;
            assert_err(&response, "follow_up");
            assert_eq!(
                response["error"],
                "Extension commands are not allowed with follow_up"
            );

            drop(in_tx);
            let result = server.await;
            assert!(result.is_ok(), "rpc server error: {result:?}");
        });
    }

    #[test]
    fn rpc_startup_queue_modes_reach_extension_session_state() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let cwd = temp.path().to_path_buf();
            let ext_entry_path = cwd.join("queue-state-ext.mjs");
            std::fs::write(&ext_entry_path, RPC_QUEUE_STATE_EXTENSION_EXT)
                .expect("write extension source");

            let mut agent_session = build_test_agent_session(Session::in_memory());
            agent_session
                .enable_extensions(&[], &cwd, None, &[ext_entry_path])
                .await
                .expect("enable extensions");

            let mut options = build_test_rpc_options(&handle, cwd.join("auth.json"));
            options.config.steering_mode = Some("all".to_string());
            options.config.follow_up_mode = Some("all".to_string());

            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(Mutex::new(out_rx));

            let server =
                handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

            let prompt = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"1","type":"prompt","message":"/report-queue-state"}"#,
                "prompt(report-queue-state-startup)",
            )
            .await;
            assert_ok(&prompt, "prompt");

            let message = wait_for_custom_message(
                &in_tx,
                &out_rx,
                "queue-state",
                "queue-state startup message",
            )
            .await;
            let reported_state: Value = serde_json::from_str(
                message["content"]
                    .as_str()
                    .expect("queue-state content should be string"),
            )
            .expect("queue-state content should be json");
            assert_eq!(reported_state["steeringMode"], "all");
            assert_eq!(reported_state["followUpMode"], "all");

            drop(in_tx);
            let result = server.await;
            assert!(result.is_ok(), "rpc server error: {result:?}");
        });
    }

    // -----------------------------------------------------------------------
    // try_send_line_with_backpressure
    // -----------------------------------------------------------------------

    #[test]
    fn try_send_line_with_backpressure_enqueues_when_capacity_available() {
        let (tx, _rx) = mpsc::channel::<String>(1);
        assert!(try_send_line_with_backpressure(&tx, "line".to_string()));
        assert!(matches!(
            tx.try_send("next".to_string()),
            Err(mpsc::SendError::Full(_))
        ));
    }

    #[test]
    fn try_send_line_with_backpressure_stops_when_receiver_closed() {
        let (tx, rx) = mpsc::channel::<String>(1);
        drop(rx);
        assert!(!try_send_line_with_backpressure(&tx, "line".to_string()));
    }

    #[test]
    fn try_send_line_with_backpressure_waits_until_capacity_is_available() {
        let (tx, mut rx) = mpsc::channel::<String>(1);
        tx.try_send("occupied".to_string())
            .expect("seed initial occupied slot");

        let expected = "delayed-line".to_string();
        let expected_for_thread = expected.clone();
        let recv_handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            let deadline = Instant::now() + Duration::from_millis(300);
            let mut received = Vec::new();
            while received.len() < 2 && Instant::now() < deadline {
                if let Ok(msg) = rx.try_recv() {
                    received.push(msg);
                } else {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            assert_eq!(received.len(), 2, "should receive both queued lines");
            let first = received.remove(0);
            let second = received.remove(0);
            assert_eq!(first, "occupied");
            assert_eq!(second, expected_for_thread);
        });

        assert!(try_send_line_with_backpressure(&tx, expected));
        drop(tx);
        recv_handle.join().expect("receiver thread should finish");
    }

    #[test]
    fn try_send_line_with_backpressure_preserves_large_payload() {
        let (tx, mut rx) = mpsc::channel::<String>(1);
        tx.try_send("busy".to_string())
            .expect("seed initial busy slot");

        let large = "x".repeat(256 * 1024);
        let large_for_thread = large.clone();
        let recv_handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            let deadline = Instant::now() + Duration::from_millis(500);
            let mut received = Vec::new();
            while received.len() < 2 && Instant::now() < deadline {
                if let Ok(msg) = rx.try_recv() {
                    received.push(msg);
                } else {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            assert_eq!(received.len(), 2, "should receive busy + payload lines");
            let payload = received.remove(1);
            assert_eq!(payload.len(), large_for_thread.len());
            assert_eq!(payload, large_for_thread);
        });

        assert!(try_send_line_with_backpressure(&tx, large));
        drop(tx);
        recv_handle.join().expect("receiver thread should finish");
    }

    #[test]
    fn try_send_line_with_backpressure_detects_disconnect_while_waiting() {
        let (tx, rx) = mpsc::channel::<String>(1);
        tx.try_send("busy".to_string())
            .expect("seed initial busy slot");

        let drop_handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            drop(rx);
        });

        assert!(
            !try_send_line_with_backpressure(&tx, "line-after-disconnect".to_string()),
            "send should stop after receiver disconnects while channel is full"
        );
        drop_handle.join().expect("drop thread should finish");
    }

    #[test]
    fn try_send_line_with_backpressure_high_volume_preserves_order_and_count() {
        let (tx, mut rx) = mpsc::channel::<String>(4);
        let lines: Vec<String> = (0..256)
            .map(|idx| format!("line-{idx:03}: {}", "x".repeat(64)))
            .collect();
        let expected = lines.clone();

        let recv_handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(4);
            let mut received = Vec::new();
            while received.len() < expected.len() && Instant::now() < deadline {
                if let Ok(msg) = rx.try_recv() {
                    received.push(msg);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(
                received.len(),
                expected.len(),
                "should receive every line under sustained backpressure"
            );
            assert_eq!(received, expected, "line ordering must remain stable");
        });

        for line in lines {
            assert!(try_send_line_with_backpressure(&tx, line));
        }
        drop(tx);
        recv_handle.join().expect("receiver thread should finish");
    }

    #[test]
    fn try_send_line_with_backpressure_preserves_partial_line_without_newline() {
        let (tx, mut rx) = mpsc::channel::<String>(1);
        tx.try_send("busy".to_string())
            .expect("seed initial busy slot");

        let partial_json = "{\"type\":\"prompt\",\"message\":\"tail-fragment-ascii\"".to_string();
        let expected = partial_json.clone();

        let recv_handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            let first = rx.try_recv().expect("seeded line should be available");
            assert_eq!(first, "busy");
            let deadline = Instant::now() + Duration::from_millis(500);
            let second = loop {
                if let Ok(line) = rx.try_recv() {
                    break line;
                }
                assert!(
                    Instant::now() < deadline,
                    "partial payload should be available"
                );
                std::thread::sleep(Duration::from_millis(5));
            };
            assert_eq!(second, expected);
        });

        assert!(try_send_line_with_backpressure(&tx, partial_json));
        drop(tx);
        recv_handle.join().expect("receiver thread should finish");
    }

    // -----------------------------------------------------------------------
    // RpcOutputPressureState
    // -----------------------------------------------------------------------

    #[test]
    fn rpc_output_pressure_classifies_only_low_value_updates_as_sheddable() {
        assert_eq!(
            rpc_output_pressure_class(&rpc_text_delta_event("hello", "o")),
            RpcOutputPressureClass::MessageDelta
        );
        assert_eq!(
            rpc_output_pressure_class(&rpc_tool_update_event("partial output")),
            RpcOutputPressureClass::ToolUpdate
        );
        assert_eq!(
            rpc_output_pressure_class(&rpc_agent_end_event("final")),
            RpcOutputPressureClass::Semantic
        );
    }

    #[test]
    fn rpc_output_pressure_conformance_matrix_classifies_event_families() {
        let user = Message::User(UserMessage {
            content: UserContent::Text("hello".to_string()),
            timestamp: 0,
        });
        let assistant = Message::Assistant(rpc_test_assistant_message("done"));
        let cases = vec![
            (
                "agent_start",
                AgentEvent::AgentStart {
                    session_id: Arc::from("rpc-pressure-session"),
                },
                RpcOutputPressureClass::Semantic,
            ),
            (
                "turn_start",
                AgentEvent::TurnStart {
                    session_id: Arc::from("rpc-pressure-session"),
                    turn_index: 0,
                    timestamp: 0,
                },
                RpcOutputPressureClass::Semantic,
            ),
            (
                "message_start",
                AgentEvent::MessageStart { message: user },
                RpcOutputPressureClass::Semantic,
            ),
            (
                "message_text_start",
                rpc_text_start_event("hello"),
                RpcOutputPressureClass::Semantic,
            ),
            (
                "message_text_delta",
                rpc_text_delta_event("hello", "o"),
                RpcOutputPressureClass::MessageDelta,
            ),
            (
                "message_thinking_delta",
                rpc_thinking_delta_event("thinking", "ink"),
                RpcOutputPressureClass::MessageDelta,
            ),
            (
                "message_tool_call_delta",
                rpc_tool_call_delta_event("tool", "{\"arg\""),
                RpcOutputPressureClass::MessageDelta,
            ),
            (
                "message_text_end",
                rpc_text_end_event("hello"),
                RpcOutputPressureClass::Semantic,
            ),
            (
                "message_tool_call_end",
                rpc_tool_call_end_event("tool"),
                RpcOutputPressureClass::Semantic,
            ),
            (
                "message_end",
                AgentEvent::MessageEnd {
                    message: assistant.clone(),
                },
                RpcOutputPressureClass::Semantic,
            ),
            (
                "tool_start",
                rpc_tool_start_event(),
                RpcOutputPressureClass::Semantic,
            ),
            (
                "tool_update",
                rpc_tool_update_event("partial output"),
                RpcOutputPressureClass::ToolUpdate,
            ),
            (
                "tool_end",
                rpc_tool_end_event("final output"),
                RpcOutputPressureClass::Semantic,
            ),
            (
                "turn_end",
                AgentEvent::TurnEnd {
                    session_id: Arc::from("rpc-pressure-session"),
                    turn_index: 0,
                    message: assistant,
                    tool_results: Vec::new(),
                    latency_breakdown: None,
                },
                RpcOutputPressureClass::Semantic,
            ),
            (
                "agent_end",
                rpc_agent_end_event("done"),
                RpcOutputPressureClass::Semantic,
            ),
            (
                "auto_compaction_start",
                AgentEvent::AutoCompactionStart {
                    reason: "manual".to_string(),
                },
                RpcOutputPressureClass::Semantic,
            ),
            (
                "auto_retry_start",
                AgentEvent::AutoRetryStart {
                    attempt: 1,
                    max_attempts: 3,
                    delay_ms: 10,
                    error_message: "temporary".to_string(),
                },
                RpcOutputPressureClass::Semantic,
            ),
            (
                "extension_error",
                AgentEvent::ExtensionError {
                    extension_id: Some("ext.test".to_string()),
                    event: "onAgentEvent".to_string(),
                    error: "failed".to_string(),
                },
                RpcOutputPressureClass::Semantic,
            ),
        ];

        let matrix = cases
            .iter()
            .map(|(name, event, expected)| {
                let actual = rpc_output_pressure_class(event);
                assert_eq!(actual, *expected, "unexpected class for {name}");
                json!({
                    "event": name,
                    "class": format!("{actual:?}"),
                    "expected": format!("{expected:?}"),
                    "verdict": "pass",
                })
            })
            .collect::<Vec<_>>();

        write_rpc_pressure_evidence(&json!({
            "schema": RPC_OUTPUT_PRESSURE_SCHEMA_V1,
            "case": "event_classification_conformance_matrix",
            "event_count": matrix.len(),
            "matrix": matrix,
            "coalescible_classes": ["MessageDelta", "ToolUpdate"],
            "semantic_preservation": "all non-delta lifecycle events are classified Semantic",
            "verdict": "pass",
        }));
    }

    #[test]
    fn rpc_output_pressure_coalesces_stream_deltas_without_blocking() {
        let (tx, _rx) = std::sync::mpsc::sync_channel::<String>(1);
        tx.try_send("occupied".to_string())
            .expect("seed occupied output slot");
        let mut pressure = RpcOutputPressureState::default();
        let mut latencies_us = Vec::new();

        for idx in 0..256 {
            let event = rpc_text_delta_event(&format!("full-{idx}"), &format!("delta-{idx}"));
            let serialized = agent_event(event.clone());
            let start = Instant::now();
            pressure.send_agent_event(&tx, &event, serialized);
            latencies_us.push(u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX));
        }

        let mut sorted = latencies_us.clone();
        sorted.sort_unstable();
        let p99_us = sorted[rpc_percentile_index(sorted.len(), 99, 100)];
        let snapshot = pressure.snapshot();
        assert_eq!(snapshot.pending, 1);
        assert_eq!(snapshot.message_deltas_coalesced, 256);
        assert_eq!(snapshot.tool_updates_coalesced, 0);
        assert!(
            p99_us <= RPC_OUTPUT_PRESSURE_DEBUG_BUDGET_US,
            "coalescable RPC updates should not block on a full output channel: p99={p99_us}us"
        );

        write_rpc_pressure_evidence(&json!({
            "schema": RPC_OUTPUT_PRESSURE_SCHEMA_V1,
            "case": "coalesce_stream_deltas_full_output_channel",
            "input_events": 256,
            "pending_events": snapshot.pending,
            "coalesced_message_delta_count": snapshot.message_deltas_coalesced,
            "coalesced_tool_update_count": snapshot.tool_updates_coalesced,
            "latency_budget_us": RPC_OUTPUT_PRESSURE_DEBUG_BUDGET_US,
            "p99_us": p99_us,
            "verdict": "pass",
            "semantic_events_preserved_by": "pending update flushed before next semantic event and final agent_end carries complete messages",
        }));
    }

    #[test]
    fn rpc_output_pressure_flushes_latest_update_before_semantic_event() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(1);
        tx.try_send("occupied".to_string())
            .expect("seed occupied output slot");
        let mut pressure = RpcOutputPressureState::default();

        let first = rpc_text_delta_event("first", "first");
        pressure.send_agent_event(&tx, &first, agent_event(first.clone()));
        let latest = rpc_text_delta_event("latest full content", "latest");
        pressure.send_agent_event(&tx, &latest, agent_event(latest.clone()));

        let recv_handle = std::thread::spawn(move || {
            let mut received = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(2);
            while received.len() < 3 && Instant::now() < deadline {
                match rx.try_recv() {
                    Ok(line) => received.push(line),
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
            received
        });

        let done = rpc_agent_end_event("latest full content");
        pressure.send_agent_event(&tx, &done, agent_event(done.clone()));
        drop(tx);

        let received = recv_handle.join().expect("receiver thread should finish");
        assert_eq!(
            received.first().map(String::as_str),
            Some("occupied"),
            "seeded output should remain first"
        );
        assert_eq!(
            received.len(),
            3,
            "latest update plus semantic end must flush"
        );

        let update: Value = serde_json::from_str(&received[1]).expect("parse flushed update");
        assert_eq!(update["type"], "message_update");
        assert_eq!(
            update["message"]["content"][0]["text"],
            "latest full content"
        );
        assert_eq!(
            update["assistantMessageEvent"]["delta"], "latest",
            "the newest live delta should replace stale pending deltas"
        );

        let done: Value = serde_json::from_str(&received[2]).expect("parse semantic end");
        assert_eq!(done["type"], "agent_end");
        assert_eq!(
            done["messages"][0]["content"][0]["text"],
            "latest full content"
        );
        assert_eq!(pressure.snapshot().pending, 0);
    }

    #[test]
    fn rpc_output_pressure_conformance_matrix_flushes_each_coalesced_class_before_semantic() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(1);
        tx.try_send("occupied".to_string())
            .expect("seed occupied output slot");
        let mut pressure = RpcOutputPressureState::default();

        let stale_delta = rpc_text_delta_event("first full text", "first");
        pressure.send_agent_event(&tx, &stale_delta, agent_event(stale_delta.clone()));
        let latest_delta = rpc_thinking_delta_event("latest thinking text", "latest-thinking");
        pressure.send_agent_event(&tx, &latest_delta, agent_event(latest_delta.clone()));
        let stale_tool = rpc_tool_update_event("first tool output");
        pressure.send_agent_event(&tx, &stale_tool, agent_event(stale_tool.clone()));
        let latest_tool = rpc_tool_update_event("latest tool output");
        pressure.send_agent_event(&tx, &latest_tool, agent_event(latest_tool.clone()));

        let before_flush = pressure.snapshot();
        assert_eq!(before_flush.pending, 2);
        assert_eq!(before_flush.message_deltas_coalesced, 2);
        assert_eq!(before_flush.tool_updates_coalesced, 2);

        let recv_handle = std::thread::spawn(move || {
            let mut received = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(2);
            while received.len() < 4 && Instant::now() < deadline {
                match rx.try_recv() {
                    Ok(line) => received.push(line),
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
            received
        });

        let semantic = rpc_agent_end_event("semantic final text");
        pressure.send_agent_event(&tx, &semantic, agent_event(semantic.clone()));
        drop(tx);

        let received = recv_handle.join().expect("receiver thread should finish");
        assert_eq!(
            received.first().map(String::as_str),
            Some("occupied"),
            "pre-existing output remains first"
        );
        assert_eq!(
            received.len(),
            4,
            "both pending classes and semantic event flush"
        );

        let message_update: Value =
            serde_json::from_str(&received[1]).expect("parse flushed message update");
        assert_eq!(message_update["type"], "message_update");
        assert_eq!(
            message_update["assistantMessageEvent"]["type"],
            "thinking_delta"
        );
        assert_eq!(
            message_update["assistantMessageEvent"]["delta"],
            "latest-thinking"
        );

        let tool_update: Value =
            serde_json::from_str(&received[2]).expect("parse flushed tool update");
        assert_eq!(tool_update["type"], "tool_execution_update");
        assert_eq!(
            tool_update["partialResult"]["content"][0]["text"],
            "latest tool output"
        );

        let agent_end: Value = serde_json::from_str(&received[3]).expect("parse semantic end");
        assert_eq!(agent_end["type"], "agent_end");
        assert_eq!(
            agent_end["messages"][0]["content"][0]["text"],
            "semantic final text"
        );

        let after_flush = pressure.snapshot();
        assert_eq!(after_flush.pending, 0);

        write_rpc_pressure_evidence(&json!({
            "schema": RPC_OUTPUT_PRESSURE_SCHEMA_V1,
            "case": "mixed_class_coalescing_flushes_before_semantic",
            "event_count": 5,
            "class_count": 3,
            "coalesced_count_by_class": {
                "MessageDelta": before_flush.message_deltas_coalesced,
                "ToolUpdate": before_flush.tool_updates_coalesced,
                "Semantic": 0,
            },
            "pending_class_count_before_semantic": before_flush.pending,
            "pending_class_count_after_semantic": after_flush.pending,
            "preserved_semantic_count": 1,
            "flushed_before_semantic": ["MessageDelta", "ToolUpdate"],
            "latency_budget_us": RPC_OUTPUT_PRESSURE_DEBUG_BUDGET_US,
            "verdict": "pass",
        }));
    }

    // -----------------------------------------------------------------------
    // RpcStateSnapshot::pending_count
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_pending_count() {
        let snapshot = RpcStateSnapshot {
            steering_count: 3,
            follow_up_count: 7,
            steering_mode: QueueMode::All,
            follow_up_mode: QueueMode::OneAtATime,
            auto_compaction_enabled: false,
            auto_retry_enabled: true,
        };
        assert_eq!(snapshot.pending_count(), 10);
    }

    #[test]
    fn snapshot_pending_count_zero() {
        let snapshot = RpcStateSnapshot {
            steering_count: 0,
            follow_up_count: 0,
            steering_mode: QueueMode::All,
            follow_up_mode: QueueMode::All,
            auto_compaction_enabled: false,
            auto_retry_enabled: false,
        };
        assert_eq!(snapshot.pending_count(), 0);
    }

    #[test]
    fn shared_state_blocks_follow_up_when_steering_queue_reaches_total_cap() {
        let config = Config::default();
        let mut shared = RpcSharedState::new(&config);

        for idx in 0..MAX_RPC_PENDING_MESSAGES {
            shared
                .push_steering(build_user_message(&format!("steer-{idx}"), &[]))
                .expect("steering enqueue within total cap");
        }

        let err = shared
            .push_follow_up(build_user_message("follow-up-overflow", &[]))
            .expect_err("follow-up enqueue should respect total pending cap");
        assert!(matches!(err, Error::Session(_)));
        assert_eq!(shared.pending_count(), MAX_RPC_PENDING_MESSAGES);
    }

    #[test]
    fn shared_state_blocks_steering_when_follow_up_queue_reaches_total_cap() {
        let config = Config::default();
        let mut shared = RpcSharedState::new(&config);

        for idx in 0..MAX_RPC_PENDING_MESSAGES {
            shared
                .push_follow_up(build_user_message(&format!("follow-up-{idx}"), &[]))
                .expect("follow-up enqueue within total cap");
        }

        let err = shared
            .push_steering(build_user_message("steer-overflow", &[]))
            .expect_err("steering enqueue should respect total pending cap");
        assert!(matches!(err, Error::Session(_)));
        assert_eq!(shared.pending_count(), MAX_RPC_PENDING_MESSAGES);
    }

    // -----------------------------------------------------------------------
    // retry_delay_ms
    // -----------------------------------------------------------------------

    #[test]
    fn retry_delay_first_attempt_is_base() {
        let config = Config::default();
        // attempt 0 and 1 should both use the base delay (shift = attempt - 1 saturating)
        assert_eq!(retry_delay_ms(&config, 0), config.retry_base_delay_ms());
        assert_eq!(retry_delay_ms(&config, 1), config.retry_base_delay_ms());
    }

    #[test]
    fn retry_delay_doubles_each_attempt() {
        let config = Config::default();
        let base = config.retry_base_delay_ms();
        // attempt 2: base * 2, attempt 3: base * 4
        assert_eq!(retry_delay_ms(&config, 2), base * 2);
        assert_eq!(retry_delay_ms(&config, 3), base * 4);
    }

    #[test]
    fn retry_delay_capped_at_max() {
        let config = Config::default();
        let max = config.retry_max_delay_ms();
        // Large attempt number should be capped
        let delay = retry_delay_ms(&config, 30);
        assert_eq!(delay, max);
    }

    #[test]
    fn retry_delay_saturates_on_overflow() {
        let config = Config::default();
        // u32::MAX attempt should not panic
        let delay = retry_delay_ms(&config, u32::MAX);
        assert!(delay <= config.retry_max_delay_ms());
    }

    // -----------------------------------------------------------------------
    // should_auto_compact
    // -----------------------------------------------------------------------

    #[test]
    fn auto_compact_below_threshold() {
        // 50k tokens used, 200k window, 40k reserve → threshold = 160k → no compact
        assert!(!should_auto_compact(50_000, 200_000, 40_000));
    }

    #[test]
    fn auto_compact_above_threshold() {
        // 170k tokens used, 200k window, 40k reserve → threshold = 160k → compact
        assert!(should_auto_compact(170_000, 200_000, 40_000));
    }

    #[test]
    fn auto_compact_exact_threshold() {
        // Exactly at threshold → not above → no compact
        assert!(!should_auto_compact(160_000, 200_000, 40_000));
    }

    #[test]
    fn auto_compact_reserve_exceeds_window() {
        // reserve > window → window - reserve saturates to 0 → any tokens > 0 triggers compact
        assert!(should_auto_compact(1, 100, 200));
    }

    #[test]
    fn auto_compact_zero_tokens() {
        assert!(!should_auto_compact(0, 200_000, 40_000));
    }

    #[test]
    fn auto_compaction_missing_api_key_omits_absent_result_field() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 1)
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let mut model = dummy_entry("test-model", false);
            model.model.provider = "test-provider".to_string();

            let agent = Agent::new(
                Arc::new(NoopProvider),
                ToolRegistry::new(&[], Path::new("."), None),
                AgentConfig::default(),
            );
            let mut inner_session = Session::in_memory();
            inner_session.header.provider = Some("test-provider".to_string());
            inner_session.header.model_id = Some("test-model".to_string());
            inner_session.append_message(crate::session::SessionMessage::User {
                content: UserContent::Text("older user turn".to_string()),
                timestamp: Some(0),
            });
            inner_session.append_message(crate::session::SessionMessage::Assistant {
                message: AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent::new("older assistant turn"))],
                    api: "test-api".to_string(),
                    provider: "test-provider".to_string(),
                    model: "test-model".to_string(),
                    usage: Usage {
                        total_tokens: 200_000,
                        ..Usage::default()
                    },
                    stop_reason: StopReason::Stop,
                    stop_details: None,
                    error_message: None,
                    timestamp: 0,
                },
            });
            inner_session.append_message(crate::session::SessionMessage::User {
                content: UserContent::Text("newer user turn".to_string()),
                timestamp: Some(0),
            });
            inner_session.append_message(crate::session::SessionMessage::Assistant {
                message: AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent::new("newer assistant turn"))],
                    api: "test-api".to_string(),
                    provider: "test-provider".to_string(),
                    model: "test-model".to_string(),
                    usage: Usage {
                        total_tokens: 250_000,
                        ..Usage::default()
                    },
                    stop_reason: StopReason::Stop,
                    stop_details: None,
                    error_message: None,
                    timestamp: 0,
                },
            });
            inner_session.append_message(crate::session::SessionMessage::User {
                content: UserContent::Text("recent".to_string()),
                timestamp: Some(0),
            });

            let agent_session = AgentSession::new(
                agent,
                Arc::new(asupersync::sync::Mutex::new(inner_session)),
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );

            let mut config = Config::default();
            config.compaction = Some(crate::config::CompactionSettings {
                enabled: Some(true),
                reserve_tokens: Some(2),
                keep_recent_tokens: Some(1),
            });

            let auth_dir = tempfile::tempdir().expect("tempdir");
            let auth = AuthStorage::load(auth_dir.path().join("auth.json")).expect("auth load");
            let options = RpcOptions {
                config,
                resources: ResourceLoader::empty(false),
                available_models: vec![model],
                scoped_models: Vec::new(),
                cli_api_key: None,
                auth,
                runtime_handle,
            };

            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(16);
            maybe_auto_compact(
                Arc::new(asupersync::sync::Mutex::new(agent_session)),
                options,
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
                out_tx,
            )
            .await;

            let events = out_rx
                .try_iter()
                .map(|line| serde_json::from_str::<Value>(&line).expect("event json"))
                .collect::<Vec<_>>();
            let start_idx = events
                .iter()
                .position(|event| event["type"] == "auto_compaction_start")
                .expect("auto_compaction_start");
            let end_idx = events
                .iter()
                .position(|event| event["type"] == "auto_compaction_end")
                .expect("auto_compaction_end");
            assert!(start_idx < end_idx, "unexpected event order: {events:?}");

            let end = &events[end_idx];
            assert_eq!(end["aborted"].as_bool(), Some(false));
            assert_eq!(end["willRetry"].as_bool(), Some(false));
            assert_eq!(end["errorMessage"], "Missing API key for compaction");
            assert!(
                end.get("result").is_none(),
                "failed auto_compaction_end must omit absent result: {end}"
            );
        });
    }

    // -----------------------------------------------------------------------
    // rpc_flatten_content_blocks
    // -----------------------------------------------------------------------

    #[test]
    fn flatten_content_blocks_unwraps_inner_0() {
        let mut value = json!({
            "content": [
                {"0": {"type": "text", "text": "hello"}}
            ]
        });
        rpc_flatten_content_blocks(&mut value);
        let blocks = value["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hello");
        assert!(blocks[0].get("0").is_none());
    }

    #[test]
    fn flatten_content_blocks_preserves_non_wrapped() {
        let mut value = json!({
            "content": [
                {"type": "text", "text": "already flat"}
            ]
        });
        rpc_flatten_content_blocks(&mut value);
        let blocks = value["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "already flat");
    }

    #[test]
    fn flatten_content_blocks_no_content_field() {
        let mut value = json!({"role": "assistant"});
        rpc_flatten_content_blocks(&mut value); // should not panic
        assert_eq!(value, json!({"role": "assistant"}));
    }

    #[test]
    fn flatten_content_blocks_non_object() {
        let mut value = json!("just a string");
        rpc_flatten_content_blocks(&mut value); // should not panic
    }

    #[test]
    fn flatten_content_blocks_existing_keys_not_overwritten() {
        // If a block already has a key that conflicts with inner "0", preserve outer
        let mut value = json!({
            "content": [
                {"type": "existing", "0": {"type": "inner", "extra": "data"}}
            ]
        });
        rpc_flatten_content_blocks(&mut value);
        let blocks = value["content"].as_array().unwrap();
        // "type" should keep the outer "existing" value, not be overwritten by inner "inner"
        assert_eq!(blocks[0]["type"], "existing");
        // "extra" from inner should be merged in
        assert_eq!(blocks[0]["extra"], "data");
    }

    // -----------------------------------------------------------------------
    // parse_prompt_images
    // -----------------------------------------------------------------------

    #[test]
    fn parse_prompt_images_none() {
        let images = parse_prompt_images(None).unwrap();
        assert!(images.is_empty());
    }

    #[test]
    fn parse_prompt_images_empty_array() {
        let val = json!([]);
        let images = parse_prompt_images(Some(&val)).unwrap();
        assert!(images.is_empty());
    }

    #[test]
    fn parse_prompt_images_valid() {
        let val = json!([{
            "type": "image",
            "source": {
                "type": "base64",
                "mediaType": "image/png",
                "data": "iVBORw0KGgo="
            }
        }]);
        let images = parse_prompt_images(Some(&val)).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[0].data, "iVBORw0KGgo=");
    }

    #[test]
    fn parse_prompt_images_skips_non_image_type() {
        let val = json!([{
            "type": "text",
            "text": "hello"
        }]);
        let images = parse_prompt_images(Some(&val)).unwrap();
        assert!(images.is_empty());
    }

    #[test]
    fn parse_prompt_images_skips_non_base64_source() {
        let val = json!([{
            "type": "image",
            "source": {
                "type": "url",
                "url": "https://example.com/img.png"
            }
        }]);
        let images = parse_prompt_images(Some(&val)).unwrap();
        assert!(images.is_empty());
    }

    #[test]
    fn parse_prompt_images_not_array_errors() {
        let val = json!("not-an-array");
        assert!(parse_prompt_images(Some(&val)).is_err());
    }

    #[test]
    fn parse_prompt_images_multiple_valid() {
        let val = json!([
            {
                "type": "image",
                "source": {"type": "base64", "mediaType": "image/jpeg", "data": "abc"}
            },
            {
                "type": "image",
                "source": {"type": "base64", "mediaType": "image/webp", "data": "def"}
            }
        ]);
        let images = parse_prompt_images(Some(&val)).unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].mime_type, "image/jpeg");
        assert_eq!(images[1].mime_type, "image/webp");
    }

    // -----------------------------------------------------------------------
    // extract_user_text
    // -----------------------------------------------------------------------

    #[test]
    fn extract_user_text_from_text_content() {
        let content = UserContent::Text("hello world".to_string());
        assert_eq!(extract_user_text(&content), Some("hello world".to_string()));
    }

    #[test]
    fn extract_user_text_from_blocks() {
        let content = UserContent::Blocks(vec![
            ContentBlock::Image(ImageContent {
                data: String::new(),
                mime_type: "image/png".to_string(),
            }),
            ContentBlock::Text(TextContent::new("found it")),
        ]);
        assert_eq!(extract_user_text(&content), Some("found it".to_string()));
    }

    #[test]
    fn extract_user_text_blocks_no_text() {
        let content = UserContent::Blocks(vec![ContentBlock::Image(ImageContent {
            data: String::new(),
            mime_type: "image/png".to_string(),
        })]);
        assert_eq!(extract_user_text(&content), None);
    }

    // -----------------------------------------------------------------------
    // parse_thinking_level
    // -----------------------------------------------------------------------

    #[test]
    fn parse_thinking_level_all_variants() {
        assert_eq!(parse_thinking_level("off").unwrap(), ThinkingLevel::Off);
        assert_eq!(parse_thinking_level("none").unwrap(), ThinkingLevel::Off);
        assert_eq!(parse_thinking_level("0").unwrap(), ThinkingLevel::Off);
        assert_eq!(
            parse_thinking_level("minimal").unwrap(),
            ThinkingLevel::Minimal
        );
        assert_eq!(parse_thinking_level("min").unwrap(), ThinkingLevel::Minimal);
        assert_eq!(parse_thinking_level("low").unwrap(), ThinkingLevel::Low);
        assert_eq!(parse_thinking_level("1").unwrap(), ThinkingLevel::Low);
        assert_eq!(
            parse_thinking_level("medium").unwrap(),
            ThinkingLevel::Medium
        );
        assert_eq!(parse_thinking_level("med").unwrap(), ThinkingLevel::Medium);
        assert_eq!(parse_thinking_level("2").unwrap(), ThinkingLevel::Medium);
        assert_eq!(parse_thinking_level("high").unwrap(), ThinkingLevel::High);
        assert_eq!(parse_thinking_level("3").unwrap(), ThinkingLevel::High);
        assert_eq!(parse_thinking_level("xhigh").unwrap(), ThinkingLevel::XHigh);
        assert_eq!(parse_thinking_level("4").unwrap(), ThinkingLevel::XHigh);
        assert_eq!(parse_thinking_level("max").unwrap(), ThinkingLevel::Max);
        assert_eq!(parse_thinking_level("5").unwrap(), ThinkingLevel::Max);
    }

    #[test]
    fn parse_thinking_level_case_insensitive() {
        assert_eq!(parse_thinking_level("HIGH").unwrap(), ThinkingLevel::High);
        assert_eq!(
            parse_thinking_level("Medium").unwrap(),
            ThinkingLevel::Medium
        );
        assert_eq!(parse_thinking_level("  Off  ").unwrap(), ThinkingLevel::Off);
    }

    #[test]
    fn parse_thinking_level_invalid() {
        assert!(parse_thinking_level("invalid").is_err());
        assert!(parse_thinking_level("").is_err());
        assert!(parse_thinking_level("6").is_err());
    }

    // -----------------------------------------------------------------------
    // supports_xhigh + clamp_thinking_level
    // -----------------------------------------------------------------------

    #[test]
    fn supports_xhigh_known_models() {
        assert!(dummy_entry("gpt-5.1-codex-max", true).supports_xhigh());
        assert!(dummy_entry("gpt-5.2", true).supports_xhigh());
        assert!(dummy_entry("gpt-5.4", true).supports_xhigh());
        assert!(dummy_entry("gpt-5.2-codex", true).supports_xhigh());
        assert!(dummy_entry("gpt-5.3-codex", true).supports_xhigh());
    }

    #[test]
    fn supports_xhigh_unknown_models() {
        assert!(!dummy_entry("claude-opus-4-6", true).supports_xhigh());
        assert!(!dummy_entry("gpt-4o", true).supports_xhigh());
        assert!(!dummy_entry("", true).supports_xhigh());
    }

    #[test]
    fn clamp_thinking_non_reasoning_model() {
        let entry = dummy_entry("claude-3-haiku", false);
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::High),
            ThinkingLevel::Off
        );
    }

    #[test]
    fn clamp_thinking_xhigh_without_support() {
        let entry = dummy_entry("claude-opus-4-6", true);
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::XHigh),
            ThinkingLevel::High
        );
    }

    #[test]
    fn clamp_thinking_xhigh_with_support() {
        let entry = dummy_entry("gpt-5.2", true);
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::XHigh),
            ThinkingLevel::XHigh
        );
    }

    #[test]
    fn clamp_thinking_normal_level_passthrough() {
        let entry = dummy_entry("claude-opus-4-6", true);
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::Medium),
            ThinkingLevel::Medium
        );
    }

    // -----------------------------------------------------------------------
    // available_thinking_levels
    // -----------------------------------------------------------------------

    #[test]
    fn available_thinking_levels_non_reasoning() {
        let entry = dummy_entry("gpt-4o-mini", false);
        let levels = available_thinking_levels(&entry);
        assert_eq!(levels, vec![ThinkingLevel::Off]);
    }

    #[test]
    fn available_thinking_levels_reasoning_no_xhigh() {
        let entry = dummy_entry("claude-opus-4-6", true);
        let levels = available_thinking_levels(&entry);
        assert_eq!(
            levels,
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ]
        );
    }

    #[test]
    fn available_thinking_levels_reasoning_with_xhigh() {
        let entry = dummy_entry("gpt-5.2", true);
        let levels = available_thinking_levels(&entry);
        assert_eq!(
            levels,
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::XHigh,
            ]
        );
    }

    // -----------------------------------------------------------------------
    // rpc_model_from_entry
    // -----------------------------------------------------------------------

    #[test]
    fn rpc_model_from_entry_basic() {
        let entry = dummy_entry("claude-opus-4-6", true);
        let value = rpc_model_from_entry(&entry);
        assert_eq!(value["id"], "claude-opus-4-6");
        assert_eq!(value["name"], "claude-opus-4-6");
        assert_eq!(value["provider"], "anthropic");
        assert_eq!(value["reasoning"], true);
        assert_eq!(value["contextWindow"], 200_000);
        assert_eq!(value["maxTokens"], 8192);
    }

    #[test]
    fn rpc_model_from_entry_input_types() {
        let mut entry = dummy_entry("gpt-4o", false);
        entry.model.input = vec![InputType::Text, InputType::Image];
        let value = rpc_model_from_entry(&entry);
        let input = value["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0], "text");
        assert_eq!(input[1], "image");
    }

    #[test]
    fn rpc_model_from_entry_cost_present() {
        let entry = dummy_entry("test-model", false);
        let value = rpc_model_from_entry(&entry);
        assert!(value.get("cost").is_some());
        let cost = &value["cost"];
        assert_eq!(cost["input"], 3.0);
        assert_eq!(cost["output"], 15.0);
    }

    #[test]
    fn current_model_entry_matches_provider_alias_and_model_case() {
        let mut model = dummy_entry("gpt-4o-mini", true);
        model.model.provider = "openrouter".to_string();
        let options = rpc_options_with_models(vec![model]);

        let mut session = Session::in_memory();
        session.header.provider = Some("open-router".to_string());
        session.header.model_id = Some("GPT-4O-MINI".to_string());

        let resolved = current_model_entry(&session, &options).expect("resolve aliased model");
        assert_eq!(resolved.model.provider, "openrouter");
        assert_eq!(resolved.model.id, "gpt-4o-mini");
    }

    #[test]
    fn current_or_runtime_model_entry_falls_back_when_header_is_unresolved() {
        let mut runtime = dummy_entry("test-model", false);
        runtime.model.provider = "test-provider".to_string();
        let options = rpc_options_with_models(vec![runtime]);

        let mut session = Session::in_memory();
        session.header.provider = Some("missing-provider".to_string());
        session.header.model_id = Some("missing-model".to_string());

        let resolved =
            current_or_runtime_model_entry(&session, "test-provider", "test-model", &options)
                .expect("resolve runtime fallback");
        assert_eq!(resolved.model.provider, "test-provider");
        assert_eq!(resolved.model.id, "test-model");
        assert_eq!(
            resolved.clamp_thinking_level(ThinkingLevel::High),
            ThinkingLevel::Off
        );
    }

    #[test]
    fn cycle_model_for_rpc_does_not_mutate_provider_when_credentials_are_missing() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 1)
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let mut current = dummy_entry("gpt-4o-mini", true);
            current.model.provider = "openai".to_string();
            current.model.api = "openai-completions".to_string();
            current.model.base_url = "https://api.openai.com/v1".to_string();
            current.auth_header = true;

            let next = ModelEntry {
                model: Model {
                    id: "cloud-model".to_string(),
                    name: "cloud-model".to_string(),
                    api: "openai-completions".to_string(),
                    provider: "acme-remote".to_string(),
                    base_url: "https://example.invalid/v1".to_string(),
                    reasoning: true,
                    input: vec![InputType::Text],
                    cost: ModelCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                    },
                    context_window: 128_000,
                    max_tokens: 8_192,
                    headers: HashMap::new(),
                },
                api_key: None,
                headers: HashMap::new(),
                auth_header: true,
                compat: None,
                oauth_config: None,
            };

            let provider =
                crate::providers::create_provider(&current, None).expect("create current provider");
            let agent = Agent::new(
                provider,
                ToolRegistry::new(&[], Path::new("."), None),
                AgentConfig::default(),
            );

            let mut session = Session::in_memory();
            session.header.provider = Some(current.model.provider.clone());
            session.header.model_id = Some(current.model.id.clone());
            let mut agent_session = AgentSession::new(
                agent,
                Arc::new(asupersync::sync::Mutex::new(session)),
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );

            let options = rpc_options_with_models(vec![current.clone(), next]);
            let err = cycle_model_for_rpc(&mut agent_session, &options)
                .await
                .expect_err("missing credentials should abort model cycling");
            assert!(
                err.to_string().contains("Missing credentials"),
                "unexpected error: {err}"
            );
            assert_eq!(
                agent_session.agent.provider().name(),
                current.model.provider
            );
            assert_eq!(agent_session.agent.provider().model_id(), current.model.id);

            let cx = AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(
                session.header.provider.as_deref(),
                Some(current.model.provider.as_str())
            );
            assert_eq!(
                session.header.model_id.as_deref(),
                Some(current.model.id.as_str())
            );
        });
    }

    #[test]
    fn cycle_model_for_rpc_uses_runtime_model_when_header_is_missing() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 1)
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let mut current = dummy_entry("test-model", false);
            current.model.provider = "test-provider".to_string();
            current.model.api = "test-api".to_string();
            current.model.base_url = "https://example.test/v1".to_string();

            let mut next = dummy_entry("after-runtime", true);
            next.api_key = Some("inline-next-key".to_string());
            let options = rpc_options_with_models(vec![current, next.clone()]);

            let mut agent_session = build_test_agent_session(Session::in_memory());
            let result = cycle_model_for_rpc(&mut agent_session, &options)
                .await
                .expect("cycle should succeed")
                .expect("should choose next model");

            assert_eq!(result.0.model.provider, next.model.provider);
            assert_eq!(result.0.model.id, next.model.id);
            assert_eq!(agent_session.agent.provider().name(), next.model.provider);
            assert_eq!(agent_session.agent.provider().model_id(), next.model.id);

            let cx = AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(
                session.header.provider.as_deref(),
                Some(next.model.provider.as_str())
            );
            assert_eq!(
                session.header.model_id.as_deref(),
                Some(next.model.id.as_str())
            );
        });
    }

    #[test]
    fn cycle_model_for_rpc_uses_cli_api_key_override_for_remote_model() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 1)
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let mut current = dummy_entry("test-model", false);
            current.model.provider = "test-provider".to_string();
            current.model.api = "test-api".to_string();
            current.model.base_url = "https://example.test/v1".to_string();
            current.auth_header = false;
            current.api_key = None;

            let mut next = dummy_entry("cloud-model", true);
            next.model.provider = "openai".to_string();
            next.model.api = "openai-completions".to_string();
            next.model.base_url = "https://api.openai.com/v1".to_string();
            next.auth_header = true;
            next.api_key = None;

            let mut options = rpc_options_with_models(vec![current, next.clone()]);
            options.cli_api_key = Some("cli-override-key".to_string());

            let mut agent_session = build_test_agent_session(Session::in_memory());
            let result = cycle_model_for_rpc(&mut agent_session, &options)
                .await
                .expect("cycle should succeed")
                .expect("should choose next model");

            assert_eq!(result.0.model.provider, next.model.provider);
            assert_eq!(result.0.model.id, next.model.id);
            assert_eq!(
                agent_session.agent.stream_options().api_key.as_deref(),
                Some("cli-override-key")
            );
        });
    }

    #[test]
    fn apply_thinking_level_inherits_cancelled_context_without_partial_mutation() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let agent_session = build_test_agent_session(Session::in_memory());
            let session_handle = Arc::new(asupersync::sync::Mutex::new(agent_session));
            let inner_session_handle = {
                let guard = session_handle
                    .lock(&AgentCx::for_request())
                    .await
                    .expect("session lock");
                Arc::clone(&guard.session)
            };
            let hold_cx = AgentCx::for_request();
            let held_guard = inner_session_handle
                .lock(hold_cx.cx())
                .await
                .expect("session lock");

            let ambient_cx = asupersync::Cx::for_testing();
            ambient_cx.set_cancel_requested(true);
            let _current = asupersync::Cx::set_current(Some(ambient_cx));

            let err = {
                let apply = apply_thinking_level(Arc::clone(&session_handle), ThinkingLevel::High);
                futures::pin_mut!(apply);
                let inner = asupersync::time::timeout(
                    asupersync::time::wall_now(),
                    Duration::from_millis(100),
                    apply,
                )
                .await;
                let outcome =
                    inner.expect("cancelled thinking helper should finish before timeout");
                outcome.expect_err("lock acquisition should honor inherited cancellation")
            };
            assert!(
                err.to_string().contains("session lock failed"),
                "unexpected error: {err}"
            );

            drop(held_guard);

            let verify_cx = AgentCx::for_request();
            let session_arc = {
                let guard = session_handle.lock(&verify_cx).await.expect("session lock");
                Arc::clone(&guard.session)
            };
            let session = session_arc
                .lock(verify_cx.cx())
                .await
                .expect("session lock");
            assert!(session.header.thinking_level.is_none());
            drop(session);
            let agent_thinking_level = {
                let guard = session_handle.lock(&verify_cx).await.expect("session lock");
                guard.agent.stream_options().thinking_level
            };
            assert!(agent_thinking_level.is_none());
        });
    }

    #[test]
    fn apply_thinking_level_canonicalizes_header_without_duplicate_history() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let mut session = Session::in_memory();
            session.header.thinking_level = Some("HIGH".to_string());
            let agent_session = build_test_agent_session(session);
            let session_handle = Arc::new(asupersync::sync::Mutex::new(agent_session));

            apply_thinking_level(Arc::clone(&session_handle), ThinkingLevel::High)
                .await
                .expect("apply thinking level");

            let verify_cx = AgentCx::for_request();
            let guard = session_handle.lock(&verify_cx).await.expect("session lock");
            let session = guard
                .session
                .lock(verify_cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.thinking_level.as_deref(), Some("high"));
            let thinking_changes = session
                .entries
                .iter()
                .filter(|entry| {
                    matches!(entry, crate::session::SessionEntry::ThinkingLevelChange(_))
                })
                .count();
            assert_eq!(thinking_changes, 0);
            drop(session);

            assert_eq!(
                guard.agent.stream_options().thinking_level,
                Some(ThinkingLevel::High)
            );
        });
    }

    #[test]
    fn rpc_set_model_persists_clamped_thinking_header_even_when_runtime_is_already_off() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let mut next = dummy_entry("llama3.2", false);
            next.model.provider = "ollama".to_string();
            next.model.api = "openai-completions".to_string();
            next.model.base_url = "http://127.0.0.1:11434/v1".to_string();

            let temp = tempfile::tempdir().expect("tempdir");
            let auth_path = temp.path().join("auth.json");
            let mut options = build_test_rpc_options(&handle, auth_path);
            options.available_models = vec![next.clone()];

            let agent_session = build_test_agent_session(Session::in_memory());
            let session_handle = Arc::clone(&agent_session.session);
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(8);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(Mutex::new(out_rx));

            let server =
                handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

            let response = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"1","type":"set_model","provider":"ollama","modelId":"llama3.2"}"#,
                "set_model(sync-thinking)",
            )
            .await;
            assert_ok(&response, "set_model");

            drop(in_tx);
            let result = server.await;
            assert!(result.is_ok(), "rpc server error: {result:?}");

            let verify_cx = AgentCx::for_request();
            let session = session_handle
                .lock(verify_cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("ollama"));
            assert_eq!(session.header.model_id.as_deref(), Some("llama3.2"));
            assert_eq!(session.header.thinking_level.as_deref(), Some("off"));
            let thinking_changes = session
                .entries
                .iter()
                .filter(|entry| {
                    matches!(entry, crate::session::SessionEntry::ThinkingLevelChange(_))
                })
                .count();
            assert_eq!(thinking_changes, 1);
        });
    }

    #[test]
    fn rpc_prompt_command_inherits_deadline_from_run() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let state = Arc::new(RpcDeadlineProbeState::default());
            let provider: Arc<dyn Provider> = Arc::new(RpcDeadlineProbeProvider {
                state: Arc::clone(&state),
            });
            let agent_session =
                build_test_agent_session_with_provider(Session::in_memory(), provider);

            let auth_path = tempfile::tempdir()
                .expect("tempdir")
                .path()
                .join("auth.json");
            let options = build_test_rpc_options(&runtime_handle, auth_path);

            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(Mutex::new(out_rx));

            let expected_deadline = asupersync::time::wall_now() + Duration::from_secs(30);
            let ambient_cx = AgentCx::for_request_with_budget(asupersync::Budget {
                deadline: Some(expected_deadline),
                ..asupersync::Budget::INFINITE
            });
            let _current = asupersync::Cx::set_current(Some(ambient_cx.cx().clone()));

            let client_out_rx = Arc::clone(&out_rx);
            let client = async move {
                let response = send_recv(
                    &in_tx,
                    &client_out_rx,
                    r#"{"id":"1","type":"prompt","message":"deadline please"}"#,
                    "prompt(deadline)",
                )
                .await;
                assert_eq!(response["command"], "prompt");
                assert_eq!(
                    response["success"], true,
                    "prompt should succeed under inherited deadline: {response}"
                );

                // Wait for the agent_end event to ensure the provider is actually called
                // before we drop the connection. Use timeout to prevent hanging.
                let mut saw_agent_end = false;
                for _ in 0..50 {
                    let Ok(msg) = client_out_rx
                        .lock()
                        .expect("lock rx")
                        .recv_timeout(Duration::from_secs(5))
                    else {
                        break;
                    };
                    if let Ok(json) = serde_json::from_str::<Value>(&msg) {
                        if json["type"] == "agent_end" {
                            saw_agent_end = true;
                            break;
                        }
                    }
                }
                assert!(saw_agent_end, "expected agent_end event before dropping");

                drop(in_tx);
            };

            let (server_result, ()) =
                futures::future::join(run(agent_session, options, in_rx, out_tx), client).await;
            assert!(server_result.is_ok(), "rpc server error: {server_result:?}");
            assert_eq!(state.calls.load(Ordering::SeqCst), 1);
            let deadlines = state
                .observed_deadlines
                .lock()
                .expect("lock rpc deadline probe")
                .clone();
            assert_eq!(deadlines.as_slice(), &[Some(expected_deadline)]);
        });
    }

    #[test]
    fn cycle_model_for_rpc_inherits_cancelled_context_when_session_lock_is_held() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let current = dummy_entry("current-model", true);
            let mut next = dummy_entry("next-model", true);
            next.api_key = Some("inline-next-key".to_string());

            let provider =
                crate::providers::create_provider(&current, None).expect("create current provider");
            let agent = Agent::new(
                provider,
                ToolRegistry::new(&[], Path::new("."), None),
                AgentConfig::default(),
            );

            let mut session = Session::in_memory();
            session.header.provider = Some(current.model.provider.clone());
            session.header.model_id = Some(current.model.id.clone());
            let mut agent_session = AgentSession::new(
                agent,
                Arc::new(asupersync::sync::Mutex::new(session)),
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            let options = rpc_options_with_models(vec![current.clone(), next]);
            let session_handle = Arc::clone(&agent_session.session);

            let hold_cx = AgentCx::for_request();
            let held_guard = session_handle
                .lock(hold_cx.cx())
                .await
                .expect("session lock");

            let ambient_cx = asupersync::Cx::for_testing();
            ambient_cx.set_cancel_requested(true);
            let _current = asupersync::Cx::set_current(Some(ambient_cx));

            let err = {
                let cycle = cycle_model_for_rpc(&mut agent_session, &options);
                futures::pin_mut!(cycle);
                let inner = asupersync::time::timeout(
                    asupersync::time::wall_now(),
                    Duration::from_millis(100),
                    cycle,
                )
                .await;
                let outcome = inner.expect("cancelled cycle helper should finish before timeout");
                outcome.expect_err("lock acquisition should honor inherited cancellation")
            };
            assert!(
                err.to_string().contains("inner session lock failed"),
                "unexpected error: {err}"
            );

            drop(held_guard);

            assert_eq!(
                agent_session.agent.provider().name(),
                current.model.provider
            );
            assert_eq!(agent_session.agent.provider().model_id(), current.model.id);

            let verify_cx = AgentCx::for_request();
            let session = agent_session
                .session
                .lock(verify_cx.cx())
                .await
                .expect("session lock");
            assert_eq!(
                session.header.provider.as_deref(),
                Some(current.model.provider.as_str())
            );
            assert_eq!(
                session.header.model_id.as_deref(),
                Some(current.model.id.as_str())
            );
        });
    }

    #[test]
    fn session_state_resolves_model_for_provider_alias() {
        let mut model = dummy_entry("gpt-4o-mini", true);
        model.model.provider = "openrouter".to_string();
        let options = rpc_options_with_models(vec![model]);

        let mut session = Session::in_memory();
        session.header.provider = Some("open-router".to_string());
        session.header.model_id = Some("gpt-4o-mini".to_string());

        let snapshot = RpcStateSnapshot {
            steering_count: 0,
            follow_up_count: 0,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            auto_compaction_enabled: false,
            auto_retry_enabled: false,
        };

        let state = session_state(&session, &options, &snapshot, false, false);
        assert_eq!(state["model"]["provider"], "openrouter");
        assert_eq!(state["model"]["id"], "gpt-4o-mini");
    }

    // -----------------------------------------------------------------------
    // error_hints_value
    // -----------------------------------------------------------------------

    #[test]
    fn error_hints_value_produces_expected_shape() {
        let error = Error::validation("test error");
        let value = error_hints_value(&error);
        assert!(value.get("summary").is_some());
        assert!(value.get("hints").is_some());
        assert!(value.get("contextFields").is_some());
        assert!(value["hints"].is_array());
    }

    // -----------------------------------------------------------------------
    // rpc_parse_extension_ui_response_id edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn parse_ui_response_id_empty_string() {
        let value = json!({"requestId": ""});
        assert_eq!(rpc_parse_extension_ui_response_id(&value), None);
    }

    #[test]
    fn parse_ui_response_id_whitespace_only() {
        let value = json!({"requestId": "   "});
        assert_eq!(rpc_parse_extension_ui_response_id(&value), None);
    }

    #[test]
    fn parse_ui_response_id_trims() {
        let value = json!({"requestId": "  req-1  "});
        assert_eq!(
            rpc_parse_extension_ui_response_id(&value),
            Some("req-1".to_string())
        );
    }

    #[test]
    fn parse_ui_response_id_prefers_request_id_over_id_alias() {
        let value = json!({"requestId": "req-1", "id": "legacy-id"});
        assert_eq!(
            rpc_parse_extension_ui_response_id(&value),
            Some("req-1".to_string())
        );
    }

    #[test]
    fn parse_ui_response_id_falls_back_to_id_alias_when_request_id_not_string() {
        let value = json!({"requestId": 123, "id": "legacy-id"});
        assert_eq!(
            rpc_parse_extension_ui_response_id(&value),
            Some("legacy-id".to_string())
        );
    }

    #[test]
    fn parse_ui_response_id_falls_back_to_id_alias_when_request_id_blank() {
        let value = json!({"requestId": "", "id": "legacy-id"});
        assert_eq!(
            rpc_parse_extension_ui_response_id(&value),
            Some("legacy-id".to_string())
        );
    }

    #[test]
    fn parse_ui_response_id_falls_back_to_id_alias_when_request_id_whitespace() {
        let value = json!({"requestId": "   ", "id": "legacy-id"});
        assert_eq!(
            rpc_parse_extension_ui_response_id(&value),
            Some("legacy-id".to_string())
        );
    }

    #[test]
    fn parse_ui_response_id_neither_field() {
        let value = json!({"type": "something"});
        assert_eq!(rpc_parse_extension_ui_response_id(&value), None);
    }

    // -----------------------------------------------------------------------
    // rpc_parse_extension_ui_response edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn parse_editor_response_requires_string() {
        let active = ExtensionUiRequest::new("req-1", "editor", json!({"title": "t"}));
        let ok = json!({"type": "extension_ui_response", "requestId": "req-1", "value": "code"});
        assert!(rpc_parse_extension_ui_response(&ok, &active).is_ok());

        let bad = json!({"type": "extension_ui_response", "requestId": "req-1", "value": 42});
        assert!(rpc_parse_extension_ui_response(&bad, &active).is_err());
    }

    #[test]
    fn parse_notify_response_returns_ack() {
        let active = ExtensionUiRequest::new("req-1", "notify", json!({"title": "t"}));
        let val = json!({"type": "extension_ui_response", "requestId": "req-1"});
        let resp = rpc_parse_extension_ui_response(&val, &active).unwrap();
        assert!(!resp.cancelled);
    }

    #[test]
    fn parse_unknown_method_errors() {
        let active = ExtensionUiRequest::new("req-1", "unknown_method", json!({}));
        let val = json!({"type": "extension_ui_response", "requestId": "req-1"});
        assert!(rpc_parse_extension_ui_response(&val, &active).is_err());
    }

    #[test]
    fn parse_select_with_object_options() {
        let active = ExtensionUiRequest::new(
            "req-1",
            "select",
            json!({"title": "pick", "options": [{"label": "Alpha", "value": "a"}, {"label": "Beta"}]}),
        );
        // Selecting by value key
        let val_a = json!({"type": "extension_ui_response", "requestId": "req-1", "value": "a"});
        let resp = rpc_parse_extension_ui_response(&val_a, &active).unwrap();
        assert_eq!(resp.value, Some(json!("a")));

        // Selecting by label fallback (no value key in option)
        let val_b = json!({"type": "extension_ui_response", "requestId": "req-1", "value": "Beta"});
        let resp = rpc_parse_extension_ui_response(&val_b, &active).unwrap();
        assert_eq!(resp.value, Some(json!("Beta")));
    }
}
