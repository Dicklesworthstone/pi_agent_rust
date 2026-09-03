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

use crate::agent::{
    AbortHandle, AgentEvent, AgentSession, InputSource, ProviderAdmissionGate, QueueMode,
    QueuedAgentMessage, SessionActionAdmissionGate,
};
use crate::agent_cx::AgentCx;
use crate::auth::AuthStorage;
use crate::compaction::{
    ResolvedCompactionSettings, compact, compact_auto, compaction_details_to_value,
    prepare_compaction,
};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::error_hints;
use crate::extensions::{
    EXTENSION_EVENT_TIMEOUT_MS, ExtensionEventName, ExtensionManager, ExtensionUiRequest,
    ExtensionUiResponse,
};
use crate::model::{
    ContentBlock, ImageContent, Message, StopReason, TextContent, ThinkingLevel, UserContent,
    UserMessage,
};
use crate::models::{ModelEntry, model_requires_configured_credential};
use crate::provider::InputType;
use crate::provider_metadata::provider_ids_match;
use crate::providers;
use crate::resources::ResourceLoader;
use crate::session::{AutosaveFlushTrigger, Session, SessionEntry, SessionMessage};
use crate::tools::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};
use asupersync::channel::{mpsc, oneshot};
use asupersync::runtime::RuntimeHandle;
use asupersync::sync::{Mutex, OwnedMutexGuard};
use asupersync::time::{sleep, wall_now};
use memchr::memchr_iter;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// Ask-tool picker bridge (bd-cv653.3.8): when present, ask requests are
    /// emitted as `ask_request` frames and answered via the `ask_response`
    /// command; absent, the tool resolves via `ask_policy`.
    pub ask_tool: Option<crate::ask::AskTool>,
}

/// Closes the Ask picker on every RPC exit path, including early errors and
/// cancellation where the normal stdin-EOF cleanup is never reached.
struct AskUiCloseGuard(crate::ask::AskTool);

impl Drop for AskUiCloseGuard {
    fn drop(&mut self) {
        self.0.close_channel_ui();
    }
}

/// Closes the extension UI channel and releases every waiting request on all
/// RPC exit paths. The forwarder owns a manager clone while the manager owns
/// the matching channel sender, so relying on normal EOF cleanup alone would
/// retain both sides when `run` returns early or is cancelled.
struct ExtensionUiCloseGuard {
    manager: ExtensionManager,
    ui_state: Arc<std::sync::Mutex<RpcUiBridgeState>>,
}

impl ExtensionUiCloseGuard {
    fn close(&self) {
        self.ui_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .close_and_cancel_timers();
        self.manager.close_ui_sender_and_cancel_pending();
    }
}

impl Drop for ExtensionUiCloseGuard {
    fn drop(&mut self) {
        self.close();
    }
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
            steering_count: state.steering.len() + state.steering_in_flight.len(),
            follow_up_count: state.follow_up.len() + state.follow_up_in_flight.len(),
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
        "set-plan-mode" | "setPlanMode" => "set_plan_mode",
        "approve-plan" | "approvePlan" => "approve_plan",
        "reject-plan" | "rejectPlan" => "reject_plan",
        _ => command_type,
    }
}

fn command_can_advance_rpc_session(command_type: &str) -> bool {
    matches!(
        command_type,
        "prompt"
            | "steer"
            | "follow_up"
            | "set_plan_mode"
            | "approve_plan"
            | "reject_plan"
            | "set_model"
            | "cycle_model"
            | "set_thinking_level"
            | "cycle_thinking_level"
            | "set_session_name"
            | "bash"
            | "compact"
            | "checkpoint"
            | "rewind"
            | "fresh"
            | "retry"
            | "new_session"
            | "switch_session"
            | "fork"
    )
}

fn command_can_queue_while_rpc_agent_streams(command_type: &str) -> bool {
    matches!(command_type, "prompt" | "steer" | "follow_up")
}

fn context_window_tokens_for_entry(entry: &ModelEntry) -> u32 {
    if entry.model.context_window == 0 {
        ResolvedCompactionSettings::default().context_window_tokens
    } else {
        entry.model.context_window
    }
}

fn command_resumes_rpc_agent(
    command_type: &str,
    parsed: &Value,
    manager: Option<&ExtensionManager>,
) -> bool {
    match command_type {
        "prompt" => parsed
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| resolve_extension_command(message, manager).is_none()),
        "steer" | "follow_up" | "retry" => true,
        _ => false,
    }
}

fn command_payload_can_advance_rpc_session(
    command_type: &str,
    parsed: &Value,
    manager: Option<&ExtensionManager>,
) -> bool {
    match command_type {
        "prompt" => {
            parsed.get("message").and_then(Value::as_str).is_some()
                && parse_prompt_images(parsed.get("images")).is_ok()
                && parse_streaming_behavior(streaming_behavior_value(parsed)).is_ok()
        }
        "steer" | "follow_up" => parsed
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| resolve_extension_command(message, manager).is_none()),
        "set_model" => {
            parsed.get("provider").and_then(Value::as_str).is_some()
                && parsed.get("modelId").and_then(Value::as_str).is_some()
        }
        "set_thinking_level" => parsed
            .get("level")
            .and_then(Value::as_str)
            .is_some_and(|level| parse_thinking_level(level).is_ok()),
        "set_session_name" => parsed.get("name").and_then(Value::as_str).is_some(),
        "bash" => parsed.get("command").and_then(Value::as_str).is_some(),
        "compact" => {
            parse_optional_u32_field(parsed, "reserveTokens").is_ok()
                && parse_optional_u32_field(parsed, "keepRecentTokens").is_ok()
        }
        "switch_session" => parsed.get("sessionPath").and_then(Value::as_str).is_some(),
        "fork" => parsed.get("entryId").and_then(Value::as_str).is_some(),
        _ => true,
    }
}

/// Payload of a completed RPC `fork`: `(selected_text, previous_session_file,
/// source_session_id, new_session_id)`; `None` when an extension cancelled the
/// fork.
type ForkCompletion = Option<(String, Option<String>, String, String)>;

async fn take_last_rpc_user_turn_for_retry(session: &mut AgentSession) -> Result<Option<String>> {
    // Session is the durable authority. Truncating only Agent history is undone
    // by `run_agent_with_text`, which rehydrates Agent from this path before it
    // appends the retried prompt. Read the same retryable turn shape as
    // checkpoint retry, then move the active leaf behind that user entry while
    // retaining the original branch in the session tree.
    #[derive(Debug)]
    enum ProjectedUserTurn {
        Text { entry_id: String, text: String },
        Other,
        NotUser,
    }

    let provider_admission = session.provider_admission_gate();
    provider_admission.ensure_allowed()?;
    let save_enabled = session.save_enabled();
    let (text, messages) = {
        let cx = AgentCx::for_request();
        let session_store = Arc::clone(&session.session);
        let mut inner = OwnedMutexGuard::lock(session_store, cx.cx())
            .await
            .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
        let mut candidate = inner.clone();

        let selected = {
            let path = candidate.entries_for_current_path();
            let last_compaction = path
                .iter()
                .rposition(|entry| matches!(entry, SessionEntry::Compaction(_)));
            let mut projected = if last_compaction.is_some() {
                // Session projection begins with the compaction summary, which
                // is a non-text user message and therefore is not retryable.
                vec![ProjectedUserTurn::Other]
            } else {
                Vec::new()
            };
            let mut checkpoint_positions = HashMap::<String, usize>::new();

            let mut append_entry = |entry: &SessionEntry| match entry {
                SessionEntry::Message(message_entry) => {
                    let Some(message) =
                        crate::session::session_message_to_model(&message_entry.message)
                    else {
                        return;
                    };
                    let projected_turn = match message {
                        Message::User(UserMessage {
                            content: UserContent::Text(text),
                            ..
                        }) => message_entry
                            .base
                            .id
                            .clone()
                            .map_or(ProjectedUserTurn::NotUser, |entry_id| {
                                ProjectedUserTurn::Text { entry_id, text }
                            }),
                        Message::User(_) => ProjectedUserTurn::Other,
                        _ => ProjectedUserTurn::NotUser,
                    };
                    projected.push(projected_turn);
                }
                SessionEntry::BranchSummary(_) => projected.push(ProjectedUserTurn::Other),
                SessionEntry::Custom(custom) if custom.custom_type == "checkpoint" => {
                    if let Some(id) = &custom.base.id {
                        checkpoint_positions.insert(id.clone(), projected.len());
                    }
                }
                SessionEntry::Custom(custom) if custom.custom_type == "rewind" => {
                    let checkpoint_entry_id = custom
                        .data
                        .as_ref()
                        .and_then(|data| data.get("checkpointEntryId"))
                        .and_then(Value::as_str);
                    let Some(boundary) = checkpoint_entry_id
                        .and_then(|id| checkpoint_positions.get(id))
                        .copied()
                    else {
                        return;
                    };
                    projected.truncate(boundary);
                    checkpoint_positions.retain(|_, position| *position <= boundary);
                    let has_report = custom
                        .data
                        .as_ref()
                        .and_then(|data| data.get("summary"))
                        .and_then(Value::as_str)
                        .is_some_and(|summary| !summary.is_empty());
                    if has_report {
                        projected.push(ProjectedUserTurn::NotUser);
                    }
                }
                _ => {}
            };

            if let Some(compaction_index) = last_compaction {
                let SessionEntry::Compaction(compaction) = path[compaction_index] else {
                    return Err(Error::session("RPC retry compaction projection drifted"));
                };
                let has_kept_entry = path.iter().any(|entry| {
                    entry
                        .base_id()
                        .is_some_and(|id| id == &compaction.first_kept_entry_id)
                });
                let mut keep = false;
                let mut past_compaction = false;
                for (index, entry) in path.iter().enumerate() {
                    if index == compaction_index {
                        past_compaction = true;
                    }
                    if !keep {
                        if has_kept_entry {
                            if entry
                                .base_id()
                                .is_some_and(|id| id == &compaction.first_kept_entry_id)
                            {
                                keep = true;
                            } else {
                                continue;
                            }
                        } else if past_compaction {
                            keep = true;
                        } else {
                            continue;
                        }
                    }
                    append_entry(entry);
                }
            } else {
                for entry in path {
                    append_entry(entry);
                }
            }

            projected
                .into_iter()
                .rev()
                .find(|turn| !matches!(turn, ProjectedUserTurn::NotUser))
        };

        let Some(ProjectedUserTurn::Text { entry_id, text }) = selected else {
            return Ok(None);
        };

        if !candidate.navigate_to(&entry_id) || !candidate.revert_last_user_message() {
            return Err(Error::session(
                "RPC retry found a projected user turn but could not rewind to its durable entry",
            ));
        }
        let messages = candidate.to_messages_for_current_path();
        let _provider_transition = provider_admission
            .begin_transition(
                "retry rewind persistence was interrupted before live installation completed"
                    .to_string(),
                cx.cx(),
            )
            .await?;
        if save_enabled
            && let Err(first_err) = candidate.save().await
            && let Err(retry_err) = candidate.save().await
        {
            let reason = format!(
                "retry rewind persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
            );
            provider_admission.block(reason.clone());
            return Err(Error::session_persistence(reason));
        }
        session.invalidate_background_compaction();
        *inner = candidate;
        provider_admission.clear();
        (text, messages)
    };

    session.agent.replace_messages(messages);
    Ok(Some(text))
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
    deferred_agent_end: Option<Arc<std::sync::Mutex<Option<AgentEvent>>>>,
) -> impl Fn(AgentEvent) + Send + Sync + 'static {
    let coalescer = extensions.map(crate::extensions::EventCoalescer::new);
    let output_pressure = Arc::new(std::sync::Mutex::new(RpcOutputPressureState::default()));

    move |event: AgentEvent| {
        if matches!(event, AgentEvent::AgentEnd { .. })
            && let Some(deferred) = &deferred_agent_end
        {
            output_pressure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .flush_pending(&out_tx);
            if let Some(coalescer) = &coalescer {
                coalescer.dispatch_agent_event_lazy(&event, &runtime_handle);
            }
            *deferred
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(event);
            return;
        }
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

async fn rpc_dispatch_session_before_fork(
    manager: Option<ExtensionManager>,
    entry_id: &str,
    summary: &str,
    session_id: &str,
) -> bool {
    let Some(manager) = manager else {
        return false;
    };

    manager
        .dispatch_cancellable_event(
            ExtensionEventName::SessionBeforeFork,
            Some(json!({
                "entryId": entry_id,
                "summary": summary,
                "sessionId": session_id,
            })),
            EXTENSION_EVENT_TIMEOUT_MS,
        )
        .await
        .unwrap_or(false)
}

async fn rpc_dispatch_session_fork_event(manager: Option<ExtensionManager>, payload: Value) {
    let Some(manager) = manager else {
        return;
    };

    let _ = manager
        .dispatch_event(ExtensionEventName::SessionFork, Some(payload))
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

#[derive(Debug, Clone)]
struct RpcFailoverPrimary {
    provider: String,
    model_id: String,
    requested_thinking_level: ThinkingLevel,
}

#[derive(Debug)]
struct RpcSharedState {
    steering: VecDeque<QueuedAgentMessage>,
    follow_up: VecDeque<QueuedAgentMessage>,
    steering_in_flight: VecDeque<RpcInFlightMessage>,
    follow_up_in_flight: VecDeque<RpcInFlightMessage>,
    completed_tool_transcript: Option<RpcCompletedToolTranscript>,
    next_lease_sequence: u64,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    follow_up_fetch_generation: Arc<AtomicU64>,
    auto_compaction_enabled: bool,
    auto_retry_enabled: bool,
    /// Cross-turn failover state (bd-cv653.3.2): cooldown tracker and the
    /// currently active non-primary model `(provider, model_id)`, if any.
    failover_cooldown: Option<crate::failover::CooldownTracker>,
    /// The provider/model active before the first failover in the current
    /// chain. This identity is explicit because the Session header and newest
    /// ModelChange both advance to the fallback during a committed failover.
    failover_primary: Option<RpcFailoverPrimary>,
    active_failover_model: Option<(String, String)>,
    /// Position of the last used entry in the active chain (per-chain walk).
    failover_chain_position: Option<usize>,
    /// Shared with AgentSession and extension hostcalls: every RPC admission
    /// and transition observes the same permanent quarantine authority.
    provider_admission: ProviderAdmissionGate,
}

#[derive(Debug, Clone)]
struct RpcInFlightMessage {
    delivery: QueuedAgentMessage,
    session_entry_baseline: usize,
    lease_sequence: u64,
}

#[derive(Debug)]
struct RpcCompletedToolTranscript {
    session_id: String,
    base_leaf_id: Option<String>,
    entries: Vec<QueuedAgentMessage>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RpcTurnPhase {
    Idle,
    Streaming,
    Compacting,
}

/// Snapshot the two-flag turn handoff without accepting the torn state where
/// compaction became active between the first compaction read and the streaming
/// read. `SeqCst` plus the second compaction read makes `Idle` observable only
/// before the handoff begins or after compaction has actually finished.
fn rpc_turn_phase(is_streaming: &AtomicBool, is_compacting: &AtomicBool) -> RpcTurnPhase {
    if is_compacting.load(Ordering::SeqCst) {
        return RpcTurnPhase::Compacting;
    }
    let streaming = is_streaming.load(Ordering::SeqCst);
    if is_compacting.load(Ordering::SeqCst) {
        RpcTurnPhase::Compacting
    } else if streaming {
        RpcTurnPhase::Streaming
    } else {
        RpcTurnPhase::Idle
    }
}

fn lock_rpc_turn_phase(lock: &std::sync::Mutex<()>) -> std::sync::MutexGuard<'_, ()> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl RpcSharedState {
    fn new(config: &Config) -> Self {
        Self::new_with_provider_admission(config, ProviderAdmissionGate::default())
    }

    fn new_with_provider_admission(
        config: &Config,
        provider_admission: ProviderAdmissionGate,
    ) -> Self {
        Self {
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            steering_in_flight: VecDeque::new(),
            follow_up_in_flight: VecDeque::new(),
            completed_tool_transcript: None,
            next_lease_sequence: 0,
            steering_mode: config.steering_queue_mode(),
            follow_up_mode: config.follow_up_queue_mode(),
            follow_up_fetch_generation: Arc::new(AtomicU64::new(0)),
            auto_compaction_enabled: config.compaction_enabled(),
            auto_retry_enabled: config.retry_enabled(),
            failover_cooldown: config
                .retry
                .as_ref()
                .and_then(|r| r.fallback_chains.as_ref())
                .map(|_| crate::failover::CooldownTracker::new(config.failover_cooldown_secs())),
            failover_primary: None,
            active_failover_model: None,
            failover_chain_position: None,
            provider_admission,
        }
    }

    fn bind_provider_admission(&mut self, provider_admission: ProviderAdmissionGate) {
        if let Some(reason) = self.provider_admission.reason() {
            provider_admission.block(reason);
        }
        self.provider_admission = provider_admission;
    }

    fn pending_count(&self) -> usize {
        self.steering.len()
            + self.follow_up.len()
            + self.steering_in_flight.len()
            + self.follow_up_in_flight.len()
    }

    fn ensure_session_advancement_allowed(&self) -> Result<()> {
        if let Some(reason) = self.provider_admission.reason() {
            return Err(Error::session_persistence(format!(
                "RPC input admission is quarantined after an indeterminate transition: {reason}"
            )));
        }
        Ok(())
    }

    fn push_steering(&mut self, message: QueuedAgentMessage) -> Result<()> {
        self.ensure_session_advancement_allowed()?;
        if self.pending_count() >= MAX_RPC_PENDING_MESSAGES {
            return Err(Error::session(
                "Steering queue is full (Do you have too many pending commands?)",
            ));
        }
        self.steering.push_back(message);
        Ok(())
    }

    fn push_follow_up(&mut self, message: QueuedAgentMessage) -> Result<()> {
        self.ensure_session_advancement_allowed()?;
        if self.pending_count() >= MAX_RPC_PENDING_MESSAGES {
            return Err(Error::session("Follow-up queue is full"));
        }
        self.follow_up.push_back(message);
        Ok(())
    }

    fn lease_steering(&mut self, session_entry_baseline: usize) -> Vec<QueuedAgentMessage> {
        let messages: Vec<_> = match self.steering_mode {
            QueueMode::All => self.steering.drain(..).collect(),
            QueueMode::OneAtATime => self.steering.pop_front().into_iter().collect(),
        };
        for delivery in messages.iter().cloned() {
            let lease_sequence = self.next_lease_sequence;
            self.next_lease_sequence = self.next_lease_sequence.saturating_add(1);
            self.steering_in_flight.push_back(RpcInFlightMessage {
                delivery,
                session_entry_baseline,
                lease_sequence,
            });
        }
        messages
    }

    fn lease_follow_up(&mut self, session_entry_baseline: usize) -> Vec<QueuedAgentMessage> {
        let messages: Vec<_> = match self.follow_up_mode {
            QueueMode::All => self.follow_up.drain(..).collect(),
            QueueMode::OneAtATime => self.follow_up.pop_front().into_iter().collect(),
        };
        for delivery in messages.iter().cloned() {
            let lease_sequence = self.next_lease_sequence;
            self.next_lease_sequence = self.next_lease_sequence.saturating_add(1);
            self.follow_up_in_flight.push_back(RpcInFlightMessage {
                delivery,
                session_entry_baseline,
                lease_sequence,
            });
        }
        messages
    }

    fn lease_follow_up_for_fetch(
        &mut self,
        session_entry_baseline: usize,
    ) -> Vec<QueuedAgentMessage> {
        let messages = self.lease_follow_up(session_entry_baseline);
        if !messages.is_empty() {
            self.follow_up_fetch_generation
                .fetch_add(1, Ordering::SeqCst);
        }
        messages
    }

    fn acknowledge_in_flight(&mut self) {
        self.steering_in_flight.clear();
        self.follow_up_in_flight.clear();
    }

    fn in_flight_in_lease_order(&self) -> Vec<&RpcInFlightMessage> {
        let mut messages = self
            .steering_in_flight
            .iter()
            .chain(&self.follow_up_in_flight)
            .collect::<Vec<_>>();
        messages.sort_by_key(|message| message.lease_sequence);
        messages
    }

    fn stage_completed_tool_transcript(
        &mut self,
        session: &Session,
        messages: &[Message],
    ) -> Result<()> {
        let session_id = session.header.id.clone();
        let base_leaf_id = session.leaf_id().map(str::to_string);
        if let Some(staged) = &self.completed_tool_transcript {
            if staged.session_id != session_id || staged.base_leaf_id != base_leaf_id {
                return Err(Error::session(
                    "session base changed during completed-tool transcript recovery",
                ));
            }
            if messages.is_empty() {
                return Ok(());
            }
            if staged.entries.len() != messages.len() {
                return Err(Error::session(
                    "live completed-tool transcript changed during terminal recovery",
                ));
            }
            for (existing, message) in staged.entries.iter().zip(messages) {
                if serde_json::to_vec(existing.message())? != serde_json::to_vec(message)? {
                    return Err(Error::session(
                        "live completed-tool transcript changed during terminal recovery",
                    ));
                }
            }
            return Ok(());
        }
        if !messages.is_empty() {
            self.completed_tool_transcript = Some(RpcCompletedToolTranscript {
                session_id,
                base_leaf_id,
                entries: messages
                    .iter()
                    .cloned()
                    .map(QueuedAgentMessage::generated)
                    .collect(),
            });
        }
        Ok(())
    }

    fn completed_tool_transcript_entries(&self) -> &[QueuedAgentMessage] {
        self.completed_tool_transcript
            .as_ref()
            .map_or(&[], |transcript| transcript.entries.as_slice())
    }

    fn clear_all_pending(&mut self) {
        self.steering.clear();
        self.follow_up.clear();
        self.acknowledge_in_flight();
        self.completed_tool_transcript = None;
    }

    fn clear_failover_lifecycle(&mut self) {
        self.failover_primary = None;
        self.active_failover_model = None;
        self.failover_chain_position = None;
        if let Some(tracker) = self.failover_cooldown.as_mut() {
            tracker.reset();
        }
    }
}

/// Tracks a running bash command so it can be aborted.
struct RunningBash {
    id: String,
    abort_tx: Option<oneshot::Sender<()>>,
}

impl RunningBash {
    fn request_abort(&mut self, cx: &asupersync::Cx) {
        if let Some(abort_tx) = self.abort_tx.take() {
            let _ = abort_tx.send(cx, ());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RpcSessionTransitionSnapshot {
    session_id: String,
    leaf_id: Option<String>,
    entry_count: usize,
    provider: Option<String>,
    model_id: Option<String>,
    thinking_level: Option<String>,
}

struct RpcSessionTransitionPermits {
    session_action_admission: SessionActionAdmissionGate,
    _provider: OwnedMutexGuard<()>,
    _session_action: OwnedMutexGuard<()>,
}

struct RpcSessionTransitionAuthority {
    session: OwnedMutexGuard<AgentSession>,
    permits: RpcSessionTransitionPermits,
}

impl RpcSessionTransitionPermits {
    fn commit_session_change(&self) {
        self.session_action_admission.advance_generation();
    }
}

async fn rpc_session_transition_snapshot(
    session: &Arc<Mutex<AgentSession>>,
    cx: &AgentCx,
) -> Result<RpcSessionTransitionSnapshot> {
    let guard = OwnedMutexGuard::lock(Arc::clone(session), cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    rpc_session_transition_snapshot_from_guard(&guard, cx).await
}

async fn rpc_session_transition_snapshot_from_guard(
    guard: &AgentSession,
    cx: &AgentCx,
) -> Result<RpcSessionTransitionSnapshot> {
    let inner = guard
        .session
        .lock(cx.cx())
        .await
        .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
    Ok(RpcSessionTransitionSnapshot {
        session_id: inner.header.id.clone(),
        leaf_id: inner.leaf_id().map(str::to_string),
        entry_count: inner.entries.len(),
        provider: inner.header.provider.clone(),
        model_id: inner.header.model_id.clone(),
        thinking_level: inner.header.thinking_level.clone(),
    })
}

async fn rpc_session_transition_blocker(
    is_streaming: &AtomicBool,
    is_compacting: &AtomicBool,
    turn_phase_linearizer: &std::sync::Mutex<()>,
    session: &Arc<Mutex<AgentSession>>,
    shared_state: &Arc<Mutex<RpcSharedState>>,
    bash_state: &Arc<Mutex<Option<RunningBash>>>,
    cx: &AgentCx,
) -> Result<Option<&'static str>> {
    let phase = {
        let _phase_guard = lock_rpc_turn_phase(turn_phase_linearizer);
        rpc_turn_phase(is_streaming, is_compacting)
    };
    match phase {
        RpcTurnPhase::Streaming => {
            return Ok(Some(
                "Agent is currently streaming; abort or wait before changing sessions",
            ));
        }
        RpcTurnPhase::Compacting => {
            return Ok(Some(
                "Agent is currently compacting; wait before changing sessions",
            ));
        }
        RpcTurnPhase::Idle => {}
    }

    let guard = OwnedMutexGuard::lock(Arc::clone(session), cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    guard.ensure_provider_reentry_allowed()?;
    let provider_admission = guard.provider_admission_gate();
    let staged_follow_up = guard.agent.has_staged_follow_up();
    let pending_extension_action = guard.has_pending_extension_idle_actions();
    drop(guard);
    if staged_follow_up {
        return Ok(Some(
            "An accepted follow-up is still pending; resume it before changing sessions",
        ));
    }
    if pending_extension_action {
        return Ok(Some(
            "An extension-triggered action is still pending; resume it before changing sessions",
        ));
    }

    let mut state = OwnedMutexGuard::lock(Arc::clone(shared_state), cx)
        .await
        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
    state.bind_provider_admission(provider_admission);
    state.ensure_session_advancement_allowed()?;
    let pending_input_count = state.pending_count();
    drop(state);
    if pending_input_count > 0 {
        return Ok(Some(
            "Acknowledged RPC input is still pending; resume or persist it before changing sessions",
        ));
    }

    let bash_running = OwnedMutexGuard::lock(Arc::clone(bash_state), cx)
        .await
        .map_err(|err| Error::session(format!("bash state lock failed: {err}")))?
        .is_some();
    Ok(bash_running
        .then_some("A background bash command is still running; wait before changing sessions"))
}

#[allow(clippy::too_many_arguments)]
async fn acquire_rpc_session_transition(
    baseline: &RpcSessionTransitionSnapshot,
    is_streaming: &AtomicBool,
    is_compacting: &AtomicBool,
    turn_phase_linearizer: &std::sync::Mutex<()>,
    session: &Arc<Mutex<AgentSession>>,
    shared_state: &Arc<Mutex<RpcSharedState>>,
    bash_state: &Arc<Mutex<Option<RunningBash>>>,
    cx: &AgentCx,
) -> Result<RpcSessionTransitionAuthority> {
    // Background bash admission holds bash-state while it snapshots the outer
    // AgentSession and only then publishes RunningBash. Check bash-state before
    // taking transition authority so both paths follow bash-state -> outer.
    // The RPC dispatcher serializes command admission, so no new background
    // bash command can start while this command awaits; an existing worker can
    // only clear the published state.
    if OwnedMutexGuard::lock(Arc::clone(bash_state), cx)
        .await
        .map_err(|err| Error::session(format!("bash state lock failed: {err}")))?
        .is_some()
    {
        return Err(Error::session(
            "A background bash command is still running; wait before changing sessions",
        ));
    }

    // The global order is outer AgentSession -> provider admission -> Session
    // action admission. Provider callbacks and outer-held extension events can
    // both re-enter Session host actions, so taking the action permit first
    // would create action <-> provider and action <-> outer wait cycles. Any
    // action that finishes before the final permit is acquired is detected by
    // the source snapshot recheck below.
    let guard = OwnedMutexGuard::lock(Arc::clone(session), cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    let provider_admission = guard.provider_admission_gate();
    let session_action_admission = guard.session_action_admission_gate();
    let provider_permit = provider_admission.acquire(cx.cx()).await?;
    provider_admission.ensure_allowed()?;
    let session_action_permit = session_action_admission.acquire(cx.cx()).await?;

    let phase = {
        let _phase_guard = lock_rpc_turn_phase(turn_phase_linearizer);
        rpc_turn_phase(is_streaming, is_compacting)
    };
    let phase_blocker = match phase {
        RpcTurnPhase::Streaming => {
            Some("Agent is currently streaming; abort or wait before changing sessions")
        }
        RpcTurnPhase::Compacting => {
            Some("Agent is currently compacting; wait before changing sessions")
        }
        RpcTurnPhase::Idle => None,
    };
    if let Some(reason) = phase_blocker {
        return Err(Error::session(reason));
    }

    guard.ensure_provider_reentry_allowed()?;
    if guard.agent.has_staged_follow_up() {
        return Err(Error::session(
            "An accepted follow-up is still pending; resume it before changing sessions",
        ));
    }
    if guard.has_pending_extension_idle_actions() {
        return Err(Error::session(
            "An extension-triggered action is still pending; resume it before changing sessions",
        ));
    }

    let mut state = OwnedMutexGuard::lock(Arc::clone(shared_state), cx)
        .await
        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
    state.bind_provider_admission(provider_admission.clone());
    state.ensure_session_advancement_allowed()?;
    if state.pending_count() > 0 {
        return Err(Error::session(
            "Acknowledged RPC input is still pending; resume or persist it before changing sessions",
        ));
    }
    drop(state);

    let current = rpc_session_transition_snapshot_from_guard(&guard, cx).await?;
    if current != *baseline {
        return Err(Error::session(
            "an accepted action modified the source Session while the transition was pending; the transition was rejected so the action remains owned by that Session",
        ));
    }
    Ok(RpcSessionTransitionAuthority {
        session: guard,
        permits: RpcSessionTransitionPermits {
            session_action_admission,
            _provider: provider_permit,
            _session_action: session_action_permit,
        },
    })
}

#[derive(Debug, Clone)]
struct RpcUiBridgeTimer {
    cancel_tx: Arc<std::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

impl RpcUiBridgeTimer {
    fn new() -> (Self, oneshot::Receiver<()>) {
        let (cancel_tx, cancel_rx) = oneshot::channel();
        (
            Self {
                cancel_tx: Arc::new(std::sync::Mutex::new(Some(cancel_tx))),
            },
            cancel_rx,
        )
    }

    fn cancel(&self) {
        let cancel_tx = self
            .cancel_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(cancel_tx) = cancel_tx {
            let _ = cancel_tx.send_blocking(());
        }
    }
}

#[derive(Debug, Clone)]
struct RpcUiBridgeRequest {
    request: ExtensionUiRequest,
    generation: u64,
    timer: Option<RpcUiBridgeTimer>,
}

impl RpcUiBridgeRequest {
    fn cancel_timer(&self) {
        if let Some(timer) = &self.timer {
            timer.cancel();
        }
    }
}

#[derive(Debug, Default)]
struct RpcUiBridgeState {
    active: Option<RpcUiBridgeRequest>,
    queue: VecDeque<RpcUiBridgeRequest>,
    next_generation: u64,
    closed: bool,
}

struct RpcUiBridgeExpiration {
    response: ExtensionUiResponse,
    next: Option<RpcUiBridgeRequest>,
}

impl RpcUiBridgeState {
    fn active_matches(&self, request_id: &str, generation: u64) -> bool {
        !self.closed
            && self.active.as_ref().is_some_and(|active| {
                active.request.id == request_id && active.generation == generation
            })
    }

    fn admit(
        &mut self,
        request: ExtensionUiRequest,
    ) -> Option<(RpcUiBridgeRequest, bool, Option<oneshot::Receiver<()>>)> {
        if self.closed {
            return None;
        }
        self.next_generation = self.next_generation.wrapping_add(1);
        let (timer, cancel_rx) = if request.deadline().is_some() {
            let (timer, cancel_rx) = RpcUiBridgeTimer::new();
            (Some(timer), Some(cancel_rx))
        } else {
            (None, None)
        };
        let admitted = RpcUiBridgeRequest {
            request,
            generation: self.next_generation,
            timer,
        };
        let emit_now = self.active.is_none();
        if emit_now {
            self.active = Some(admitted.clone());
        } else {
            self.queue.push_back(admitted.clone());
        }
        Some((admitted, emit_now, cancel_rx))
    }

    fn finish_active(&mut self) -> Option<RpcUiBridgeRequest> {
        if let Some(active) = self.active.take() {
            active.cancel_timer();
        }
        let next = self.queue.pop_front();
        self.active.clone_from(&next);
        next
    }

    fn close_and_cancel_timers(&mut self) {
        self.closed = true;
        if let Some(active) = self.active.take() {
            active.cancel_timer();
        }
        for queued in self.queue.drain(..) {
            queued.cancel_timer();
        }
    }

    fn expire(&mut self, request_id: &str, generation: u64) -> Option<RpcUiBridgeExpiration> {
        if self.active_matches(request_id, generation) {
            let expired = self.active.take()?;
            expired.cancel_timer();
            let next = self.queue.pop_front();
            self.active.clone_from(&next);
            return Some(RpcUiBridgeExpiration {
                response: rpc_extension_ui_timeout_response(&expired.request),
                next,
            });
        }

        let queue_index = self.queue.iter().position(|queued| {
            queued.request.id == request_id && queued.generation == generation
        })?;
        let expired = self.queue.remove(queue_index)?;
        expired.cancel_timer();
        Some(RpcUiBridgeExpiration {
            response: rpc_extension_ui_timeout_response(&expired.request),
            next: None,
        })
    }
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
    // Boxed: clippy::large_futures.
    let result = Box::pin(run(session, options, in_rx, out_tx)).await;

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
    let provider_admission = session.provider_admission_gate();
    let session = Arc::new(Mutex::new(session));
    let shared_state = Arc::new(Mutex::new(RpcSharedState::new_with_provider_admission(
        &options.config,
        provider_admission,
    )));
    let is_streaming = Arc::new(AtomicBool::new(false));
    let is_compacting = Arc::new(AtomicBool::new(false));
    let turn_phase_linearizer = Arc::new(std::sync::Mutex::new(()));
    let abort_handle: Arc<Mutex<Option<AbortHandle>>> = Arc::new(Mutex::new(None));
    let bash_state: Arc<Mutex<Option<RunningBash>>> = Arc::new(Mutex::new(None));
    let retry_abort = Arc::new(AtomicBool::new(false));

    {
        use futures::future::BoxFuture;
        let steering_state = Arc::clone(&shared_state);
        let follow_state = Arc::clone(&shared_state);
        let steering_session = Arc::clone(&session_handle);
        let follow_session = Arc::clone(&session_handle);
        let steering_cx = cx.clone();
        let follow_cx = cx.clone();
        let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
            .await
            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
        let initial_plan_mode = {
            let inner = guard
                .session
                .lock(&cx)
                .await
                .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
            replayed_plan_mode(&inner)
        };
        guard.agent.reset_session_scoped_state(initial_plan_mode);
        guard.set_queue_modes(
            options.config.steering_queue_mode(),
            options.config.follow_up_queue_mode(),
        );
        let steering_fetcher = move || -> BoxFuture<'static, Vec<QueuedAgentMessage>> {
            let steering_state = Arc::clone(&steering_state);
            let steering_session = Arc::clone(&steering_session);
            let steering_cx = steering_cx.clone();
            Box::pin(async move {
                let Some(session_entry_baseline) = (async {
                    let session = steering_session.lock(&steering_cx).await.ok()?;
                    Some(session.entries.len())
                })
                .await
                else {
                    return Vec::new();
                };
                steering_state.lock(&steering_cx).await.map_or_else(
                    |_| Vec::new(),
                    |mut state| state.lease_steering(session_entry_baseline),
                )
            })
        };
        let follow_fetcher = move || -> BoxFuture<'static, Vec<QueuedAgentMessage>> {
            let follow_state = Arc::clone(&follow_state);
            let follow_session = Arc::clone(&follow_session);
            let follow_cx = follow_cx.clone();
            Box::pin(async move {
                let Some(session_entry_baseline) = (async {
                    let session = follow_session.lock(&follow_cx).await.ok()?;
                    Some(session.entries.len())
                })
                .await
                else {
                    return Vec::new();
                };
                follow_state.lock(&follow_cx).await.map_or_else(
                    |_| Vec::new(),
                    |mut state| state.lease_follow_up_for_fetch(session_entry_baseline),
                )
            })
        };
        guard
            .agent
            .register_message_fetchers(Some(Arc::new(steering_fetcher)), None);
        guard
            .agent
            .register_initial_follow_up_fetcher(Arc::new(follow_fetcher));
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

    let rpc_ui_state: Option<Arc<std::sync::Mutex<RpcUiBridgeState>>> = rpc_extension_manager
        .as_ref()
        .map(|_| Arc::new(std::sync::Mutex::new(RpcUiBridgeState::default())));
    let extension_ui_close_guard = rpc_extension_manager
        .as_ref()
        .zip(rpc_ui_state.as_ref())
        .map(|(manager, ui_state)| ExtensionUiCloseGuard {
            manager: manager.clone(),
            ui_state: Arc::clone(ui_state),
        });

    // Ask-tool frames (bd-cv653.3.8): each picker request is emitted as an
    // `ask_request` event keyed by its request id; the client answers with
    // an `ask_response` command. Response routing is id-keyed through the
    // AskTool's own pending map (with its built-in wait budget), so no
    // active/queue ordering state is needed here.
    if let Some(ref ask) = options.ask_tool {
        let (ask_ui_tx, mut ask_ui_rx) =
            asupersync::channel::mpsc::channel::<crate::ask::AskUiRequest>(4);
        ask.install_channel_ui(ask_ui_tx);
        let ask_forwarder = ask.clone();
        let out_tx_ask = out_tx.clone();
        options.runtime_handle.spawn(async move {
            let cx = AgentCx::for_request();
            while let Ok(request) = ask_ui_rx.recv(&cx).await {
                let frame = event(&ask_request_rpc_event(&request));
                ask_forwarder.try_forward_channel_ui_request(&request.id, || {
                    out_tx_ask.try_send(frame).is_ok()
                });
            }
        });
    }
    // The spawned forwarder owns both a receiver and an AskTool clone whose
    // installed handler owns the matching sender. This guard breaks that
    // ownership cycle even if a later `?` or task cancellation exits `run`
    // before the explicit stdin-EOF close below.
    let _ask_ui_close_guard = options
        .ask_tool
        .as_ref()
        .map(|ask| AskUiCloseGuard(ask.clone()));

    let extension_ui_forwarder = if let Some(ref manager) = rpc_extension_manager {
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
        Some(options.runtime_handle.spawn(async move {
            const MAX_UI_PENDING_REQUESTS: usize = 64;
            let cx = AgentCx::for_request();
            while let Ok(request) = extension_ui_rx.recv(&cx).await {
                if request.expects_response() {
                    let admitted = {
                        let mut guard = ui_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if !manager_ui.ui_request_is_pending(&request.id) {
                            None
                        } else if guard.active.is_none()
                            || guard.queue.len() < MAX_UI_PENDING_REQUESTS
                        {
                            guard.admit(request.clone())
                        } else {
                            drop(guard);
                            let _ = manager_ui.respond_ui(ExtensionUiResponse {
                                id: request.id.clone(),
                                value: None,
                                cancelled: true,
                            });
                            None
                        }
                    };

                    if let Some((admitted, emit_now, cancel_rx)) = admitted {
                        if let Some(cancel_rx) = cancel_rx {
                            rpc_schedule_extension_ui_timeout(
                                &runtime_handle_ui,
                                Arc::clone(&ui_state),
                                manager_ui.clone(),
                                out_tx_ui.clone(),
                                &admitted,
                                cancel_rx,
                            );
                        }
                        if emit_now {
                            rpc_publish_extension_ui_request(
                                Arc::clone(&ui_state),
                                manager_ui.clone(),
                                out_tx_ui.clone(),
                                admitted,
                            );
                        }
                    }
                } else {
                    // Fire-and-forget UI updates should not be queued.
                    let rpc_event = request.to_rpc_event();
                    let guard = ui_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !guard.closed {
                        let _ = rpc_try_send_extension_ui_event(&out_tx_ui, &rpc_event);
                    }
                }
            }
        }))
    } else {
        None
    };

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

        // A cancelled or failed terminal writer can leave acknowledged input
        // authoritative in the shared queues after the turn flags return idle.
        // Recover it before any command can advance the live session or deliver
        // the envelope through Agent (which intentionally consumes only its
        // provider-visible Message). Read-only/control commands remain usable
        // so clients can inspect health or abort unrelated work.
        if command_can_advance_rpc_session(command_type) {
            let phase = {
                let _phase_guard = lock_rpc_turn_phase(&turn_phase_linearizer);
                rpc_turn_phase(&is_streaming, &is_compacting)
            };
            if phase == RpcTurnPhase::Compacting {
                let _ = out_tx.send(response_error(
                    id.clone(),
                    command_type,
                    format!("Agent is currently compacting; wait before running {command_type}"),
                ));
                continue;
            }
            if phase == RpcTurnPhase::Streaming
                && !command_can_queue_while_rpc_agent_streams(command_type)
            {
                let _ = out_tx.send(response_error(
                    id.clone(),
                    command_type,
                    format!("Agent is currently streaming; wait before running {command_type}"),
                ));
                continue;
            }
            if phase == RpcTurnPhase::Idle
                && command_payload_can_advance_rpc_session(
                    command_type,
                    &parsed,
                    rpc_extension_manager.as_ref(),
                )
            {
                let recovery_cx = AgentCx::for_request();
                let quarantine_reason = match OwnedMutexGuard::lock(
                    Arc::clone(&shared_state),
                    &recovery_cx,
                )
                .await
                {
                    Ok(state) => state.provider_admission.reason(),
                    Err(err) => {
                        let quarantine_error = Error::session(format!(
                            "failed to inspect RPC persistence quarantine before {command_type}: {err}"
                        ));
                        let _ = out_tx.send(response_error_with_hints(
                            id.clone(),
                            command_type,
                            &quarantine_error,
                        ));
                        continue;
                    }
                };
                if let Some(reason) = quarantine_reason {
                    // Same shape as the other quarantine rejections ("RPC input
                    // admission is quarantined ...", "provider re-entry is
                    // quarantined ...") so clients can recognise the state
                    // instead of parsing the underlying persistence failure.
                    let quarantine_error = Error::session_persistence(format!(
                        "{command_type} is quarantined after an indeterminate transition: {reason}"
                    ));
                    let _ = out_tx.send(response_error_with_hints(
                        id.clone(),
                        command_type,
                        &quarantine_error,
                    ));
                    continue;
                }
                let resumes_agent = command_resumes_rpc_agent(
                    command_type,
                    &parsed,
                    rpc_extension_manager.as_ref(),
                );
                let recovery_plan = match terminal_rpc_recovery_plan(
                    &session,
                    &shared_state,
                    resumes_agent,
                    &recovery_cx,
                )
                .await
                {
                    Ok(plan) => plan,
                    Err(err) => {
                        let recovery_error = Error::session(format!(
                            "failed to inspect terminal RPC recovery state before {command_type}: {err}"
                        ));
                        let _ = out_tx.send(response_error_with_hints(
                            id.clone(),
                            command_type,
                            &recovery_error,
                        ));
                        continue;
                    }
                };
                let recovery_result = match recovery_plan {
                    RpcTerminalRecoveryPlan::None => None,
                    RpcTerminalRecoveryPlan::RecordedToolTranscript { recovery_count } => Some((
                        recovery_count,
                        preserve_recorded_tool_transcript(&session, &shared_state, &recovery_cx)
                            .await,
                    )),
                    RpcTerminalRecoveryPlan::All { recovery_count } => Some((
                        recovery_count,
                        preserve_terminal_rpc_input(&session, &shared_state, &recovery_cx).await,
                    )),
                };
                if let Some((recovery_count, Err(err))) = recovery_result {
                    let recovery_error = Error::session(format!(
                        "failed to preserve {recovery_count} terminal RPC recovery item(s) before {command_type}: {err}"
                    ));
                    let _ = out_tx.send(response_error_with_hints(
                        id.clone(),
                        command_type,
                        &recovery_error,
                    ));
                    continue;
                }
            }
        }

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

                match rpc_turn_phase(&is_streaming, &is_compacting) {
                    RpcTurnPhase::Compacting => {
                        let resp = response_error(
                            id,
                            "prompt",
                            "Agent is currently compacting; wait before sending another prompt"
                                .to_string(),
                        );
                        let _ = out_tx.send(resp);
                        continue;
                    }
                    RpcTurnPhase::Idle => {}
                    RpcTurnPhase::Streaming => {
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
                                "Agent is currently streaming; specify streamingBehavior"
                                    .to_string(),
                            );
                            let _ = out_tx.send(resp);
                            continue;
                        }

                        let expanded = options.resources.expand_input(&message);
                        let queued_result = {
                            let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                                .await
                                .map_err(|err| {
                                    Error::session(format!("state lock failed: {err}"))
                                })?;
                            let _phase_guard = lock_rpc_turn_phase(&turn_phase_linearizer);
                            match rpc_turn_phase(&is_streaming, &is_compacting) {
                                RpcTurnPhase::Streaming => match streaming_behavior {
                                    Some(StreamingBehavior::Steer) => {
                                        state.push_steering(QueuedAgentMessage::authored(
                                            build_user_message(&expanded, &images),
                                            message.clone(),
                                        ))
                                    }
                                    Some(StreamingBehavior::FollowUp) => {
                                        state.push_follow_up(QueuedAgentMessage::authored(
                                            build_user_message(&expanded, &images),
                                            message.clone(),
                                        ))
                                    }
                                    None => Ok(()), // Unreachable due to check above
                                },
                                RpcTurnPhase::Compacting => Err(Error::session(
                                    "Agent began compacting before the message could be queued",
                                )),
                                RpcTurnPhase::Idle => Err(Error::session(
                                    "Agent stopped streaming before the message could be queued",
                                )),
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
                }

                // Primary restoration is a precondition for accepting an idle
                // provider turn. Do it before acknowledging the input so a
                // lock, auth, registry, invariant, or persistence failure is
                // returned to the caller instead of losing an already-ACKed
                // prompt before it ever reaches AgentSession.
                if let Err(err) = maybe_restore_primary(
                    Arc::clone(&session),
                    Arc::clone(&shared_state),
                    out_tx.clone(),
                    &options,
                    &cx,
                )
                .await
                {
                    let _ = out_tx.send(response_error_with_hints(id, "prompt", &err));
                    continue;
                }
                if let Err(err) = sync_runtime_before_rpc_ack(&session, &cx).await {
                    let _ = out_tx.send(response_error_with_hints(id, "prompt", &err));
                    continue;
                }

                // Acknowledge only after all pre-turn fallible transitions.
                let _ = out_tx.send(response_ok(id, "prompt", None));

                is_streaming.store(true, Ordering::SeqCst);

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let shared_state = Arc::clone(&shared_state);
                let is_streaming = Arc::clone(&is_streaming);
                let is_compacting = Arc::clone(&is_compacting);
                let turn_phase_linearizer = Arc::clone(&turn_phase_linearizer);
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
                                turn_phase_linearizer,
                                abort_handle_slot,
                                out_tx,
                                retry_abort,
                                options,
                                expanded,
                                Some(message),
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
                match rpc_turn_phase(&is_streaming, &is_compacting) {
                    RpcTurnPhase::Compacting => {
                        let resp = response_error(
                            id,
                            "steer",
                            "Agent is currently compacting; wait before steering".to_string(),
                        );
                        let _ = out_tx.send(resp);
                        continue;
                    }
                    RpcTurnPhase::Idle => {}
                    RpcTurnPhase::Streaming => {
                        let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                            .await
                            .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                        let _phase_guard = lock_rpc_turn_phase(&turn_phase_linearizer);
                        let result = match rpc_turn_phase(&is_streaming, &is_compacting) {
                            RpcTurnPhase::Streaming => {
                                state.push_steering(QueuedAgentMessage::authored(
                                    build_user_message(&expanded, &[]),
                                    message.clone(),
                                ))
                            }
                            RpcTurnPhase::Compacting => Err(Error::session(
                                "Agent began compacting before steering could be queued",
                            )),
                            RpcTurnPhase::Idle => Err(Error::session(
                                "Agent stopped streaming before steering could be queued",
                            )),
                        };

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
                }

                if let Err(err) = maybe_restore_primary(
                    Arc::clone(&session),
                    Arc::clone(&shared_state),
                    out_tx.clone(),
                    &options,
                    &cx,
                )
                .await
                {
                    let _ = out_tx.send(response_error_with_hints(id, "steer", &err));
                    continue;
                }
                if let Err(err) = sync_runtime_before_rpc_ack(&session, &cx).await {
                    let _ = out_tx.send(response_error_with_hints(id, "steer", &err));
                    continue;
                }

                let _ = out_tx.send(response_ok(id, "steer", None));

                is_streaming.store(true, Ordering::SeqCst);

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let shared_state = Arc::clone(&shared_state);
                let is_streaming = Arc::clone(&is_streaming);
                let is_compacting = Arc::clone(&is_compacting);
                let turn_phase_linearizer = Arc::clone(&turn_phase_linearizer);
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
                        turn_phase_linearizer,
                        abort_handle_slot,
                        out_tx,
                        retry_abort,
                        options,
                        expanded,
                        Some(message),
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
                match rpc_turn_phase(&is_streaming, &is_compacting) {
                    RpcTurnPhase::Compacting => {
                        let resp = response_error(
                            id,
                            "follow_up",
                            "Agent is currently compacting; wait before sending a follow-up"
                                .to_string(),
                        );
                        let _ = out_tx.send(resp);
                        continue;
                    }
                    RpcTurnPhase::Idle => {}
                    RpcTurnPhase::Streaming => {
                        let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                            .await
                            .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                        let _phase_guard = lock_rpc_turn_phase(&turn_phase_linearizer);
                        let result = match rpc_turn_phase(&is_streaming, &is_compacting) {
                            RpcTurnPhase::Streaming => {
                                state.push_follow_up(QueuedAgentMessage::authored(
                                    build_user_message(&expanded, &[]),
                                    message.clone(),
                                ))
                            }
                            RpcTurnPhase::Compacting => Err(Error::session(
                                "Agent began compacting before the follow-up could be queued",
                            )),
                            RpcTurnPhase::Idle => Err(Error::session(
                                "Agent stopped streaming before the follow-up could be queued",
                            )),
                        };

                        match result {
                            Ok(()) => {
                                let _ = out_tx.send(response_ok(id, "follow_up", None));
                            }
                            Err(err) => {
                                let _ =
                                    out_tx.send(response_error_with_hints(id, "follow_up", &err));
                            }
                        }
                        continue;
                    }
                }

                if let Err(err) = maybe_restore_primary(
                    Arc::clone(&session),
                    Arc::clone(&shared_state),
                    out_tx.clone(),
                    &options,
                    &cx,
                )
                .await
                {
                    let _ = out_tx.send(response_error_with_hints(id, "follow_up", &err));
                    continue;
                }
                if let Err(err) = sync_runtime_before_rpc_ack(&session, &cx).await {
                    let _ = out_tx.send(response_error_with_hints(id, "follow_up", &err));
                    continue;
                }

                let _ = out_tx.send(response_ok(id, "follow_up", None));

                is_streaming.store(true, Ordering::SeqCst);

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let shared_state = Arc::clone(&shared_state);
                let is_streaming = Arc::clone(&is_streaming);
                let is_compacting = Arc::clone(&is_compacting);
                let turn_phase_linearizer = Arc::clone(&turn_phase_linearizer);
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
                        turn_phase_linearizer,
                        abort_handle_slot,
                        out_tx,
                        retry_abort,
                        options,
                        expanded,
                        Some(message),
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

            "set_plan_mode" => {
                // bd-cv653.3.5: enable/disable plan mode. mode: "on"|"off".
                let mode = parsed
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("on")
                    .to_ascii_lowercase();
                let Ok(guard) = OwnedMutexGuard::lock(Arc::clone(&session), &cx).await else {
                    let _ = out_tx.send(response_error(id, "set_plan_mode", "session lock failed"));
                    continue;
                };
                let plan_state = guard.agent.plan_state();
                let enabled = !matches!(mode.as_str(), "off" | "false" | "0");
                let Ok(mut inner) = guard.session.lock(cx.cx()).await else {
                    let _ = out_tx.send(response_error(id, "set_plan_mode", "session busy"));
                    continue;
                };
                if enabled {
                    plan_state.enter_planning();
                    inner.append_custom_entry(
                        "plan_mode".to_string(),
                        Some(json!({"mode": "planning"})),
                    );
                } else {
                    plan_state.exit();
                    inner
                        .append_custom_entry("plan_mode".to_string(), Some(json!({"mode": "off"})));
                }
                drop(inner);
                drop(guard);
                let _ = out_tx.send(response_ok(
                    id,
                    "set_plan_mode",
                    Some(json!({"planMode": if enabled { "planning" } else { "off" }})),
                ));
            }

            "approve_plan" => {
                let Ok(guard) = OwnedMutexGuard::lock(Arc::clone(&session), &cx).await else {
                    let _ = out_tx.send(response_error(id, "approve_plan", "session lock failed"));
                    continue;
                };
                let plan_state = guard.agent.plan_state();
                match plan_state.approve() {
                    Some(plan) => {
                        if let Ok(mut inner) = guard.session.lock(cx.cx()).await {
                            inner.append_custom_entry(
                                "plan_mode".to_string(),
                                Some(json!({"mode": "approved"})),
                            );
                        }
                        let _ = out_tx.send(response_ok(
                            id,
                            "approve_plan",
                            Some(json!({"approved": true, "plan": plan})),
                        ));
                    }
                    None => {
                        let _ = out_tx.send(response_error(
                            id,
                            "approve_plan",
                            "no submitted plan to approve",
                        ));
                    }
                }
            }

            "reject_plan" => {
                let Ok(guard) = OwnedMutexGuard::lock(Arc::clone(&session), &cx).await else {
                    let _ = out_tx.send(response_error(id, "reject_plan", "session lock failed"));
                    continue;
                };
                let plan_state = guard.agent.plan_state();
                if plan_state.reject() {
                    if let Ok(mut inner) = guard.session.lock(cx.cx()).await {
                        inner.append_custom_entry(
                            "plan_mode".to_string(),
                            Some(json!({"mode": "rejected"})),
                        );
                    }
                    let _ = out_tx.send(response_ok(
                        id,
                        "reject_plan",
                        Some(json!({"rejected": true})),
                    ));
                } else {
                    let _ = out_tx.send(response_error(
                        id,
                        "reject_plan",
                        "no submitted plan to reject",
                    ));
                }
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
                    let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let inner_session = guard.session.lock(&cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    session_stats(&inner_session, guard.save_enabled())
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
                    let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                    state.bind_provider_admission(guard.provider_admission_gate());
                    state.ensure_session_advancement_allowed()?;
                    let provider_impl = providers::create_provider(
                        &entry,
                        guard
                            .extensions
                            .as_ref()
                            .map(crate::extensions::ExtensionRegion::manager),
                    )?;
                    let current_thinking = guard
                        .agent
                        .stream_options()
                        .thinking_level
                        .unwrap_or_default();
                    let clamped_level = entry.clamp_thinking_level(current_thinking);
                    let _provider_transition =
                        apply_model_change(&mut guard, &mut state, &entry, clamped_level).await?;

                    guard.agent.set_provider(provider_impl);
                    guard.agent.set_keyword_max_thinking_level(
                        entry.clamp_thinking_level(crate::model::ThinkingLevel::Max),
                    );
                    guard.agent.set_tool_call_dialect(entry.tool_call_dialect());
                    guard
                        .agent
                        .set_model_accepts_images(entry.model.input.contains(&InputType::Image));
                    {
                        let stream_options = guard.agent.stream_options_mut();
                        stream_options.api_key.clone_from(&key);
                        stream_options.headers.clone_from(&entry.headers);
                        stream_options.max_tokens = Some(entry.model.max_tokens);
                        stream_options.thinking_level = Some(clamped_level);
                    }
                    guard.set_compaction_context_window(context_window_tokens_for_entry(&entry));
                    guard.refresh_extension_completion_host_state();
                    if let Some(region) = &guard.extensions {
                        region.manager().set_current_model(
                            Some(entry.model.provider.clone()),
                            Some(entry.model.id.clone()),
                        );
                    }
                    state.clear_failover_lifecycle();
                    state.provider_admission.clear();
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
                let result: Result<Option<(ModelEntry, crate::model::ThinkingLevel, bool)>> =
                    async {
                        {
                            let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                                .await
                                .map_err(|err| {
                                    Error::session(format!("session lock failed: {err}"))
                                })?;
                            let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                                .await
                                .map_err(|err| {
                                    Error::session(format!("state lock failed: {err}"))
                                })?;
                            state.ensure_session_advancement_allowed()?;
                            cycle_model_for_rpc(&mut guard, &mut state, &options).await
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
                let result = apply_thinking_level_for_session(
                    Arc::clone(&session),
                    Arc::clone(&shared_state),
                    clamped_level,
                    &cx,
                )
                .await;

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
                if let Err(err) =
                    apply_thinking_level(Arc::clone(&session), Arc::clone(&shared_state), next)
                        .await
                {
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
                let origin_session_id = {
                    let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let inner = guard.session.lock(&cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    inner.header.id.clone()
                };
                *running = Some(RunningBash {
                    id: run_id.clone(),
                    abort_tx: Some(abort_tx),
                });

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let shared_state = Arc::clone(&shared_state);
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
                            // then shared state then inner `Session`), and the
                            // inner guard is still released before the persist
                            // step below.
                            let (should_persist, append_error) = if let Ok(guard) =
                                OwnedMutexGuard::lock(Arc::clone(&session), &bash_cx).await
                            {
                                match OwnedMutexGuard::lock(
                                    Arc::clone(&shared_state),
                                    &bash_cx,
                                )
                                .await
                                {
                                    Ok(state) if state.provider_admission.reason().is_some() => (
                                        false,
                                        Some(
                                            "session persistence is quarantined after an indeterminate provider transition"
                                                .to_string(),
                                        ),
                                    ),
                                    Ok(_state) => {
                                        if let Ok(mut inner_session) =
                                            guard.session.lock(&bash_cx).await
                                        {
                                            if inner_session.header.id == origin_session_id {
                                                inner_session.append_message(
                                                    SessionMessage::BashExecution {
                                                        command: command.clone(),
                                                        output: result.output.clone(),
                                                        exit_code: result.exit_code,
                                                        cancelled: Some(result.cancelled),
                                                        truncated: Some(result.truncated),
                                                        full_output_path: result
                                                            .full_output_path
                                                            .clone(),
                                                        timestamp: Some(
                                                            chrono::Utc::now().timestamp_millis(),
                                                        ),
                                                        extra: std::collections::HashMap::default(),
                                                    },
                                                );
                                                (true, None)
                                            } else {
                                                (
                                                    false,
                                                    Some(
                                                        "active session changed while bash was running"
                                                            .to_string(),
                                                    ),
                                                )
                                            }
                                        } else {
                                            (
                                                false,
                                                Some("inner session lock poisoned".to_string()),
                                            )
                                        }
                                    }
                                    Err(_) => (
                                        false,
                                        Some("shared state lock failed".to_string()),
                                    ),
                                }
                            } else {
                                (false, Some("outer session lock failed".to_string()))
                            };

                            let (persisted, persistence_warning, persistence_status) =
                                if should_persist {
                                    if let Ok(mut guard) =
                                        OwnedMutexGuard::lock(Arc::clone(&session), &bash_cx).await
                                    {
                                        match OwnedMutexGuard::lock(
                                            Arc::clone(&shared_state),
                                            &bash_cx,
                                        )
                                        .await
                                        {
                                            Ok(state)
                                                if state.provider_admission.reason().is_some() =>
                                            {
                                                (
                                                    false,
                                                    Some(
                                                        "Session persistence is quarantined after an indeterminate provider transition"
                                                            .to_string(),
                                                    ),
                                                    json!({
                                                        "event": "session.persistence.quarantined",
                                                        "severity": "error",
                                                        "summary": "Bash history persistence was refused after an indeterminate provider transition.",
                                                        "action": "Restart and reconcile the session before further mutation.",
                                                        "sliIds": ["sli_failure_recovery_success_rate"],
                                                        "pendingMessageCount": null,
                                                    }),
                                                )
                                            }
                                            Ok(_state) if guard.save_enabled() => {
                                                let persist_result = guard.persist_session().await;
                                                let pending_message_count = guard
                                                    .session
                                                    .lock(&bash_cx)
                                                    .await
                                                    .map_or(Value::Null, |inner_session| {
                                                        json!(
                                                            inner_session
                                                                .autosave_metrics()
                                                                .pending_mutations
                                                        )
                                                    });
                                                match persist_result {
                                                    Ok(()) => (
                                                        true,
                                                        None,
                                                        json!({
                                                            "event": "session.persistence.healthy",
                                                            "severity": "ok",
                                                            "summary": "Session history persisted.",
                                                            "action": "No action required.",
                                                            "sliIds": ["sli_resume_ready_p95_ms"],
                                                            "pendingMessageCount": pending_message_count,
                                                        }),
                                                    ),
                                                    Err(err) => {
                                                        tracing::warn!(
                                                            error = %err,
                                                            "Failed to persist bash execution history to session"
                                                        );
                                                        (
                                                            false,
                                                            Some(format!("Failed to persist bash execution to session: {err}")),
                                                            json!({
                                                                "event": "session.persistence.backlog",
                                                                "severity": "warning",
                                                                "summary": "Session history persistence failed after bash execution.",
                                                                "action": "Trigger manual save or verify session storage permissions.",
                                                                "sliIds": ["sli_resume_ready_p95_ms", "sli_failure_recovery_success_rate"],
                                                                "pendingMessageCount": pending_message_count,
                                                                "errorMessage": err.to_string(),
                                                            }),
                                                        )
                                                    }
                                                }
                                            }
                                            Ok(_state) => {
                                                let pending_message_count = guard
                                                    .session
                                                    .lock(&bash_cx)
                                                    .await
                                                    .map_or(Value::Null, |inner_session| {
                                                        json!(
                                                            inner_session
                                                                .autosave_metrics()
                                                                .pending_mutations
                                                        )
                                                    });
                                                (
                                                    false,
                                                    None,
                                                    json!({
                                                        "event": "session.persistence.disabled",
                                                        "severity": "info",
                                                        "summary": "Session persistence is disabled; bash history is retained in memory only.",
                                                        "action": "Enable session saving to make command history durable.",
                                                        "sliIds": [],
                                                        "pendingMessageCount": pending_message_count,
                                                    }),
                                                )
                                            }
                                            Err(_) => (
                                                false,
                                                Some(
                                                    "Failed to acquire shared state lock for persistence"
                                                        .to_string(),
                                                ),
                                                json!({
                                                    "event": "session.persistence.backlog",
                                                    "severity": "warning",
                                                    "summary": "Shared state lock acquisition failed after bash execution.",
                                                    "action": "Check session concurrency.",
                                                    "sliIds": ["sli_resume_ready_p95_ms", "sli_failure_recovery_success_rate"],
                                                    "pendingMessageCount": null,
                                                }),
                                            ),
                                        }
                                    } else {
                                        (
                                            false,
                                            Some(
                                                "Failed to acquire session lock for persistence"
                                                    .to_string(),
                                            ),
                                            json!({
                                                "event": "session.persistence.backlog",
                                                "severity": "warning",
                                                "summary": "Session lock acquisition failed after bash execution.",
                                                "action": "Check session concurrency.",
                                                "sliIds": ["sli_resume_ready_p95_ms", "sli_failure_recovery_success_rate"],
                                                "pendingMessageCount": null,
                                            }),
                                        )
                                    }
                                } else {
                                    (
                                        false,
                                        append_error.as_deref().map(|e| {
                                            format!(
                                                "Failed to append bash execution to session: {e}"
                                            )
                                        }),
                                        json!({
                                            "event": "session.history.append_failed",
                                            "severity": "error",
                                            "summary": "Failed to append bash execution message to session.",
                                            "action": "Check session health.",
                                            "sliIds": ["sli_failure_recovery_success_rate"],
                                            "pendingMessageCount": null,
                                        }),
                                    )
                                };

                            let mut payload = json!({
                                "output": result.output,
                                "exitCode": result.exit_code,
                                "cancelled": result.cancelled,
                                "truncated": result.truncated,
                                "fullOutputPath": result.full_output_path,
                                "persisted": persisted,
                                "persistenceStatus": persistence_status,
                            });
                            if let Some(warn) = persistence_warning {
                                payload["persistenceWarning"] = json!(warn);
                            }

                            response_ok(
                                id_clone,
                                "bash",
                                Some(payload),
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
                if let Some(running_bash) = running.as_mut() {
                    running_bash.request_abort(&cx);
                }
                let _ = out_tx.send(response_ok(id, "abort_bash", None));
            }

            "compact" => {
                if rpc_turn_phase(&is_streaming, &is_compacting) != RpcTurnPhase::Idle {
                    let _ = out_tx.send(response_error(
                        id,
                        "compact",
                        "Agent is currently busy; wait before compacting".to_string(),
                    ));
                    continue;
                }
                let _compacting_guard = ClearFlagOnDrop(Arc::clone(&is_compacting));
                is_compacting.store(true, Ordering::SeqCst);
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
                    let provider_admission = guard.provider_admission_gate();
                    guard.invalidate_background_compaction();
                    let _provider_call = provider_admission.acquire(&cx).await?;
                    provider_admission.ensure_allowed()?;
                    let session_store = Arc::clone(&guard.session);
                    let mut inner_session = OwnedMutexGuard::lock(session_store, &cx)
                        .await
                        .map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                    let mut candidate = inner_session.clone();
                    candidate.ensure_entry_ids();
                    let path_entries = candidate
                        .entries_for_current_path()
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>();

                    let key = guard
                        .agent
                        .stream_options()
                        .api_key
                        .clone()
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

                    let compact_res = compact(
                        prep,
                        provider,
                        &key,
                        custom_instructions.as_deref(),
                    )
                    .await;
                    let result_data = compact_res?;

                    let details_value = compaction_details_to_value(&result_data.details)?;
                    let details_value = match result_data.snap_payload.as_ref() {
                        Some(payload) => {
                            crate::compaction_snap::payload_to_details(Some(details_value), payload)
                        }
                        None => details_value,
                    };

                    candidate.append_compaction(
                        result_data.summary.clone(),
                        result_data.first_kept_entry_id.clone(),
                        result_data.tokens_before,
                        Some(details_value.clone()),
                        None,
                    );
                    // Post-compaction context estimate (heuristic, ignores usage).
                    let tokens_after = crate::compaction::estimate_entries_context_tokens(
                        &candidate.entries_for_current_path(),
                    );
                    let messages = candidate.to_messages_for_current_path();
                    let save_enabled = guard.save_enabled();
                    if save_enabled {
                        provider_admission.block(
                            "manual compaction persistence was interrupted before live installation completed"
                                .to_string(),
                        );
                        if let Err(first_err) = candidate.save().await
                            && let Err(retry_err) = candidate.save().await
                        {
                            let reason = format!(
                                "manual compaction persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
                            );
                            provider_admission.block(reason.clone());
                            return Err(Error::session_persistence(reason));
                        }
                    }
                    *inner_session = candidate;
                    guard.agent.replace_messages(messages);
                    if save_enabled {
                        provider_admission.clear();
                    }

                    Ok(json!({
                        "summary": result_data.summary,
                        "firstKeptEntryId": result_data.first_kept_entry_id,
                        "tokensBefore": result_data.tokens_before,
                        "tokensAfter": tokens_after,
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

            "checkpoint" => {
                let name = parsed
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("checkpoint")
                    .to_string();
                let note = parsed
                    .get("note")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let result: Result<Value> = async {
                    let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let messages = guard.agent.messages().to_vec();
                    let checkpoint_payload = {
                        let mut inner = guard.session.lock(&cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        let checkpoint = crate::checkpoint::mark_checkpoint(
                            &mut inner,
                            &name,
                            note.as_deref(),
                            &messages,
                        );
                        serde_json::to_value(&checkpoint)?
                    };
                    guard.persist_session().await?;
                    Ok(checkpoint_payload)
                }
                .await;
                match result {
                    Ok(data) => {
                        let _ = out_tx.send(response_ok(id, "checkpoint", Some(data)));
                    }
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "checkpoint", &err));
                    }
                }
            }

            "rewind" => {
                let name = parsed
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let result: Result<Value> = async {
                    let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                    state.bind_provider_admission(guard.provider_admission_gate());
                    state.ensure_session_advancement_allowed()?;
                    let session_store = Arc::clone(&guard.session);
                    let mut inner = OwnedMutexGuard::lock(session_store, &cx)
                        .await
                        .map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                    let checkpoint = crate::checkpoint::find_checkpoint(&inner, name.as_deref());
                    let Some(checkpoint) = checkpoint else {
                        return Err(Error::validation(name.as_ref().map_or_else(
                            || "No checkpoints yet".to_string(),
                            |name| format!("No checkpoint named '{name}'"),
                        )));
                    };
                    let messages = guard.agent.messages().to_vec();
                    let span: Vec<Message> =
                        messages[checkpoint.message_count.min(messages.len())..].to_vec();
                    if span.is_empty() {
                        return Ok(json!({
                            "checkpoint": checkpoint.name,
                            "collapsedMessages": 0,
                            "note": "active context already at checkpoint"
                        }));
                    }
                    guard.invalidate_background_compaction();
                    let _provider_transition = state.provider_admission.acquire(&cx).await?;
                    state.provider_admission.ensure_allowed()?;
                    let provider = guard.agent.provider();
                    // Keyless providers (replay/test/local) summarize fine
                    // without a key; credentialed ones carry theirs.
                    let api_key = guard
                        .agent
                        .stream_options()
                        .api_key
                        .clone()
                        .unwrap_or_default();
                    let settings = crate::compaction::ResolvedCompactionSettings {
                        enabled: true,
                        ..Default::default()
                    };
                    let summary =
                        crate::checkpoint::summarize_span(&span, provider, &api_key, &settings)
                            .await
                            .unwrap_or_else(|err| {
                                format!("(summarization failed: {err}; collapsed without a report)")
                            });
                    let mut agent_messages = messages;
                    agent_messages.truncate(checkpoint.message_count.min(agent_messages.len()));
                    let collapsed = span.len();
                    if !summary.is_empty() {
                        agent_messages.push(Message::User(UserMessage {
                            content: UserContent::Text(crate::checkpoint::rewind_report_text(
                                &checkpoint.name,
                                &summary,
                            )),
                            timestamp: 0,
                        }));
                    }
                    let outcome = crate::checkpoint::RewindOutcome {
                        schema: crate::checkpoint::CHECKPOINT_SCHEMA.to_string(),
                        checkpoint: checkpoint.name.clone(),
                        checkpoint_entry_id: checkpoint.entry_id.clone(),
                        collapsed_messages: collapsed,
                        summary: summary.clone(),
                        summary_tokens_estimate: (summary.len() / 4) as u64,
                        tree_preserved: true,
                    };
                    let mut candidate = inner.clone();
                    candidate.append_custom_entry(
                        "rewind".to_string(),
                        Some(serde_json::to_value(&outcome).unwrap_or_default()),
                    );
                    let save_enabled = guard.save_enabled();
                    if save_enabled {
                        state.provider_admission.block(
                            "rewind persistence was interrupted before live installation completed"
                                .to_string(),
                        );
                    }
                    if save_enabled
                        && let Err(first_err) = candidate.save().await
                            && let Err(retry_err) = candidate.save().await
                        {
                            let reason = format!(
                                "rewind persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
                            );
                            state.provider_admission.block(reason.clone());
                            return Err(Error::session_persistence(reason));
                        }
                    guard.invalidate_background_compaction();
                    *inner = candidate;
                    guard.agent.replace_messages(agent_messages);
                    if save_enabled {
                        state.provider_admission.clear();
                    }
                    Ok(serde_json::to_value(&outcome)?)
                }
                .await;
                match result {
                    Ok(data) => {
                        let _ = out_tx.send(response_ok(id, "rewind", Some(data)));
                    }
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "rewind", &err));
                    }
                }
            }

            "fresh" => {
                let result: Result<Value> = async {
                    let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                    state.bind_provider_admission(guard.provider_admission_gate());
                    state.ensure_session_advancement_allowed()?;
                    // uuid suffix: a bare millisecond stamp can collide
                    // across rapid calls, defeating the cache-reset purpose.
                    let new_id = format!(
                        "fresh-{}-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_millis()),
                        uuid::Uuid::new_v4().simple()
                    );
                    let session_store = Arc::clone(&guard.session);
                    let mut inner = OwnedMutexGuard::lock(session_store, &cx)
                        .await
                        .map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                    let mut candidate = inner.clone();
                    candidate.append_custom_entry(
                        "fresh".to_string(),
                        Some(json!({
                            "schema": "pi.fresh.v1",
                            "newSessionId": new_id,
                            "reason": "operator fresh: provider cache + stream bookkeeping reset",
                        })),
                    );
                    let save_enabled = guard.save_enabled();
                    let _provider_transition = state
                        .provider_admission
                        .begin_transition(
                            "fresh-session persistence was interrupted before live installation completed"
                                .to_string(),
                            &cx,
                        )
                        .await?;
                    if save_enabled
                        && let Err(first_err) = candidate.save().await
                            && let Err(retry_err) = candidate.save().await
                        {
                            let reason = format!(
                                "fresh-session persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
                            );
                            state.provider_admission.block(reason.clone());
                            return Err(Error::session_persistence(reason));
                        }
                    *inner = candidate;
                    crate::app::rebind_stream_options_session(guard.agent.stream_options_mut(), &new_id);
                    guard.refresh_extension_completion_host_state();
                    state.provider_admission.clear();
                    Ok(json!({ "schema": "pi.fresh.v1", "newSessionId": new_id }))
                }
                .await;
                match result {
                    Ok(data) => {
                        let _ = out_tx.send(response_ok(id, "fresh", Some(data)));
                    }
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "fresh", &err));
                    }
                }
            }

            "retry" => {
                // Retry re-EXECUTES the recovered turn immediately: queueing
                // it as steering would silently wait for (and then pollute)
                // the next unrelated prompt.
                match rpc_turn_phase(&is_streaming, &is_compacting) {
                    RpcTurnPhase::Compacting => {
                        let _ = out_tx.send(response_error(
                            id,
                            "retry",
                            "Agent is currently compacting; wait before retrying".to_string(),
                        ));
                        continue;
                    }
                    RpcTurnPhase::Streaming => {
                        let _ = out_tx.send(response_error(
                            id,
                            "retry",
                            "Agent is currently streaming; abort or wait before retrying"
                                .to_string(),
                        ));
                        continue;
                    }
                    RpcTurnPhase::Idle => {}
                }
                if let Err(err) = maybe_restore_primary(
                    Arc::clone(&session),
                    Arc::clone(&shared_state),
                    out_tx.clone(),
                    &options,
                    &cx,
                )
                .await
                {
                    let _ = out_tx.send(response_error_with_hints(id, "retry", &err));
                    continue;
                }
                if let Err(err) = sync_runtime_before_rpc_ack(&session, &cx).await {
                    let _ = out_tx.send(response_error_with_hints(id, "retry", &err));
                    continue;
                }
                let retry_turn = {
                    let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(&session), &cx).await
                    else {
                        let _ = out_tx.send(response_error(
                            id,
                            "retry",
                            "session lock failed".to_string(),
                        ));
                        continue;
                    };
                    take_last_rpc_user_turn_for_retry(&mut guard).await
                };
                let text = match retry_turn {
                    Ok(Some(text)) => text,
                    Ok(None) => {
                        let _ = out_tx.send(response_error(
                            id,
                            "retry",
                            "No user turn to retry".to_string(),
                        ));
                        continue;
                    }
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "retry", &err));
                        continue;
                    }
                };
                let _ = out_tx.send(response_ok(
                    id,
                    "retry",
                    Some(json!({
                        "schema": "pi.retry.v1",
                        "rerunning": true,
                        "characters": text.len()
                    })),
                ));
                is_streaming.store(true, Ordering::SeqCst);
                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let shared_state = Arc::clone(&shared_state);
                let is_streaming = Arc::clone(&is_streaming);
                let is_compacting = Arc::clone(&is_compacting);
                let turn_phase_linearizer = Arc::clone(&turn_phase_linearizer);
                let abort_handle_slot = Arc::clone(&abort_handle);
                let retry_abort = retry_abort.clone();
                let options = options.clone();
                let prompt_cx = cx.clone();
                options.runtime_handle.clone().spawn(future_with_current_cx(
                    prompt_cx.cx().clone(),
                    async move {
                        run_prompt_with_retry(
                            session,
                            shared_state,
                            is_streaming,
                            is_compacting,
                            turn_phase_linearizer,
                            abort_handle_slot,
                            out_tx,
                            retry_abort,
                            options,
                            text,
                            Some(String::new()),
                            Vec::new(),
                            prompt_cx,
                        )
                        .await;
                    },
                ));
            }

            "new_session" => {
                if let Some(reason) = rpc_session_transition_blocker(
                    &is_streaming,
                    &is_compacting,
                    &turn_phase_linearizer,
                    &session,
                    &shared_state,
                    &bash_state,
                    &cx,
                )
                .await?
                {
                    let _ = out_tx.send(response_error(id, "new_session", reason.to_string()));
                    continue;
                }
                let transition_baseline = rpc_session_transition_snapshot(&session, &cx).await?;
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
                let session_transition = match acquire_rpc_session_transition(
                    &transition_baseline,
                    &is_streaming,
                    &is_compacting,
                    &turn_phase_linearizer,
                    &session,
                    &shared_state,
                    &bash_state,
                    &cx,
                )
                .await
                {
                    Ok(permit) => permit,
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "new_session", &err));
                        continue;
                    }
                };
                let RpcSessionTransitionAuthority {
                    session: mut guard,
                    permits: session_transition_permit,
                } = session_transition;

                let parent = parsed
                    .get("parentSession")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let result: Result<(String, Option<String>)> = async {
                    let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                    state.bind_provider_admission(guard.provider_admission_gate());
                    state.ensure_session_advancement_allowed()?;
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
                    guard.invalidate_background_compaction();
                    state.provider_admission.ensure_allowed()?;
                    {
                        let session_store = Arc::clone(&guard.session);
                        let mut inner_session = OwnedMutexGuard::lock(session_store, &cx)
                            .await
                            .map_err(|err| {
                                Error::session(format!("inner session lock failed: {err}"))
                            })?;
                        *inner_session = new_session;
                        session_transition_permit.commit_session_change();
                    }
                    guard.agent.clear_messages();
                    crate::app::rebind_stream_options_session(
                        guard.agent.stream_options_mut(),
                        &session_id,
                    );
                    guard
                        .agent
                        .reset_session_scoped_state(crate::plan::PlanMode::Off);
                    guard.refresh_extension_completion_host_state();
                    if let Some(region) = &guard.extensions {
                        region.manager().invalidate_ctx_cache();
                    }
                    state.clear_all_pending();
                    state.clear_failover_lifecycle();

                    Ok((session_id, previous_session_file))
                }
                .await;
                drop(guard);
                drop(session_transition_permit);
                match result {
                    Ok((session_id, previous_session_file)) => {
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
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "new_session", &err));
                    }
                }
            }

            "switch_session" => {
                if let Some(reason) = rpc_session_transition_blocker(
                    &is_streaming,
                    &is_compacting,
                    &turn_phase_linearizer,
                    &session,
                    &shared_state,
                    &bash_state,
                    &cx,
                )
                .await?
                {
                    let _ = out_tx.send(response_error(id, "switch_session", reason.to_string()));
                    continue;
                }
                let Some(session_path) = parsed.get("sessionPath").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "switch_session",
                        "Missing sessionPath".to_string(),
                    ));
                    continue;
                };
                let transition_baseline = rpc_session_transition_snapshot(&session, &cx).await?;

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
                    Ok(mut new_session) => {
                        let session_transition = match acquire_rpc_session_transition(
                            &transition_baseline,
                            &is_streaming,
                            &is_compacting,
                            &turn_phase_linearizer,
                            &session,
                            &shared_state,
                            &bash_state,
                            &cx,
                        )
                        .await
                        {
                            Ok(authority) => authority,
                            Err(err) => {
                                let _ = out_tx.send(response_error_with_hints(
                                    id,
                                    "switch_session",
                                    &err,
                                ));
                                continue;
                            }
                        };
                        let RpcSessionTransitionAuthority {
                            session: mut guard,
                            permits: session_transition_permit,
                        } = session_transition;
                        let target_session_file = new_session.path.as_ref().map_or_else(
                            || resolved_path.display().to_string(),
                            |p| p.display().to_string(),
                        );
                        let result: Result<(Option<String>, String)> = async {
                            // Acquire every fallible transition authority and
                            // prepare the target provider before replacing the
                            // live Session. After the assignment below, only
                            // infallible in-memory installation remains.
                            let mut state =
                                OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                                    .await
                                    .map_err(|err| {
                                        Error::session(format!("state lock failed: {err}"))
                                    })?;
                            state.bind_provider_admission(guard.provider_admission_gate());
                            state.ensure_session_advancement_allowed()?;

                            let requested_model = new_session
                                .effective_model_for_current_path()
                                .unwrap_or_else(|| {
                                    let provider = guard.agent.provider();
                                    (
                                        provider.name().to_string(),
                                        provider.model_id().to_string(),
                                    )
                                });
                            let entry = model_entry_for_provider_and_id(
                                &requested_model.0,
                                &requested_model.1,
                                &options,
                            )
                                .cloned()
                                .or_else(|| {
                                    crate::models::ad_hoc_model_entry(
                                        &requested_model.0,
                                        &requested_model.1,
                                    )
                                })
                                .ok_or_else(|| {
                                    Error::validation(format!(
                                        "Unable to switch Session runtime to {}/{}",
                                        requested_model.0, requested_model.1
                                    ))
                                })?;
                            let key = resolve_model_key(
                                options.cli_api_key.as_deref(),
                                &options.auth,
                                &entry,
                            );
                            if model_requires_configured_credential(&entry) && key.is_none() {
                                return Err(Error::auth(format!(
                                    "Missing credentials for resumed Session model {}/{}",
                                    requested_model.0, requested_model.1
                                )));
                            }
                            let provider_impl = providers::create_provider(
                                &entry,
                                guard
                                    .extensions
                                    .as_ref()
                                    .map(crate::extensions::ExtensionRegion::manager),
                            )?;
                            let (thinking, normalization_changed) =
                                normalize_resumed_session_model(&mut new_session, &entry);

                            // Acquire and validate the live commit target before
                            // writing normalized target bytes. Once persistence
                            // succeeds, no cancellation-aware/fallible step may
                            // remain between the durable and live transitions.
                            let session_store = Arc::clone(&guard.session);
                            let mut inner_session = OwnedMutexGuard::lock(session_store, &cx)
                                .await
                                .map_err(|err| {
                                    Error::session(format!("inner session lock failed: {err}"))
                                })?;
                            let previous_session_file =
                                inner_session.path.as_ref().map(|p| p.display().to_string());
                            guard.invalidate_background_compaction();
                            state.provider_admission.ensure_allowed()?;
                            if guard.save_enabled() && normalization_changed
                                && let Err(first_err) = new_session.save().await
                                    && let Err(retry_err) = new_session.save().await
                                {
                                    return Err(Error::session_persistence(format!(
                                        "resumed Session normalization remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
                                    )));
                                }

                            let target_plan_mode = replayed_plan_mode(&new_session);
                            let messages = new_session.to_messages_for_current_path();
                            let session_id = new_session.header.id.clone();

                            *inner_session = new_session;
                            session_transition_permit.commit_session_change();
                            drop(inner_session);
                            guard.agent.replace_messages(messages);
                            crate::app::rebind_stream_options_session(
                                guard.agent.stream_options_mut(),
                                &session_id,
                            );
                            guard.agent.reset_session_scoped_state(target_plan_mode);

                            guard.agent.set_provider(provider_impl);
                            guard.agent.set_keyword_max_thinking_level(
                                entry.clamp_thinking_level(crate::model::ThinkingLevel::Max),
                            );
                            guard
                                .agent
                                .set_tool_call_dialect(entry.tool_call_dialect());
                            guard.agent.set_model_accepts_images(
                                entry.model.input.contains(&InputType::Image),
                            );
                            {
                                let stream_options = guard.agent.stream_options_mut();
                                stream_options.api_key.clone_from(&key);
                                stream_options.headers.clone_from(&entry.headers);
                                stream_options.max_tokens = Some(entry.model.max_tokens);
                                stream_options.thinking_level = Some(thinking);
                            }
                            guard.set_compaction_context_window(
                                context_window_tokens_for_entry(&entry),
                            );
                            guard.refresh_extension_completion_host_state();
                            if let Some(region) = &guard.extensions {
                                region.manager().set_current_model(
                                    Some(entry.model.provider.clone()),
                                    Some(entry.model.id.clone()),
                                );
                            }
                            state.clear_all_pending();
                            state.clear_failover_lifecycle();
                            Ok((previous_session_file, session_id))
                        }
                        .await;
                        drop(guard);
                        drop(session_transition_permit);

                        match result {
                            Ok((previous_session_file, session_id)) => {
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
                                let _ = out_tx.send(response_error_with_hints(
                                    id,
                                    "switch_session",
                                    &err,
                                ));
                            }
                        }
                    }
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "switch_session", &err));
                    }
                }
            }

            "fork" => {
                if let Some(reason) = rpc_session_transition_blocker(
                    &is_streaming,
                    &is_compacting,
                    &turn_phase_linearizer,
                    &session,
                    &shared_state,
                    &bash_state,
                    &cx,
                )
                .await?
                {
                    let _ = out_tx.send(response_error(id, "fork", reason.to_string()));
                    continue;
                }
                let Some(entry_id) = parsed.get("entryId").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(id, "fork", "Missing entryId".to_string()));
                    continue;
                };
                let transition_baseline = match rpc_session_transition_snapshot(&session, &cx).await
                {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "fork", &err));
                        continue;
                    }
                };

                let result: Result<ForkCompletion> = async {
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

                    if rpc_dispatch_session_before_fork(
                        rpc_extension_manager.clone(),
                        entry_id,
                        &fork_plan.selected_text,
                        &header_snapshot.id,
                    )
                    .await
                    {
                        return Ok(None);
                    }

                    // Phase 2: Build new session without holding any lock.
                    let selected_text = fork_plan.selected_text.clone();
                    let previous_session_file = parent_path.clone();

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
                    let origin_session_id = header_snapshot.id;

                    // Phase 3: prepare and persist the complete target runtime,
                    // then atomically install Session + agent model state.
                    let session_id = {
                        let session_transition = acquire_rpc_session_transition(
                            &transition_baseline,
                            &is_streaming,
                            &is_compacting,
                            &turn_phase_linearizer,
                            &session,
                            &shared_state,
                            &bash_state,
                            &cx,
                        )
                        .await?;
                        let RpcSessionTransitionAuthority {
                            session: mut guard,
                            permits: session_transition_permit,
                        } = session_transition;
                        let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
                            .await
                            .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                        state.bind_provider_admission(guard.provider_admission_gate());
                        state.ensure_session_advancement_allowed()?;

                        let requested_model = new_session
                            .effective_model_for_current_path()
                            .unwrap_or_else(|| {
                                let provider = guard.agent.provider();
                                (
                                    provider.name().to_string(),
                                    provider.model_id().to_string(),
                                )
                            });
                        let entry = model_entry_for_provider_and_id(
                            &requested_model.0,
                            &requested_model.1,
                            &options,
                        )
                        .cloned()
                        .or_else(|| {
                            crate::models::ad_hoc_model_entry(
                                &requested_model.0,
                                &requested_model.1,
                            )
                        })
                        .ok_or_else(|| {
                            Error::validation(format!(
                                "Unable to install forked Session runtime {}/{}",
                                requested_model.0, requested_model.1
                            ))
                        })?;
                        let key = resolve_model_key(
                            options.cli_api_key.as_deref(),
                            &options.auth,
                            &entry,
                        );
                        if model_requires_configured_credential(&entry) && key.is_none() {
                            return Err(Error::auth(format!(
                                "Missing credentials for forked Session model {}/{}",
                                requested_model.0, requested_model.1
                            )));
                        }
                        let provider_impl = providers::create_provider(
                            &entry,
                            guard
                                .extensions
                                .as_ref()
                                .map(crate::extensions::ExtensionRegion::manager),
                        )?;
                        let (thinking, _) =
                            normalize_resumed_session_model(&mut new_session, &entry);

                        // Resolve and validate the live commit target before
                        // publishing the fork file. After save succeeds the
                        // assignment and runtime-state install are infallible.
                        let session_store = Arc::clone(&guard.session);
                        let mut inner = OwnedMutexGuard::lock(session_store, &cx)
                            .await
                            .map_err(|err| {
                                Error::session(format!("inner session lock failed: {err}"))
                            })?;
                        if inner.header.id != origin_session_id {
                            return Err(Error::session(
                                "active Session changed while the fork target was being prepared",
                            ));
                        }
                        guard.invalidate_background_compaction();
                        state.provider_admission.ensure_allowed()?;
                        if save_enabled
                            && let Err(first_err) = new_session.save().await
                                && let Err(retry_err) = new_session.save().await
                            {
                                return Err(Error::session_persistence(format!(
                                    "forked Session persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
                                )));
                            }

                        let target_plan_mode = replayed_plan_mode(&new_session);
                        let messages = new_session.to_messages_for_current_path();
                        let session_id = new_session.header.id.clone();
                        *inner = new_session;
                        session_transition_permit.commit_session_change();
                        drop(inner);
                        guard.agent.replace_messages(messages);
                        crate::app::rebind_stream_options_session(
                            guard.agent.stream_options_mut(),
                            &session_id,
                        );
                        guard.agent.reset_session_scoped_state(target_plan_mode);
                        guard.agent.set_provider(provider_impl);
                        guard.agent.set_keyword_max_thinking_level(
                            entry.clamp_thinking_level(crate::model::ThinkingLevel::Max),
                        );
                        guard
                            .agent
                            .set_tool_call_dialect(entry.tool_call_dialect());
                        guard
                            .agent
                            .set_model_accepts_images(entry.model.input.contains(&InputType::Image));
                        {
                            let stream_options = guard.agent.stream_options_mut();
                            stream_options.api_key.clone_from(&key);
                            stream_options.headers.clone_from(&entry.headers);
                            stream_options.max_tokens = Some(entry.model.max_tokens);
                            stream_options.thinking_level = Some(thinking);
                        }
                        guard.set_compaction_context_window(context_window_tokens_for_entry(&entry));
                        guard.refresh_extension_completion_host_state();
                        if let Some(region) = &guard.extensions {
                            region.manager().set_current_model(
                                Some(entry.model.provider.clone()),
                                Some(entry.model.id.clone()),
                            );
                        }
                        state.clear_all_pending();
                        state.clear_failover_lifecycle();
                        session_id
                    };

                    Ok(Some((
                        selected_text,
                        previous_session_file,
                        origin_session_id,
                        session_id,
                    )))
                }
                .await;

                match result {
                    Ok(None) => {
                        let _ = out_tx.send(response_ok(
                            id,
                            "fork",
                            Some(json!({ "cancelled": true })),
                        ));
                    }
                    Ok(Some((
                        selected_text,
                        previous_session_file,
                        source_session_id,
                        new_session_id,
                    ))) => {
                        rpc_dispatch_session_fork_event(
                            rpc_extension_manager.clone(),
                            json!({
                                "entryId": entry_id,
                                "summary": selected_text.clone(),
                                "sessionId": source_session_id,
                                "newSessionId": new_session_id,
                                "previousSessionFile": previous_session_file,
                            }),
                        )
                        .await;
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

            "ask_response" => {
                let Some(ask) = options.ask_tool.as_ref() else {
                    let _ = out_tx.send(response_error(
                        id,
                        "ask_response",
                        "The ask tool is not enabled in this session",
                    ));
                    continue;
                };
                match rpc_parse_ask_response(&parsed) {
                    Ok((request_id, response)) => {
                        let resolved = ask.respond_ui(&request_id, response);
                        let _ = out_tx.send(response_ok(
                            id,
                            "ask_response",
                            Some(json!({ "resolved": resolved })),
                        ));
                    }
                    Err(message) => {
                        let _ = out_tx.send(response_error(id, "ask_response", message));
                    }
                }
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
                    let Some(request_generation) =
                        rpc_parse_extension_ui_response_generation(&parsed)
                    else {
                        let _ = out_tx.send(response_error(
                            id,
                            "extension_ui_response",
                            "Missing requestGeneration field",
                        ));
                        continue;
                    };

                    let (response, next_request) = {
                        let mut guard = ui_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);

                        let Some(active) = guard.active.clone() else {
                            let _ = out_tx.send(response_error(
                                id,
                                "extension_ui_response",
                                "No active extension UI request",
                            ));
                            continue;
                        };

                        if active.request.id != request_id {
                            let _ = out_tx.send(response_error(
                                id,
                                "extension_ui_response",
                                format!(
                                    "Unexpected requestId: {request_id} (active: {})",
                                    active.request.id
                                ),
                            ));
                            continue;
                        }
                        if active.generation != request_generation {
                            let _ = out_tx.send(response_error(
                                id,
                                "extension_ui_response",
                                format!(
                                    "Unexpected requestGeneration: {request_generation} (active: {})",
                                    active.generation
                                ),
                            ));
                            continue;
                        }

                        let response =
                            match rpc_parse_extension_ui_response(&parsed, &active.request) {
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

                        let next = guard.finish_active();
                        (response, next)
                    };

                    let resolved = manager.respond_ui(response);
                    let _ = out_tx.send(response_ok(
                        id,
                        "extension_ui_response",
                        Some(json!({ "resolved": resolved })),
                    ));

                    if let Some(next) = next_request {
                        rpc_publish_extension_ui_request(
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

    // stdin has closed. No future ask_response frame can arrive, so dismiss
    // every pending picker before waiting for the in-flight turn to drain.
    // Otherwise a turn blocked in `ask` and this drain loop wait on each
    // other until the five-minute picker timeout expires.
    if let Some(ask) = options.ask_tool.as_ref() {
        ask.close_channel_ui();
    }

    // No future extension_ui_response frame can arrive either. Atomically
    // close the bridge before cancelling manager requests, then wait until the
    // forwarding task has observed the disconnected channel. This ordering
    // suppresses post-close publication, cancels every bridge timer, and also
    // covers an idle RPC session before the work-drain loop's early-exit check.
    if let Some(guard) = extension_ui_close_guard.as_ref() {
        guard.close();
    }
    if let Some(forwarder) = extension_ui_forwarder {
        forwarder.await;
    }

    // Drain any in-flight work (streaming turn, extension
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

    let preservation_cx = AgentCx::for_request();
    preserve_terminal_rpc_input(&session, &shared_state, &preservation_cx)
        .await
        .map_err(|err| {
            Error::session(format!(
                "RPC stdin closed before terminal session state could be preserved: {err}"
            ))
        })?;

    flush_rpc_session_on_shutdown(&session, &preservation_cx).await?;

    Ok(())
}

// =============================================================================
// Prompt Execution
// =============================================================================

fn rpc_recovery_message_fingerprint(
    message: &Message,
    timestamp_is_synthetic: bool,
) -> Result<Value> {
    let mut value = serde_json::to_value(message)?;
    if timestamp_is_synthetic && let Some(object) = value.as_object_mut() {
        // Legacy rows and summary projections can synthesize a fresh timestamp
        // on every rebuild. Authored timestamps remain part of causal identity.
        object.remove("timestamp");
    }
    Ok(value)
}

fn completed_live_tool_effect_suffix(
    session: &Session,
    live_messages: &[Message],
) -> Result<Vec<Message>> {
    let persisted_messages = session.to_messages_for_current_path_with_timestamp_provenance();
    if persisted_messages.len() > live_messages.len() {
        // Session is already ahead of the live Agent view (for example after
        // an explicit test/session mutation). There cannot be a live suffix
        // to recover, so keep Session authoritative.
        return Ok(Vec::new());
    }
    for (index, ((persisted, timestamp_is_synthetic), live)) in
        persisted_messages.iter().zip(live_messages).enumerate()
    {
        if rpc_recovery_message_fingerprint(persisted, *timestamp_is_synthetic)?
            != rpc_recovery_message_fingerprint(live, *timestamp_is_synthetic)?
        {
            return Err(Error::session(format!(
                "persisted Session diverged from the live Agent transcript at message {index}; refusing terminal recovery"
            )));
        }
    }

    let unpersisted = &live_messages[persisted_messages.len()..];
    let mut open_tool_calls = HashMap::<String, String>::new();
    let mut last_closed_tool_cycle = None;
    for (index, message) in unpersisted.iter().enumerate() {
        match message {
            Message::Assistant(assistant) => {
                if matches!(
                    assistant.stop_reason,
                    StopReason::PauseTurn | StopReason::Error | StopReason::Aborted
                ) {
                    if !open_tool_calls.is_empty() {
                        break;
                    }
                    continue;
                }
                let tool_calls = assistant.content.iter().filter_map(|block| match block {
                    ContentBlock::ToolCall(tool_call) => Some(tool_call),
                    _ => None,
                });
                let mut next_calls = HashMap::new();
                for tool_call in tool_calls {
                    if next_calls
                        .insert(tool_call.id.clone(), tool_call.name.clone())
                        .is_some()
                    {
                        return Ok(last_closed_tool_cycle
                            .map_or_else(Vec::new, |end| unpersisted[..=end].to_vec()));
                    }
                }
                if !next_calls.is_empty() {
                    if !open_tool_calls.is_empty() {
                        break;
                    }
                    open_tool_calls = next_calls;
                } else if !open_tool_calls.is_empty() {
                    break;
                }
            }
            Message::ToolResult(tool_result) => {
                let Some(expected_name) = open_tool_calls.remove(&tool_result.tool_call_id) else {
                    break;
                };
                if expected_name != tool_result.tool_name {
                    break;
                }
                if open_tool_calls.is_empty() {
                    last_closed_tool_cycle = Some(index);
                }
            }
            Message::User(_) | Message::Custom(_) if !open_tool_calls.is_empty() => break,
            Message::User(_) | Message::Custom(_) => {}
        }
    }
    let Some(last_closed_tool_cycle) = last_closed_tool_cycle else {
        return Ok(Vec::new());
    };
    Ok(unpersisted[..=last_closed_tool_cycle].to_vec())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RpcInputPreservation {
    Include,
    LeaveForNextAgentTurn,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RpcTerminalRecoveryPlan {
    None,
    RecordedToolTranscript { recovery_count: usize },
    All { recovery_count: usize },
}

async fn preserve_terminal_rpc_input(
    session: &Arc<Mutex<AgentSession>>,
    shared_state: &Arc<Mutex<RpcSharedState>>,
    cx: &AgentCx,
) -> Result<usize> {
    preserve_terminal_rpc_state(session, shared_state, RpcInputPreservation::Include, cx).await
}

async fn preserve_recorded_tool_transcript(
    session: &Arc<Mutex<AgentSession>>,
    shared_state: &Arc<Mutex<RpcSharedState>>,
    cx: &AgentCx,
) -> Result<usize> {
    preserve_terminal_rpc_state(
        session,
        shared_state,
        RpcInputPreservation::LeaveForNextAgentTurn,
        cx,
    )
    .await
}

async fn preserve_terminal_rpc_state(
    session: &Arc<Mutex<AgentSession>>,
    shared_state: &Arc<Mutex<RpcSharedState>>,
    input_preservation: RpcInputPreservation,
    cx: &AgentCx,
) -> Result<usize> {
    // Acquire locks before touching either queue. If this future is cancelled
    // while waiting, the accepted inputs are still authoritative in shared
    // state. The session -> shared-state order matches the Agent fetch path.
    let mut guard = OwnedMutexGuard::lock(Arc::clone(session), cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    let session_store = Arc::clone(&guard.session);
    let save_enabled = guard.save_enabled();
    let mut inner = OwnedMutexGuard::lock(session_store, cx)
        .await
        .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
    let mut state = OwnedMutexGuard::lock(Arc::clone(shared_state), cx)
        .await
        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
    // The quarantine is checked before looking at what there is to preserve:
    // an indeterminate transition makes the persistence target untrustworthy
    // for any terminal write, and `rpc_retry_restore_save_failure_latches_without_live_mutation`
    // pins that even an empty terminal pass must surface it. The RPC loop
    // therefore exits with this error after a quarantined command; clients
    // see [SESSION_PERSISTENCE_FAILED] rather than a silent clean exit.
    if let Some(reason) = state.provider_admission.reason() {
        return Err(Error::session_persistence(format!(
            "terminal RPC persistence is quarantined after an indeterminate transition: {reason}"
        )));
    }
    let completed_tool_transcript =
        completed_live_tool_effect_suffix(&inner, guard.agent.messages())?;
    state.stage_completed_tool_transcript(&inner, &completed_tool_transcript)?;
    let completed_tool_transcript_count = state.completed_tool_transcript_entries().len();
    if completed_tool_transcript_count == 0
        && (input_preservation == RpcInputPreservation::LeaveForNextAgentTurn
            || state.pending_count() == 0)
    {
        return Ok(0);
    }

    // A private first-save candidate would choose and retain its own session
    // filename. If cancellation landed after that file reached disk but before
    // the candidate was installed, retrying from a live `path == None` session
    // would choose a second file that entry-ID reconciliation cannot see. Finish
    // an empty live save first while the acknowledged queues are still untouched.
    // Cancellation during this pinning save cannot strand candidate entries
    // because the candidate has not been created yet; a successful return pins
    // the one target every later candidate and retry must reconcile against.
    if save_enabled && inner.path.is_none() {
        inner.save().await?;
    }

    let input_recovery_count = if input_preservation == RpcInputPreservation::Include {
        state.pending_count()
    } else {
        0
    };
    let recovery_count = completed_tool_transcript_count.saturating_add(input_recovery_count);
    let state = &mut *state;
    // Build and flush a private candidate while the authoritative queues and
    // live Session entries remain unchanged. If this future is cancelled during
    // the candidate flush, retry sees the original queues and reuses their
    // stable entry metadata against the pinned persistence target.
    let mut candidate = inner.clone();
    let mut parent_id = candidate.leaf_id().map(str::to_string);
    for delivery in state.completed_tool_transcript_entries() {
        let message = delivery.message().clone();
        let (entry_id, timestamp, bound_parent_id) =
            delivery.bind_persistence_identity(parent_id.take());
        parent_id = Some(entry_id.clone());
        candidate.append_model_message_with_identity(
            message,
            &entry_id,
            &timestamp,
            bound_parent_id.as_deref(),
        )?;
    }
    if input_preservation == RpcInputPreservation::Include {
        let in_flight = state.in_flight_in_lease_order();
        let in_flight_matches = find_represented_rpc_deliveries(&candidate, &in_flight)?;
        for (in_flight, represented) in in_flight.into_iter().zip(in_flight_matches) {
            if represented {
                // Clones held in Agent's private queue share this lazy identity.
                // Binding represented deliveries lets terminal recovery discard a
                // staged duplicate without allocating IDs on the ordinary path.
                let _ = in_flight.delivery.bind_persistence_identity(None);
                continue;
            }
            let message = in_flight.delivery.message().clone();
            let (entry_id, timestamp, bound_parent_id) = in_flight
                .delivery
                .bind_persistence_identity(parent_id.take());
            parent_id = Some(entry_id.clone());
            candidate.append_model_message_with_identity(
                message,
                &entry_id,
                &timestamp,
                bound_parent_id.as_deref(),
            )?;
        }
        for delivery in state.steering.iter_mut().chain(&mut state.follow_up) {
            let message = delivery.message().clone();
            let (entry_id, timestamp, bound_parent_id) =
                delivery.bind_persistence_identity(parent_id.take());
            parent_id = Some(entry_id.clone());
            candidate.append_model_message_with_identity(
                message,
                &entry_id,
                &timestamp,
                bound_parent_id.as_deref(),
            )?;
        }
    }

    if save_enabled
        && let Err(persist_err) = candidate.flush_autosave(AutosaveFlushTrigger::Manual).await
    {
        return Err(persist_err);
    }

    let messages = candidate.to_messages_for_current_path();
    guard.invalidate_background_compaction();
    *inner = candidate;
    guard.agent.replace_messages(messages);
    if input_preservation == RpcInputPreservation::Include {
        let in_flight_ids: HashSet<String> = state
            .steering_in_flight
            .iter()
            .chain(&state.follow_up_in_flight)
            .filter_map(|in_flight| {
                in_flight
                    .delivery
                    .persistence_entry_id()
                    .map(str::to_string)
            })
            .collect();
        guard.agent.discard_queued_persistence_ids(&in_flight_ids);
        state.clear_all_pending();
    } else {
        state.completed_tool_transcript = None;
    }
    Ok(recovery_count)
}

async fn terminal_rpc_recovery_plan(
    session: &Arc<Mutex<AgentSession>>,
    shared_state: &Arc<Mutex<RpcSharedState>>,
    resumes_agent: bool,
    cx: &AgentCx,
) -> Result<RpcTerminalRecoveryPlan> {
    let guard = OwnedMutexGuard::lock(Arc::clone(session), cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    let session_store = Arc::clone(&guard.session);
    let inner = OwnedMutexGuard::lock(session_store, cx)
        .await
        .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
    let state = OwnedMutexGuard::lock(Arc::clone(shared_state), cx)
        .await
        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
    if let Some(reason) = state.provider_admission.reason() {
        return Err(Error::session_persistence(format!(
            "RPC session advancement is quarantined after an indeterminate transition: {reason}"
        )));
    }
    let pending_count = state.pending_count();
    let completed_tool_effect_count =
        completed_live_tool_effect_suffix(&inner, guard.agent.messages())?
            .len()
            .max(state.completed_tool_transcript_entries().len());

    // A max-time boundary can return leased steering to Agent's private queue.
    // Keep that delivery executable when the next command starts a provider
    // turn. Shared state remains the durable authority until normal turn
    // acknowledgement confirms that the resumed delivery reached Session.
    let pending_input_can_resume = if resumes_agent {
        let in_flight = state.in_flight_in_lease_order();
        let represented = find_represented_rpc_deliveries(&inner, &in_flight)?;
        in_flight
            .into_iter()
            .zip(represented)
            .all(|(in_flight, represented)| {
                represented || guard.agent.has_staged_delivery(&in_flight.delivery)
            })
    } else {
        false
    };

    if completed_tool_effect_count > 0 && pending_input_can_resume {
        Ok(RpcTerminalRecoveryPlan::RecordedToolTranscript {
            recovery_count: completed_tool_effect_count,
        })
    } else if completed_tool_effect_count > 0 || (pending_count > 0 && !pending_input_can_resume) {
        Ok(RpcTerminalRecoveryPlan::All {
            recovery_count: pending_count.saturating_add(completed_tool_effect_count),
        })
    } else {
        Ok(RpcTerminalRecoveryPlan::None)
    }
}

fn find_represented_rpc_deliveries(
    session: &Session,
    deliveries: &[&RpcInFlightMessage],
) -> Result<Vec<bool>> {
    let current_path_ids: HashSet<&str> = session
        .entries_for_current_path()
        .into_iter()
        .filter_map(|entry| entry.base_id().map(String::as_str))
        .collect();
    let mut candidates = Vec::new();
    for (index, entry) in session.entries.iter().enumerate() {
        if !entry
            .base_id()
            .is_some_and(|id| current_path_ids.contains(id.as_str()))
        {
            continue;
        }
        let SessionEntry::Message(message) = entry else {
            continue;
        };
        candidates.push((index, serde_json::to_vec(&message.message)?));
    }

    let mut matched_entries = HashSet::new();
    let mut represented = Vec::with_capacity(deliveries.len());
    for in_flight in deliveries {
        let expected =
            serde_json::to_vec(&SessionMessage::from(in_flight.delivery.message().clone()))?;
        let matched = candidates.iter().find_map(|(index, encoded)| {
            (*index >= in_flight.session_entry_baseline
                && !matched_entries.contains(index)
                && encoded == &expected)
                .then_some(*index)
        });
        if let Some(index) = matched {
            matched_entries.insert(index);
        }
        represented.push(matched.is_some());
    }
    Ok(represented)
}

async fn flush_rpc_session_on_shutdown(
    session: &Arc<Mutex<AgentSession>>,
    cx: &AgentCx,
) -> Result<()> {
    let guard = OwnedMutexGuard::lock(Arc::clone(session), cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    if !guard.save_enabled() {
        return Ok(());
    }
    let session_store = Arc::clone(&guard.session);
    let mut inner = OwnedMutexGuard::lock(session_store, cx)
        .await
        .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
    inner.flush_autosave_on_shutdown().await
}

async fn acknowledge_durable_rpc_in_flight(
    session: &Arc<Mutex<AgentSession>>,
    shared_state: &Arc<Mutex<RpcSharedState>>,
    cx: &AgentCx,
) -> Result<usize> {
    // Match terminal recovery's lock order. Queue authority stays held across
    // the forced flush, so a newly leased delivery cannot be acknowledged by
    // an older turn's completion.
    let mut guard = OwnedMutexGuard::lock(Arc::clone(session), cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    let save_enabled = guard.save_enabled();
    let session_store = Arc::clone(&guard.session);
    let mut inner = OwnedMutexGuard::lock(session_store, cx)
        .await
        .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
    let mut state = OwnedMutexGuard::lock(Arc::clone(shared_state), cx)
        .await
        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
    let in_flight_count = state.steering_in_flight.len() + state.follow_up_in_flight.len();
    if in_flight_count == 0 {
        return Ok(0);
    }

    let in_flight = state.in_flight_in_lease_order();
    let represented = find_represented_rpc_deliveries(&inner, &in_flight)?;
    if represented.iter().any(|represented| !represented) {
        return Ok(0);
    }
    for in_flight in in_flight {
        let _ = in_flight.delivery.bind_persistence_identity(None);
    }

    if save_enabled {
        inner.flush_autosave(AutosaveFlushTrigger::Manual).await?;
    }
    let in_flight_ids: HashSet<String> = state
        .steering_in_flight
        .iter()
        .chain(&state.follow_up_in_flight)
        .filter_map(|in_flight| {
            in_flight
                .delivery
                .persistence_entry_id()
                .map(str::to_string)
        })
        .collect();
    guard.agent.discard_queued_persistence_ids(&in_flight_ids);
    state.acknowledge_in_flight();
    Ok(in_flight_count)
}

fn finish_rpc_turn_durability<T>(
    result: Result<T>,
    durable_ack_result: Result<usize>,
) -> Result<T> {
    match durable_ack_result {
        Ok(_) => result,
        Err(persist_err) => {
            let message = match &result {
                Ok(_) => format!("failed to durably acknowledge queued RPC input: {persist_err}"),
                Err(primary_err) => format!(
                    "failed to durably acknowledge queued RPC input: {persist_err}; primary provider/tool turn also failed: {primary_err}"
                ),
            };
            Err(Error::session_persistence(message))
        }
    }
}

async fn restore_rpc_retry_tail(
    session: &Arc<Mutex<AgentSession>>,
    shared_state: &Arc<Mutex<RpcSharedState>>,
    cx: &AgentCx,
    require_incomplete_tail: bool,
) -> Result<()> {
    let mut guard = OwnedMutexGuard::lock(Arc::clone(session), cx)
        .await
        .map_err(|err| Error::session(format!("retry restoration session lock failed: {err}")))?;
    let mut state = OwnedMutexGuard::lock(Arc::clone(shared_state), cx)
        .await
        .map_err(|err| Error::session(format!("retry restoration state lock failed: {err}")))?;
    state.bind_provider_admission(guard.provider_admission_gate());
    state.ensure_session_advancement_allowed()?;
    let session_store = Arc::clone(&guard.session);
    let mut inner = OwnedMutexGuard::lock(session_store, cx)
        .await
        .map_err(|err| Error::session(format!("retry restoration inner lock failed: {err}")))?;
    let mut candidate = inner.clone();
    let reverted = candidate.revert_incomplete_response();
    if require_incomplete_tail && !reverted {
        return Err(Error::session(
            "retry restoration invariant failed: the completed error response had no incomplete assistant tail",
        ));
    }
    if !reverted {
        return Ok(());
    }

    let restored_messages = candidate.to_messages_for_current_path();
    let save_enabled = guard.save_enabled();
    let _provider_transition = state
        .provider_admission
        .begin_transition(
            "retry restoration persistence was interrupted before live installation completed"
                .to_string(),
            cx,
        )
        .await?;
    if save_enabled
        && let Err(first_err) = candidate.save().await
        && let Err(retry_err) = candidate.save().await
    {
        let reason = format!(
            "retry restoration persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
        );
        state.provider_admission.block(reason.clone());
        return Err(Error::session_persistence(reason));
    }

    guard.invalidate_background_compaction();
    *inner = candidate;
    guard.agent.replace_messages(restored_messages);
    state.provider_admission.clear();
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn run_prompt_with_retry(
    session: Arc<Mutex<AgentSession>>,
    shared_state: Arc<Mutex<RpcSharedState>>,
    is_streaming: Arc<AtomicBool>,
    is_compacting: Arc<AtomicBool>,
    turn_phase_linearizer: Arc<std::sync::Mutex<()>>,
    abort_handle_slot: Arc<Mutex<Option<AbortHandle>>>,
    out_tx: std::sync::mpsc::SyncSender<String>,
    retry_abort: Arc<AtomicBool>,
    options: RpcOptions,
    message: String,
    keyword_scan_source: Option<String>,
    images: Vec<ImageContent>,
    cx: AgentCx,
) {
    retry_abort.store(false, Ordering::SeqCst);
    is_streaming.store(true, Ordering::SeqCst);
    let _streaming_guard = ClearFlagOnDrop(Arc::clone(&is_streaming));
    let _compacting_handoff_guard = ClearFlagOnDrop(Arc::clone(&is_compacting));

    let provider_reentry_blocked = match OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx).await
    {
        Ok(state) => state.provider_admission.reason(),
        Err(err) => {
            let error = Error::session(format!("retry state lock failed: {err}"));
            let mut payload = json!({
                "type": "agent_end",
                "messages": [],
                "error": error.to_string(),
            });
            payload["errorHints"] = error_hints_value(&error);
            let _ = out_tx.send(event(&payload));
            return;
        }
    };
    if let Some(reason) = provider_reentry_blocked {
        let error = Error::session_persistence(reason);
        let mut payload = json!({
            "type": "agent_end",
            "messages": [],
            "error": error.to_string(),
        });
        payload["errorHints"] = error_hints_value(&error);
        let _ = out_tx.send(event(&payload));
        return;
    }

    let max_retries = options.config.retry_max_retries();
    let mut retry_count: u32 = 0;
    let mut failovers_this_turn: u32 = 0;
    // Distinct from retry_count: a failover resets the retry BUDGET but must
    // never replay the first attempt (which would re-add the user message and
    // re-execute completed tool cycles — pi_agent_rust#125 semantics).
    let mut first_attempt_done = false;
    let mut follow_up_first = false;
    let mut expected_follow_up_fetch: Option<(Arc<AtomicU64>, u64)> = None;
    let deferred_agent_end = Arc::new(std::sync::Mutex::new(None::<AgentEvent>));
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
            final_error = Some("abort handle lock failed".to_string());
            final_error_hints = None;
            break;
        }

        let runtime_for_events = options.runtime_handle.clone();
        *deferred_agent_end
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

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
            // Servers an extension registered after startup (`registerMcpServer`
            // from a later callback) reach the session-owned MCP manager at the
            // next turn, exactly like the SDK and classic TUI turn runners; a
            // retry of the same turn does not repeat the sync.
            if !first_attempt_done
                && let (Some(mcp), Some(ext)) = (guard.mcp_manager(), extensions.as_ref())
            {
                crate::mcp::sync_extension_registrations(&mcp, ext, &mut guard.agent).await;
            }
            let event_handler = rpc_agent_event_handler(
                out_tx.clone(),
                runtime_for_events,
                extensions,
                Some(Arc::clone(&deferred_agent_end)),
            );

            if first_attempt_done {
                // Retry: resume the turn from the last completed state instead of
                // replaying it from the user message. The incomplete output of the
                // failed request was stripped via `revert_incomplete_response`
                // below; every completed tool cycle stays on the path, so the
                // retry re-issues only the failed provider request — no tool
                // re-execution, no re-billing of prior work (pi_agent_rust#125).
                if follow_up_first {
                    let ready_linearizer = Arc::clone(&turn_phase_linearizer);
                    let ready_compacting = Arc::clone(&is_compacting);
                    let ready = Arc::new(AtomicBool::new(false));
                    let ready_for_callback = Arc::clone(&ready);
                    let expected_fetch = expected_follow_up_fetch.clone();
                    let result = guard
                        .run_continue_with_follow_up_with_abort(
                            true,
                            Some(abort_signal),
                            move || {
                                let source_ready =
                                    expected_fetch
                                        .as_ref()
                                        .is_none_or(|(generation, expected)| {
                                            generation.load(Ordering::SeqCst) == *expected
                                        });
                                if !source_ready {
                                    return false;
                                }
                                let _phase_guard = lock_rpc_turn_phase(&ready_linearizer);
                                ready_compacting.store(false, Ordering::SeqCst);
                                ready_for_callback.store(true, Ordering::SeqCst);
                                true
                            },
                            event_handler,
                        )
                        .await;
                    if ready.load(Ordering::SeqCst) {
                        follow_up_first = false;
                        expected_follow_up_fetch = None;
                    }
                    result
                } else {
                    guard
                        .run_continue_with_abort(Some(abort_signal), event_handler)
                        .await
                }
            } else {
                // First attempt: add the user message and run the turn.
                first_attempt_done = true;
                guard
                    .agent
                    .set_magic_keyword_scan_override(keyword_scan_source.clone());
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
            }
        };
        let durability_cx = AgentCx::for_request();
        let result = finish_rpc_turn_durability(
            result,
            acknowledge_durable_rpc_in_flight(&session, &shared_state, &durability_cx).await,
        );

        if matches!(
            &result,
            Ok(message)
                if !matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
        ) {
            // The provider turn has stopped consuming steering/follow-up input.
            // Claim finalization before any further await so no message can be
            // acknowledged into a queue with no active consumer.
            let _phase_guard = lock_rpc_turn_phase(&turn_phase_linearizer);
            is_compacting.store(true, Ordering::SeqCst);
        }

        if let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(&abort_handle_slot), &cx).await {
            *guard = None;
        }

        let require_incomplete_tail = matches!(
            &result,
            Ok(message) if message.stop_reason == StopReason::Error
        );
        let mut quota_credential: Option<(String, String)> = None;

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
                            let session_store = Arc::clone(&guard.session);
                            let inner_session =
                                OwnedMutexGuard::lock(session_store, &cx).await.ok();
                            inner_session.and_then(|inner| {
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
                    let internally_staged_follow_up =
                        match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
                            Ok(guard) => guard.agent.has_staged_follow_up(),
                            Err(err) => {
                                final_error = Some(format!(
                                    "session lock failed while checking staged input: {err}"
                                ));
                                final_error_hints = None;
                                break;
                            }
                        };
                    let late_queued_input =
                        match OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx).await {
                            Ok(state) if !state.steering.is_empty() => Some((false, None)),
                            Ok(state) if !state.follow_up.is_empty() => {
                                let generation = Arc::clone(&state.follow_up_fetch_generation);
                                let expected = generation.load(Ordering::SeqCst).wrapping_add(1);
                                Some((true, Some((generation, expected))))
                            }
                            Ok(_) if internally_staged_follow_up => Some((true, None)),
                            Ok(_) => None,
                            Err(err) => {
                                final_error =
                                    Some(format!("state lock failed while finalizing turn: {err}"));
                                final_error_hints = None;
                                break;
                            }
                        };
                    if let Some((needs_follow_up_first, expected_fetch)) = late_queued_input {
                        // A queue insertion that linearized immediately before
                        // our phase claim must not be stranded for a future
                        // unrelated turn. Follow-ups stay in the shared queue
                        // until `Agent` drains them after successful preflight.
                        follow_up_first = needs_follow_up_first;
                        expected_follow_up_fetch = expected_fetch;
                        if !follow_up_first {
                            let _phase_guard = lock_rpc_turn_phase(&turn_phase_linearizer);
                            is_compacting.store(false, Ordering::SeqCst);
                        }
                        continue;
                    }
                    success = true;
                    break;
                }
            }
            Err(err) => {
                let err_str = err.to_string();
                if err.is_session_persistence() {
                    final_error = Some(err_str);
                    final_error_hints = Some(error_hints_value(&err));
                    break;
                }
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
                final_error = Some(err_str.clone());
                final_error_hints = Some(error_hints_value(&err));
                if crate::failover::classify_failover(&err_str)
                    == Some(crate::failover::FailoverClass::Quota)
                {
                    let guard = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
                        Ok(guard) => guard,
                        Err(lock_err) => {
                            let restore_err = Error::session(format!(
                                "retry credential snapshot session lock failed: {lock_err}"
                            ));
                            final_error = Some(restore_err.to_string());
                            final_error_hints = Some(error_hints_value(&restore_err));
                            break;
                        }
                    };
                    let provider = guard.agent.provider();
                    let provider_name = provider.name().to_string();
                    if let Some(key) = guard.agent.stream_options().api_key.clone() {
                        quota_credential = Some((provider_name, key));
                    }
                }
            }
        }

        let retry_enabled = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
            .await
            .is_ok_and(|state| state.auto_retry_enabled);
        if !retry_enabled || retry_count >= max_retries {
            // Failover (bd-cv653.3.2): the same-model retry budget is spent.
            // If the error class is failover-eligible and a chain entry
            // remains, swap providers and continue the turn there. The
            // per-turn cap bounds total failover cost.
            let failover_result = if failovers_this_turn < options.config.max_failovers_per_turn() {
                try_failover_to_next_chain_entry(
                    Arc::clone(&session),
                    Arc::clone(&shared_state),
                    out_tx.clone(),
                    &options,
                    final_error.as_deref(),
                    require_incomplete_tail,
                    (retry_count > 0).then_some(retry_count),
                    &cx,
                )
                .await
            } else {
                Ok(false)
            };
            match failover_result {
                Ok(true) => {
                    if let Some((provider_name, key)) = quota_credential.as_ref() {
                        crate::auth::report_provider_rate_limit(provider_name, key);
                    }
                    retry_count = 0;
                    failovers_this_turn += 1;
                    final_error = None;
                    final_error_hints = None;
                    continue;
                }
                Ok(false) => {
                    if let Some((provider_name, key)) = quota_credential.as_ref() {
                        crate::auth::report_provider_rate_limit(provider_name, key);
                    }
                }
                Err(restore_err) => {
                    let transition_error = restore_err.to_string();
                    final_error = Some(final_error.take().map_or_else(
                        || transition_error.clone(),
                        |provider_error| {
                            format!("{transition_error}; original provider error: {provider_error}")
                        },
                    ));
                    final_error_hints = Some(error_hints_value(&restore_err));
                }
            }
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
        if let Err(restore_err) =
            restore_rpc_retry_tail(&session, &shared_state, &cx, require_incomplete_tail).await
        {
            let transition_error = restore_err.to_string();
            final_error = Some(final_error.take().map_or_else(
                || transition_error.clone(),
                |provider_error| {
                    format!("{transition_error}; original provider error: {provider_error}")
                },
            ));
            final_error_hints = Some(error_hints_value(&restore_err));
            break;
        }
        let mut guard = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
            Ok(guard) => guard,
            Err(lock_err) => {
                let restore_err = Error::session(format!(
                    "retry credential rotation session lock failed: {lock_err}"
                ));
                final_error = Some(restore_err.to_string());
                final_error_hints = Some(error_hints_value(&restore_err));
                break;
            }
        };
        let provider_admission = guard.provider_admission_gate();
        let _credential_rotation_permit = match provider_admission.acquire(&cx).await {
            Ok(permit) => permit,
            Err(admission_err) => {
                final_error = Some(admission_err.to_string());
                final_error_hints = Some(error_hints_value(&admission_err));
                break;
            }
        };
        if let Err(admission_err) = provider_admission.ensure_allowed() {
            final_error = Some(admission_err.to_string());
            final_error_hints = Some(error_hints_value(&admission_err));
            break;
        }
        // Rotation bookkeeping and key replacement are one admission-bound
        // mutation. Tail restoration must succeed before either can occur,
        // and no provider call may observe the backed-off key in between.
        if let Some((provider_name, key)) = quota_credential.as_ref() {
            crate::auth::report_provider_rate_limit(provider_name, key);
        }
        // Credential rotation (bd-cv653.3.2): re-resolve the key so a
        // backed-off credential rotates on the retry. CLI-pinned keys
        // never rotate.
        if options.cli_api_key.is_none() {
            let provider_name = guard.agent.provider().name().to_string();
            if let Some(fresh) = options.auth.resolve_api_key(&provider_name, None) {
                let changed =
                    guard.agent.stream_options().api_key.as_deref() != Some(fresh.as_str());
                if changed {
                    guard.agent.stream_options_mut().api_key = Some(fresh);
                    guard.refresh_extension_completion_host_state();
                }
            }
        }
    }

    if retry_count > 0 {
        let _ = out_tx.send(agent_event(AgentEvent::AutoRetryEnd {
            success,
            attempt: retry_count,
            final_error: if success { None } else { final_error.clone() },
        }));
    }

    // Failover lifecycle (bd-2vmu6.1): a turn that swapped to a fallback chain
    // entry closes its `FailoverStart` here, before the terminal `agent_end`,
    // whether the fallback succeeded, failed, or was aborted. Restoring the
    // primary after cooldown is a separate lifecycle (`restoredPrimary: true`
    // from `maybe_restore_primary`). The turn context may already be
    // cancelled, so the shared state is read with a fresh request context.
    if failovers_this_turn > 0 {
        let lifecycle_cx = AgentCx::for_request();
        let (provider, model) = OwnedMutexGuard::lock(Arc::clone(&shared_state), &lifecycle_cx)
            .await
            .map_or_else(
                |_| (String::new(), String::new()),
                |state| state.active_failover_model.clone().unwrap_or_default(),
            );
        let _ = out_tx.send(agent_event(AgentEvent::FailoverEnd {
            success,
            provider,
            model,
            restored_primary: false,
        }));
    }

    if !success {
        // Close the admission window before preserving any acknowledged input
        // that the terminal provider error/abort left in the shared queues.
        // Messages are recorded exactly once but are not executed after a
        // failed or explicitly aborted turn, avoiding both silent loss at EOF
        // and surprising post-abort side effects.
        {
            let _phase_guard = lock_rpc_turn_phase(&turn_phase_linearizer);
            is_compacting.store(true, Ordering::SeqCst);
        }
        // Terminal preservation is recovery work, so it must outlive a
        // cancelled turn context. Reusing `cx` here makes every lock fail
        // immediately after parent cancellation and can strand input that was
        // already acknowledged to the RPC client.
        let preservation_cx = AgentCx::for_request();
        let preservation_quarantined =
            match OwnedMutexGuard::lock(Arc::clone(&shared_state), &preservation_cx).await {
                Ok(state) => state.provider_admission.reason().is_some(),
                Err(err) => {
                    let preservation_error =
                        format!("failed to inspect RPC persistence quarantine: {err}");
                    final_error = Some(final_error.map_or_else(
                        || preservation_error.clone(),
                        |terminal| format!("{terminal}; {preservation_error}"),
                    ));
                    true
                }
            };
        if !preservation_quarantined
            && let Err(err) =
                preserve_terminal_rpc_input(&session, &shared_state, &preservation_cx).await
        {
            let preservation_error = format!("failed to preserve queued RPC input: {err}");
            final_error = Some(final_error.map_or_else(
                || preservation_error.clone(),
                |terminal| format!("{terminal}; {preservation_error}"),
            ));
        }
        // Emit the terminal event BEFORE clearing is_streaming: the stdin-EOF
        // drain only guarantees flush-before-shutdown for events queued while
        // a flag is still set (gh #137).
        let terminal_messages = deferred_agent_end
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .and_then(|event| match event {
                AgentEvent::AgentEnd { messages, .. } => Some(messages),
                _ => None,
            })
            .unwrap_or_default();
        let mut payload = json!({
            "type": "agent_end",
            "messages": terminal_messages,
            "error": final_error.unwrap_or_else(|| "Request failed".to_string())
        });
        if let Some(hints) = final_error_hints {
            payload["errorHints"] = hints;
        }
        let _ = out_tx.send(event(&payload));
        is_streaming.store(false, Ordering::SeqCst);
        return;
    }

    let terminal_messages = deferred_agent_end
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .and_then(|event| match event {
            AgentEvent::AgentEnd { messages, .. } => Some(messages),
            _ => None,
        })
        .unwrap_or_default();
    let _ = out_tx.send(event(&json!({
        "type": "agent_end",
        "messages": terminal_messages,
        "error": Value::Null,
    })));

    // Claim the turn-finalization/compaction phase before any await. While the
    // previous provider turn has ended, its compaction decision and possible
    // mutation are not yet complete, so admitting another turn here could race
    // two snapshots and append competing compactions.
    let auto_compaction_enabled = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
        .await
        .is_ok_and(|state| state.auto_compaction_enabled);
    // Release the streaming phase only after the exclusion flag is visible, so
    // both new-turn admission and stdin-EOF draining see a continuous handoff.
    is_streaming.store(false, Ordering::SeqCst);
    if auto_compaction_enabled {
        maybe_auto_compact(
            session,
            shared_state,
            options,
            Arc::clone(&is_compacting),
            out_tx,
        )
        .await;
    }
}

async fn sync_runtime_before_rpc_ack(
    session: &Arc<Mutex<AgentSession>>,
    cx: &AgentCx,
) -> Result<()> {
    let mut guard = OwnedMutexGuard::lock(Arc::clone(session), cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    guard.sync_runtime_selection_from_session_header().await
}

/// Cooldown restore (bd-cv653.3.2): when a previous turn failed over and the
/// cooldown elapsed, durably restore the explicitly recorded primary before
/// changing the live provider or shared failover state.
async fn maybe_restore_primary(
    session: Arc<Mutex<AgentSession>>,
    shared_state: Arc<Mutex<RpcSharedState>>,
    out_tx: std::sync::mpsc::SyncSender<String>,
    options: &RpcOptions,
    cx: &AgentCx,
) -> Result<()> {
    // Transition lock order is AgentSession -> shared state -> inner Session.
    // Keep all three through the synchronous install so direct Session readers
    // cannot observe a new header before the Agent/shared state changes.
    let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), cx)
        .await
        .map_err(|err| Error::session(format!("primary restore session lock failed: {err}")))?;
    let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), cx)
        .await
        .map_err(|err| Error::session(format!("primary restore state lock failed: {err}")))?;
    state.bind_provider_admission(guard.provider_admission_gate());
    state.ensure_session_advancement_allowed()?;
    let Some((active_provider, active_model)) = state.active_failover_model.clone() else {
        return Ok(());
    };
    let Some(primary) = state.failover_primary.clone() else {
        let reason =
            "primary restore invariant failed: active fallback has no recorded primary".to_string();
        state.provider_admission.block(reason.clone());
        return Err(Error::session_persistence(reason));
    };
    let provider = primary.provider;
    let model_id = primary.model_id;

    let runtime_provider = guard.agent.provider();
    if !crate::provider_metadata::provider_ids_match(runtime_provider.name(), &active_provider)
        || !runtime_provider
            .model_id()
            .eq_ignore_ascii_case(&active_model)
    {
        let reason = format!(
            "primary restore invariant failed: runtime {}/{} does not match recorded fallback {active_provider}/{active_model}",
            runtime_provider.name(),
            runtime_provider.model_id()
        );
        state.provider_admission.block(reason.clone());
        return Err(Error::session_persistence(reason));
    }

    let session_store = Arc::clone(&guard.session);
    let mut inner = OwnedMutexGuard::lock(session_store, cx)
        .await
        .map_err(|err| Error::session(format!("primary restore inner lock failed: {err}")))?;
    let session_matches_active = inner.effective_model_for_current_path().is_some_and(
        |(session_provider, session_model)| {
            crate::provider_metadata::provider_ids_match(&session_provider, &active_provider)
                && session_model.eq_ignore_ascii_case(&active_model)
        },
    );
    if !session_matches_active {
        let reason = format!(
            "primary restore invariant failed: Session path does not match recorded fallback {active_provider}/{active_model}"
        );
        state.provider_admission.block(reason.clone());
        return Err(Error::session_persistence(reason));
    }
    if !state
        .failover_cooldown
        .as_ref()
        .is_some_and(|tracker| tracker.should_use_primary(std::time::Instant::now()))
    {
        return Ok(());
    }

    let Some(entry) = options
        .available_models
        .iter()
        .find(|m| {
            crate::provider_metadata::provider_ids_match(&m.model.provider, &provider)
                && m.model.id.eq_ignore_ascii_case(&model_id)
        })
        .cloned()
        .or_else(|| crate::models::ad_hoc_model_entry(&provider, &model_id))
    else {
        return Err(Error::validation(format!(
            "Unable to restore primary provider/model {provider}/{model_id}"
        )));
    };

    let key = resolve_model_key(options.cli_api_key.as_deref(), &options.auth, &entry);
    if model_requires_configured_credential(&entry) && key.is_none() {
        return Err(Error::auth(format!(
            "Missing credentials for primary provider/model {provider}/{model_id}"
        )));
    }
    let provider_impl = providers::create_provider(
        &entry,
        guard
            .extensions
            .as_ref()
            .map(crate::extensions::ExtensionRegion::manager),
    )?;

    let target_thinking = entry.clamp_thinking_level(primary.requested_thinking_level);
    let target_thinking_text = target_thinking.to_string();
    let mut candidate = inner.clone();
    let thinking_changed = candidate
        .effective_thinking_level_for_current_path()
        .as_deref()
        != Some(target_thinking_text.as_str());
    candidate.set_model_header(
        Some(provider.clone()),
        Some(model_id.clone()),
        Some(target_thinking_text.clone()),
    );
    candidate.append_model_change_with_role(
        provider.clone(),
        model_id.clone(),
        Some("primary_restore".to_string()),
    );
    if thinking_changed {
        candidate.append_thinking_level_change(target_thinking_text);
    }
    let save_enabled = guard.save_enabled();
    guard.invalidate_background_compaction();
    let _provider_transition = state
        .provider_admission
        .begin_transition(
            "primary restore persistence was interrupted before live installation completed"
                .to_string(),
            cx,
        )
        .await?;
    if save_enabled
        && let Err(first_err) = candidate.save().await
        && let Err(retry_err) = candidate.save().await
    {
        let reason = format!(
            "primary restore persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
        );
        state.provider_admission.block(reason.clone());
        return Err(Error::session_persistence(reason));
    }

    *inner = candidate;
    guard.agent.set_provider(provider_impl);
    guard.agent.set_keyword_max_thinking_level(
        entry.clamp_thinking_level(crate::model::ThinkingLevel::Max),
    );
    guard.agent.set_tool_call_dialect(entry.tool_call_dialect());
    guard
        .agent
        .set_model_accepts_images(entry.model.input.contains(&InputType::Image));
    {
        let stream_options = guard.agent.stream_options_mut();
        stream_options.api_key.clone_from(&key);
        stream_options.headers.clone_from(&entry.headers);
        stream_options.max_tokens = Some(entry.model.max_tokens);
        stream_options.thinking_level = Some(target_thinking);
    }
    guard.set_compaction_context_window(context_window_tokens_for_entry(&entry));
    guard.refresh_extension_completion_host_state();
    if let Some(region) = &guard.extensions {
        region
            .manager()
            .set_current_model(Some(provider.clone()), Some(model_id.clone()));
    }
    state.clear_failover_lifecycle();
    state.provider_admission.clear();

    drop(inner);
    drop(state);
    drop(guard);
    let _ = out_tx.send(agent_event(AgentEvent::FailoverEnd {
        success: true,
        provider,
        model: model_id,
        restored_primary: true,
    }));
    Ok(())
}

/// Failover walk (bd-cv653.3.2): classify the terminal error; if it is
/// failover-eligible, resolve the next chain entry, swap the provider in the
/// session's agent, emit FailoverStart, and record the cooldown + audit entry.
/// Returns true when a swap happened (the caller restarts the retry budget).
async fn try_failover_to_next_chain_entry(
    session: Arc<Mutex<AgentSession>>,
    shared_state: Arc<Mutex<RpcSharedState>>,
    out_tx: std::sync::mpsc::SyncSender<String>,
    options: &RpcOptions,
    error_text: Option<&str>,
    require_incomplete_tail: bool,
    retry_attempt_to_end: Option<u32>,
    cx: &AgentCx,
) -> Result<bool> {
    let Some(error_text) = error_text else {
        return Ok(false);
    };
    let Some(class) = crate::failover::classify_failover(error_text) else {
        return Ok(false); // auth/loud errors never fail over
    };
    let Some(chains) = options
        .config
        .retry
        .as_ref()
        .and_then(|r| r.fallback_chains.as_ref())
    else {
        return Ok(false);
    };

    // Hold both transition authorities until the staged Session candidate is
    // durable and the infallible in-memory install is complete. The lock order
    // matches the other RPC session->shared-state paths.
    let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), cx)
        .await
        .map_err(|err| Error::session(format!("failover session lock failed: {err}")))?;
    let provider = guard.agent.provider();
    let (current_provider, current_model) =
        (provider.name().to_string(), provider.model_id().to_string());

    let mut state = OwnedMutexGuard::lock(Arc::clone(&shared_state), cx)
        .await
        .map_err(|err| Error::session(format!("failover state lock failed: {err}")))?;
    state.bind_provider_admission(guard.provider_admission_gate());
    state.ensure_session_advancement_allowed()?;
    let primary_model = match (
        state.active_failover_model.as_ref(),
        state.failover_primary.as_ref(),
    ) {
        (None, None) => RpcFailoverPrimary {
            provider: current_provider.clone(),
            model_id: current_model.clone(),
            requested_thinking_level: guard
                .agent
                .stream_options()
                .thinking_level
                .unwrap_or_default(),
        },
        (Some((active_provider, active_model)), Some(primary))
            if crate::provider_metadata::provider_ids_match(&current_provider, active_provider)
                && current_model.eq_ignore_ascii_case(active_model) =>
        {
            primary.clone()
        }
        (Some((active_provider, active_model)), Some(_)) => {
            let reason = format!(
                "failover state invariant failed: runtime {current_provider}/{current_model} does not match recorded fallback {active_provider}/{active_model}"
            );
            state.provider_admission.block(reason.clone());
            return Err(Error::session_persistence(reason));
        }
        (active, primary) => {
            let reason = format!(
                "failover state invariant failed: active fallback presence={} recorded primary presence={}",
                active.is_some(),
                primary.is_some()
            );
            state.provider_admission.block(reason.clone());
            return Err(Error::session_persistence(reason));
        }
    };
    // A fallback chain belongs to the original primary identity. Once the
    // first entry is active, resolving from the live fallback would make an
    // exact primary chain disappear and prevent later entries from running.
    let Some(chain) = crate::failover::chain_for(
        chains,
        "default",
        &primary_model.provider,
        &primary_model.model_id,
    ) else {
        return Ok(false);
    };
    let mut position = state.failover_chain_position.unwrap_or(0);

    // The caller enforces max_failovers_per_turn for this turn. `position` is
    // durable process state across turns and must not be compared with that
    // per-turn budget, or a cap of one permanently blocks chain entry two.
    while position < chain.entries.len() {
        let spec = &chain.entries[position];
        // The live model itself and a spec already walked earlier in this chain
        // cannot be a swap: installing them would emit a phantom
        // FailoverStart/End pair and spend a unit of the per-turn cap on a
        // no-op (bd-oqo03.1).
        let is_current = crate::provider_metadata::split_provider_model_spec(spec).is_some_and(
            |(provider, model_id)| {
                crate::provider_metadata::provider_ids_match(&current_provider, provider)
                    && current_model.eq_ignore_ascii_case(model_id)
            },
        );
        let is_duplicate = chain.entries[..position]
            .iter()
            .any(|earlier| earlier.eq_ignore_ascii_case(spec));
        if is_current || is_duplicate {
            position += 1;
            continue;
        }
        let candidate = (|| {
            let (provider, model_id) = crate::provider_metadata::split_provider_model_spec(spec)?;
            options
                .available_models
                .iter()
                .find(|m| {
                    crate::provider_metadata::provider_ids_match(&m.model.provider, provider)
                        && m.model.id.eq_ignore_ascii_case(model_id)
                })
                .cloned()
                .or_else(|| crate::models::ad_hoc_model_entry(provider, model_id))
        })();
        position += 1;
        let Some(entry) = candidate else {
            continue;
        };
        let key = resolve_model_key(options.cli_api_key.as_deref(), &options.auth, &entry);
        if model_requires_configured_credential(&entry) && key.is_none() {
            // Skip entries we cannot authenticate: failing over into an
            // auth error would be strictly worse than the quota error.
            continue;
        }

        let Ok(provider_impl) = providers::create_provider(
            &entry,
            guard
                .extensions
                .as_ref()
                .map(crate::extensions::ExtensionRegion::manager),
        ) else {
            continue;
        };

        let to_provider = entry.model.provider.clone();
        let to_model = entry.model.id.clone();

        // Mutate and persist a private Session candidate. The live transcript,
        // provider/options, shared cooldown, and event stream remain untouched
        // if restoration, the inner lock, or persistence fails.
        let session_store = Arc::clone(&guard.session);
        let mut inner = OwnedMutexGuard::lock(session_store, cx)
            .await
            .map_err(|err| Error::session(format!("failover inner session lock failed: {err}")))?;
        let mut candidate = inner.clone();
        let reverted = candidate.revert_incomplete_response();
        if require_incomplete_tail && !reverted {
            return Err(Error::session(
                "failover restoration invariant failed: the completed error response had no incomplete assistant tail",
            ));
        }
        let restored_messages = candidate.to_messages_for_current_path();
        let target_thinking = entry.clamp_thinking_level(primary_model.requested_thinking_level);
        let target_thinking_text = target_thinking.to_string();
        let thinking_changed = candidate
            .effective_thinking_level_for_current_path()
            .as_deref()
            != Some(target_thinking_text.as_str());
        candidate.set_model_header(
            Some(to_provider.clone()),
            Some(to_model.clone()),
            Some(target_thinking_text.clone()),
        );
        candidate.append_custom_entry(
            "failover".to_string(),
            Some(serde_json::json!({
                "from": format!("{current_provider}/{current_model}"),
                "to": format!("{to_provider}/{to_model}"),
                "class": format!("{class:?}").to_ascii_lowercase(),
                "attempt": position,
            })),
        );
        candidate.append_model_change_with_role(
            to_provider.clone(),
            to_model.clone(),
            Some("failover".to_string()),
        );
        if thinking_changed {
            candidate.append_thinking_level_change(target_thinking_text);
        }
        let save_enabled = guard.save_enabled();
        guard.invalidate_background_compaction();
        let _provider_transition = state
            .provider_admission
            .begin_transition(
                "failover Session persistence was interrupted before live installation completed"
                    .to_string(),
                cx,
            )
            .await?;
        if save_enabled
            && let Err(first_err) = candidate.save().await
            && let Err(retry_err) = candidate.save().await
        {
            let reason = format!(
                "failover Session persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
            );
            state.provider_admission.block(reason.clone());
            return Err(Error::session_persistence(reason));
        }

        // No fallible operation remains in the transition after installation.
        *inner = candidate;
        guard.agent.replace_messages(restored_messages);
        guard.agent.set_provider(provider_impl);
        guard.agent.set_keyword_max_thinking_level(
            entry.clamp_thinking_level(crate::model::ThinkingLevel::Max),
        );
        guard.agent.set_tool_call_dialect(entry.tool_call_dialect());
        guard
            .agent
            .set_model_accepts_images(entry.model.input.contains(&InputType::Image));
        {
            let stream_options = guard.agent.stream_options_mut();
            stream_options.api_key.clone_from(&key);
            stream_options.headers.clone_from(&entry.headers);
            stream_options.max_tokens = Some(entry.model.max_tokens);
            stream_options.thinking_level = Some(target_thinking);
        }
        guard.set_compaction_context_window(context_window_tokens_for_entry(&entry));
        guard.refresh_extension_completion_host_state();
        if let Some(region) = &guard.extensions {
            region
                .manager()
                .set_current_model(Some(to_provider.clone()), Some(to_model.clone()));
        }

        state.failover_primary = Some(primary_model.clone());
        state.active_failover_model = Some((to_provider.clone(), to_model.clone()));
        state.failover_chain_position = Some(position);
        if let Some(tracker) = state.failover_cooldown.as_mut() {
            tracker.record_primary_failure(std::time::Instant::now());
        }
        state.provider_admission.clear();

        let event = agent_event(AgentEvent::FailoverStart {
            from_provider: current_provider.clone(),
            from_model: current_model.clone(),
            to_provider: to_provider.clone(),
            to_model: to_model.clone(),
            class: format!("{class:?}").to_ascii_lowercase(),
            attempt: position as u32,
        });
        drop(inner);
        drop(state);
        drop(guard);
        if let Some(attempt) = retry_attempt_to_end {
            let _ = out_tx.send(agent_event(AgentEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: Some(error_text.to_string()),
            }));
        }
        let _ = out_tx.send(event);
        return Ok(true);
    }
    Ok(false)
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

    let deferred_agent_end = Arc::new(std::sync::Mutex::new(None::<AgentEvent>));

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
        let event_handler = rpc_agent_event_handler(
            out_tx.clone(),
            runtime_handle,
            extensions,
            Some(Arc::clone(&deferred_agent_end)),
        );
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

    // Emit exactly one terminal event after AgentSession has completed its
    // persistence work, but before clearing streaming so EOF drains it.
    let terminal_messages = deferred_agent_end
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .and_then(|event| match event {
            AgentEvent::AgentEnd { messages, .. } => Some(messages),
            _ => None,
        })
        .unwrap_or_default();
    let mut payload = json!({
        "type": "agent_end",
        "messages": terminal_messages,
        "error": Value::Null,
    });
    if let Err(err) = result {
        payload["error"] = Value::String(err.to_string());
        payload["errorHints"] = error_hints_value(&err);
    }
    let _ = out_tx.send(event(&payload));
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

/// The `ask_request` event frame for one picker request (bd-cv653.3.8).
fn ask_request_rpc_event(request: &crate::ask::AskUiRequest) -> Value {
    json!({
        "type": "ask_request",
        "id": request.id,
        "questions": request.request.questions,
        "timeoutMs": crate::ask::ASK_UI_TIMEOUT_MS,
    })
}

/// Parse an `ask_response` command: `requestId` (or `id` alias) plus either
/// `dismissed: true` or an `answers` array of `{questionId, selected[],
/// other?}` objects.
fn rpc_parse_ask_response(
    parsed: &Value,
) -> std::result::Result<(String, crate::ask::AskResponse), String> {
    let request_id = parsed
        .get("requestId")
        .or_else(|| parsed.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "Missing requestId field".to_string())?
        .to_string();
    let dismissed = parsed
        .get("dismissed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if dismissed {
        return Ok((
            request_id,
            crate::ask::AskResponse {
                answers: Vec::new(),
                dismissed: true,
            },
        ));
    }
    let answers_value = parsed
        .get("answers")
        .cloned()
        .ok_or_else(|| "Missing answers field (or dismissed: true)".to_string())?;
    let answers: Vec<crate::ask::AskAnswer> = serde_json::from_value(answers_value)
        .map_err(|error| format!("Invalid answers: {error}"))?;
    if answers.is_empty() {
        return Err("answers must not be empty (use dismissed: true to cancel)".to_string());
    }
    Ok((
        request_id,
        crate::ask::AskResponse {
            answers,
            dismissed: false,
        },
    ))
}

fn rpc_publish_extension_ui_request(
    ui_state: Arc<std::sync::Mutex<RpcUiBridgeState>>,
    manager: ExtensionManager,
    out_tx_ui: std::sync::mpsc::SyncSender<String>,
    active: RpcUiBridgeRequest,
) {
    rpc_publish_extension_ui_request_at_seam(ui_state, manager, active, |frame| {
        rpc_try_send_extension_ui_frame(&out_tx_ui, frame)
    });
}

fn rpc_publish_extension_ui_request_at_seam(
    ui_state: Arc<std::sync::Mutex<RpcUiBridgeState>>,
    manager: ExtensionManager,
    mut active: RpcUiBridgeRequest,
    mut publish: impl FnMut(String) -> bool,
) {
    loop {
        let request_id = active.request.id.clone();
        let generation = active.generation;
        let mut rpc_event = active.request.to_rpc_event();
        if let Some(event) = rpc_event.as_object_mut() {
            event.insert("requestGeneration".to_string(), Value::from(generation));
        }

        let expiration = {
            let mut guard = ui_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !guard.active_matches(&request_id, generation) {
                return;
            }

            let remaining = active.request.remaining_timeout(std::time::Instant::now());
            if remaining.is_some_and(|remaining| remaining.is_zero()) {
                guard.expire(&request_id, generation)
            } else {
                if let Some(remaining) = remaining {
                    let remaining_ms =
                        u64::try_from(remaining.as_nanos().div_ceil(1_000_000)).unwrap_or(u64::MAX);
                    if let Some(event) = rpc_event.as_object_mut() {
                        event.insert("timeout_ms".to_string(), Value::from(remaining_ms));
                    }
                }

                // Serialization happens while expiry holds the same state
                // lock. Recheck the absolute deadline after that potentially
                // extension-controlled work and immediately before the
                // nonblocking publication linearization point.
                let frame = event(&rpc_event);
                let expired_during_serialization = active
                    .request
                    .deadline()
                    .is_some_and(|deadline| deadline <= std::time::Instant::now());
                if expired_during_serialization || !publish(frame) {
                    guard.expire(&request_id, generation)
                } else {
                    return;
                }
            }
        };

        let Some(expiration) = expiration else {
            return;
        };
        let _ = manager.respond_ui(expiration.response);
        let Some(next) = expiration.next else {
            return;
        };
        active = next;
    }
}

fn rpc_try_send_extension_ui_event(
    out_tx_ui: &std::sync::mpsc::SyncSender<String>,
    rpc_event: &Value,
) -> bool {
    rpc_try_send_extension_ui_frame(out_tx_ui, event(rpc_event))
}

fn rpc_try_send_extension_ui_frame(
    out_tx_ui: &std::sync::mpsc::SyncSender<String>,
    frame: String,
) -> bool {
    out_tx_ui.try_send(frame).is_ok()
}

fn rpc_schedule_extension_ui_timeout(
    runtime_handle: &RuntimeHandle,
    ui_state: Arc<std::sync::Mutex<RpcUiBridgeState>>,
    manager: ExtensionManager,
    out_tx_ui: std::sync::mpsc::SyncSender<String>,
    admitted: &RpcUiBridgeRequest,
    cancel_rx: oneshot::Receiver<()>,
) {
    let Some(deadline) = admitted.request.deadline() else {
        return;
    };
    let request_id = admitted.request.id.clone();
    let generation = admitted.generation;
    runtime_handle.spawn(async move {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let cancelled = Box::pin(async move {
            let mut cancel_rx = cancel_rx;
            let cx = AgentCx::for_current_or_request();
            let _ = cancel_rx.recv(cx.cx()).await;
        });
        let deadline = Box::pin(sleep(wall_now(), remaining));
        if matches!(
            futures::future::select(cancelled, deadline).await,
            futures::future::Either::Left(_)
        ) {
            return;
        }
        rpc_resolve_extension_ui_default(ui_state, manager, out_tx_ui, request_id, generation);
    });
}

fn rpc_resolve_extension_ui_default(
    ui_state: Arc<std::sync::Mutex<RpcUiBridgeState>>,
    manager: ExtensionManager,
    out_tx_ui: std::sync::mpsc::SyncSender<String>,
    request_id: String,
    generation: u64,
) {
    let expiration = {
        let mut guard = ui_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.expire(&request_id, generation)
    };
    let Some(expiration) = expiration else {
        return;
    };

    // The private admission generation prevents a stale sleeper for request
    // A from resolving a later request B that reuses A's public id. It also
    // lets a queued request expire without disturbing the active slot.
    let _ = manager.respond_ui(expiration.response);
    if let Some(next) = expiration.next {
        rpc_publish_extension_ui_request(ui_state, manager, out_tx_ui, next);
    }
}

fn rpc_extension_ui_timeout_response(request: &ExtensionUiRequest) -> ExtensionUiResponse {
    if request.is_capability_prompt() {
        request.auto_deny_response()
    } else {
        ExtensionUiResponse {
            id: request.id.clone(),
            value: None,
            cancelled: true,
        }
    }
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

fn rpc_parse_extension_ui_response_generation(parsed: &Value) -> Option<u64> {
    parsed.get("requestGeneration").and_then(Value::as_u64)
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
            if active.is_capability_prompt() {
                let scope = parsed
                    .get("value")
                    .and_then(Value::as_object)
                    .or_else(|| parsed.as_object())
                    .ok_or_else(|| {
                        "capability confirm requires scoped `allow` and `persist` booleans"
                            .to_string()
                    })?;
                let allow = scope
                    .get("allow")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| "capability confirm requires boolean `allow`".to_string())?;
                let persist = scope
                    .get("persist")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| "capability confirm requires boolean `persist`".to_string())?;
                let remember = scope
                    .get("remember")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| "capability confirm requires boolean `remember`".to_string())?;
                if persist && !remember {
                    return Err(
                        "capability confirm cannot persist a decision without remembering it"
                            .to_string(),
                    );
                }
                return Ok(ExtensionUiResponse {
                    id: active.id.clone(),
                    value: Some(json!({
                        "allow": allow,
                        "persist": persist,
                        "remember": remember,
                    })),
                    cancelled: false,
                });
            }
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
        "getEditorText" | "get_editor_text" | "getAllThemes" | "get_all_themes" | "getTheme"
        | "get_theme" | "setTheme" | "set_theme" => {
            let value = parsed
                .get("value")
                .cloned()
                .ok_or_else(|| format!("{} response requires `value` field", active.method))?;
            Ok(ExtensionUiResponse {
                id: active.id.clone(),
                value: Some(value),
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
    fn parse_capability_confirm_requires_explicit_scope() {
        let active = ExtensionUiRequest::new_capability_prompt(
            "req-cap",
            "trusted-extension",
            "exec",
            json!({"title": "Capability"}),
        );
        let scoped = json!({
            "type": "extension_ui_response",
            "requestId": "req-cap",
            "value": {"allow": true, "persist": false, "remember": false}
        });
        let response =
            rpc_parse_extension_ui_response(&scoped, &active).expect("parse scoped capability");
        assert_eq!(
            response.value,
            Some(json!({"allow": true, "persist": false, "remember": false}))
        );

        for unscoped in [
            json!({"requestId": "req-cap", "confirmed": true}),
            json!({"requestId": "req-cap", "value": true}),
            json!({"requestId": "req-cap", "value": {"allow": true}}),
            json!({"requestId": "req-cap", "value": {"allow": true, "persist": false}}),
        ] {
            assert!(
                rpc_parse_extension_ui_response(&unscoped, &active).is_err(),
                "unscoped capability decision must fail: {unscoped}"
            );
        }
    }

    #[test]
    fn parse_capability_confirm_accepts_top_level_scope() {
        let active = ExtensionUiRequest::new_capability_prompt(
            "req-cap",
            "trusted-extension",
            "exec",
            json!({"title": "Capability"}),
        );
        let top_level = json!({
            "type": "extension_ui_response",
            "requestId": "req-cap",
            "allow": false,
            "persist": true,
            "remember": true,
        });

        let response = rpc_parse_extension_ui_response(&top_level, &active)
            .expect("parse top-level capability scope");
        assert_eq!(
            response.value,
            Some(json!({"allow": false, "persist": true, "remember": true}))
        );
    }

    #[test]
    fn parse_capability_confirm_rejects_persist_without_remember() {
        let active = ExtensionUiRequest::new_capability_prompt(
            "req-cap",
            "trusted-extension",
            "exec",
            json!({"title": "Capability"}),
        );
        let contradictory = json!({
            "type": "extension_ui_response",
            "requestId": "req-cap",
            "value": {"allow": true, "persist": true, "remember": false},
        });

        let err = rpc_parse_extension_ui_response(&contradictory, &active)
            .expect_err("persistence without session remembering must fail");
        assert_eq!(
            err,
            "capability confirm cannot persist a decision without remembering it"
        );
    }

    #[test]
    fn capability_rpc_timeout_is_typed_auto_deny_not_user_cancel() {
        let request = ExtensionUiRequest::new_capability_prompt(
            "req-cap-timeout",
            "trusted-extension",
            "exec",
            json!({"title": "Capability"}),
        );

        let response = rpc_extension_ui_timeout_response(&request);
        assert_eq!(response.id, "req-cap-timeout");
        assert_eq!(
            response.value,
            Some(json!({
                "allow": false,
                "persist": false,
                "remember": false,
                "reason": "auto_deny",
            }))
        );
        assert!(!response.cancelled);

        let ordinary = rpc_extension_ui_timeout_response(&ExtensionUiRequest::new(
            "req-confirm-timeout",
            "confirm",
            json!({"title": "Ordinary"}),
        ));
        assert_eq!(ordinary.value, None);
        assert!(ordinary.cancelled);
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
    fn parser_supports_every_query_method_that_expects_a_response() {
        for method in [
            "getEditorText",
            "get_editor_text",
            "getAllThemes",
            "get_all_themes",
            "getTheme",
            "get_theme",
            "setTheme",
            "set_theme",
        ] {
            let active = ExtensionUiRequest::new("req-query", method, json!({}));
            assert!(
                active.expects_response(),
                "fixture method must be response-bearing"
            );
            let parsed = json!({"requestId": "req-query", "value": {"ok": true}});
            let response = rpc_parse_extension_ui_response(&parsed, &active)
                .unwrap_or_else(|error| panic!("{method}: {error}"));
            assert_eq!(response.value, Some(json!({"ok": true})), "{method}");
        }
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
    fn parse_response_generation_requires_an_unsigned_integer() {
        assert_eq!(
            rpc_parse_extension_ui_response_generation(&json!({"requestGeneration": 7})),
            Some(7)
        );
        for invalid in [
            json!({}),
            json!({"requestGeneration": "7"}),
            json!({"requestGeneration": -1}),
        ] {
            assert!(rpc_parse_extension_ui_response_generation(&invalid).is_none());
        }
    }

    #[test]
    fn bridge_state_default_is_empty() {
        let state = RpcUiBridgeState::default();
        assert!(state.active.is_none());
        assert!(state.queue.is_empty());
        assert!(!state.closed);
    }

    #[test]
    fn stale_timeout_cannot_resolve_later_activation_that_reuses_public_id() {
        let mut state = RpcUiBridgeState::default();
        let (first, first_emits, first_cancel_rx) = state
            .admit(ExtensionUiRequest::new(
                "reused-id",
                "confirm",
                json!({"title": "first"}),
            ))
            .expect("bridge is open");
        assert!(first_emits);
        assert!(first_cancel_rx.is_none());
        let first_expiration = state
            .expire("reused-id", first.generation)
            .expect("the first activation is live");
        assert_eq!(first_expiration.response.id, "reused-id");
        assert!(first_expiration.next.is_none());

        let (second, second_emits, second_cancel_rx) = state
            .admit(ExtensionUiRequest::new(
                "reused-id",
                "confirm",
                json!({"title": "second"}),
            ))
            .expect("bridge is open");
        assert!(second_emits);
        assert!(second_cancel_rx.is_none());

        assert_ne!(first.generation, second.generation);
        assert!(
            state.expire("reused-id", first.generation).is_none(),
            "the first activation's late sleeper must be inert"
        );
        let late_client_generation = rpc_parse_extension_ui_response_generation(&json!({
            "requestGeneration": first.generation,
        }))
        .expect("late response carries A's generation");
        assert!(
            !state.active_matches("reused-id", late_client_generation),
            "a late client response to A must not correlate with B"
        );
        assert!(state.active_matches("reused-id", second.generation));
        assert_eq!(
            state
                .active
                .as_ref()
                .and_then(|active| { active.request.payload.get("title").and_then(Value::as_str) }),
            Some("second")
        );
    }

    #[test]
    fn queued_deadline_expires_without_disturbing_unbounded_active_request() {
        let mut state = RpcUiBridgeState::default();
        let (active, active_emits, active_cancel_rx) = state
            .admit(ExtensionUiRequest::new(
                "unbounded-active",
                "confirm",
                json!({"title": "active"}),
            ))
            .expect("bridge is open");
        assert!(active_emits);
        assert!(active_cancel_rx.is_none());
        let mut bounded = ExtensionUiRequest::new_capability_prompt(
            "bounded-queued",
            "trusted-extension",
            "exec",
            json!({"title": "queued"}),
        )
        .with_timeout_ms(30_000);
        bounded.bind_deadline(std::time::Instant::now());
        assert!(bounded.deadline().is_some(), "fixture must be bounded");
        let (queued, queued_emits, queued_cancel_rx) =
            state.admit(bounded).expect("bridge is open");
        assert!(!queued_emits);
        let mut queued_cancel_rx = queued_cancel_rx.expect("bounded request owns a waiter");

        let expiration = state
            .expire("bounded-queued", queued.generation)
            .expect("queued request owns an independent deadline identity");

        assert_eq!(expiration.response.id, "bounded-queued");
        assert_eq!(
            expiration.response.value,
            Some(json!({
                "allow": false,
                "persist": false,
                "remember": false,
                "reason": "auto_deny",
            }))
        );
        assert!(!expiration.response.cancelled);
        assert!(expiration.next.is_none());
        assert_eq!(
            state.active.as_ref().map(|active| active.generation),
            Some(active.generation)
        );
        assert!(state.queue.is_empty());
        assert_eq!(
            queued_cancel_rx.try_recv(),
            Ok(()),
            "terminal queue expiry cancels its outstanding sleeper"
        );
    }

    #[test]
    fn extension_ui_publication_is_nonblocking_under_backpressure() {
        let (out_tx, _out_rx) = std::sync::mpsc::sync_channel(1);
        out_tx
            .send("already full".to_string())
            .expect("fixture fills the output channel");

        assert!(
            !rpc_try_send_extension_ui_event(
                &out_tx,
                &json!({"type": "extension_ui_request", "id": "blocked"}),
            ),
            "a full client channel must fail closed without consuming prompt budget"
        );
    }

    #[test]
    fn full_channel_publication_fails_closed_and_advances_entire_fifo() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let mut bridge = RpcUiBridgeState::default();
            let (first, first_emits, _) = bridge
                .admit(ExtensionUiRequest::new(
                    "first",
                    "confirm",
                    json!({"title": "first"}),
                ))
                .expect("bridge is open");
            let (_, second_emits, _) = bridge
                .admit(ExtensionUiRequest::new(
                    "second",
                    "confirm",
                    json!({"title": "second"}),
                ))
                .expect("bridge is open");
            assert!(first_emits);
            assert!(!second_emits);

            let state = Arc::new(std::sync::Mutex::new(bridge));
            let (out_tx, _out_rx) = std::sync::mpsc::sync_channel(1);
            out_tx
                .send("already full".to_string())
                .expect("fixture fills the output channel");

            rpc_publish_extension_ui_request(
                Arc::clone(&state),
                ExtensionManager::new(),
                out_tx,
                first,
            );

            let guard = state.lock().expect("bridge lock");
            assert!(guard.active.is_none());
            assert!(guard.queue.is_empty());
        });
    }

    #[test]
    fn expired_active_request_is_never_published() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let mut request =
                ExtensionUiRequest::new("already-expired", "confirm", json!({"title": "too late"}))
                    .with_timeout_ms(0);
            request.bind_deadline(std::time::Instant::now());

            let mut bridge = RpcUiBridgeState::default();
            let (active, emits, _) = bridge.admit(request).expect("bridge is open");
            assert!(emits);
            let state = Arc::new(std::sync::Mutex::new(bridge));
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel(4);

            rpc_publish_extension_ui_request(
                Arc::clone(&state),
                ExtensionManager::new(),
                out_tx,
                active,
            );

            assert!(
                out_rx.try_recv().is_err(),
                "an expired frame must not cross the publication boundary"
            );
            let guard = state.lock().expect("bridge lock");
            assert!(guard.active.is_none());
        });
    }

    #[test]
    fn queued_scheduler_expires_bounded_successor_behind_unbounded_active() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();
        runtime.block_on(async move {
            let mut bridge = RpcUiBridgeState::default();
            let (active, active_emits, _) = bridge
                .admit(ExtensionUiRequest::new(
                    "unbounded-active",
                    "confirm",
                    json!({"title": "active"}),
                ))
                .expect("bridge is open");
            assert!(active_emits);

            let mut bounded = ExtensionUiRequest::new_capability_prompt(
                "bounded-queued",
                "trusted-extension",
                "exec",
                json!({"title": "queued"}),
            )
            .with_timeout_ms(10);
            bounded.bind_deadline(std::time::Instant::now());
            let (queued, queued_emits, cancel_rx) = bridge.admit(bounded).expect("bridge is open");
            assert!(!queued_emits);
            let cancel_rx = cancel_rx.expect("bounded request owns a waiter");

            let state = Arc::new(std::sync::Mutex::new(bridge));
            let (out_tx, _out_rx) = std::sync::mpsc::sync_channel(4);
            rpc_schedule_extension_ui_timeout(
                &runtime_handle,
                Arc::clone(&state),
                ExtensionManager::new(),
                out_tx,
                &queued,
                cancel_rx,
            );
            sleep(wall_now(), Duration::from_millis(50)).await;

            let guard = state.lock().expect("bridge lock");
            assert_eq!(
                guard.active.as_ref().map(|active| active.generation),
                Some(active.generation)
            );
            assert!(guard.queue.is_empty());
        });
    }

    #[test]
    fn normal_resolution_cancels_and_releases_long_deadline_sleeper() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();
        runtime.block_on(async move {
            let mut request = ExtensionUiRequest::new(
                "promptly-answered",
                "confirm",
                json!({"title": "answer now"}),
            )
            .with_timeout_ms(30_000);
            request.bind_deadline(std::time::Instant::now());

            let mut bridge = RpcUiBridgeState::default();
            let (active, emits, cancel_rx) = bridge.admit(request).expect("bridge is open");
            assert!(emits);
            let cancel_rx = cancel_rx.expect("bounded request owns a waiter");
            let state = Arc::new(std::sync::Mutex::new(bridge));
            let (out_tx, _out_rx) = std::sync::mpsc::sync_channel(4);
            rpc_schedule_extension_ui_timeout(
                &runtime_handle,
                Arc::clone(&state),
                ExtensionManager::new(),
                out_tx,
                &active,
                cancel_rx,
            );

            {
                let mut guard = state.lock().expect("bridge lock");
                assert!(guard.finish_active().is_none());
            }
            sleep(wall_now(), Duration::from_millis(20)).await;
            assert_eq!(
                Arc::strong_count(&state),
                1,
                "cancellation must release the task's captured bridge state promptly"
            );
        });
    }

    #[test]
    fn rpc_close_guard_cancels_long_timer_and_suppresses_post_close_publication() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();
        runtime.block_on(async move {
            let mut active_request = ExtensionUiRequest::new(
                "terminal-close",
                "confirm",
                json!({"title": "must not outlive RPC"}),
            )
            .with_timeout_ms(30_000);
            active_request.bind_deadline(std::time::Instant::now());
            let mut queued_request = ExtensionUiRequest::new(
                "terminal-close-queued",
                "input",
                json!({"title": "also must not outlive RPC"}),
            )
            .with_timeout_ms(30_000);
            queued_request.bind_deadline(std::time::Instant::now());

            let mut bridge = RpcUiBridgeState::default();
            let (active, active_emits, active_cancel_rx) =
                bridge.admit(active_request).expect("bridge is open");
            let (queued, queued_emits, queued_cancel_rx) =
                bridge.admit(queued_request).expect("bridge is open");
            assert!(active_emits);
            assert!(!queued_emits);
            let active_cancel_rx = active_cancel_rx.expect("active request owns a waiter");
            let queued_cancel_rx = queued_cancel_rx.expect("queued request owns a waiter");
            let state = Arc::new(std::sync::Mutex::new(bridge));
            let manager = ExtensionManager::new();
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel(4);
            rpc_schedule_extension_ui_timeout(
                &runtime_handle,
                Arc::clone(&state),
                manager.clone(),
                out_tx.clone(),
                &active,
                active_cancel_rx,
            );
            rpc_schedule_extension_ui_timeout(
                &runtime_handle,
                Arc::clone(&state),
                manager.clone(),
                out_tx.clone(),
                &queued,
                queued_cancel_rx,
            );

            let close_guard = ExtensionUiCloseGuard {
                manager: manager.clone(),
                ui_state: Arc::clone(&state),
            };

            let (seam_entered_tx, seam_entered_rx) = std::sync::mpsc::sync_channel(1);
            let (release_seam_tx, release_seam_rx) = std::sync::mpsc::sync_channel(1);
            let publisher_state = Arc::clone(&state);
            let publisher_manager = manager.clone();
            let publisher_out_tx = out_tx.clone();
            let post_close_active = active.clone();
            let publisher = std::thread::spawn(move || {
                rpc_publish_extension_ui_request_at_seam(
                    publisher_state,
                    publisher_manager,
                    active,
                    |frame| {
                        seam_entered_tx.send(()).expect("announce publication seam");
                        release_seam_rx.recv().expect("release publication seam");
                        rpc_try_send_extension_ui_frame(&publisher_out_tx, frame)
                    },
                );
            });
            seam_entered_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("publisher should reach the locked publication seam");
            assert!(
                matches!(state.try_lock(), Err(std::sync::TryLockError::WouldBlock)),
                "the actual try_send seam must retain the bridge linearizer"
            );

            let (close_started_tx, close_started_rx) = std::sync::mpsc::sync_channel(1);
            let (close_done_tx, close_done_rx) = std::sync::mpsc::sync_channel(1);
            let closer = std::thread::spawn(move || {
                close_started_tx.send(()).expect("start terminal close");
                drop(close_guard);
                close_done_tx.send(()).expect("report terminal close");
            });
            close_started_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("terminal close thread should start");
            assert!(
                matches!(
                    close_done_rx.recv_timeout(Duration::from_millis(20)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ),
                "terminal close must wait while publication owns the linearizer"
            );

            release_seam_tx.send(()).expect("release publisher");
            publisher.join().expect("publisher thread");
            close_done_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("terminal close should finish after publication releases the lock");
            closer.join().expect("terminal close thread");

            let published_before_close = out_rx
                .try_recv()
                .expect("the frame linearized before close should be published");
            assert!(published_before_close.contains("terminal-close"));

            rpc_publish_extension_ui_request(
                Arc::clone(&state),
                manager,
                out_tx,
                post_close_active,
            );

            sleep(wall_now(), Duration::from_millis(20)).await;
            assert_eq!(
                Arc::strong_count(&state),
                1,
                "RAII close must wake and release every long-deadline timer task"
            );
            let mut guard = state.lock().expect("bridge lock");
            assert!(guard.closed);
            assert!(guard.active.is_none());
            assert!(guard.queue.is_empty());
            assert!(
                guard
                    .admit(ExtensionUiRequest::new(
                        "too-late",
                        "confirm",
                        json!({"title": "must be rejected"}),
                    ))
                    .is_none(),
                "terminal close must reject every later bridge admission"
            );
            drop(guard);
            assert!(
                out_rx.try_recv().is_err(),
                "no second extension UI frame may cross the terminal close boundary"
            );
        });
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
    use super::tests::{build_test_rpc_options, dummy_entry};
    use super::*;
    use crate::agent::{Agent, AgentConfig, AgentSession};
    use crate::model::{AssistantMessage, Usage};
    use crate::provider::{InputType, Model, ModelCost, Provider};
    use crate::resources::ResourceLoader;
    use crate::session::Session;
    use crate::tools::ToolRegistry;
    use async_trait::async_trait;
    use futures::stream;
    use std::collections::HashMap;
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

    fn rpc_fork_source_session() -> (Session, String) {
        let mut session = Session::in_memory();
        session.header.provider = Some("anthropic".to_string());
        session.header.model_id = Some("test-model".to_string());
        session.header.thinking_level = Some("off".to_string());
        session.append_message(SessionMessage::User {
            content: UserContent::Text("fork this prompt".to_string()),
            timestamp: Some(0),
        });
        let entry_id = session
            .entries_for_current_path()
            .last()
            .and_then(|entry| entry.base_id())
            .cloned()
            .expect("fork source entry id");
        (session, entry_id)
    }

    fn rpc_fork_test_options(
        runtime_handle: &asupersync::runtime::RuntimeHandle,
        auth_path: PathBuf,
    ) -> RpcOptions {
        let mut options = build_test_rpc_options(runtime_handle, auth_path);
        let mut model = dummy_entry("test-model", false);
        model.api_key = Some("test-key".to_string());
        options.available_models.push(model);
        options
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
            let provider_probe = Arc::clone(&provider);
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
                ..Default::default()
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
                ask_tool: None,
            };

            run_prompt_with_retry(
                Arc::clone(&session),
                Arc::clone(&shared_state),
                is_streaming,
                is_compacting,
                Arc::new(std::sync::Mutex::new(())),
                abort_handle_slot,
                out_tx,
                retry_abort,
                options,
                "hello".to_string(),
                None,
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
            assert_eq!(
                provider_probe.calls.load(Ordering::SeqCst),
                2,
                "the production retry loop must make exactly one resumed provider call"
            );

            let verify_cx = AgentCx::for_request();
            let guard = session.lock(&verify_cx).await.expect("agent session lock");
            let inner = guard
                .session
                .lock(verify_cx.cx())
                .await
                .expect("inner session lock");
            let path = inner.entries_for_current_path();
            assert_eq!(
                path.iter()
                    .filter(|entry| matches!(entry, SessionEntry::Message(message) if matches!(&message.message, SessionMessage::User { .. })))
                    .count(),
                1,
                "retry must preserve the original user message exactly once"
            );
            assert_eq!(
                path.iter()
                    .filter(|entry| matches!(
                        entry,
                        SessionEntry::Message(message)
                            if matches!(
                                &message.message,
                                SessionMessage::Assistant { message }
                                    if message.stop_reason == StopReason::Stop
                            )
                    ))
                    .count(),
                1,
                "retry must leave exactly one successful assistant tail"
            );
            assert!(
                path.iter().all(|entry| !matches!(
                    entry,
                    SessionEntry::Message(message)
                        if matches!(
                            &message.message,
                            SessionMessage::Assistant { message }
                                if message.stop_reason == StopReason::Error
                        )
                )),
                "the failed assistant tail must be absent from the active path"
            );
        });
    }

    /// bd-2vmu6.1: a turn that swaps to a fallback chain entry closes its
    /// `FailoverStart` with exactly one `FailoverEnd { restoredPrimary: false }`
    /// before the terminal `agent_end`, here on the failure path (the fallback
    /// entry points at an unreachable local port, so its turn fails
    /// deterministically without a network). Removing the emission from the
    /// terminal section of `run_prompt_with_retry` fails this test.
    #[test]
    fn rpc_fallback_turn_emits_exactly_one_failover_end_before_agent_end() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 8)
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(Arc::new(AlwaysErrorProvider), tools, AgentConfig::default());
            let session_temp = tempfile::tempdir().expect("session tempdir");
            let inner_session = Arc::new(Mutex::new(Session::create_with_dir(Some(
                session_temp.path().join("sessions"),
            ))));
            let agent_session = AgentSession::new(
                agent,
                inner_session,
                true,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            let session = Arc::new(Mutex::new(agent_session));

            let fallback = crate::models::ModelEntry {
                model: Model {
                    id: "fallback-model".to_string(),
                    name: "fallback-model".to_string(),
                    api: "openai-completions".to_string(),
                    provider: "openai".to_string(),
                    // Nothing listens here: the fallback turn fails fast.
                    base_url: "http://127.0.0.1:1/v1".to_string(),
                    reasoning: false,
                    input: vec![InputType::Text],
                    cost: ModelCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                    },
                    context_window: 8_192,
                    max_tokens: 1_024,
                    headers: HashMap::new(),
                },
                api_key: Some("fallback-key".to_string()),
                headers: HashMap::new(),
                auth_header: true,
                compat: None,
                oauth_config: None,
            };
            let mut config = Config::default();
            config.retry = Some(crate::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(0),
                fallback_chains: Some(HashMap::from([(
                    "default".to_string(),
                    vec!["openai/fallback-model".to_string()],
                )])),
                max_failovers_per_turn: Some(1),
                ..Default::default()
            });
            let mut shared = RpcSharedState::new(&config);
            shared.auto_compaction_enabled = false;
            let shared_state = Arc::new(Mutex::new(shared));
            let auth_temp = tempfile::tempdir().expect("auth tempdir");
            let options = RpcOptions {
                config,
                resources: ResourceLoader::empty(false),
                available_models: vec![fallback],
                scoped_models: Vec::new(),
                cli_api_key: None,
                auth: AuthStorage::load(auth_temp.path().join("auth.json")).expect("auth load"),
                runtime_handle,
                ask_tool: None,
            };
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);

            run_prompt_with_retry(
                Arc::clone(&session),
                Arc::clone(&shared_state),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(std::sync::Mutex::new(())),
                Arc::new(Mutex::new(None)),
                out_tx,
                Arc::new(AtomicBool::new(false)),
                options,
                "hello".to_string(),
                None,
                Vec::new(),
                AgentCx::for_request(),
            )
            .await;

            let events: Vec<Value> = out_rx
                .try_iter()
                .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
                .collect();
            let kinds: Vec<&str> = events
                .iter()
                .filter_map(|value| value.get("type").and_then(Value::as_str))
                .collect();
            let position = |kind: &str| kinds.iter().position(|k| *k == kind);
            let start = position("failover_start")
                .unwrap_or_else(|| panic!("failover_start must be emitted; saw {kinds:?}"));
            let end = position("failover_end")
                .unwrap_or_else(|| panic!("failover_end must be emitted; saw {kinds:?}"));
            let agent_end = position("agent_end")
                .unwrap_or_else(|| panic!("agent_end must be emitted; saw {kinds:?}"));
            assert!(start < end && end < agent_end, "lifecycle order: {kinds:?}");
            assert_eq!(
                kinds.iter().filter(|k| **k == "failover_end").count(),
                1,
                "exactly one failover_end per failover_start: {kinds:?}"
            );
            let end_event = &events[end];
            assert_eq!(end_event["success"], Value::Bool(false));
            assert_eq!(end_event["restoredPrimary"], Value::Bool(false));
            assert_eq!(end_event["provider"], "openai");
            assert_eq!(end_event["model"], "fallback-model");
            assert!(
                !events[agent_end]["error"].is_null(),
                "the fallback turn fails: {}",
                events[agent_end]
            );
        });
    }

    /// bd-oqo03.1 (RPC side): a chain that names the live model first, then a
    /// duplicate, must not install a phantom swap or spend the walk on it; the
    /// first real fallback is installed with exactly one `FailoverStart`.
    #[test]
    fn rpc_failover_walk_skips_current_and_duplicate_entries() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 8)
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(Arc::new(AlwaysErrorProvider), tools, AgentConfig::default());
            let session_temp = tempfile::tempdir().expect("session tempdir");
            let inner_session = Arc::new(Mutex::new(Session::create_with_dir(Some(
                session_temp.path().join("sessions"),
            ))));
            let agent_session = AgentSession::new(
                agent,
                inner_session,
                true,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            let session = Arc::new(Mutex::new(agent_session));

            let fallback = crate::models::ModelEntry {
                model: Model {
                    id: "fallback-model".to_string(),
                    name: "fallback-model".to_string(),
                    api: "openai-completions".to_string(),
                    provider: "openai".to_string(),
                    base_url: "http://127.0.0.1:1/v1".to_string(),
                    reasoning: false,
                    input: vec![InputType::Text],
                    cost: ModelCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                    },
                    context_window: 8_192,
                    max_tokens: 1_024,
                    headers: HashMap::new(),
                },
                api_key: Some("fallback-key".to_string()),
                headers: HashMap::new(),
                auth_header: true,
                compat: None,
                oauth_config: None,
            };
            let mut config = Config::default();
            config.retry = Some(crate::config::RetrySettings {
                fallback_chains: Some(HashMap::from([(
                    "default".to_string(),
                    vec![
                        "test-provider/test-model".to_string(),
                        "test-provider/test-model".to_string(),
                        "openai/fallback-model".to_string(),
                    ],
                )])),
                max_failovers_per_turn: Some(1),
                ..Default::default()
            });
            let shared_state = Arc::new(Mutex::new(RpcSharedState::new(&config)));
            let auth_temp = tempfile::tempdir().expect("auth tempdir");
            let options = RpcOptions {
                config,
                resources: ResourceLoader::empty(false),
                available_models: vec![fallback],
                scoped_models: Vec::new(),
                cli_api_key: None,
                auth: AuthStorage::load(auth_temp.path().join("auth.json")).expect("auth load"),
                runtime_handle,
                ask_tool: None,
            };
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(16);
            let cx = AgentCx::for_request();

            assert!(
                try_failover_to_next_chain_entry(
                    Arc::clone(&session),
                    Arc::clone(&shared_state),
                    out_tx,
                    &options,
                    Some("server error"),
                    false,
                    None,
                    &cx,
                )
                .await
                .expect("walk past the current and duplicate entries"),
                "the real fallback must be installed"
            );

            let guard = session.lock(&cx).await.expect("agent session lock");
            assert_eq!(guard.agent.provider().name(), "openai");
            assert_eq!(guard.agent.provider().model_id(), "fallback-model");
            drop(guard);

            let starts: Vec<Value> = out_rx
                .try_iter()
                .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
                .filter(|value| value.get("type").and_then(Value::as_str) == Some("failover_start"))
                .collect();
            assert_eq!(
                starts.len(),
                1,
                "exactly one swap was announced: {starts:?}"
            );
            assert_eq!(starts[0]["toProvider"], "openai");
            assert_eq!(starts[0]["toModel"], "fallback-model");

            let state = shared_state.lock(&cx).await.expect("shared state lock");
            assert_eq!(
                state.failover_chain_position,
                Some(3),
                "the cursor advanced past the skipped entries and the swap"
            );
        });
    }

    #[test]
    fn rpc_failover_requires_tail_then_commits_restored_candidate() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 8)
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(Arc::new(AlwaysErrorProvider), tools, AgentConfig::default());
            let session_temp = tempfile::tempdir().expect("session tempdir");
            let inner_session = Arc::new(Mutex::new(Session::create_with_dir(Some(
                session_temp.path().join("sessions"),
            ))));
            let agent_session = AgentSession::new(
                agent,
                inner_session,
                true,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            let session = Arc::new(Mutex::new(agent_session));

            let fallback = crate::models::ModelEntry {
                model: Model {
                    id: "fallback-model".to_string(),
                    name: "fallback-model".to_string(),
                    api: "anthropic".to_string(),
                    provider: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    reasoning: false,
                    input: vec![InputType::Text],
                    cost: ModelCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                    },
                    context_window: 8_192,
                    max_tokens: 1_024,
                    headers: HashMap::new(),
                },
                api_key: Some("fallback-key".to_string()),
                headers: HashMap::from([("x-fallback".to_string(), "true".to_string())]),
                auth_header: true,
                compat: None,
                oauth_config: None,
            };
            let mut config = Config::default();
            config.retry = Some(crate::config::RetrySettings {
                fallback_chains: Some(HashMap::from([(
                    "default".to_string(),
                    vec!["anthropic/fallback-model".to_string()],
                )])),
                max_failovers_per_turn: Some(1),
                ..Default::default()
            });
            let shared_state = Arc::new(Mutex::new(RpcSharedState::new(&config)));
            let auth_path = tempfile::tempdir()
                .expect("tempdir")
                .path()
                .join("auth.json");
            let options = RpcOptions {
                config,
                resources: ResourceLoader::empty(false),
                available_models: vec![fallback],
                scoped_models: Vec::new(),
                cli_api_key: None,
                auth: AuthStorage::load(auth_path).expect("auth load"),
                runtime_handle,
                ask_tool: None,
            };
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(16);
            let cx = AgentCx::for_request();

            let err = try_failover_to_next_chain_entry(
                Arc::clone(&session),
                Arc::clone(&shared_state),
                out_tx.clone(),
                &options,
                Some("server error"),
                true,
                None,
                &cx,
            )
            .await
            .expect_err("known assistant errors require a restorable tail");
            assert!(
                err.to_string().contains("no incomplete assistant tail"),
                "unexpected failover invariant error: {err}"
            );

            let guard = session.lock(&cx).await.expect("agent session lock");
            assert_eq!(guard.agent.provider().name(), "test-provider");
            assert_eq!(guard.agent.provider().model_id(), "test-model");
            assert!(guard.agent.stream_options().api_key.is_none());
            assert!(guard.agent.stream_options().headers.is_empty());
            let inner = guard
                .session
                .lock(cx.cx())
                .await
                .expect("inner session lock");
            assert!(
                inner.entries.is_empty(),
                "failed transition appended metadata"
            );
            assert!(inner.header.provider.is_none());
            assert!(inner.header.model_id.is_none());
            drop(inner);
            drop(guard);

            let state = shared_state.lock(&cx).await.expect("shared state lock");
            assert!(state.failover_primary.is_none());
            assert!(state.active_failover_model.is_none());
            assert!(state.failover_chain_position.is_none());
            assert!(state.provider_admission.reason().is_none());
            assert!(
                state
                    .failover_cooldown
                    .as_ref()
                    .is_none_or(|tracker| tracker.failed_at().is_none()),
                "failed transition mutated cooldown state"
            );
            assert!(
                out_rx.try_iter().next().is_none(),
                "failed transition emitted a failover lifecycle event"
            );
            drop(state);

            let mut guard = session.lock(&cx).await.expect("agent session lock");
            let session_store = Arc::clone(&guard.session);
            let mut inner = session_store
                .lock(cx.cx())
                .await
                .expect("inner session lock");
            inner.append_message(SessionMessage::User {
                content: UserContent::Text("hello".to_string()),
                timestamp: Some(0),
            });
            inner.append_message(SessionMessage::Assistant {
                message: AssistantMessage {
                    content: Vec::new(),
                    api: "test-api".to_string(),
                    provider: "test-provider".to_string(),
                    model: "test-model".to_string(),
                    usage: Usage::default(),
                    stop_reason: StopReason::Error,
                    stop_details: None,
                    error_message: Some("server error".to_string()),
                    timestamp: 0,
                },
            });
            guard
                .agent
                .replace_messages(inner.to_messages_for_current_path());
            drop(inner);
            drop(guard);

            assert!(
                try_failover_to_next_chain_entry(
                    Arc::clone(&session),
                    Arc::clone(&shared_state),
                    out_tx,
                    &options,
                    Some("server error"),
                    true,
                    Some(2),
                    &cx,
                )
                .await
                .expect("restored failover candidate should commit"),
                "eligible fallback was not installed"
            );

            let guard = session.lock(&cx).await.expect("agent session lock");
            assert_eq!(guard.agent.provider().name(), "anthropic");
            assert_eq!(guard.agent.provider().model_id(), "fallback-model");
            assert_eq!(
                guard.agent.stream_options().api_key.as_deref(),
                Some("fallback-key")
            );
            assert_eq!(
                guard
                    .agent
                    .stream_options()
                    .headers
                    .get("x-fallback")
                    .map(String::as_str),
                Some("true")
            );
            let inner = guard
                .session
                .lock(cx.cx())
                .await
                .expect("inner session lock");
            let persisted_path = inner.path.clone().expect("persisted failover path");
            let path = inner.entries_for_current_path();
            assert_eq!(
                serde_json::to_value(guard.agent.messages()).expect("serialize Agent messages"),
                serde_json::to_value(inner.to_messages_for_current_path())
                    .expect("serialize Session path messages"),
                "Agent transcript must match the installed Session candidate"
            );
            assert!(path.iter().all(|entry| !matches!(
                entry,
                SessionEntry::Message(message)
                    if matches!(
                        &message.message,
                        SessionMessage::Assistant { message }
                            if message.stop_reason == StopReason::Error
                    )
            )));
            assert!(path.iter().any(|entry| matches!(
                entry,
                SessionEntry::Custom(custom) if custom.custom_type == "failover"
            )));
            assert!(path.iter().any(|entry| matches!(
                entry,
                SessionEntry::ModelChange(change)
                    if change.provider == "anthropic"
                        && change.model_id == "fallback-model"
                        && change.role.as_deref() == Some("failover")
            )));
            drop(inner);
            drop(guard);

            let reopened = Session::open(persisted_path.to_string_lossy().as_ref())
                .await
                .expect("reopen committed failover Session");
            let reopened_path = reopened.entries_for_current_path();
            assert!(reopened_path.iter().all(|entry| !matches!(
                entry,
                SessionEntry::Message(message)
                    if matches!(
                        &message.message,
                        SessionMessage::Assistant { message }
                            if message.stop_reason == StopReason::Error
                    )
            )));
            assert!(reopened_path.iter().any(|entry| matches!(
                entry,
                SessionEntry::ModelChange(change)
                    if change.provider == "anthropic"
                        && change.model_id == "fallback-model"
                        && change.role.as_deref() == Some("failover")
            )));

            let state = shared_state.lock(&cx).await.expect("shared state lock");
            assert_eq!(
                state.failover_primary.as_ref().map(|primary| (
                    primary.provider.as_str(),
                    primary.model_id.as_str(),
                    primary.requested_thinking_level,
                )),
                Some(("test-provider", "test-model", ThinkingLevel::Off))
            );
            assert_eq!(
                state
                    .active_failover_model
                    .as_ref()
                    .map(|(provider, model)| (provider.as_str(), model.as_str())),
                Some(("anthropic", "fallback-model"))
            );
            assert_eq!(state.failover_chain_position, Some(1));
            assert!(state.provider_admission.reason().is_none());
            drop(state);

            let retry_end = out_rx.try_recv().expect("auto_retry_end event");
            let retry_end: Value = serde_json::from_str(&retry_end).expect("retry-end event JSON");
            assert_eq!(
                retry_end.get("type").and_then(Value::as_str),
                Some("auto_retry_end")
            );
            assert_eq!(retry_end.get("attempt").and_then(Value::as_u64), Some(2));
            let failover_start = out_rx.try_recv().expect("failover_start event");
            let failover_start: Value =
                serde_json::from_str(&failover_start).expect("failover event JSON");
            assert_eq!(
                failover_start.get("type").and_then(Value::as_str),
                Some("failover_start")
            );
            assert!(
                out_rx.try_recv().is_err(),
                "unexpected extra failover event"
            );
        });
    }

    #[test]
    fn rpc_multihop_failover_cooldown_restores_explicit_primary_identity() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 8)
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let model_entry = |provider: &str,
                               model_id: &str,
                               api: &str,
                               base_url: &str,
                               key: &str,
                               header_name: &str| {
                crate::models::ModelEntry {
                    model: Model {
                        id: model_id.to_string(),
                        name: model_id.to_string(),
                        api: api.to_string(),
                        provider: provider.to_string(),
                        base_url: base_url.to_string(),
                        reasoning: false,
                        input: vec![InputType::Text],
                        cost: ModelCost {
                            input: 0.0,
                            output: 0.0,
                            cache_read: 0.0,
                            cache_write: 0.0,
                        },
                        context_window: 8_192,
                        max_tokens: 1_024,
                        headers: HashMap::new(),
                    },
                    api_key: Some(key.to_string()),
                    headers: HashMap::from([(header_name.to_string(), "true".to_string())]),
                    auth_header: true,
                    compat: None,
                    oauth_config: None,
                }
            };
            let mut primary = model_entry(
                "openai",
                "primary-model",
                "openai-completions",
                "https://api.openai.com/v1",
                "primary-key",
                "x-primary",
            );
            primary.model.reasoning = true;
            primary.model.input = vec![InputType::Text, InputType::Image];
            primary.model.context_window = 16_384;
            primary.model.max_tokens = 1_536;
            let mut fallback = model_entry(
                "anthropic",
                "fallback-model",
                "anthropic",
                "https://api.anthropic.com",
                "fallback-key",
                "x-fallback",
            );
            fallback.model.context_window = 4_096;
            fallback.model.max_tokens = 2_048;
            fallback.compat = Some(crate::models::CompatConfig {
                tool_call_dialect: Some(crate::dialects::Dialect::Xmlish),
                ..Default::default()
            });
            let mut second_fallback = model_entry(
                "cohere",
                "second-fallback-model",
                "cohere",
                "https://api.cohere.com",
                "second-fallback-key",
                "x-second-fallback",
            );
            second_fallback.model.context_window = 2_048;
            second_fallback.model.max_tokens = 3_072;
            second_fallback.model.reasoning = true;
            let provider = providers::create_provider(&primary, None).expect("primary provider");
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let mut agent = Agent::new(provider, tools, AgentConfig::default());
            agent.stream_options_mut().api_key = Some("primary-key".to_string());
            agent
                .stream_options_mut()
                .headers
                .clone_from(&primary.headers);
            agent.stream_options_mut().max_tokens = Some(primary.model.max_tokens);
            agent.stream_options_mut().thinking_level = Some(ThinkingLevel::High);
            agent.set_model_accepts_images(true);
            let session_temp = tempfile::tempdir().expect("session tempdir");
            let inner_session = Arc::new(Mutex::new(Session::create_with_dir(Some(
                session_temp.path().join("sessions"),
            ))));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&inner_session),
                true,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            {
                let seed_cx = AgentCx::for_request();
                let mut inner = inner_session
                    .lock(seed_cx.cx())
                    .await
                    .expect("seed Session");
                inner.append_message(SessionMessage::User {
                    content: UserContent::Text("hello".to_string()),
                    timestamp: Some(0),
                });
                inner.append_message(SessionMessage::Assistant {
                    message: AssistantMessage {
                        content: Vec::new(),
                        api: "openai-completions".to_string(),
                        provider: "openai".to_string(),
                        model: "primary-model".to_string(),
                        usage: Usage::default(),
                        stop_reason: StopReason::Error,
                        stop_details: None,
                        error_message: Some("server error".to_string()),
                        timestamp: 0,
                    },
                });
                agent_session
                    .agent
                    .replace_messages(inner.to_messages_for_current_path());
            }
            let session = Arc::new(Mutex::new(agent_session));

            let mut config = Config::default();
            config.retry = Some(crate::config::RetrySettings {
                fallback_chains: Some(HashMap::from([(
                    "openai/primary-model".to_string(),
                    vec![
                        "anthropic/fallback-model".to_string(),
                        "cohere/second-fallback-model".to_string(),
                    ],
                )])),
                failover_cooldown_secs: Some(0),
                max_failovers_per_turn: Some(1),
                ..Default::default()
            });
            let shared_state = Arc::new(Mutex::new(RpcSharedState::new(&config)));
            let auth_temp = tempfile::tempdir().expect("auth tempdir");
            let options = RpcOptions {
                config,
                resources: ResourceLoader::empty(false),
                available_models: vec![primary, fallback, second_fallback],
                scoped_models: Vec::new(),
                cli_api_key: None,
                auth: AuthStorage::load(auth_temp.path().join("auth.json")).expect("auth load"),
                runtime_handle,
                ask_tool: None,
            };
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(8);
            let cx = AgentCx::for_request();

            assert!(
                try_failover_to_next_chain_entry(
                    Arc::clone(&session),
                    Arc::clone(&shared_state),
                    out_tx.clone(),
                    &options,
                    Some("server error"),
                    true,
                    None,
                    &cx,
                )
                .await
                .expect("failover commit")
            );
            {
                let guard = session.lock(&cx).await.expect("fallback AgentSession lock");
                assert_eq!(guard.agent.provider().name(), "anthropic");
                assert_eq!(guard.agent.provider().model_id(), "fallback-model");
                assert_eq!(
                    guard.agent.stream_options().thinking_level,
                    Some(ThinkingLevel::Off)
                );
                assert_eq!(guard.agent.stream_options().max_tokens, Some(2_048));
                assert!(!guard.agent.model_accepts_images());
                assert_eq!(
                    guard.agent.tool_call_dialect(),
                    crate::dialects::Dialect::Xmlish
                );
                assert_eq!(guard.compaction_settings().context_window_tokens, 4_096);
                let inner = guard
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("fallback Session lock");
                assert_eq!(inner.header.thinking_level.as_deref(), Some("off"));
            }

            // Simulate a later turn failing on the first fallback. With a
            // per-turn cap of one, the persistent chain cursor must still be
            // allowed to advance to entry two, and the exact chain must still
            // resolve from the recorded primary rather than the live fallback.
            {
                let mut guard = session.lock(&cx).await.expect("fallback AgentSession lock");
                let mut inner = guard
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("fallback Session lock");
                inner.append_message(SessionMessage::Assistant {
                    message: AssistantMessage {
                        content: Vec::new(),
                        api: "anthropic".to_string(),
                        provider: "anthropic".to_string(),
                        model: "fallback-model".to_string(),
                        usage: Usage::default(),
                        stop_reason: StopReason::Error,
                        stop_details: None,
                        error_message: Some("server error".to_string()),
                        timestamp: 1,
                    },
                });
                inner.save().await.expect("persist second failed tail");
                let messages = inner.to_messages_for_current_path();
                drop(inner);
                guard.agent.replace_messages(messages);
            }
            assert!(
                try_failover_to_next_chain_entry(
                    Arc::clone(&session),
                    Arc::clone(&shared_state),
                    out_tx.clone(),
                    &options,
                    Some("server error"),
                    true,
                    None,
                    &cx,
                )
                .await
                .expect("second failover commit")
            );
            {
                let guard = session
                    .lock(&cx)
                    .await
                    .expect("second fallback AgentSession lock");
                assert_eq!(guard.agent.provider().name(), "cohere");
                assert_eq!(guard.agent.provider().model_id(), "second-fallback-model");
                assert_eq!(
                    guard.agent.stream_options().thinking_level,
                    Some(ThinkingLevel::High),
                    "a later reasoning-capable fallback must recover the primary request"
                );
                assert_eq!(guard.agent.stream_options().max_tokens, Some(3_072));
                assert_eq!(guard.compaction_settings().context_window_tokens, 2_048);
            }
            maybe_restore_primary(
                Arc::clone(&session),
                Arc::clone(&shared_state),
                out_tx,
                &options,
                &cx,
            )
            .await
            .expect("primary restore commit");

            let guard = session.lock(&cx).await.expect("AgentSession lock");
            assert_eq!(guard.agent.provider().name(), "openai");
            assert_eq!(guard.agent.provider().model_id(), "primary-model");
            assert_eq!(
                guard.agent.stream_options().thinking_level,
                Some(ThinkingLevel::High)
            );
            assert_eq!(guard.agent.stream_options().max_tokens, Some(1_536));
            assert!(guard.agent.model_accepts_images());
            assert_eq!(guard.compaction_settings().context_window_tokens, 16_384);
            assert_eq!(
                guard.agent.stream_options().api_key.as_deref(),
                Some("primary-key")
            );
            assert_eq!(
                guard
                    .agent
                    .stream_options()
                    .headers
                    .get("x-primary")
                    .map(String::as_str),
                Some("true")
            );
            let inner = guard.session.lock(cx.cx()).await.expect("Session lock");
            let roles = inner
                .entries_for_current_path()
                .iter()
                .filter_map(|entry| match entry {
                    SessionEntry::ModelChange(change) => change.role.as_deref(),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(roles, vec!["failover", "failover", "primary_restore"]);
            assert_eq!(inner.header.thinking_level.as_deref(), Some("high"));
            let persisted_path = inner
                .path
                .clone()
                .expect("primary restoration must persist a Session path");
            drop(inner);
            drop(guard);

            let reopened = Session::open(persisted_path.to_string_lossy().as_ref())
                .await
                .expect("reopen restored primary Session");
            assert!(
                reopened
                    .effective_model_for_current_path()
                    .is_some_and(|(provider, model)| {
                        provider == "openai" && model == "primary-model"
                    }),
                "reopened Session must resolve the original primary identity"
            );
            let reopened_roles = reopened
                .entries_for_current_path()
                .iter()
                .filter_map(|entry| match entry {
                    SessionEntry::ModelChange(change) => change.role.as_deref(),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                reopened_roles,
                vec!["failover", "failover", "primary_restore"]
            );
            assert_eq!(
                reopened
                    .effective_thinking_level_for_current_path()
                    .as_deref(),
                Some("high")
            );

            let state = shared_state.lock(&cx).await.expect("shared state lock");
            assert!(state.failover_primary.is_none());
            assert!(state.active_failover_model.is_none());
            assert!(state.failover_chain_position.is_none());
            assert!(state.provider_admission.reason().is_none());
            drop(state);

            let failover_start: Value =
                serde_json::from_str(&out_rx.try_recv().expect("failover_start event"))
                    .expect("failover_start JSON");
            assert_eq!(
                failover_start.get("type").and_then(Value::as_str),
                Some("failover_start")
            );
            let second_failover_start: Value =
                serde_json::from_str(&out_rx.try_recv().expect("second failover_start event"))
                    .expect("second failover_start JSON");
            assert_eq!(
                second_failover_start.get("type").and_then(Value::as_str),
                Some("failover_start")
            );
            assert_eq!(
                second_failover_start.get("attempt").and_then(Value::as_u64),
                Some(2)
            );
            let failover_end: Value =
                serde_json::from_str(&out_rx.try_recv().expect("failover_end event"))
                    .expect("failover_end JSON");
            assert_eq!(
                failover_end.get("type").and_then(Value::as_str),
                Some("failover_end")
            );
            assert_eq!(
                failover_end.get("restoredPrimary").and_then(Value::as_bool),
                Some(true)
            );
            assert!(out_rx.try_recv().is_err(), "unexpected lifecycle event");
        });
    }

    #[test]
    fn rpc_terminal_preservation_honors_transition_quarantine() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(Arc::new(AlwaysErrorProvider), tools, AgentConfig::default());
            let inner_session = Arc::new(Mutex::new(Session::in_memory()));
            let session = Arc::new(Mutex::new(AgentSession::new(
                agent,
                Arc::clone(&inner_session),
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            )));
            let shared_state = Arc::new(Mutex::new(RpcSharedState::new(&Config::default())));
            let cx = AgentCx::for_request();
            {
                let mut state = shared_state.lock(&cx).await.expect("shared state lock");
                state
                    .push_follow_up(QueuedAgentMessage::from_authored_message(
                        build_user_message("must remain queued", &[]),
                    ))
                    .expect("queue follow-up before quarantine");
                state
                    .provider_admission
                    .block("indeterminate failover persistence".to_string());
            }

            let plan_error = terminal_rpc_recovery_plan(&session, &shared_state, false, &cx)
                .await
                .expect_err("quarantine must block a no-op terminal recovery plan");
            assert!(plan_error.is_session_persistence());

            {
                let mut state = shared_state.lock(&cx).await.expect("shared state lock");
                let queue_error = state
                    .push_steering(QueuedAgentMessage::from_authored_message(
                        build_user_message("must be rejected", &[]),
                    ))
                    .expect_err("streaming queue admission must honor quarantine");
                assert!(queue_error.is_session_persistence());
                assert_eq!(state.pending_count(), 1);
            }

            let error = preserve_terminal_rpc_input(&session, &shared_state, &cx)
                .await
                .expect_err("quarantine must block every terminal persistence path");
            assert!(error.is_session_persistence());
            let state = shared_state.lock(&cx).await.expect("shared state lock");
            assert_eq!(state.pending_count(), 1);
            drop(state);
            let inner = inner_session.lock(&cx).await.expect("Session lock");
            assert!(inner.entries.is_empty());
            drop(inner);
            let guard = session.lock(&cx).await.expect("AgentSession lock");
            assert!(guard.agent.messages().is_empty());
        });
    }

    #[test]
    fn rpc_retry_restore_save_failure_latches_without_live_mutation() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 8)
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let blocked_path = temp.path().join("blocked.jsonl");
            std::fs::create_dir_all(&blocked_path).expect("create blocking directory");
            let mut stored = Session::in_memory();
            stored.path = Some(blocked_path);
            stored.append_message(SessionMessage::User {
                content: UserContent::Text("hello".to_string()),
                timestamp: Some(0),
            });
            stored.append_message(SessionMessage::Assistant {
                message: AssistantMessage {
                    content: Vec::new(),
                    api: "test-api".to_string(),
                    provider: "test-provider".to_string(),
                    model: "test-model".to_string(),
                    usage: Usage::default(),
                    stop_reason: StopReason::Error,
                    stop_details: None,
                    error_message: Some("server error".to_string()),
                    timestamp: 0,
                },
            });
            let original_messages = stored.to_messages_for_current_path();
            let inner_session = Arc::new(Mutex::new(stored));
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let mut agent =
                Agent::new(Arc::new(AlwaysErrorProvider), tools, AgentConfig::default());
            agent.replace_messages(original_messages.clone());
            let session = Arc::new(Mutex::new(AgentSession::new(
                agent,
                Arc::clone(&inner_session),
                true,
                crate::compaction::ResolvedCompactionSettings::default(),
            )));
            let shared_state = Arc::new(Mutex::new(RpcSharedState::new(&Config::default())));
            let cx = AgentCx::for_request();

            let error = restore_rpc_retry_tail(&session, &shared_state, &cx, true)
                .await
                .expect_err("unwritable candidate must fail restoration");
            assert!(error.is_session_persistence());
            let state = shared_state.lock(&cx).await.expect("shared state lock");
            assert!(state.provider_admission.reason().is_some());
            drop(state);
            let inner = inner_session.lock(&cx).await.expect("Session lock");
            assert_eq!(
                serde_json::to_value(inner.to_messages_for_current_path())
                    .expect("serialize live Session path"),
                serde_json::to_value(&original_messages).expect("serialize original path")
            );
            assert!(
                inner
                    .entries_for_current_path()
                    .iter()
                    .any(|entry| matches!(
                        entry,
                        SessionEntry::Message(message)
                            if matches!(
                                &message.message,
                                SessionMessage::Assistant { message }
                                    if message.stop_reason == StopReason::Error
                            )
                    ))
            );
            drop(inner);
            let guard = session.lock(&cx).await.expect("AgentSession lock");
            assert_eq!(
                serde_json::to_value(guard.agent.messages()).expect("serialize Agent messages"),
                serde_json::to_value(&original_messages).expect("serialize original messages")
            );
            drop(guard);
            assert!(
                preserve_terminal_rpc_input(&session, &shared_state, &cx)
                    .await
                    .is_err(),
                "terminal persistence must honor the production-set quarantine"
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
                ..Default::default()
            });

            let mut shared = RpcSharedState::new(&config);
            shared.auto_compaction_enabled = false;
            shared
                .push_follow_up(QueuedAgentMessage::from_authored_message(
                    build_user_message("preexisting queued follow-up", &[]),
                ))
                .expect("queue terminal follow-up");
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

            let prompt_task_handle = runtime_handle.clone();
            let options = RpcOptions {
                config,
                resources: ResourceLoader::empty(false),
                available_models: Vec::new(),
                scoped_models: Vec::new(),
                cli_api_key: None,
                auth,
                runtime_handle,
                ask_tool: None,
            };

            let mut timeline = Vec::new();
            let mut last_agent_end_error = None::<String>;
            let retry_abort_for_signal = Arc::clone(&retry_abort);
            let prompt_task = prompt_task_handle.spawn(async move {
                run_prompt_with_retry(
                    Arc::clone(&session),
                    Arc::clone(&shared_state),
                    is_streaming,
                    is_compacting,
                    Arc::new(std::sync::Mutex::new(())),
                    abort_handle_slot,
                    out_tx,
                    retry_abort,
                    options,
                    "hello".to_string(),
                    None,
                    Vec::new(),
                    AgentCx::for_request(),
                )
                .await;
                (session, shared_state)
            });

            let retry_start_deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match out_rx.try_recv() {
                    Ok(line) => {
                        let value = serde_json::from_str::<Value>(&line)
                            .expect("retry timeline event JSON");
                        let kind = value["type"].as_str().expect("retry event type");
                        timeline.push(kind.to_string());
                        if kind == "auto_retry_start" {
                            retry_abort_for_signal.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        assert!(
                            std::time::Instant::now() < retry_start_deadline,
                            "timed out waiting for causal auto_retry_start event"
                        );
                        sleep(wall_now(), Duration::from_millis(5)).await;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        panic!("RPC retry output disconnected before auto_retry_start");
                    }
                }
            }

            let (session, shared_state) = prompt_task.await;

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
            let agent_end_positions = timeline
                .iter()
                .enumerate()
                .filter_map(|(index, kind)| (kind == "agent_end").then_some(index))
                .collect::<Vec<_>>();
            assert_eq!(
                agent_end_positions.len(),
                1,
                "retry timeline must have exactly one terminal agent_end: {timeline:?}"
            );
            let agent_end_idx = agent_end_positions[0];

            assert!(
                retry_start_idx < retry_end_idx && retry_end_idx < agent_end_idx,
                "unexpected retry timeline ordering: {timeline:?}"
            );
            assert_eq!(
                last_agent_end_error.as_deref(),
                Some("Retry aborted"),
                "expected retry-abort terminal error, timeline: {timeline:?}"
            );
            assert_eq!(
                shared_state
                    .lock(&AgentCx::for_request())
                    .await
                    .expect("shared state lock")
                    .pending_count(),
                0,
                "terminal finalization must consume preexisting queued input"
            );
            let terminal_cx = AgentCx::for_request();
            let guard = session
                .lock(&terminal_cx)
                .await
                .expect("agent session lock");
            let inner = guard
                .session
                .lock(&terminal_cx)
                .await
                .expect("session lock");
            assert_eq!(
                inner
                    .entries_for_current_path()
                    .iter()
                    .filter(|entry| matches!(
                        entry,
                        crate::session::SessionEntry::Message(message)
                            if matches!(
                                &message.message,
                                SessionMessage::User {
                                    content: UserContent::Text(text),
                                    ..
                                } if text == "preexisting queued follow-up"
                            )
                    ))
                    .count(),
                1,
                "preexisting terminal follow-up must be retained exactly once"
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
                ..Default::default()
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
                ask_tool: None,
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
                Arc::new(std::sync::Mutex::new(())),
                abort_handle_slot,
                out_tx,
                retry_abort,
                options,
                "hello".to_string(),
                None,
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
                ..Default::default()
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
                ask_tool: None,
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

                let retry_abort_wait = async {
                    let mut timeline = Vec::new();
                    let mut cancellation_requested = false;
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
                                if kind == "auto_retry_start" && !cancellation_requested {
                                    cancel_cx.set_cancel_requested(true);
                                    cancellation_requested = true;
                                }
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
                // Boxed: clippy::large_futures.
                futures::future::join(Box::pin(run(agent_session, options, in_rx, out_tx)), client)
                    .await;
            assert!(server_result.is_ok(), "rpc server error: {server_result:?}");
        });
    }

    #[test]
    fn rpc_prompt_rejects_header_sync_failure_before_ack_or_provider_entry() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let blocked_path = temp.path().join("blocked.jsonl");
            std::fs::create_dir_all(&blocked_path).expect("create blocking directory");
            let provider = Arc::new(FlakyProvider::new());
            let agent = Agent::new(
                provider.clone(),
                ToolRegistry::new(&[], temp.path(), None),
                AgentConfig {
                    stream_options: crate::provider::StreamOptions {
                        thinking_level: Some(ThinkingLevel::High),
                        ..crate::provider::StreamOptions::default()
                    },
                    ..AgentConfig::default()
                },
            );
            let mut durable_session = Session::in_memory();
            durable_session.path = Some(blocked_path);
            durable_session.header.provider = Some("test-provider".to_string());
            durable_session.header.model_id = Some("test-model".to_string());
            durable_session.header.thinking_level = Some("high".to_string());
            let mut agent_session = AgentSession::new(
                agent,
                Arc::new(asupersync::sync::Mutex::new(durable_session)),
                true,
                crate::compaction::ResolvedCompactionSettings::default(),
            );

            let auth = AuthStorage::load(temp.path().join("auth.json")).expect("auth load");
            let mut model = dummy_entry("test-model", false);
            model.model.provider = "test-provider".to_string();
            model.auth_header = false;
            model.api_key = None;
            let mut registry = crate::models::ModelRegistry::load(&auth, None);
            registry.merge_entries(vec![model.clone()]);
            agent_session.set_model_registry(registry);
            agent_session.set_auth_storage(auth.clone());

            let options = RpcOptions {
                config: Config::default(),
                resources: ResourceLoader::empty(false),
                available_models: vec![model],
                scoped_models: Vec::new(),
                cli_api_key: None,
                auth,
                runtime_handle,
                ask_tool: None,
            };
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(4);
            let send_cx = asupersync::Cx::for_testing();
            in_tx
                .send(
                    &send_cx,
                    r#"{"id":"1","type":"prompt","message":"must not run"}"#.to_string(),
                )
                .await
                .expect("send prompt");
            in_tx
                .send(&send_cx, r#"{"id":"2","type":"compact"}"#.to_string())
                .await
                .expect("send compact after rejected prompt");
            drop(in_tx);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(32);

            // Boxed: clippy::large_futures. The rejected command quarantines
            // terminal persistence, so the loop surfaces that on stdin close
            // instead of exiting clean (see preserve_terminal_rpc_state).
            let loop_error = Box::pin(run(agent_session, options, in_rx, out_tx))
                .await
                .expect_err("quarantined terminal persistence must surface on stdin close");
            assert!(
                loop_error.to_string().contains("quarantined"),
                "unexpected loop error: {loop_error}"
            );

            let events = out_rx
                .try_iter()
                .map(|line| serde_json::from_str::<Value>(&line).expect("event json"))
                .collect::<Vec<_>>();
            let response = events
                .iter()
                .find(|value| value["type"] == "response" && value["command"] == "prompt")
                .expect("prompt response");
            assert_eq!(
                response["success"], false,
                "unexpected response: {response}"
            );
            assert!(
                response["error"]
                    .as_str()
                    .is_some_and(|message| message.contains("synchronization persistence")),
                "unexpected response: {response}"
            );
            assert!(
                events.iter().all(|value| value["type"] != "agent_end"),
                "a rejected pre-ACK prompt must not spawn a provider turn: {events:?}"
            );
            let compact_response = events
                .iter()
                .find(|value| value["type"] == "response" && value["command"] == "compact")
                .expect("compact response");
            assert_eq!(
                compact_response["success"], false,
                "the shared quarantine must block manual compaction: {compact_response}"
            );
            assert!(
                compact_response["error"]
                    .as_str()
                    .is_some_and(|message| message.contains("quarantined")),
                "unexpected compact response: {compact_response}"
            );
            assert_eq!(
                provider.calls.load(Ordering::SeqCst),
                0,
                "provider.stream must remain unreachable after admission failure"
            );
        });
    }

    #[test]
    fn rpc_fresh_persistence_failure_keeps_live_session_unchanged() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let blocked_path = temp.path().join("blocked.jsonl");
            std::fs::create_dir_all(&blocked_path).expect("create blocking directory");
            let provider = Arc::new(FlakyProvider::new());
            let agent = Agent::new(
                provider.clone(),
                ToolRegistry::new(&[], temp.path(), None),
                AgentConfig {
                    stream_options: crate::provider::StreamOptions {
                        session_id: Some("original-provider-session".to_string()),
                        ..crate::provider::StreamOptions::default()
                    },
                    ..AgentConfig::default()
                },
            );
            let mut initial = Session::in_memory();
            initial.path = Some(blocked_path);
            let inner_session = Arc::new(asupersync::sync::Mutex::new(initial));
            let agent_session = AgentSession::new(
                agent,
                Arc::clone(&inner_session),
                true,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            let options = build_test_rpc_options(&runtime_handle, temp.path().join("auth.json"));
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(4);
            let send_cx = asupersync::Cx::for_testing();
            in_tx
                .send(&send_cx, r#"{"id":"1","type":"fresh"}"#.to_string())
                .await
                .expect("send fresh");
            drop(in_tx);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(32);

            // Boxed: clippy::large_futures. The failed fresh-session
            // persistence quarantines terminal persistence, which the loop
            // surfaces on stdin close (see preserve_terminal_rpc_state).
            let loop_error = Box::pin(run(agent_session, options, in_rx, out_tx))
                .await
                .expect_err("quarantined terminal persistence must surface on stdin close");
            assert!(
                loop_error.to_string().contains("quarantined"),
                "unexpected loop error: {loop_error}"
            );

            let events = out_rx
                .try_iter()
                .map(|line| serde_json::from_str::<Value>(&line).expect("event json"))
                .collect::<Vec<_>>();
            let response = events
                .iter()
                .find(|value| value["type"] == "response" && value["command"] == "fresh")
                .expect("fresh response");
            assert_eq!(
                response["success"], false,
                "unexpected response: {response}"
            );
            let cx = asupersync::Cx::for_testing();
            let session = inner_session.lock(&cx).await.expect("session lock");
            assert!(
                session.entries_for_current_path().iter().all(|entry| {
                    !matches!(
                        entry,
                        crate::session::SessionEntry::Custom(custom)
                            if custom.custom_type == "fresh"
                    )
                }),
                "failed persistence must not install the fresh marker"
            );
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn rpc_new_session_rejects_real_js_queued_actions_without_cross_session_delivery() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let extension_path = temp.path().join("switch-owner.mjs");
            std::fs::write(
                &extension_path,
                r#"
                export default function init(pi) {
                  pi.on("session_before_switch", async () => {
                    pi.sendMessage({
                      customType: "immediate-note",
                      content: "immediate source action",
                      display: true
                    }, {});
                    pi.sendMessage({
                      customType: "trigger-note",
                      content: "queued trigger action",
                      display: true
                    }, { triggerTurn: true });
                    pi.sendMessage({
                      customType: "next-turn-note",
                      content: "durable next-turn source action",
                      display: true
                    }, { deliverAs: "nextTurn" });
                    pi.sendUserMessage("queued user action", {});
                  });
                }
                "#,
            )
            .expect("write transition extension");

            let provider = Arc::new(FlakyProvider::new());
            let agent = Agent::new(
                provider.clone(),
                ToolRegistry::new(&[], temp.path(), None),
                AgentConfig::default(),
            );
            let inner_session = Arc::new(asupersync::sync::Mutex::new(Session::in_memory()));
            let original_session_id = inner_session
                .lock(&AgentCx::for_request())
                .await
                .expect("initial session lock")
                .header
                .id
                .clone();
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&inner_session),
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            agent_session
                .enable_extensions(&[], temp.path(), None, &[extension_path])
                .await
                .expect("enable transition extension");
            let options = build_test_rpc_options(&runtime_handle, temp.path().join("auth.json"));
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(4);
            let send_cx = asupersync::Cx::for_testing();
            in_tx
                .send(&send_cx, r#"{"id":"1","type":"new_session"}"#.to_string())
                .await
                .expect("send new_session");
            drop(in_tx);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(32);

            // Boxed: clippy::large_futures.
            Box::pin(run(agent_session, options, in_rx, out_tx))
                .await
                .expect("rpc server loop");

            let response = out_rx
                .try_iter()
                .map(|line| serde_json::from_str::<Value>(&line).expect("event json"))
                .find(|value| value["type"] == "response" && value["command"] == "new_session")
                .expect("new_session response");
            assert_eq!(
                response["success"], false,
                "unexpected response: {response}"
            );
            assert!(
                response["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("extension-triggered action")),
                "unexpected transition error: {response}"
            );

            let cx = AgentCx::for_request();
            let inner = inner_session.lock(&cx).await.expect("source session lock");
            assert_eq!(inner.header.id, original_session_id);
            let durable_custom_types = inner
                .entries_for_current_path()
                .iter()
                .filter_map(|entry| match entry {
                    SessionEntry::Message(message) => match &message.message {
                        SessionMessage::Custom { custom_type, .. } => Some(custom_type.as_str()),
                        _ => None,
                    },
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(durable_custom_types.len(), 2);
            assert!(durable_custom_types.contains(&"immediate-note"));
            assert!(durable_custom_types.contains(&"next-turn-note"));
            assert!(!durable_custom_types.contains(&"trigger-note"));
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn rpc_new_session_rejects_real_js_direct_source_session_mutation() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let extension_path = temp.path().join("direct-source-mutation.mjs");
            std::fs::write(
                &extension_path,
                r#"
                export default function init(pi) {
                  pi.on("session_before_switch", async () => {
                    await pi.session("appendEntry", {
                      customType: "direct-source-entry",
                      data: { owner: "source" }
                    });
                  });
                }
                "#,
            )
            .expect("write direct source mutation extension");

            let provider = Arc::new(FlakyProvider::new());
            let agent = Agent::new(
                provider.clone(),
                ToolRegistry::new(&[], temp.path(), None),
                AgentConfig::default(),
            );
            let inner_session = Arc::new(asupersync::sync::Mutex::new(Session::in_memory()));
            let original_session_id = inner_session
                .lock(&AgentCx::for_request())
                .await
                .expect("initial session lock")
                .header
                .id
                .clone();
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&inner_session),
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            agent_session
                .enable_extensions(&[], temp.path(), None, &[extension_path])
                .await
                .expect("enable direct source mutation extension");
            let options = build_test_rpc_options(&runtime_handle, temp.path().join("auth.json"));
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(4);
            let send_cx = asupersync::Cx::for_testing();
            in_tx
                .send(&send_cx, r#"{"id":"1","type":"new_session"}"#.to_string())
                .await
                .expect("send new_session");
            drop(in_tx);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(32);

            // Boxed: clippy::large_futures.
            Box::pin(run(agent_session, options, in_rx, out_tx))
                .await
                .expect("rpc server loop");

            let response = out_rx
                .try_iter()
                .map(|line| serde_json::from_str::<Value>(&line).expect("event json"))
                .find(|value| value["type"] == "response" && value["command"] == "new_session")
                .expect("new_session response");
            assert_eq!(
                response["success"], false,
                "unexpected response: {response}"
            );
            assert!(
                response["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("modified the source Session")),
                "unexpected transition error: {response}"
            );

            let cx = AgentCx::for_request();
            let inner = inner_session.lock(&cx).await.expect("source session lock");
            assert_eq!(inner.header.id, original_session_id);
            assert!(inner.entries_for_current_path().iter().any(|entry| {
                matches!(
                    entry,
                    SessionEntry::Custom(custom)
                        if custom.custom_type == "direct-source-entry"
                            && custom.data == Some(json!({"owner": "source"}))
                )
            }));
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn rpc_fork_dispatches_real_js_lifecycle_against_the_owned_sessions() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let extension_path = temp.path().join("fork-lifecycle.mjs");
            std::fs::write(
                &extension_path,
                r#"
                export default function init(pi) {
                  let beforeEntryId = null;
                  let beforeSessionId = null;
                  pi.on("session_before_fork", (event) => {
                    beforeEntryId = event.entryId;
                    beforeSessionId = event.sessionId;
                    return null;
                  });
                  pi.on("session_fork", async (event) => {
                    await pi.session("appendEntry", {
                      customType: "rpc-fork-lifecycle",
                      data: {
                        beforeEntryId,
                        beforeSessionId,
                        afterEntryId: event.entryId,
                        afterSessionId: event.sessionId,
                        newSessionId: event.newSessionId
                      }
                    });
                  });
                }
                "#,
            )
            .expect("write fork lifecycle extension");

            let provider = Arc::new(FlakyProvider::new());
            let agent = Agent::new(
                provider.clone(),
                ToolRegistry::new(&[], temp.path(), None),
                AgentConfig::default(),
            );
            let (source, entry_id) = rpc_fork_source_session();
            let source_session_id = source.header.id.clone();
            let inner_session = Arc::new(asupersync::sync::Mutex::new(source));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&inner_session),
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            agent_session
                .enable_extensions(&[], temp.path(), None, &[extension_path])
                .await
                .expect("enable fork lifecycle extension");
            let options = rpc_fork_test_options(
                &runtime_handle,
                temp.path().join("fork-lifecycle-auth.json"),
            );
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(4);
            let send_cx = asupersync::Cx::for_testing();
            in_tx
                .send(
                    &send_cx,
                    json!({"id": "1", "type": "fork", "entryId": entry_id.clone()}).to_string(),
                )
                .await
                .expect("send fork");
            drop(in_tx);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(32);

            // Boxed: clippy::large_futures.
            Box::pin(run(agent_session, options, in_rx, out_tx))
                .await
                .expect("rpc server loop");

            let response = out_rx
                .try_iter()
                .map(|line| serde_json::from_str::<Value>(&line).expect("event json"))
                .find(|value| value["type"] == "response" && value["command"] == "fork")
                .expect("fork response");
            assert_eq!(response["success"], true, "unexpected response: {response}");
            assert_eq!(response["data"]["cancelled"], false);
            assert_eq!(response["data"]["text"], "fork this prompt");

            let cx = AgentCx::for_request();
            let inner = inner_session.lock(&cx).await.expect("forked session lock");
            let new_session_id = inner.header.id.clone();
            assert_ne!(new_session_id, source_session_id);
            let lifecycle = inner
                .entries_for_current_path()
                .iter()
                .find_map(|entry| match entry {
                    SessionEntry::Custom(custom) if custom.custom_type == "rpc-fork-lifecycle" => {
                        custom.data.as_ref()
                    }
                    _ => None,
                })
                .expect("session_fork lifecycle marker");
            assert_eq!(lifecycle["beforeEntryId"], entry_id);
            assert_eq!(lifecycle["afterEntryId"], entry_id);
            assert_eq!(lifecycle["beforeSessionId"], source_session_id);
            assert_eq!(lifecycle["afterSessionId"], source_session_id);
            assert_eq!(lifecycle["newSessionId"], new_session_id);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn rpc_fork_real_js_veto_keeps_hook_actions_on_the_source_session() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let extension_path = temp.path().join("fork-veto.mjs");
            std::fs::write(
                &extension_path,
                r#"
                export default function init(pi) {
                  pi.on("session_before_fork", async (event) => {
                    await pi.session("appendEntry", {
                      customType: "rpc-fork-veto-source",
                      data: { entryId: event.entryId, sessionId: event.sessionId }
                    });
                    return { cancel: true };
                  });
                  pi.on("session_fork", async () => {
                    await pi.session("appendEntry", {
                      customType: "rpc-fork-after-veto",
                      data: null
                    });
                  });
                }
                "#,
            )
            .expect("write fork veto extension");

            let provider = Arc::new(FlakyProvider::new());
            let agent = Agent::new(
                provider.clone(),
                ToolRegistry::new(&[], temp.path(), None),
                AgentConfig::default(),
            );
            let (source, entry_id) = rpc_fork_source_session();
            let source_session_id = source.header.id.clone();
            let inner_session = Arc::new(asupersync::sync::Mutex::new(source));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&inner_session),
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            agent_session
                .enable_extensions(&[], temp.path(), None, &[extension_path])
                .await
                .expect("enable fork veto extension");
            let options =
                rpc_fork_test_options(&runtime_handle, temp.path().join("fork-veto-auth.json"));
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(4);
            let send_cx = asupersync::Cx::for_testing();
            in_tx
                .send(
                    &send_cx,
                    json!({"id": "1", "type": "fork", "entryId": entry_id.clone()}).to_string(),
                )
                .await
                .expect("send fork");
            drop(in_tx);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(32);

            // Boxed: clippy::large_futures.
            Box::pin(run(agent_session, options, in_rx, out_tx))
                .await
                .expect("rpc server loop");

            let response = out_rx
                .try_iter()
                .map(|line| serde_json::from_str::<Value>(&line).expect("event json"))
                .find(|value| value["type"] == "response" && value["command"] == "fork")
                .expect("fork response");
            assert_eq!(response["success"], true, "unexpected response: {response}");
            assert_eq!(response["data"]["cancelled"], true);

            let cx = AgentCx::for_request();
            let inner = inner_session.lock(&cx).await.expect("source session lock");
            assert_eq!(inner.header.id, source_session_id);
            let veto_marker = inner
                .entries_for_current_path()
                .iter()
                .find_map(|entry| match entry {
                    SessionEntry::Custom(custom)
                        if custom.custom_type == "rpc-fork-veto-source" =>
                    {
                        custom.data.as_ref()
                    }
                    _ => None,
                })
                .expect("session_before_fork source marker");
            assert_eq!(veto_marker["entryId"], entry_id);
            assert_eq!(veto_marker["sessionId"], source_session_id);
            assert!(inner.entries_for_current_path().iter().all(|entry| {
                !matches!(
                    entry,
                    SessionEntry::Custom(custom)
                        if custom.custom_type == "rpc-fork-after-veto"
                )
            }));
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn rpc_new_session_cancelled_real_js_hook_keeps_actions_on_source_session() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let extension_path = temp.path().join("cancel-switch-owner.mjs");
            std::fs::write(
                &extension_path,
                r#"
                export default function init(pi) {
                  pi.on("session_before_switch", async () => {
                    await pi.session("appendEntry", {
                      customType: "cancel-direct-source-entry",
                      data: { owner: "source" }
                    });
                    pi.sendMessage({
                      customType: "cancel-immediate-note",
                      content: "immediate cancelled action",
                      display: true
                    }, {});
                    pi.sendMessage({
                      customType: "cancel-trigger-note",
                      content: "queued cancelled trigger action",
                      display: true
                    }, { triggerTurn: true });
                    pi.sendMessage({
                      customType: "cancel-next-turn-note",
                      content: "durable cancelled next-turn action",
                      display: true
                    }, { deliverAs: "nextTurn" });
                    pi.sendUserMessage("queued cancelled user action", {});
                    return { cancelled: true };
                  });
                }
                "#,
            )
            .expect("write cancelling transition extension");

            let provider = Arc::new(FlakyProvider::new());
            let agent = Agent::new(
                provider.clone(),
                ToolRegistry::new(&[], temp.path(), None),
                AgentConfig::default(),
            );
            let inner_session = Arc::new(asupersync::sync::Mutex::new(Session::in_memory()));
            let original_session_id = inner_session
                .lock(&AgentCx::for_request())
                .await
                .expect("initial session lock")
                .header
                .id
                .clone();
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&inner_session),
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            agent_session
                .enable_extensions(&[], temp.path(), None, &[extension_path])
                .await
                .expect("enable cancelling transition extension");
            let options = build_test_rpc_options(&runtime_handle, temp.path().join("auth.json"));
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(4);
            let send_cx = asupersync::Cx::for_testing();
            in_tx
                .send(&send_cx, r#"{"id":"1","type":"new_session"}"#.to_string())
                .await
                .expect("send cancelled new_session");
            drop(in_tx);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(32);

            // Boxed: clippy::large_futures.
            Box::pin(run(agent_session, options, in_rx, out_tx))
                .await
                .expect("rpc server loop");

            let response = out_rx
                .try_iter()
                .map(|line| serde_json::from_str::<Value>(&line).expect("event json"))
                .find(|value| value["type"] == "response" && value["command"] == "new_session")
                .expect("new_session response");
            assert_eq!(response["success"], true, "unexpected response: {response}");
            assert_eq!(response["data"]["cancelled"], true);

            let cx = AgentCx::for_request();
            let inner = inner_session.lock(&cx).await.expect("source session lock");
            assert_eq!(inner.header.id, original_session_id);
            let durable_custom_types = inner
                .entries_for_current_path()
                .iter()
                .filter_map(|entry| match entry {
                    SessionEntry::Message(message) => match &message.message {
                        SessionMessage::Custom { custom_type, .. } => Some(custom_type.as_str()),
                        _ => None,
                    },
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(durable_custom_types.len(), 2);
            assert!(durable_custom_types.contains(&"cancel-immediate-note"));
            assert!(durable_custom_types.contains(&"cancel-next-turn-note"));
            assert!(!durable_custom_types.contains(&"cancel-trigger-note"));
            assert!(inner.entries_for_current_path().iter().any(|entry| {
                matches!(
                    entry,
                    SessionEntry::Custom(custom)
                        if custom.custom_type == "cancel-direct-source-entry"
                            && custom.data == Some(json!({"owner": "source"}))
                )
            }));
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
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

fn emit_auto_compaction_error(
    out_tx: &std::sync::mpsc::SyncSender<String>,
    error_message: impl Into<String>,
) {
    let _ = out_tx.send(agent_event(AgentEvent::AutoCompactionEnd {
        result: None,
        aborted: false,
        will_retry: false,
        error_message: Some(error_message.into()),
    }));
}

#[allow(clippy::too_many_lines)]
async fn maybe_auto_compact(
    session: Arc<Mutex<AgentSession>>,
    shared_state: Arc<Mutex<RpcSharedState>>,
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
    let Ok(state) = OwnedMutexGuard::lock(shared_state, &cx).await else {
        return;
    };
    if state.ensure_session_advancement_allowed().is_err() {
        return;
    }
    drop(state);
    let (origin_session_id, path_entries, context_window, reserve_tokens, settings) = {
        let Ok(guard) = OwnedMutexGuard::lock(Arc::clone(&session), cx.cx()).await else {
            return;
        };
        if guard.ensure_provider_reentry_allowed().is_err() {
            return;
        }
        let (origin_session_id, path_entries, context_window) = {
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
            let origin_session_id = inner_session.header.id.clone();
            let path_entries = inner_session
                .entries_for_current_path()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            (origin_session_id, path_entries, entry.model.context_window)
        };

        let reserve_tokens = options.config.compaction_reserve_tokens();
        // Carry the configured mode (shake-first/aggressive) and the real
        // model window: `prepare_compaction` re-checks the threshold against
        // `context_window_tokens`, so the 128k default would silently disable
        // auto-compaction for smaller-window models.
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens,
            keep_recent_tokens: options.config.compaction_keep_recent_tokens(),
            context_window_tokens: if context_window == 0 {
                ResolvedCompactionSettings::default().context_window_tokens
            } else {
                context_window
            },
            mode: options.config.compaction_mode(),
            render_mode: options.config.compaction_render_mode(),
        };

        (
            origin_session_id,
            path_entries,
            context_window,
            reserve_tokens,
            settings,
        )
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
    let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(&session), cx.cx()).await else {
        emit_auto_compaction_error(&out_tx, "Session lock failed during auto-compaction");
        return;
    };
    if let Err(err) = guard.ensure_provider_reentry_allowed() {
        emit_auto_compaction_error(&out_tx, err.to_string());
        return;
    }
    let provider_admission = guard.provider_admission_gate();
    guard.invalidate_background_compaction();
    let Ok(_provider_permit) = provider_admission.acquire(cx.cx()).await else {
        emit_auto_compaction_error(
            &out_tx,
            "Provider admission lock failed during auto-compaction",
        );
        return;
    };
    if let Err(err) = provider_admission.ensure_allowed() {
        emit_auto_compaction_error(&out_tx, err.to_string());
        return;
    }
    let Some(key) = guard.agent.stream_options().api_key.clone() else {
        let _ = out_tx.send(agent_event(AgentEvent::AutoCompactionEnd {
            result: None,
            aborted: false,
            will_retry: false,
            error_message: Some("Missing API key for compaction".to_string()),
        }));
        return;
    };
    let provider = guard.agent.provider();
    let result = compact_auto(prep, provider, &key, None).await;

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

            let details_value = match result.snap_payload.as_ref() {
                Some(payload) => {
                    crate::compaction_snap::payload_to_details(Some(details_value), payload)
                }
                None => details_value,
            };

            let save_enabled = guard.save_enabled();
            let (messages, tokens_after, pending_message_count) = {
                let session_store = Arc::clone(&guard.session);
                let Ok(mut inner_session) = OwnedMutexGuard::lock(session_store, cx.cx()).await
                else {
                    emit_auto_compaction_error(
                        &out_tx,
                        "Inner session lock failed while applying auto-compaction",
                    );
                    return;
                };
                if inner_session.header.id != origin_session_id {
                    emit_auto_compaction_error(
                        &out_tx,
                        "Active session changed while auto-compaction was running",
                    );
                    return;
                }
                let mut candidate = inner_session.clone();
                candidate.append_compaction(
                    result.summary.clone(),
                    result.first_kept_entry_id.clone(),
                    result.tokens_before,
                    Some(details_value.clone()),
                    None,
                );
                // Post-compaction context estimate (heuristic, ignores usage).
                let tokens_after = crate::compaction::estimate_entries_context_tokens(
                    &candidate.entries_for_current_path(),
                );
                let messages = candidate.to_messages_for_current_path();
                if save_enabled {
                    provider_admission.block(
                        "RPC auto-compaction persistence was interrupted before live installation completed"
                            .to_string(),
                    );
                    if let Err(first_err) = candidate.save().await
                        && let Err(retry_err) = candidate.save().await
                    {
                        let reason = format!(
                            "RPC auto-compaction persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
                        );
                        provider_admission.block(reason.clone());
                        emit_auto_compaction_error(&out_tx, reason);
                        return;
                    }
                }
                let pending_message_count = json!(candidate.autosave_metrics().pending_mutations);
                *inner_session = candidate;
                (messages, tokens_after, pending_message_count)
            };
            guard.agent.replace_messages(messages);
            if save_enabled {
                provider_admission.clear();
            }
            drop(guard);

            let _ = out_tx.send(agent_event(AgentEvent::AutoCompactionEnd {
                result: Some(json!({
                    "summary": result.summary,
                    "firstKeptEntryId": result.first_kept_entry_id,
                    "tokensBefore": result.tokens_before,
                    "tokensAfter": tokens_after,
                    "details": details_value,
                    "persisted": save_enabled,
                    "persistenceStatus": if save_enabled {
                        json!({
                            "event": "session.persistence.healthy",
                            "severity": "ok",
                            "summary": "Compacted session history persisted.",
                            "action": "No action required.",
                            "pendingMessageCount": pending_message_count,
                        })
                    } else {
                        json!({
                            "event": "session.persistence.disabled",
                            "severity": "info",
                            "summary": "Session persistence is disabled; compacted history is retained in memory only.",
                            "action": "Enable session saving to make compacted history durable.",
                            "pendingMessageCount": pending_message_count,
                        })
                    },
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

fn session_stats(session: &crate::session::Session, save_enabled: bool) -> Value {
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
        if !save_enabled {
            (
                "session.persistence.disabled",
                "info",
                "Session persistence is disabled; history is retained in memory only.",
                "Enable session saving to make history durable.",
                Vec::new(),
            )
        } else if pending_message_count == 0 {
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
    crate::tools::attach_child_job_discipline(&child);

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
            mime_type: crate::model::sanitize_image_mime_type(media_type),
        });
    }
    Ok(images)
}

pub(crate) fn resolve_model_key(
    cli_api_key: Option<&str>,
    auth: &AuthStorage,
    entry: &ModelEntry,
) -> Option<String> {
    crate::models::resolve_model_key(cli_api_key, auth, entry)
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

fn replayed_plan_mode(session: &crate::session::Session) -> crate::plan::PlanMode {
    session
        .entries_for_current_path()
        .iter()
        .filter_map(|entry| {
            let SessionEntry::Custom(custom) = entry else {
                return None;
            };
            if custom.custom_type != "plan_mode" {
                return None;
            }
            custom
                .data
                .as_ref()
                .and_then(|data| data.get("mode"))
                .and_then(Value::as_str)
        })
        .fold(crate::plan::PlanMode::Off, |current, mode| match mode {
            "off" => crate::plan::PlanMode::Off,
            "planning" | "rejected" => crate::plan::PlanMode::Planning,
            // Submitted plan text is memory-only. Resetting the Agent maps
            // this unreconstructable state back to read-only Planning.
            "pending_approval" => crate::plan::PlanMode::PendingApproval,
            "approved" => crate::plan::PlanMode::Approved,
            _ => current,
        })
}

fn normalize_resumed_session_model(
    session: &mut crate::session::Session,
    entry: &ModelEntry,
) -> (crate::model::ThinkingLevel, bool) {
    let requested_thinking = session_thinking_level(session).unwrap_or_default();
    let thinking = entry.clamp_thinking_level(requested_thinking);
    let previous_model = session.effective_model_for_current_path();
    let previous_thinking = session_thinking_level(session);
    let model_changed = previous_model.as_ref().is_none_or(|(provider, model_id)| {
        provider != &entry.model.provider || model_id != &entry.model.id
    });
    let thinking_changed = previous_thinking != Some(thinking);
    let thinking_string = thinking.to_string();
    let header_changed = session.header.provider.as_deref() != Some(entry.model.provider.as_str())
        || session.header.model_id.as_deref() != Some(entry.model.id.as_str())
        || session.header.thinking_level.as_deref() != Some(thinking_string.as_str());

    if model_changed {
        session.append_model_change(entry.model.provider.clone(), entry.model.id.clone());
    }
    session.set_model_header(
        Some(entry.model.provider.clone()),
        Some(entry.model.id.clone()),
        Some(thinking_string.clone()),
    );
    if thinking_changed {
        session.append_thinking_level_change(thinking_string);
    }

    (
        thinking,
        model_changed || thinking_changed || header_changed,
    )
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
    shared_state: Arc<asupersync::sync::Mutex<RpcSharedState>>,
    level: crate::model::ThinkingLevel,
) -> Result<()> {
    let cx = AgentCx::for_current_or_request();
    apply_thinking_level_for_session(session, shared_state, level, &cx).await
}

async fn apply_thinking_level_for_session(
    session: Arc<asupersync::sync::Mutex<AgentSession>>,
    shared_state: Arc<asupersync::sync::Mutex<RpcSharedState>>,
    level: crate::model::ThinkingLevel,
    cx: &AgentCx,
) -> Result<()> {
    let mut guard = OwnedMutexGuard::lock(Arc::clone(&session), cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    let mut state = OwnedMutexGuard::lock(shared_state, cx)
        .await
        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
    state.bind_provider_admission(guard.provider_admission_gate());
    state.ensure_session_advancement_allowed()?;
    let save_enabled = guard.save_enabled();
    let session_store = Arc::clone(&guard.session);
    let mut inner_session = OwnedMutexGuard::lock(session_store, cx)
        .await
        .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
    let mut candidate = inner_session.clone();
    let level_str = level.to_string();
    let history_changed = session_thinking_level(&candidate) != Some(level);
    let header_changed = candidate.header.thinking_level.as_deref() != Some(level_str.as_str());
    let changed = history_changed || header_changed;
    candidate.header.thinking_level = Some(level_str.clone());
    if history_changed {
        candidate.append_thinking_level_change(level_str);
    }
    let _provider_transition = if changed {
        Some(
            state
                .provider_admission
                .begin_transition(
                    "thinking-level persistence was interrupted before live installation completed"
                        .to_string(),
                    cx,
                )
                .await?,
        )
    } else {
        None
    };
    if save_enabled
        && changed
        && let Err(first_err) = candidate.save().await
        && let Err(retry_err) = candidate.save().await
    {
        let reason = format!(
            "thinking-level persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
        );
        state.provider_admission.block(reason.clone());
        return Err(Error::session_persistence(reason));
    }
    *inner_session = candidate;
    guard.agent.stream_options_mut().thinking_level = Some(level);
    guard.refresh_extension_completion_host_state();
    if changed {
        state.provider_admission.clear();
    }
    Ok(())
}

async fn apply_model_change(
    guard: &mut AgentSession,
    state: &mut RpcSharedState,
    entry: &ModelEntry,
    thinking_level: crate::model::ThinkingLevel,
) -> Result<OwnedMutexGuard<()>> {
    let cx = AgentCx::for_current_or_request();
    state.bind_provider_admission(guard.provider_admission_gate());
    state.ensure_session_advancement_allowed()?;
    let save_enabled = guard.save_enabled();
    let session_store = Arc::clone(&guard.session);
    let mut inner_session = OwnedMutexGuard::lock(session_store, &cx)
        .await
        .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
    let mut candidate = inner_session.clone();
    let thinking_level = thinking_level.to_string();
    let thinking_changed = candidate
        .effective_thinking_level_for_current_path()
        .as_deref()
        != Some(thinking_level.as_str());
    candidate.set_model_header(
        Some(entry.model.provider.clone()),
        Some(entry.model.id.clone()),
        Some(thinking_level.clone()),
    );
    candidate.append_model_change(entry.model.provider.clone(), entry.model.id.clone());
    if thinking_changed {
        candidate.append_thinking_level_change(thinking_level);
    }
    guard.invalidate_background_compaction();
    let provider_transition = state
        .provider_admission
        .begin_transition(
            "model selection persistence was interrupted before live installation completed"
                .to_string(),
            &cx,
        )
        .await?;
    if save_enabled
        && let Err(first_err) = candidate.save().await
        && let Err(retry_err) = candidate.save().await
    {
        let reason = format!(
            "model selection persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
        );
        state.provider_admission.block(reason.clone());
        return Err(Error::session_persistence(reason));
    }
    *inner_session = candidate;
    Ok(provider_transition)
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
    state: &mut RpcSharedState,
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
    let _provider_transition = apply_model_change(guard, state, &next_entry, next_thinking).await?;

    guard.agent.set_provider(provider_impl);
    guard.agent.set_keyword_max_thinking_level(
        next_entry.clamp_thinking_level(crate::model::ThinkingLevel::Max),
    );
    guard
        .agent
        .set_tool_call_dialect(next_entry.tool_call_dialect());
    guard
        .agent
        .set_model_accepts_images(next_entry.model.input.contains(&InputType::Image));
    {
        let stream_options = guard.agent.stream_options_mut();
        stream_options.api_key.clone_from(&key);
        stream_options.headers.clone_from(&next_entry.headers);
        stream_options.max_tokens = Some(next_entry.model.max_tokens);
        stream_options.thinking_level = Some(next_thinking);
    }
    guard.set_compaction_context_window(context_window_tokens_for_entry(&next_entry));
    guard.refresh_extension_completion_host_state();
    if let Some(region) = &guard.extensions {
        region.manager().set_current_model(
            Some(next_entry.model.provider.clone()),
            Some(next_entry.model.id.clone()),
        );
    }
    state.clear_failover_lifecycle();
    state.provider_admission.clear();

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
    use std::sync::atomic::AtomicUsize;
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

    pub(super) fn dummy_entry(id: &str, reasoning: bool) -> ModelEntry {
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
            ask_tool: None,
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

    #[derive(Debug)]
    struct CompactionSummaryProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for CompactionSummaryProvider {
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
            let summary = "causal auto-compaction summary".to_string();
            let message = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(summary.clone()))],
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(crate::model::StreamEvent::TextDelta {
                    content_index: 0,
                    delta: summary.clone(),
                }),
                Ok(crate::model::StreamEvent::TextEnd {
                    content_index: 0,
                    content: summary,
                }),
                Ok(crate::model::StreamEvent::Done {
                    reason: StopReason::Stop,
                    message,
                }),
            ])))
        }
    }

    struct GatedAutoCompactionProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        compaction_entered: Mutex<Option<asupersync::channel::oneshot::Sender<()>>>,
        compaction_gate: Mutex<Option<asupersync::channel::oneshot::Receiver<()>>>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for GatedAutoCompactionProvider {
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
            if call == 1 {
                let cx = AgentCx::for_current_or_request();
                let entered_signal = self
                    .compaction_entered
                    .lock()
                    .expect("lock compaction entered signal")
                    .take();
                if let Some(entered) = entered_signal {
                    entered
                        .send(cx.cx(), ())
                        .expect("signal auto-compaction provider entry");
                }
                let gate = self
                    .compaction_gate
                    .lock()
                    .expect("lock auto-compaction gate")
                    .take();
                if let Some(mut gate) = gate {
                    let _ = gate.recv(cx.cx()).await;
                }
            }

            let text = if call == 0 {
                "primary turn response"
            } else {
                "gated auto-compaction summary"
            };
            let message = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: if call == 0 {
                    Usage {
                        total_tokens: 230_000,
                        ..Usage::default()
                    }
                } else {
                    Usage::default()
                },
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            let mut partial = message.clone();
            partial.content.clear();
            Ok(Box::pin(stream::iter(vec![
                Ok(crate::model::StreamEvent::Start { partial }),
                Ok(crate::model::StreamEvent::TextDelta {
                    content_index: 0,
                    delta: text.to_string(),
                }),
                Ok(crate::model::StreamEvent::TextEnd {
                    content_index: 0,
                    content: text.to_string(),
                }),
                Ok(crate::model::StreamEvent::Done {
                    reason: StopReason::Stop,
                    message,
                }),
            ])))
        }
    }

    fn seed_auto_compaction_session(mut session: Session) -> Session {
        session.header.provider = Some("test-provider".to_string());
        session.header.model_id = Some("test-model".to_string());
        // Stay above the 200k model window so auto-compaction triggers, but
        // below the 400k forced-local threshold so a broken summary provider
        // cannot make these persistence tests pass through the fallback path.
        // Provider usage is cumulative, so the latest assistant usage must
        // itself cross the trigger; the earlier value plants realistic growth.
        for (index, tokens) in [110_000, 230_000].into_iter().enumerate() {
            session.append_message(crate::session::SessionMessage::User {
                content: UserContent::Text(format!("user turn {index}")),
                timestamp: Some(0),
            });
            session.append_message(crate::session::SessionMessage::Assistant {
                message: AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent::new(format!(
                        "assistant turn {index}"
                    )))],
                    api: "test-api".to_string(),
                    provider: "test-provider".to_string(),
                    model: "test-model".to_string(),
                    usage: Usage {
                        total_tokens: tokens,
                        ..Usage::default()
                    },
                    stop_reason: StopReason::Stop,
                    stop_details: None,
                    error_message: None,
                    timestamp: 0,
                },
            });
        }
        session.append_message(crate::session::SessionMessage::User {
            content: UserContent::Text("recent turn".to_string()),
            timestamp: Some(0),
        });
        session
    }

    async fn run_auto_compaction_persistence_case(
        session: Session,
        save_enabled: bool,
        runtime_handle: RuntimeHandle,
    ) -> (
        Vec<Value>,
        Arc<asupersync::sync::Mutex<Session>>,
        crate::session::AutosaveQueueMetrics,
        ProviderAdmissionGate,
    ) {
        let mut model = dummy_entry("test-model", false);
        model.model.provider = "test-provider".to_string();
        let provider: Arc<dyn Provider> = Arc::new(CompactionSummaryProvider);
        let agent = Agent::new(
            provider,
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig {
                stream_options: crate::provider::StreamOptions {
                    api_key: Some("test-key".to_string()),
                    ..crate::provider::StreamOptions::default()
                },
                ..AgentConfig::default()
            },
        );
        let seeded_session = seed_auto_compaction_session(session);
        let metrics_before = seeded_session.autosave_metrics();
        let inner_session = Arc::new(asupersync::sync::Mutex::new(seeded_session));
        let agent_session = AgentSession::new(
            agent,
            Arc::clone(&inner_session),
            save_enabled,
            crate::compaction::ResolvedCompactionSettings::default(),
        );
        let provider_admission = agent_session.provider_admission_gate();
        let mut config = Config::default();
        config.compaction = Some(crate::config::CompactionSettings {
            enabled: Some(true),
            reserve_tokens: Some(2),
            keep_recent_tokens: Some(1),
            mode: None,
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
            ask_tool: None,
        };
        let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(16);
        let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
            &options.config,
        )));
        maybe_auto_compact(
            Arc::new(asupersync::sync::Mutex::new(agent_session)),
            shared_state,
            options,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            out_tx,
        )
        .await;
        let events = out_rx
            .try_iter()
            .map(|line| serde_json::from_str::<Value>(&line).expect("event json"))
            .collect();
        (events, inner_session, metrics_before, provider_admission)
    }

    fn provider_compaction_count(session: &Session) -> usize {
        session
            .entries_for_current_path()
            .iter()
            .filter(|entry| {
                matches!(
                    entry,
                    crate::session::SessionEntry::Compaction(compaction)
                        if compaction.summary == "causal auto-compaction summary"
                )
            })
            .count()
    }

    #[derive(Debug, Clone)]
    struct CapturedQueuedKeywordCall {
        thinking_level: Option<ThinkingLevel>,
        system_prompt: Option<String>,
        messages: Vec<Message>,
    }

    struct GatedQueuedKeywordProvider {
        first_call_entered: Mutex<Option<asupersync::channel::oneshot::Sender<()>>>,
        first_call_gate: Mutex<Option<asupersync::channel::oneshot::Receiver<()>>>,
        calls: Arc<Mutex<Vec<CapturedQueuedKeywordCall>>>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for GatedQueuedKeywordProvider {
        fn name(&self) -> &str {
            "queued-keyword-provider"
        }

        fn api(&self) -> &str {
            "queued-keyword-api"
        }

        fn model_id(&self) -> &str {
            "queued-keyword-model"
        }

        async fn stream(
            &self,
            context: &crate::provider::Context<'_>,
            options: &crate::provider::StreamOptions,
        ) -> crate::error::Result<
            Pin<
                Box<
                    dyn futures::Stream<Item = crate::error::Result<crate::model::StreamEvent>>
                        + Send,
                >,
            >,
        > {
            self.calls
                .lock()
                .expect("capture queued keyword call")
                .push(CapturedQueuedKeywordCall {
                    thinking_level: options.thinking_level,
                    system_prompt: context.system_prompt.as_deref().map(str::to_string),
                    messages: context.messages.to_vec(),
                });

            let cx = AgentCx::for_current_or_request();
            let first_call_entered = self
                .first_call_entered
                .lock()
                .expect("lock queued keyword entered signal")
                .take();
            if let Some(entered) = first_call_entered {
                entered
                    .send(cx.cx(), ())
                    .expect("signal first queued keyword provider call");
            }
            let first_call_gate = self
                .first_call_gate
                .lock()
                .expect("lock queued keyword gate")
                .take();
            if let Some(mut gate) = first_call_gate {
                let _ = gate.recv(cx.cx()).await;
            }

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

    struct GatedTerminalErrorProvider {
        entered: Mutex<Option<asupersync::channel::oneshot::Sender<()>>>,
        gate: Mutex<Option<asupersync::channel::oneshot::Receiver<()>>>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for GatedTerminalErrorProvider {
        fn name(&self) -> &str {
            "gated-terminal-error-provider"
        }

        fn api(&self) -> &str {
            "gated-terminal-error-api"
        }

        fn model_id(&self) -> &str {
            "gated-terminal-error-model"
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
            self.calls.fetch_add(1, Ordering::SeqCst);
            let cx = AgentCx::for_current_or_request();
            let entered = self.entered.lock().expect("entered signal lock").take();
            if let Some(entered) = entered {
                entered
                    .send(cx.cx(), ())
                    .expect("signal terminal provider entry");
            }
            let gate = self.gate.lock().expect("terminal gate lock").take();
            if let Some(mut gate) = gate {
                let _ = gate.recv(cx.cx()).await;
            }

            let message = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Error,
                stop_details: None,
                error_message: Some("invalid API key from gated provider".to_string()),
                timestamp: 0,
            };
            Ok(Box::pin(stream::iter(vec![
                Ok(crate::model::StreamEvent::Start {
                    partial: message.clone(),
                }),
                Ok(crate::model::StreamEvent::Done {
                    reason: StopReason::Error,
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

    pub(super) fn build_test_rpc_options(
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
            ask_tool: None,
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

    async fn recv_ask_request(out_rx: &Arc<Mutex<Receiver<String>>>, label: &str) -> Value {
        let start = Instant::now();
        loop {
            let recv_result = {
                let rx = out_rx.lock().expect("lock rpc output receiver");
                rx.try_recv()
            };

            match recv_result {
                Ok(line) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&line)
                        && value.get("type").and_then(Value::as_str) == Some("ask_request")
                    {
                        return value;
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    unreachable!(
                        "{label}: output channel disconnected while waiting for ask_request"
                    );
                }
                Err(TryRecvError::Empty) => {}
            }

            assert!(
                start.elapsed() <= Duration::from_secs(10),
                "{label}: timed out waiting for ask_request"
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

    #[test]
    fn session_advancing_commands_require_stranded_input_recovery() {
        for command in [
            "prompt",
            "set_plan_mode",
            "bash",
            "checkpoint",
            "new_session",
        ] {
            assert!(
                command_can_advance_rpc_session(command),
                "{command} can advance the session"
            );
        }
        for command in [
            "abort",
            "get_state",
            "get_messages",
            "set_auto_retry",
            "extension_ui_response",
        ] {
            assert!(
                !command_can_advance_rpc_session(command),
                "{command} must remain available while recovery is blocked"
            );
        }
        for command in ["prompt", "steer", "follow_up"] {
            assert!(command_can_queue_while_rpc_agent_streams(command));
        }
        for command in ["set_model", "checkpoint", "retry", "new_session"] {
            assert!(!command_can_queue_while_rpc_agent_streams(command));
        }
    }

    #[test]
    fn provider_turn_commands_resume_pending_agent_input() {
        for command in ["steer", "follow_up", "retry"] {
            assert!(command_resumes_rpc_agent(command, &json!({}), None));
        }
        assert!(command_resumes_rpc_agent(
            "prompt",
            &json!({"message": "resume"}),
            None
        ));
        assert!(!command_resumes_rpc_agent("prompt", &json!({}), None));
        assert!(!command_resumes_rpc_agent("set_model", &json!({}), None));
        assert!(command_payload_can_advance_rpc_session(
            "prompt",
            &json!({"message": "resume"}),
            None
        ));
        for (command, payload) in [
            ("prompt", json!({})),
            ("set_model", json!({"provider": "test"})),
            ("bash", json!({})),
            ("compact", json!({"reserveTokens": "invalid"})),
        ] {
            assert!(
                !command_payload_can_advance_rpc_session(command, &payload, None),
                "invalid {command} must not trigger terminal recovery"
            );
        }
    }

    #[test]
    fn rpc_durability_failure_keeps_primary_turn_error_context() {
        let err = finish_rpc_turn_durability::<()>(
            Err(Error::provider("test", "provider failed")),
            Err(Error::session("disk flush failed")),
        )
        .expect_err("durability failure must remain terminal");

        assert!(err.is_session_persistence());
        let message = err.to_string();
        assert!(message.contains("disk flush failed"));
        assert!(message.contains("provider failed"));
    }

    #[test]
    fn rpc_retry_rewinds_durable_path_before_reappending_user_turn() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let prompt = build_user_message("[REWIND REPORT: literal user prompt", &[]);
            let mut inner = Session::in_memory();
            let original_user_id = inner.append_model_message(prompt.clone());
            let original_assistant_id =
                inner.append_model_message(Message::Assistant(Arc::new(AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent::new("original response"))],
                    stop_reason: StopReason::Stop,
                    ..AssistantMessage::default()
                })));
            let original_entry_count = inner.entries.len();
            let mut agent_session = build_test_agent_session(inner);
            agent_session.agent.replace_messages(vec![
                prompt,
                Message::Assistant(Arc::new(AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent::new("original response"))],
                    stop_reason: StopReason::Stop,
                    ..AssistantMessage::default()
                })),
            ]);

            let text = take_last_rpc_user_turn_for_retry(&mut agent_session)
                .await
                .expect("rewind retry path")
                .expect("retryable user turn");
            assert_eq!(text, "[REWIND REPORT: literal user prompt");

            let cx = AgentCx::for_request();
            let mut rewound = agent_session
                .session
                .lock(&cx)
                .await
                .expect("rewound session lock");
            assert_eq!(
                rewound.entries.len(),
                original_entry_count,
                "rewind must preserve the abandoned branch"
            );
            assert!(rewound.get_entry(&original_user_id).is_some());
            assert!(rewound.get_entry(&original_assistant_id).is_some());
            assert!(
                rewound.to_messages_for_current_path().is_empty(),
                "the active path must move behind the original root user turn"
            );

            rewound.append_model_message(build_user_message(&text, &[]));
            let active_prompt_count = rewound
                .to_messages_for_current_path()
                .iter()
                .filter(|message| {
                    matches!(
                        message,
                        Message::User(UserMessage {
                            content: UserContent::Text(text),
                            ..
                        }) if text == "[REWIND REPORT: literal user prompt"
                    )
                })
                .count();
            assert_eq!(active_prompt_count, 1);
            assert_eq!(
                rewound.entries.len(),
                original_entry_count + 1,
                "retry must append one new branch entry without deleting the original path"
            );
        });
    }

    #[test]
    fn rpc_retry_uses_rewind_aware_durable_user_provenance() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let original_prompt = build_user_message("retry before checkpoint", &[]);
            let hidden_prompt = build_user_message("hidden after checkpoint", &[]);
            let mut inner = Session::in_memory();
            let original_user_id = inner.append_model_message(original_prompt.clone());
            inner.append_model_message(Message::Assistant(Arc::new(AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("original response"))],
                stop_reason: StopReason::Stop,
                ..AssistantMessage::default()
            })));
            let checkpoint_id = inner.append_custom_entry(
                "checkpoint".to_string(),
                Some(json!({"name": "before-hidden-turn"})),
            );
            let hidden_user_id = inner.append_model_message(hidden_prompt);
            inner.append_model_message(Message::Assistant(Arc::new(AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("hidden response"))],
                stop_reason: StopReason::Stop,
                ..AssistantMessage::default()
            })));
            let rewind_id = inner.append_custom_entry(
                "rewind".to_string(),
                Some(json!({
                    "checkpoint": "before-hidden-turn",
                    "checkpointEntryId": checkpoint_id,
                    "summary": "discard the later turn"
                })),
            );
            let projected_messages = inner.to_messages_for_current_path();
            let original_entry_count = inner.entries.len();
            let mut agent_session = build_test_agent_session(inner);
            agent_session.agent.replace_messages(projected_messages);

            let text = take_last_rpc_user_turn_for_retry(&mut agent_session)
                .await
                .expect("rewind-aware retry path")
                .expect("retryable projected user turn");
            assert_eq!(text, "retry before checkpoint");

            let cx = AgentCx::for_request();
            let mut rewound = agent_session
                .session
                .lock(&cx)
                .await
                .expect("rewound session lock");
            for preserved_id in [&original_user_id, &hidden_user_id, &rewind_id] {
                assert!(
                    rewound.get_entry(preserved_id).is_some(),
                    "retry must preserve every entry on the abandoned rewind branch"
                );
            }
            assert_eq!(rewound.entries.len(), original_entry_count);
            assert!(rewound.to_messages_for_current_path().is_empty());

            rewound.append_model_message(build_user_message(&text, &[]));
            let retry_prompt_count = rewound
                .to_messages_for_current_path()
                .iter()
                .filter(|message| {
                    matches!(
                        message,
                        Message::User(UserMessage {
                            content: UserContent::Text(text),
                            ..
                        }) if text == "retry before checkpoint"
                    )
                })
                .count();
            assert_eq!(retry_prompt_count, 1);
            assert_eq!(rewound.entries.len(), original_entry_count + 1);
        });
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
                // Boxed: clippy::large_futures.
                handle.spawn(async move { Box::pin(run(agent_session, options, in_rx, out_tx)).await });

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
            let request_generation = ui_event["requestGeneration"]
                .as_u64()
                .expect("ui request generation should be an integer");

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
                "requestGeneration": request_generation,
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
                // Boxed: clippy::large_futures.
                handle.spawn(async move { Box::pin(run(agent_session, options, in_rx, out_tx)).await });

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
    fn rpc_stdin_eof_cancels_idle_unbounded_extension_ui_request() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let cwd = temp.path().to_path_buf();
            let ext_entry_path = cwd.join("idle-ui-ext.mjs");
            std::fs::write(&ext_entry_path, RPC_BUSY_EXTENSION_COMMAND_EXT)
                .expect("write extension source");

            let mut agent_session = build_test_agent_session(Session::in_memory());
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

            let options = build_test_rpc_options(&handle, cwd.join("auth.json"));
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(Mutex::new(out_rx));
            let server =
                // Boxed: clippy::large_futures.
                handle.spawn(async move { Box::pin(run(agent_session, options, in_rx, out_tx)).await });

            let ready = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"ready","type":"get_state"}"#,
                "get_state before idle extension UI",
            )
            .await;
            assert_ok(&ready, "get_state");

            let request_manager = manager.clone();
            let pending_ui = handle.spawn(async move {
                request_manager
                    .request_ui(ExtensionUiRequest::new(
                        "idle-unbounded",
                        "confirm",
                        json!({"title": "No command is running"}),
                    ))
                    .await
            });
            let ui_event = recv_ui_request(&out_rx, "idle unbounded UI before eof").await;
            assert_eq!(ui_event["id"], "idle-unbounded");
            assert!(ui_event.get("timeout_ms").is_none());

            drop(in_tx);

            let ui_response = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(1),
                pending_ui,
            )
            .await
            .expect("idle UI request must be cancelled immediately at stdin EOF")
            .expect("terminal close should resolve the UI request")
            .expect("response-bearing UI request should receive a response");
            assert_eq!(ui_response.id, "idle-unbounded");
            assert!(ui_response.cancelled);

            let server_result = server.await;
            assert!(server_result.is_ok(), "rpc server error: {server_result:?}");
        });
    }

    #[test]
    fn ask_ui_close_guard_disconnects_forwarder_channel_on_drop() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        runtime.block_on(async {
            let ask = crate::ask::AskTool::new(crate::ask::AskPolicy::Error);
            let (ask_ui_tx, mut ask_ui_rx) =
                asupersync::channel::mpsc::channel::<crate::ask::AskUiRequest>(1);
            ask.install_channel_ui(ask_ui_tx);
            let forwarder_ask = ask.clone();

            drop(AskUiCloseGuard(ask));

            let cx = AgentCx::for_request();
            let disconnected = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(1),
                ask_ui_rx.recv(&cx),
            )
            .await
            .expect("dropping the RPC guard must release the installed channel sender");
            assert!(
                disconnected.is_err(),
                "the Ask forwarder receiver must disconnect on every guarded RPC exit"
            );
            drop(forwarder_ask);
        });
    }

    #[test]
    fn extension_ui_close_guard_disconnects_channel_and_cancels_pending_request() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        let handle = runtime.handle();
        runtime.block_on(async move {
            let manager = ExtensionManager::new();
            let (ui_tx, mut ui_rx) = asupersync::channel::mpsc::channel::<ExtensionUiRequest>(1);
            manager.set_ui_sender(ui_tx);

            let request_manager = manager.clone();
            let pending = handle.spawn(async move {
                request_manager
                    .request_ui(ExtensionUiRequest::new(
                        "guarded-request",
                        "confirm",
                        json!({"title": "Wait forever"}),
                    ))
                    .await
            });
            let cx = AgentCx::for_request();
            let request = ui_rx
                .recv(&cx)
                .await
                .expect("pending request should reach the RPC forwarder");
            assert_eq!(request.id, "guarded-request");

            drop(ExtensionUiCloseGuard {
                manager,
                ui_state: Arc::new(std::sync::Mutex::new(RpcUiBridgeState::default())),
            });

            let response = pending
                .await
                .expect("terminal close should resolve the pending request")
                .expect("response-bearing request should receive a response");
            assert_eq!(response.id, "guarded-request");
            assert!(response.cancelled);

            let disconnected = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(1),
                ui_rx.recv(&cx),
            )
            .await
            .expect("dropping the RPC guard must release the extension UI sender");
            assert!(
                disconnected.is_err(),
                "the extension UI forwarder receiver must disconnect on guarded RPC exit"
            );
        });
    }

    #[test]
    fn rpc_stdin_eof_dismisses_pending_ask_tool_request() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let ask = crate::ask::AskTool::new(crate::ask::AskPolicy::Error);
            let agent_session = build_test_agent_session(Session::in_memory());
            let mut options = build_test_rpc_options(&handle, temp.path().join("auth.json"));
            options.ask_tool = Some(ask.clone());
            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(Mutex::new(out_rx));

            let server =
                // Boxed: clippy::large_futures.
                handle.spawn(async move { Box::pin(run(agent_session, options, in_rx, out_tx)).await });
            let ready = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"ready","type":"get_state"}"#,
                "get_state before ask",
            )
            .await;
            assert_ok(&ready, "get_state");

            let ask_task = handle.spawn(async move {
                crate::tools::Tool::execute(
                    &ask,
                    "rpc-eof-ask",
                    json!({
                        "questions": [{
                            "question": "Pick?",
                            "options": [{"label": "A"}, {"label": "B"}]
                        }]
                    }),
                    None,
                )
                .await
            });
            let ask_event = recv_ask_request(&out_rx, "pending ask before eof").await;
            assert_eq!(ask_event["type"], "ask_request");

            drop(in_tx);
            let server_result = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(1),
                server,
            )
            .await
            .expect("RPC EOF must not wait for the ask tool's five-minute timeout");
            assert!(server_result.is_ok(), "rpc server error: {server_result:?}");

            let ask_result = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(1),
                ask_task,
            )
            .await
            .expect("pending ask must be dismissed promptly after RPC EOF")
            .expect_err("EOF must dismiss rather than answer the picker");
            assert!(ask_result.to_string().contains("dismissed"), "{ask_result}");
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
                // Boxed: clippy::large_futures.
                handle.spawn(async move { Box::pin(run(agent_session, options, in_rx, out_tx)).await });

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
                // Boxed: clippy::large_futures.
                handle.spawn(async move { Box::pin(run(agent_session, options, in_rx, out_tx)).await });

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
                // Boxed: clippy::large_futures.
                handle.spawn(async move { Box::pin(run(agent_session, options, in_rx, out_tx)).await });

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
                // Boxed: clippy::large_futures.
                handle.spawn(async move { Box::pin(run(agent_session, options, in_rx, out_tx)).await });

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
                // Boxed: clippy::large_futures.
                handle.spawn(async move { Box::pin(run(agent_session, options, in_rx, out_tx)).await });

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
    fn shared_state_preserves_raw_keyword_source_beside_expanded_payload() {
        let config = Config::default();
        let mut shared = RpcSharedState::new(&config);
        let expanded = "generated ultrathink workflowz bytes";

        shared
            .push_steering(QueuedAgentMessage::authored(
                build_user_message(expanded, &[]),
                "please orchestrate this",
            ))
            .expect("enqueue source-aware steering");

        let queued = shared.lease_steering(0);
        assert_eq!(queued.len(), 1);
        assert_eq!(
            queued[0].keyword_scan_source(),
            Some("please orchestrate this")
        );
        assert!(matches!(
            queued[0].message(),
            Message::User(UserMessage {
                content: UserContent::Text(text),
                ..
            }) if text == expanded
        ));
    }

    #[test]
    fn rpc_follow_up_fetch_generation_advances_only_when_rpc_message_is_drained() {
        let mut shared = RpcSharedState::new(&Config::default());
        let generation = Arc::clone(&shared.follow_up_fetch_generation);

        assert!(shared.lease_follow_up_for_fetch(0).is_empty());
        assert_eq!(generation.load(Ordering::SeqCst), 0);

        shared
            .push_follow_up(QueuedAgentMessage::from_authored_message(
                build_user_message("accepted follow-up", &[]),
            ))
            .expect("enqueue RPC follow-up");
        let drained = shared.lease_follow_up_for_fetch(0);
        assert_eq!(drained.len(), 1);
        assert_eq!(generation.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn leased_rpc_input_remains_pending_until_durable_acknowledgement() {
        let mut shared = RpcSharedState::new(&Config::default());
        shared
            .push_steering(QueuedAgentMessage::from_authored_message(
                build_user_message("leased steering", &[]),
            ))
            .expect("enqueue RPC steering");

        let leased = shared.lease_steering(7);
        assert_eq!(leased.len(), 1);
        assert!(shared.steering.is_empty());
        assert_eq!(shared.steering_in_flight.len(), 1);
        assert_eq!(shared.steering_in_flight[0].session_entry_baseline, 7);
        assert_eq!(
            shared.pending_count(),
            1,
            "leasing must not discard acknowledged queue authority"
        );

        shared.acknowledge_in_flight();
        assert_eq!(shared.pending_count(), 0);
    }

    #[test]
    fn in_flight_rpc_input_preserves_cross_queue_lease_order() {
        let mut shared = RpcSharedState::new(&Config::default());
        for (kind, text) in [
            ("steering", "first steering"),
            ("follow_up", "middle follow-up"),
            ("steering", "last steering"),
        ] {
            let delivery = QueuedAgentMessage::from_authored_message(build_user_message(text, &[]));
            if kind == "steering" {
                shared.push_steering(delivery).expect("enqueue steering");
                assert_eq!(shared.lease_steering(0).len(), 1);
            } else {
                shared.push_follow_up(delivery).expect("enqueue follow-up");
                assert_eq!(shared.lease_follow_up_for_fetch(0).len(), 1);
            }
        }

        let texts = shared
            .in_flight_in_lease_order()
            .into_iter()
            .map(|in_flight| match in_flight.delivery.message() {
                Message::User(UserMessage {
                    content: UserContent::Text(text),
                    ..
                }) => text.as_str(),
                other => panic!("expected text user message, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec!["first steering", "middle follow-up", "last steering"]
        );
    }

    #[test]
    fn max_time_staged_input_survives_recorded_tool_transcript_recovery() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let represented_delivery = {
                let cx = AgentCx::for_request();
                let mut state = shared_state
                    .lock(&cx)
                    .await
                    .expect("shared state lock");
                state
                    .push_steering(QueuedAgentMessage::from_authored_message(
                        build_user_message("already represented steering", &[]),
                    ))
                    .expect("enqueue steering");
                let represented = state
                    .lease_steering(0)
                    .into_iter()
                    .next()
                    .expect("lease represented steering");
                state
                    .push_steering(QueuedAgentMessage::from_authored_message(
                        build_user_message("leased max-time steering", &[]),
                    ))
                    .expect("enqueue max-time steering");
                represented
            };

            let provider: Arc<dyn Provider> = Arc::new(NoopProvider);
            let mut agent = Agent::new(
                provider,
                ToolRegistry::new(&[], Path::new("."), None),
                AgentConfig {
                    max_time: Some(Duration::ZERO),
                    ..AgentConfig::default()
                },
            );
            let completed_assistant = Message::Assistant(Arc::new(AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "completed-tool".to_string(),
                    name: "write".to_string(),
                    arguments: json!({"path": "completed.txt"}),
                    thought_signature: None,
                })],
                stop_reason: StopReason::ToolUse,
                ..AssistantMessage::default()
            }));
            let completed_result = Message::ToolResult(Arc::new(crate::model::ToolResultMessage {
                tool_call_id: "completed-tool".to_string(),
                tool_name: "write".to_string(),
                content: vec![ContentBlock::Text(TextContent::new("wrote completed.txt"))],
                details: None,
                is_error: false,
                timestamp: 2,
            }));
            agent.replace_messages(vec![
                represented_delivery.message().clone(),
                completed_assistant,
                completed_result,
            ]);
            let fetch_state = Arc::clone(&shared_state);
            let steering_fetcher = move || {
                let fetch_state = Arc::clone(&fetch_state);
                Box::pin(async move {
                    let cx = AgentCx::for_request();
                    fetch_state
                        .lock(&cx)
                        .await
                        .map_or_else(|_| Vec::new(), |mut state| state.lease_steering(0))
                }) as futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>>
            };
            agent.register_message_fetchers(Some(Arc::new(steering_fetcher)), None);
            let mut inner = Session::in_memory();
            inner.append_model_message(represented_delivery.message().clone());
            let session = Arc::new(asupersync::sync::Mutex::new(AgentSession::new(
                agent,
                Arc::new(asupersync::sync::Mutex::new(inner)),
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            )));
            let cx = AgentCx::for_request();
            let capped = session
                .lock(&cx)
                .await
                .expect("agent session lock")
                .agent
                .run_with_message_with_abort(
                    build_user_message("initial prompt", &[]),
                    None,
                    |_| {},
                )
                .await
                .expect("time-capped run");
            assert!(
                capped.content.iter().any(|block| {
                    matches!(block, ContentBlock::Text(text) if text.text.contains("time cap reached"))
                }),
                "run must stop at the configured boundary"
            );
            assert_eq!(
                shared_state
                    .lock(&cx)
                    .await
                    .expect("shared state lock")
                    .pending_count(),
                2
            );
            assert_eq!(
                terminal_rpc_recovery_plan(&session, &shared_state, true, &cx)
                    .await
                    .expect("resume recovery decision"),
                RpcTerminalRecoveryPlan::RecordedToolTranscript { recovery_count: 2 },
                "recorded tool effects must be saved without flattening resumable input"
            );
            assert_eq!(
                preserve_recorded_tool_transcript(&session, &shared_state, &cx)
                    .await
                    .expect("preserve recorded tool transcript"),
                2,
                "only the recorded assistant/tool-result pair is recovered"
            );
            assert_eq!(
                shared_state
                    .lock(&cx)
                    .await
                    .expect("shared state after transcript recovery")
                    .pending_count(),
                2,
                "transcript recovery must leave represented and staged RPC input authoritative"
            );
            assert_eq!(
                terminal_rpc_recovery_plan(&session, &shared_state, true, &cx)
                    .await
                    .expect("post-recovery resume decision"),
                RpcTerminalRecoveryPlan::None,
                "the next provider turn must consume the still-staged delivery"
            );
            assert_eq!(
                terminal_rpc_recovery_plan(&session, &shared_state, false, &cx)
                    .await
                    .expect("non-resume recovery decision"),
                RpcTerminalRecoveryPlan::All { recovery_count: 2 },
                "non-provider session mutations must preserve staged input first"
            );
        });
    }

    #[test]
    fn streaming_rpc_queue_scans_raw_source_not_expanded_template() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let (first_call_entered, mut wait_for_first_call) =
                asupersync::channel::oneshot::channel::<()>();
            let (release_first_call, first_call_gate) =
                asupersync::channel::oneshot::channel::<()>();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let provider: Arc<dyn Provider> = Arc::new(GatedQueuedKeywordProvider {
                first_call_entered: Mutex::new(Some(first_call_entered)),
                first_call_gate: Mutex::new(Some(first_call_gate)),
                calls: Arc::clone(&calls),
            });
            let tools = ToolRegistry::new(&[], temp.path(), None);
            let mut agent = Agent::new(
                provider,
                tools,
                AgentConfig {
                    stream_options: crate::provider::StreamOptions {
                        thinking_level: Some(ThinkingLevel::Low),
                        ..crate::provider::StreamOptions::default()
                    },
                    ..AgentConfig::default()
                },
            );
            agent.set_keyword_max_thinking_level(ThinkingLevel::High);
            let session = Arc::new(asupersync::sync::Mutex::new(Session::in_memory()));
            let agent_session = AgentSession::new(
                agent,
                session,
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );

            let mut options =
                build_test_rpc_options(&runtime_handle, temp.path().join("auth.json"));
            options.resources = load_test_prompt_template_resources(
                temp.path(),
                "queued",
                "generated ultrathink workflowz payload; argument=$ARGUMENTS",
            )
            .await;

            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(Mutex::new(out_rx));
            let server = runtime_handle
                // Boxed: clippy::large_futures.
                .spawn(async move { Box::pin(run(agent_session, options, in_rx, out_tx)).await });

            let initial = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"1","type":"prompt","message":"ordinary first turn"}"#,
                "initial gated prompt",
            )
            .await;
            assert_ok(&initial, "prompt");

            let entered_cx = AgentCx::for_request();
            wait_for_first_call
                .recv(entered_cx.cx())
                .await
                .expect("first provider call entered");

            let queued = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"2","type":"prompt","message":"/queued orchestrate","streamingBehavior":"steer"}"#,
                "streaming source-aware steering prompt",
            )
            .await;
            assert_ok(&queued, "prompt");

            let release_cx = AgentCx::for_request();
            release_first_call
                .send(release_cx.cx(), ())
                .expect("release first provider call");

            loop {
                let line = recv_line(&out_rx, "source-aware streaming completion")
                    .await
                    .expect("receive streaming event");
                let value = parse_response(&line);
                if value["type"] == "agent_end" {
                    assert!(
                        value.get("error").is_none_or(Value::is_null),
                        "streaming turn should complete: {value}"
                    );
                    break;
                }
            }

            drop(in_tx);
            let server_result = server.await;
            assert!(
                server_result.is_ok(),
                "RPC server error: {server_result:?}"
            );

            let captured = calls.lock().expect("capture calls");
            assert_eq!(captured.len(), 2, "steering should trigger a second provider call");
            assert_eq!(captured[0].thinking_level, Some(ThinkingLevel::Low));
            assert_eq!(
                captured[1].thinking_level,
                Some(ThinkingLevel::Low),
                "generated ultrathink in the expanded template must stay inert"
            );
            let second_prompt = captured[1]
                .system_prompt
                .as_deref()
                .expect("orchestrate directive");
            assert!(second_prompt.contains("invoked `orchestrate`"));
            assert!(!second_prompt.contains("invoked `workflowz`"));
            assert!(captured[1].messages.iter().any(|message| {
                matches!(
                    message,
                    Message::User(UserMessage {
                        content: UserContent::Text(text),
                        ..
                    }) if text == "generated ultrathink workflowz payload; argument=orchestrate"
                )
            }));
        });
    }

    #[test]
    fn terminal_provider_error_preserves_follow_up_acknowledged_while_streaming() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let (entered, mut wait_for_entry) = asupersync::channel::oneshot::channel::<()>();
            let (release, gate) = asupersync::channel::oneshot::channel::<()>();
            let calls = Arc::new(AtomicUsize::new(0));
            let provider: Arc<dyn Provider> = Arc::new(GatedTerminalErrorProvider {
                entered: Mutex::new(Some(entered)),
                gate: Mutex::new(Some(gate)),
                calls: Arc::clone(&calls),
            });
            let tools = ToolRegistry::new(&[], temp.path(), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session_value = Session::create_with_dir(Some(temp.path().join("sessions")));
            let inner_session = Arc::new(asupersync::sync::Mutex::new(session_value));
            let agent_session = AgentSession::new(
                agent,
                Arc::clone(&inner_session),
                true,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            let options = build_test_rpc_options(&runtime_handle, temp.path().join("auth.json"));

            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(Mutex::new(out_rx));
            let server = runtime_handle
                // Boxed: clippy::large_futures.
                .spawn(async move { Box::pin(run(agent_session, options, in_rx, out_tx)).await });

            let prompt = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"1","type":"prompt","message":"terminal prompt"}"#,
                "terminal prompt acknowledgment",
            )
            .await;
            assert_ok(&prompt, "prompt");

            let entry_cx = AgentCx::for_request();
            wait_for_entry
                .recv(entry_cx.cx())
                .await
                .expect("terminal provider entered");

            let follow_up = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"2","type":"follow_up","message":"accepted during terminal turn"}"#,
                "terminal follow-up acknowledgment",
            )
            .await;
            assert_ok(&follow_up, "follow_up");
            drop(in_tx);

            let release_cx = AgentCx::for_request();
            release
                .send(release_cx.cx(), ())
                .expect("release terminal provider");

            let terminal_event = loop {
                let line = recv_line(&out_rx, "terminal provider completion")
                    .await
                    .expect("terminal provider event");
                let value = parse_response(&line);
                if value["type"] == "agent_end" {
                    break value;
                }
            };
            assert!(
                terminal_event["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("invalid API key from gated provider")),
                "terminal error must remain observable: {terminal_event}"
            );
            let session_path = inner_session
                .lock(&AgentCx::for_request())
                .await
                .expect("terminal session lock after agent_end")
                .path
                .clone()
                .expect("terminal agent_end must follow the first durable save");
            let reopened = Session::open(session_path.to_string_lossy().as_ref())
                .await
                .expect("reopen terminal session immediately after agent_end");
            let durable_accepted_count = reopened
                .to_messages_for_current_path()
                .iter()
                .filter(|message| {
                    matches!(
                        message,
                        Message::User(UserMessage {
                            content: UserContent::Text(text),
                            ..
                        }) if text == "accepted during terminal turn"
                    )
                })
                .count();
            assert_eq!(
                durable_accepted_count, 1,
                "terminal agent_end must follow durable accepted input"
            );
            server.await.expect("RPC server run");
            assert_eq!(calls.load(Ordering::SeqCst), 1);

            let remaining_events = out_rx
                .lock()
                .expect("terminal output lock")
                .try_iter()
                .map(|line| parse_response(&line))
                .collect::<Vec<_>>();
            assert!(
                remaining_events
                    .iter()
                    .all(|event| event["type"] != "agent_end"),
                "terminal provider run emitted duplicate agent_end events: {remaining_events:?}"
            );

            let session_cx = AgentCx::for_request();
            let session = inner_session
                .lock(&session_cx)
                .await
                .expect("terminal session lock");
            let accepted_count = session
                .to_messages_for_current_path()
                .iter()
                .filter(|message| {
                    matches!(
                        message,
                        Message::User(UserMessage {
                            content: UserContent::Text(text),
                            ..
                        }) if text == "accepted during terminal turn"
                    )
                })
                .count();
            assert_eq!(
                accepted_count, 1,
                "acknowledged follow-up must be recorded exactly once after terminal failure"
            );
        });
    }

    #[test]
    fn shared_state_blocks_follow_up_when_steering_queue_reaches_total_cap() {
        let config = Config::default();
        let mut shared = RpcSharedState::new(&config);

        for idx in 0..MAX_RPC_PENDING_MESSAGES {
            shared
                .push_steering(QueuedAgentMessage::from_authored_message(
                    build_user_message(&format!("steer-{idx}"), &[]),
                ))
                .expect("steering enqueue within total cap");
        }

        let err = shared
            .push_follow_up(QueuedAgentMessage::from_authored_message(
                build_user_message("follow-up-overflow", &[]),
            ))
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
                .push_follow_up(QueuedAgentMessage::from_authored_message(
                    build_user_message(&format!("follow-up-{idx}"), &[]),
                ))
                .expect("follow-up enqueue within total cap");
        }

        let err = shared
            .push_steering(QueuedAgentMessage::from_authored_message(
                build_user_message("steer-overflow", &[]),
            ))
            .expect_err("steering enqueue should respect total pending cap");
        assert!(matches!(err, Error::Session(_)));
        assert_eq!(shared.pending_count(), MAX_RPC_PENDING_MESSAGES);
    }

    #[test]
    fn session_transition_blocker_rejects_private_staged_follow_up_after_task_loss() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session = Arc::new(asupersync::sync::Mutex::new(build_test_agent_session(
                Session::in_memory(),
            )));
            let cx = AgentCx::for_request();
            session
                .lock(&cx)
                .await
                .expect("agent session lock")
                .agent
                .queue_follow_up(build_user_message("old-session follow-up", &[]));
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let bash_state = Arc::new(asupersync::sync::Mutex::new(None));
            let reason = rpc_session_transition_blocker(
                &AtomicBool::new(false),
                &AtomicBool::new(false),
                &std::sync::Mutex::new(()),
                &session,
                &shared_state,
                &bash_state,
                &cx,
            )
            .await
            .expect("transition blocker");
            assert_eq!(
                reason,
                Some("An accepted follow-up is still pending; resume it before changing sessions")
            );
        });
    }

    #[test]
    fn session_transition_blocker_rejects_acknowledged_shared_input() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session = Arc::new(asupersync::sync::Mutex::new(build_test_agent_session(
                Session::in_memory(),
            )));
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let bash_state = Arc::new(asupersync::sync::Mutex::new(None));
            let cx = AgentCx::for_request();
            shared_state
                .lock(&cx)
                .await
                .expect("shared state lock")
                .push_follow_up(QueuedAgentMessage::from_authored_message(
                    build_user_message("acknowledged shared follow-up", &[]),
                ))
                .expect("queue shared follow-up");

            let reason = rpc_session_transition_blocker(
                &AtomicBool::new(false),
                &AtomicBool::new(false),
                &std::sync::Mutex::new(()),
                &session,
                &shared_state,
                &bash_state,
                &cx,
            )
            .await
            .expect("transition blocker");
            assert_eq!(
                reason,
                Some(
                    "Acknowledged RPC input is still pending; resume or persist it before changing sessions"
                )
            );
        });
    }

    #[test]
    fn post_hook_transition_recheck_rejects_source_session_mutation() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session = Arc::new(asupersync::sync::Mutex::new(build_test_agent_session(
                Session::in_memory(),
            )));
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let bash_state = Arc::new(asupersync::sync::Mutex::new(None));
            let cx = AgentCx::for_request();
            let baseline = rpc_session_transition_snapshot(&session, &cx)
                .await
                .expect("transition baseline");
            {
                let guard = session.lock(&cx).await.expect("agent session lock");
                let mut inner = guard.session.lock(&cx).await.expect("inner session lock");
                inner.append_custom_entry(
                    "hook-action".to_string(),
                    Some(json!({"owner": "source-session"})),
                );
            }

            let Err(err) = acquire_rpc_session_transition(
                &baseline,
                &AtomicBool::new(false),
                &AtomicBool::new(false),
                &std::sync::Mutex::new(()),
                &session,
                &shared_state,
                &bash_state,
                &cx,
            )
            .await
            else {
                panic!("source mutation must reject the transition")
            };
            assert!(
                err.to_string().contains("modified the source Session"),
                "unexpected transition error: {err}"
            );

            let guard = session.lock(&cx).await.expect("agent session lock");
            let inner = guard.session.lock(&cx).await.expect("inner session lock");
            assert!(inner.entries_for_current_path().iter().any(|entry| {
                matches!(
                    entry,
                    SessionEntry::Custom(custom) if custom.custom_type == "hook-action"
                )
            }));
        });
    }

    #[test]
    fn session_transition_waits_for_outer_before_taking_action_admission() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session = Arc::new(asupersync::sync::Mutex::new(build_test_agent_session(
                Session::in_memory(),
            )));
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let bash_state = Arc::new(asupersync::sync::Mutex::new(None));
            let setup_cx = AgentCx::for_request();
            let baseline = rpc_session_transition_snapshot(&session, &setup_cx)
                .await
                .expect("transition baseline");
            let session_action_admission = session
                .lock(&setup_cx)
                .await
                .expect("agent session lock")
                .session_action_admission_gate();
            let held_session = session
                .lock(&setup_cx)
                .await
                .expect("hold outer session lock");

            let transition_cx = AgentCx::for_request();
            let turn_in_progress = AtomicBool::new(false);
            let prompt_or_tool_in_progress = AtomicBool::new(false);
            let turn_phase_linearizer = std::sync::Mutex::new(());
            let mut transition = Box::pin(acquire_rpc_session_transition(
                &baseline,
                &turn_in_progress,
                &prompt_or_tool_in_progress,
                &turn_phase_linearizer,
                &session,
                &shared_state,
                &bash_state,
                &transition_cx,
            ));
            assert!(matches!(
                futures::poll!(transition.as_mut()),
                std::task::Poll::Pending
            ));
            assert_eq!(
                session.waiters(),
                1,
                "the transition must block first on the outer AgentSession"
            );

            let action_permit = asupersync::time::timeout(
                wall_now(),
                Duration::from_secs(1),
                Box::pin(session_action_admission.acquire(setup_cx.cx())),
            )
            .await
            .expect("transition took session-action admission before the outer lock")
            .expect("session-action admission lock");
            drop(action_permit);
            drop(held_session);

            let authority =
                asupersync::time::timeout(wall_now(), Duration::from_secs(5), transition)
                    .await
                    .expect("transition did not resume after the outer lock was released")
                    .expect("transition authority");
            drop(authority);
        });
    }

    #[test]
    fn session_transition_waits_for_provider_before_taking_action_admission() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session = Arc::new(asupersync::sync::Mutex::new(build_test_agent_session(
                Session::in_memory(),
            )));
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let bash_state = Arc::new(asupersync::sync::Mutex::new(None));
            let setup_cx = AgentCx::for_request();
            let baseline = rpc_session_transition_snapshot(&session, &setup_cx)
                .await
                .expect("transition baseline");
            let (provider_admission, session_action_admission) = {
                let guard = session.lock(&setup_cx).await.expect("agent session lock");
                (
                    guard.provider_admission_gate(),
                    guard.session_action_admission_gate(),
                )
            };
            let provider_permit = provider_admission
                .acquire(setup_cx.cx())
                .await
                .expect("hold provider admission");

            let transition_cx = AgentCx::for_request();
            let turn_in_progress = AtomicBool::new(false);
            let prompt_or_tool_in_progress = AtomicBool::new(false);
            let turn_phase_linearizer = std::sync::Mutex::new(());
            let mut transition = Box::pin(acquire_rpc_session_transition(
                &baseline,
                &turn_in_progress,
                &prompt_or_tool_in_progress,
                &turn_phase_linearizer,
                &session,
                &shared_state,
                &bash_state,
                &transition_cx,
            ));
            assert!(matches!(
                futures::poll!(transition.as_mut()),
                std::task::Poll::Pending
            ));
            assert!(
                session.is_locked(),
                "the transition must hold the outer AgentSession while awaiting provider admission"
            );

            let action_permit = asupersync::time::timeout(
                wall_now(),
                Duration::from_secs(1),
                Box::pin(session_action_admission.acquire(setup_cx.cx())),
            )
            .await
            .expect("transition took session-action admission before provider admission")
            .expect("session-action admission lock");
            drop(action_permit);
            drop(provider_permit);

            let authority =
                asupersync::time::timeout(wall_now(), Duration::from_secs(5), transition)
                    .await
                    .expect("transition did not resume after provider admission was released")
                    .expect("transition authority");
            drop(authority);
        });
    }

    #[test]
    fn session_transition_checks_bash_before_taking_outer_authority() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session = Arc::new(asupersync::sync::Mutex::new(build_test_agent_session(
                Session::in_memory(),
            )));
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let bash_state = Arc::new(asupersync::sync::Mutex::new(None));
            let setup_cx = AgentCx::for_request();
            let baseline = rpc_session_transition_snapshot(&session, &setup_cx)
                .await
                .expect("transition baseline");
            let held_bash_state = bash_state
                .lock(&setup_cx)
                .await
                .expect("hold bash state lock");

            let transition_cx = AgentCx::for_request();
            let turn_in_progress = AtomicBool::new(false);
            let prompt_or_tool_in_progress = AtomicBool::new(false);
            let turn_phase_linearizer = std::sync::Mutex::new(());
            let mut transition = Box::pin(acquire_rpc_session_transition(
                &baseline,
                &turn_in_progress,
                &prompt_or_tool_in_progress,
                &turn_phase_linearizer,
                &session,
                &shared_state,
                &bash_state,
                &transition_cx,
            ));
            assert!(matches!(
                futures::poll!(transition.as_mut()),
                std::task::Poll::Pending
            ));
            assert_eq!(
                bash_state.waiters(),
                1,
                "the transition must block first on the bash-state precheck"
            );
            assert!(
                !session.is_locked(),
                "the transition must not take outer AgentSession authority while bash-state is unavailable"
            );
            drop(held_bash_state);

            let authority =
                asupersync::time::timeout(wall_now(), Duration::from_secs(5), transition)
                    .await
                    .expect("transition did not resume after bash-state was released")
                    .expect("transition authority");
            drop(authority);
        });
    }

    #[test]
    fn post_hook_transition_recheck_rejects_header_only_source_mutation() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let mut source = Session::in_memory();
            source.append_model_change("test-provider".to_string(), "test-model".to_string());
            source.set_model_header(
                Some("stale-provider".to_string()),
                Some("stale-model".to_string()),
                None,
            );
            let session = Arc::new(asupersync::sync::Mutex::new(build_test_agent_session(
                source,
            )));
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let bash_state = Arc::new(asupersync::sync::Mutex::new(None));
            let cx = AgentCx::for_request();
            let baseline = rpc_session_transition_snapshot(&session, &cx)
                .await
                .expect("transition baseline");
            {
                let guard = session.lock(&cx).await.expect("agent session lock");
                let mut inner = guard.session.lock(&cx).await.expect("inner session lock");
                inner.set_model_header(
                    Some("test-provider".to_string()),
                    Some("test-model".to_string()),
                    None,
                );
                assert_eq!(inner.entries.len(), baseline.entry_count);
                assert_eq!(inner.leaf_id(), baseline.leaf_id.as_deref());
            }

            let Err(err) = acquire_rpc_session_transition(
                &baseline,
                &AtomicBool::new(false),
                &AtomicBool::new(false),
                &std::sync::Mutex::new(()),
                &session,
                &shared_state,
                &bash_state,
                &cx,
            )
            .await
            else {
                panic!("header-only source mutation must reject the transition")
            };
            assert!(
                err.to_string().contains("modified the source Session"),
                "unexpected transition error: {err}"
            );
        });
    }

    #[test]
    fn bash_abort_keeps_running_state_until_worker_finalization() {
        let cx = asupersync::Cx::for_testing();
        let (abort_tx, mut abort_rx) = oneshot::channel();
        let mut running = Some(RunningBash {
            id: "bash-abort-transition-guard".to_string(),
            abort_tx: Some(abort_tx),
        });

        running
            .as_mut()
            .expect("running bash state")
            .request_abort(&cx);

        assert!(
            running.is_some(),
            "abort acknowledgement must not reopen session transitions before the worker finalizes"
        );
        assert!(
            running
                .as_ref()
                .expect("running bash state after abort")
                .abort_tx
                .is_none(),
            "the abort sender must be consumed exactly once"
        );
        assert_eq!(abort_rx.try_recv(), Ok(()));

        running
            .as_mut()
            .expect("running bash state after repeated abort")
            .request_abort(&cx);
        assert!(running.is_some(), "repeated abort must retain the blocker");
    }

    #[test]
    fn terminal_queue_preservation_drains_and_records_acknowledged_input_exactly_once() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session = Arc::new(asupersync::sync::Mutex::new(build_test_agent_session(
                Session::in_memory(),
            )));
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let cx = AgentCx::for_request();
            {
                let mut state = shared_state.lock(&cx).await.expect("shared state lock");
                state
                    .push_steering(QueuedAgentMessage::from_authored_message(
                        build_user_message("accepted steering first", &[]),
                    ))
                    .expect("queue steering");
                assert_eq!(state.lease_steering(0).len(), 1);
                state
                    .push_follow_up(QueuedAgentMessage::from_authored_message(
                        build_user_message("accepted follow-up", &[]),
                    ))
                    .expect("queue follow-up");
                assert_eq!(state.lease_follow_up_for_fetch(0).len(), 1);
                state
                    .push_steering(QueuedAgentMessage::from_authored_message(
                        build_user_message("accepted steering last", &[]),
                    ))
                    .expect("queue later steering");
                assert_eq!(state.lease_steering(0).len(), 1);
                assert_eq!(
                    state.pending_count(),
                    3,
                    "leased input must remain authoritative before preservation"
                );
            }

            let preserved = preserve_terminal_rpc_input(&session, &shared_state, &cx)
                .await
                .expect("preserve terminal input");
            assert_eq!(preserved, 3);
            assert_eq!(
                shared_state
                    .lock(&cx)
                    .await
                    .expect("shared state lock")
                    .pending_count(),
                0
            );

            let guard = session.lock(&cx).await.expect("agent session lock");
            let inner = guard.session.lock(&cx).await.expect("session lock");
            let user_text = inner
                .entries_for_current_path()
                .iter()
                .filter_map(|entry| match entry {
                    crate::session::SessionEntry::Message(message) => match &message.message {
                        SessionMessage::User {
                            content: UserContent::Text(text),
                            ..
                        } => Some(text.as_str()),
                        _ => None,
                    },
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                user_text,
                vec![
                    "accepted steering first",
                    "accepted follow-up",
                    "accepted steering last"
                ]
            );
        });
    }

    #[test]
    fn terminal_recovery_preserves_completed_tool_effects_after_task_drop() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let base = build_user_message("persisted prompt", &[]);
            let live_base = base.clone();
            let mut inner = Session::in_memory();
            inner.append_model_message(base.clone());
            let assistant = Message::Assistant(Arc::new(AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "tool-1".to_string(),
                    name: "write".to_string(),
                    arguments: json!({"path": "changed.txt"}),
                    thought_signature: None,
                })],
                stop_reason: StopReason::ToolUse,
                ..AssistantMessage::default()
            }));
            let tool_result = Message::ToolResult(Arc::new(crate::model::ToolResultMessage {
                tool_call_id: "tool-1".to_string(),
                tool_name: "write".to_string(),
                content: vec![ContentBlock::Text(TextContent::new("wrote changed.txt"))],
                details: None,
                is_error: false,
                timestamp: 2,
            }));
            let partial_after_effect = Message::Assistant(Arc::new(AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("unfinished response"))],
                ..AssistantMessage::default()
            }));
            let mut agent_session = build_test_agent_session(inner);
            agent_session.agent.replace_messages(vec![
                live_base,
                assistant.clone(),
                tool_result.clone(),
                partial_after_effect,
            ]);
            let session = Arc::new(asupersync::sync::Mutex::new(agent_session));
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let cx = AgentCx::for_request();

            assert_eq!(
                terminal_rpc_recovery_plan(&session, &shared_state, true, &cx)
                    .await
                    .expect("terminal recovery decision"),
                RpcTerminalRecoveryPlan::RecordedToolTranscript { recovery_count: 2 },
                "a closed assistant/tool-result pair is terminal recovery work"
            );
            assert_eq!(
                preserve_terminal_rpc_input(&session, &shared_state, &cx)
                    .await
                    .expect("preserve completed tool transcript"),
                2
            );

            let guard = session.lock(&cx).await.expect("agent session lock");
            let persisted = guard
                .session
                .lock(&cx)
                .await
                .expect("inner session lock")
                .to_messages_for_current_path();
            assert_eq!(persisted.len(), 3);
            assert_eq!(
                serde_json::to_value(&persisted[1]).expect("assistant json"),
                serde_json::to_value(assistant).expect("expected assistant json")
            );
            assert_eq!(
                serde_json::to_value(&persisted[2]).expect("tool result json"),
                serde_json::to_value(tool_result).expect("expected tool result json")
            );
            assert!(persisted.iter().all(|message| {
                !matches!(
                    message,
                    Message::Assistant(assistant)
                        if assistant.content.iter().any(|block| {
                            matches!(block, ContentBlock::Text(text) if text.text == "unfinished response")
                        })
                )
            }));
        });
    }

    #[test]
    fn terminal_recovery_refuses_an_incomplete_multi_tool_cycle() {
        let session = Session::in_memory();
        let assistant = Message::Assistant(Arc::new(AssistantMessage {
            content: vec![
                ContentBlock::ToolCall(ToolCall {
                    id: "tool-a".to_string(),
                    name: "write".to_string(),
                    arguments: json!({"path": "a.txt"}),
                    thought_signature: None,
                }),
                ContentBlock::ToolCall(ToolCall {
                    id: "tool-b".to_string(),
                    name: "write".to_string(),
                    arguments: json!({"path": "b.txt"}),
                    thought_signature: None,
                }),
            ],
            stop_reason: StopReason::ToolUse,
            ..AssistantMessage::default()
        }));
        let only_first_result = Message::ToolResult(Arc::new(crate::model::ToolResultMessage {
            tool_call_id: "tool-a".to_string(),
            tool_name: "write".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("wrote a.txt"))],
            details: None,
            is_error: false,
            timestamp: 2,
        }));

        assert!(
            completed_live_tool_effect_suffix(&session, &[assistant, only_first_result])
                .expect("inspect incomplete tool cycle")
                .is_empty(),
            "a partial multi-tool batch must not be recorded as a closed effect cycle"
        );
    }

    #[test]
    fn terminal_recovery_accepts_a_fully_completed_multi_tool_cycle() {
        let session = Session::in_memory();
        let assistant = Message::Assistant(Arc::new(AssistantMessage {
            content: vec![
                ContentBlock::ToolCall(ToolCall {
                    id: "tool-a".to_string(),
                    name: "write".to_string(),
                    arguments: json!({"path": "a.txt"}),
                    thought_signature: None,
                }),
                ContentBlock::ToolCall(ToolCall {
                    id: "tool-b".to_string(),
                    name: "write".to_string(),
                    arguments: json!({"path": "b.txt"}),
                    thought_signature: None,
                }),
            ],
            stop_reason: StopReason::ToolUse,
            ..AssistantMessage::default()
        }));
        let first_result = Message::ToolResult(Arc::new(crate::model::ToolResultMessage {
            tool_call_id: "tool-a".to_string(),
            tool_name: "write".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("wrote a.txt"))],
            details: None,
            is_error: false,
            timestamp: 2,
        }));
        let second_result = Message::ToolResult(Arc::new(crate::model::ToolResultMessage {
            tool_call_id: "tool-b".to_string(),
            tool_name: "write".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("wrote b.txt"))],
            details: None,
            is_error: false,
            timestamp: 3,
        }));
        let live = vec![assistant, first_result, second_result];

        let recovered = completed_live_tool_effect_suffix(&session, &live)
            .expect("inspect complete multi-tool cycle");
        assert_eq!(
            serde_json::to_value(recovered).expect("serialize recovered tool cycle"),
            serde_json::to_value(live).expect("serialize expected tool cycle"),
            "the whole batch becomes recoverable only after every tool result is recorded"
        );
    }

    #[test]
    fn terminal_recovery_ignores_pause_turn_server_calls_before_local_tool_effects() {
        let session = Session::in_memory();
        let server_tool_pause = Message::Assistant(Arc::new(AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "server-tool".to_string(),
                name: "web_search".to_string(),
                arguments: json!({"query": "status"}),
                thought_signature: None,
            })],
            stop_reason: StopReason::PauseTurn,
            ..AssistantMessage::default()
        }));
        let local_tool_call = Message::Assistant(Arc::new(AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "local-tool".to_string(),
                name: "write".to_string(),
                arguments: json!({"path": "result.txt"}),
                thought_signature: None,
            })],
            stop_reason: StopReason::ToolUse,
            ..AssistantMessage::default()
        }));
        let local_result = Message::ToolResult(Arc::new(crate::model::ToolResultMessage {
            tool_call_id: "local-tool".to_string(),
            tool_name: "write".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("wrote result.txt"))],
            details: None,
            is_error: false,
            timestamp: 3,
        }));
        let live = vec![server_tool_pause, local_tool_call, local_result];

        let recovered = completed_live_tool_effect_suffix(&session, &live)
            .expect("inspect PauseTurn followed by local tool cycle");
        assert_eq!(
            serde_json::to_value(recovered).expect("serialize recovered transcript"),
            serde_json::to_value(live).expect("serialize expected transcript"),
            "server-managed PauseTurn calls must not hide a later completed local tool effect"
        );
    }

    #[test]
    fn terminal_recovery_requires_authored_timestamps_but_accepts_synthetic_legacy_timestamps() {
        let mut authored_session = Session::in_memory();
        authored_session.append_model_message(Message::User(UserMessage {
            content: UserContent::Text("same authored content".to_string()),
            timestamp: 1,
        }));
        let authored_live = [Message::User(UserMessage {
            content: UserContent::Text("same authored content".to_string()),
            timestamp: 2,
        })];
        let authored_error = completed_live_tool_effect_suffix(&authored_session, &authored_live)
            .expect_err("different authored timestamps must prove prefix divergence");
        assert!(authored_error.to_string().contains("diverged"));

        let mut legacy_session = Session::in_memory();
        legacy_session.append_message(SessionMessage::User {
            content: UserContent::Text("legacy content".to_string()),
            timestamp: None,
        });
        let mut legacy_live = legacy_session.to_messages_for_current_path();
        let Message::User(user) = &mut legacy_live[0] else {
            panic!("expected projected legacy user message");
        };
        user.timestamp = user.timestamp.saturating_add(1_000);

        assert!(
            completed_live_tool_effect_suffix(&legacy_session, &legacy_live)
                .expect("synthetic legacy timestamp must not break the prefix")
                .is_empty(),
            "a reconciled transcript with no live suffix has nothing to recover"
        );
    }

    #[test]
    fn terminal_preservation_keeps_input_authoritative_on_prefix_divergence() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let persisted = Message::User(UserMessage {
                content: UserContent::Text("same content".to_string()),
                timestamp: 1,
            });
            let live = Message::User(UserMessage {
                content: UserContent::Text("same content".to_string()),
                timestamp: 2,
            });
            let mut inner = Session::in_memory();
            inner.append_model_message(persisted);
            let mut agent_session = build_test_agent_session(inner);
            agent_session.agent.replace_messages(vec![live]);
            let session = Arc::new(asupersync::sync::Mutex::new(agent_session));
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let cx = AgentCx::for_request();
            shared_state
                .lock(&cx)
                .await
                .expect("shared state lock")
                .push_steering(QueuedAgentMessage::from_authored_message(
                    build_user_message("must remain pending", &[]),
                ))
                .expect("queue steering");

            let error = preserve_terminal_rpc_input(&session, &shared_state, &cx)
                .await
                .expect_err("prefix divergence must block terminal preservation");
            assert!(error.to_string().contains("diverged"));
            assert_eq!(
                shared_state
                    .lock(&cx)
                    .await
                    .expect("shared state after rejected preservation")
                    .pending_count(),
                1,
                "rejected preservation must not clear authoritative RPC input"
            );
            let guard = session.lock(&cx).await.expect("agent session lock");
            let Message::User(user) = &guard.agent.messages()[0] else {
                panic!("expected divergent live user message");
            };
            assert_eq!(user.timestamp, 2, "live Agent state must remain untouched");
        });
    }

    #[test]
    fn staged_tool_transcript_recovery_rejects_session_base_drift() {
        let mut session = Session::in_memory();
        session.append_model_message(build_user_message("base", &[]));
        let recorded = vec![Message::ToolResult(Arc::new(
            crate::model::ToolResultMessage {
                tool_call_id: "tool-1".to_string(),
                tool_name: "write".to_string(),
                content: vec![ContentBlock::Text(TextContent::new("done"))],
                details: None,
                is_error: false,
                timestamp: 2,
            },
        ))];
        let mut state = RpcSharedState::new(&Config::default());
        state
            .stage_completed_tool_transcript(&session, &recorded)
            .expect("stage recovery transcript");
        session.append_custom_entry("drift".to_string(), Some(json!({"changed": true})));

        let err = state
            .stage_completed_tool_transcript(&session, &recorded)
            .expect_err("session-base drift must fail closed");
        assert!(err.to_string().contains("session base changed"));
    }

    #[test]
    fn terminal_preservation_recognizes_in_flight_input_already_in_live_session() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session = Arc::new(asupersync::sync::Mutex::new(build_test_agent_session(
                Session::in_memory(),
            )));
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let cx = AgentCx::for_request();
            let delivered = {
                let mut state = shared_state.lock(&cx).await.expect("shared state lock");
                state
                    .push_steering(QueuedAgentMessage::from_authored_message(
                        build_user_message("already appended in-flight", &[]),
                    ))
                    .expect("queue steering");
                state
                    .lease_steering(0)
                    .into_iter()
                    .next()
                    .expect("leased delivery")
            };
            {
                let guard = session.lock(&cx).await.expect("agent session lock");
                let mut inner = guard.session.lock(&cx).await.expect("inner session lock");
                inner.append_model_message(delivered.into_message());
            }

            let preserved = preserve_terminal_rpc_input(&session, &shared_state, &cx)
                .await
                .expect("preserve represented in-flight input");
            assert_eq!(preserved, 1);
            assert_eq!(
                shared_state
                    .lock(&cx)
                    .await
                    .expect("shared state lock")
                    .pending_count(),
                0
            );

            let guard = session.lock(&cx).await.expect("agent session lock");
            let inner = guard.session.lock(&cx).await.expect("inner session lock");
            let represented_count = inner
                .to_messages_for_current_path()
                .iter()
                .filter(|message| {
                    matches!(
                        message,
                        Message::User(UserMessage {
                            content: UserContent::Text(text),
                            ..
                        }) if text == "already appended in-flight"
                    )
                })
                .count();
            assert_eq!(
                represented_count, 1,
                "terminal recovery must not duplicate an Agent-appended delivery"
            );
        });
    }

    #[test]
    fn normal_rpc_acknowledgement_flushes_represented_input_before_release() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let inner_session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
                Some(temp.path().join("sessions")),
            )));
            let provider: Arc<dyn Provider> = Arc::new(NoopProvider);
            let agent = Agent::new(
                provider,
                ToolRegistry::new(&[], Path::new("."), None),
                AgentConfig::default(),
            );
            let session = Arc::new(asupersync::sync::Mutex::new(AgentSession::new(
                agent,
                Arc::clone(&inner_session),
                true,
                crate::compaction::ResolvedCompactionSettings::default(),
            )));
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let cx = AgentCx::for_request();
            let delivered = {
                let mut state = shared_state.lock(&cx).await.expect("shared state lock");
                state
                    .push_steering(QueuedAgentMessage::from_authored_message(
                        build_user_message("durable normal acknowledgement", &[]),
                    ))
                    .expect("queue steering");
                state
                    .lease_steering(0)
                    .into_iter()
                    .next()
                    .expect("leased delivery")
            };
            inner_session
                .lock(&cx)
                .await
                .expect("inner session lock")
                .append_model_message(delivered.message().clone());

            let acknowledged = acknowledge_durable_rpc_in_flight(&session, &shared_state, &cx)
                .await
                .expect("durably acknowledge represented input");
            assert_eq!(acknowledged, 1);
            assert_eq!(
                shared_state
                    .lock(&cx)
                    .await
                    .expect("shared state lock")
                    .pending_count(),
                0
            );

            let session_path = inner_session
                .lock(&cx)
                .await
                .expect("inner session lock")
                .path
                .clone()
                .expect("manual acknowledgement flush must create a session file");
            let reopened = Session::open(session_path.to_string_lossy().as_ref())
                .await
                .expect("reopen durably acknowledged session");
            let represented_count = reopened
                .to_messages_for_current_path()
                .iter()
                .filter(|message| {
                    matches!(
                        message,
                        Message::User(UserMessage {
                            content: UserContent::Text(text),
                            ..
                        }) if text == "durable normal acknowledgement"
                    )
                })
                .count();
            assert_eq!(represented_count, 1);
        });
    }

    fn assert_terminal_queue_persistence_replay_reuses_stable_entry_identity(
        store_kind: crate::session::SessionStoreKind,
    ) {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let mut live =
                Session::create_with_dir_and_store(Some(temp.path().join("sessions")), store_kind);
            live.append_model_message(build_user_message("durable base", &[]));
            live.save().await.expect("persist replay base entry");
            let session_path = live.path.clone().expect("pinned session path");
            assert_eq!(live.entries.len(), 1, "replay base must be durable");
            let queued = [
                QueuedAgentMessage::from_authored_message(build_user_message(
                    "cancelled-writer steering",
                    &[],
                )),
                QueuedAgentMessage::from_authored_message(build_user_message(
                    "cancelled-writer follow-up",
                    &[],
                )),
            ];

            let append_queued =
                |candidate: &mut Session, queued: &[QueuedAgentMessage]| -> Result<()> {
                    let mut parent_id = candidate.leaf_id().map(str::to_string);
                    for delivery in queued {
                        let message = delivery.message().clone();
                        let (entry_id, timestamp, bound_parent_id) =
                            delivery.bind_persistence_identity(parent_id.take());
                        parent_id = Some(entry_id.clone());
                        candidate.append_model_message_with_identity(
                            message,
                            &entry_id,
                            &timestamp,
                            bound_parent_id.as_deref(),
                        )?;
                    }
                    Ok(())
                };

            let mut cancelled_candidate = live.clone();
            append_queued(&mut cancelled_candidate, &queued).expect("prepare first candidate");
            cancelled_candidate
                .flush_autosave(AutosaveFlushTrigger::Periodic)
                .await
                .expect("simulate writer reaching disk before cancellation");
            drop(cancelled_candidate);

            live.append_model_message(build_user_message("later live branch", &[]));
            live.flush_autosave(AutosaveFlushTrigger::Periodic)
                .await
                .expect("advance and reconcile live session before retry");

            let mut retry_candidate = live.clone();
            append_queued(&mut retry_candidate, &queued).expect("prepare retry candidate");
            retry_candidate
                .flush_autosave(AutosaveFlushTrigger::Periodic)
                .await
                .expect("retry identical durable append");

            let reopened = Session::open(session_path.to_string_lossy().as_ref())
                .await
                .expect("reopen replayed session");
            assert_eq!(
                reopened.entries.len(),
                4,
                "cancelled-writer replay must not retain a duplicate durable branch"
            );
            let later_branch_count = reopened
                .entries
                .iter()
                .filter(|entry| {
                    matches!(
                        entry,
                        crate::session::SessionEntry::Message(message)
                            if matches!(
                                &message.message,
                                SessionMessage::User {
                                    content: UserContent::Text(text),
                                    ..
                                } if text == "later live branch"
                            )
                    )
                })
                .count();
            assert_eq!(later_branch_count, 1, "later live branch must be retained");
            for expected in ["cancelled-writer steering", "cancelled-writer follow-up"] {
                let count = reopened
                    .to_messages_for_current_path()
                    .iter()
                    .filter(|message| {
                        matches!(
                            message,
                            Message::User(UserMessage {
                                content: UserContent::Text(text),
                                ..
                            }) if text == expected
                        )
                    })
                    .count();
                assert_eq!(count, 1, "{expected} must be durable exactly once");
            }
        });
    }

    #[test]
    fn terminal_jsonl_queue_replay_reuses_stable_entry_identity() {
        assert_terminal_queue_persistence_replay_reuses_stable_entry_identity(
            crate::session::SessionStoreKind::Jsonl,
        );
    }

    #[cfg(feature = "sqlite-sessions")]
    #[test]
    fn terminal_sqlite_queue_replay_reuses_stable_entry_identity() {
        assert_terminal_queue_persistence_replay_reuses_stable_entry_identity(
            crate::session::SessionStoreKind::Sqlite,
        );
    }

    #[test]
    fn terminal_queue_preservation_rolls_back_after_candidate_flush_failure() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let blocked_root = tempfile::tempdir().expect("tempdir");
            let mut session_value = Session::in_memory();
            // A non-file persistence target skips the preliminary path-pinning
            // branch, then fails only after the private candidate has appended
            // both accepted deliveries.
            session_value.path = Some(blocked_root.path().to_path_buf());
            let inner_session = Arc::new(asupersync::sync::Mutex::new(session_value));
            let provider: Arc<dyn Provider> = Arc::new(NoopProvider);
            let agent = Agent::new(
                provider,
                ToolRegistry::new(&[], Path::new("."), None),
                AgentConfig::default(),
            );
            let agent_session = AgentSession::new(
                agent,
                Arc::clone(&inner_session),
                true,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            let session = Arc::new(asupersync::sync::Mutex::new(agent_session));
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let cx = AgentCx::for_request();
            {
                let mut state = shared_state.lock(&cx).await.expect("shared state lock");
                state
                    .push_steering(QueuedAgentMessage::from_authored_message(
                        build_user_message("failed-flush steering", &[]),
                    ))
                    .expect("queue steering");
                state
                    .push_follow_up(QueuedAgentMessage::from_authored_message(
                        build_user_message("failed-flush follow-up", &[]),
                    ))
                    .expect("queue follow-up");
            }

            let err = preserve_terminal_rpc_input(&session, &shared_state, &cx)
                .await
                .expect_err("candidate flush to a directory must fail persistence");
            let err_text = err.to_string();
            assert!(
                err_text.contains("directory")
                    || err_text.contains("regular file")
                    || err_text.contains("Failed"),
                "unexpected persistence error: {err}"
            );

            let state = shared_state.lock(&cx).await.expect("restored queues");
            assert_eq!(state.pending_count(), 2);
            assert!(matches!(
                state.steering.front().map(QueuedAgentMessage::message),
                Some(Message::User(UserMessage {
                    content: UserContent::Text(text),
                    ..
                })) if text == "failed-flush steering"
            ));
            assert!(matches!(
                state.follow_up.front().map(QueuedAgentMessage::message),
                Some(Message::User(UserMessage {
                    content: UserContent::Text(text),
                    ..
                })) if text == "failed-flush follow-up"
            ));
            drop(state);

            let inner = inner_session
                .lock(&cx)
                .await
                .expect("rolled-back inner session");
            assert!(
                inner.entries.is_empty(),
                "failed candidate must not leave orphan entries"
            );
            assert_eq!(
                inner.autosave_metrics().pending_mutations,
                0,
                "failed candidate must not contaminate the live autosave queue"
            );
            assert!(
                inner.to_messages_for_current_path().iter().all(|message| {
                    !matches!(
                        message,
                        Message::User(UserMessage {
                            content: UserContent::Text(text),
                            ..
                        }) if text.starts_with("failed-flush")
                    )
                }),
                "failed durable append must not remain on the active in-memory path"
            );
        });
    }

    #[test]
    fn terminal_queue_preservation_keeps_input_queued_during_cancelled_session_lock() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let session = Arc::new(asupersync::sync::Mutex::new(build_test_agent_session(
                Session::in_memory(),
            )));
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
            let setup_cx = AgentCx::for_request();
            {
                let mut state = shared_state
                    .lock(&setup_cx)
                    .await
                    .expect("shared state lock");
                state
                    .push_steering(QueuedAgentMessage::from_authored_message(
                        build_user_message("restore steering", &[]),
                    ))
                    .expect("queue steering");
                state
                    .push_follow_up(QueuedAgentMessage::from_authored_message(
                        build_user_message("restore follow-up", &[]),
                    ))
                    .expect("queue follow-up");
            }

            let held_session = session
                .lock(&setup_cx)
                .await
                .expect("hold outer session lock");
            let operation_cx = AgentCx::for_testing();
            let cancel_cx = operation_cx.clone();
            let mut preserve_task = Box::pin(preserve_terminal_rpc_input(
                &session,
                &shared_state,
                &operation_cx,
            ));
            assert!(matches!(
                futures::poll!(preserve_task.as_mut()),
                std::task::Poll::Pending
            ));
            assert_eq!(
                shared_state
                    .lock(&setup_cx)
                    .await
                    .expect("observe shared state while session is contended")
                    .pending_count(),
                2,
                "accepted inputs must remain authoritative while session locking awaits"
            );
            cancel_cx.set_cancel_requested(true);

            let result =
                match asupersync::time::timeout(wall_now(), Duration::from_secs(5), preserve_task)
                    .await
                {
                    Ok(result) => result,
                    Err(timeout_err) => {
                        drop(held_session);
                        panic!("cancelled session-lock waiter did not finish: {timeout_err}");
                    }
                };
            drop(held_session);
            assert!(
                result
                    .expect_err("cancelled outer session lock must fail")
                    .to_string()
                    .contains("session lock failed")
            );

            let state = shared_state
                .lock(&setup_cx)
                .await
                .expect("restored shared state");
            assert_eq!(state.pending_count(), 2);
            assert!(matches!(
                state.steering.front().map(QueuedAgentMessage::message),
                Some(Message::User(UserMessage {
                    content: UserContent::Text(text),
                    ..
                })) if text == "restore steering"
            ));
            assert!(matches!(
                state.follow_up.front().map(QueuedAgentMessage::message),
                Some(Message::User(UserMessage {
                    content: UserContent::Text(text),
                    ..
                })) if text == "restore follow-up"
            ));
        });
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
                mode: None,
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
                ask_tool: None,
            };

            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(16);
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &options.config,
            )));
            maybe_auto_compact(
                Arc::new(asupersync::sync::Mutex::new(agent_session)),
                shared_state,
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

    #[test]
    fn auto_compaction_save_disabled_reports_in_memory_result() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 1)
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let (events, session, metrics_before, _provider_admission) =
                run_auto_compaction_persistence_case(Session::in_memory(), false, runtime_handle)
                    .await;
            let end = events
                .iter()
                .find(|event| event["type"] == "auto_compaction_end")
                .expect("auto_compaction_end");
            assert_eq!(end["result"]["persisted"], false);
            assert_eq!(end["result"]["summary"], "causal auto-compaction summary");
            assert_eq!(
                end["result"]["persistenceStatus"]["event"],
                "session.persistence.disabled"
            );
            assert!(
                end.get("errorMessage").is_none(),
                "in-memory compaction is successful but explicitly non-durable: {end}"
            );

            let cx = asupersync::Cx::for_testing();
            let session = session.lock(&cx).await.expect("lock compacted session");
            assert_eq!(
                provider_compaction_count(&session),
                1,
                "the reported result must correspond to the provider-backed compaction mutation"
            );
            let metrics_after = session.autosave_metrics();
            assert_eq!(
                metrics_after.pending_mutations,
                metrics_before
                    .pending_mutations
                    .saturating_add(1)
                    .min(metrics_before.max_pending_mutations),
                "disabled persistence must retain the new compaction mutation"
            );
            assert_eq!(
                metrics_after.coalesced_mutations,
                metrics_before.coalesced_mutations + 1,
                "compaction must enqueue exactly one new autosave mutation"
            );
            assert_eq!(
                metrics_after.flush_started, metrics_before.flush_started,
                "disabled persistence must not claim a flush attempt"
            );
        });
    }

    #[test]
    fn auto_compaction_persistence_success_reopens_provider_summary() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 1)
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let session_root = tempfile::tempdir().expect("tempdir");
            let session = Session::create_with_dir(Some(session_root.path().to_path_buf()));
            let (events, session, metrics_before, _provider_admission) =
                run_auto_compaction_persistence_case(session, true, runtime_handle).await;
            let end = events
                .iter()
                .find(|event| event["type"] == "auto_compaction_end")
                .expect("auto_compaction_end");
            assert_eq!(end["result"]["persisted"], true);
            assert_eq!(
                end["result"]["persistenceStatus"]["event"],
                "session.persistence.healthy"
            );
            assert_eq!(end["result"]["persistenceStatus"]["pendingMessageCount"], 0);

            let cx = asupersync::Cx::for_testing();
            let session = session.lock(&cx).await.expect("lock persisted session");
            assert_eq!(provider_compaction_count(&session), 1);
            let metrics_after = session.autosave_metrics();
            assert_eq!(metrics_after.pending_mutations, 0);
            assert_eq!(
                metrics_after.coalesced_mutations,
                metrics_before.coalesced_mutations + 1
            );
            assert_eq!(
                metrics_after.flush_started,
                metrics_before.flush_started + 1
            );
            assert_eq!(
                metrics_after.flush_succeeded,
                metrics_before.flush_succeeded + 1
            );
            assert_eq!(metrics_after.flush_failed, metrics_before.flush_failed);
            let path = session.path.clone().expect("persisted session path");
            drop(session);

            let reopened = Session::open(path.to_string_lossy().as_ref())
                .await
                .expect("reopen persisted session");
            assert_eq!(
                provider_compaction_count(&reopened),
                1,
                "durable bytes must contain exactly one provider-backed compaction"
            );
        });
    }

    #[test]
    fn auto_compaction_persistence_failure_preserves_live_session_and_quarantines_provider() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 1)
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let blocked_root = tempfile::tempdir().expect("tempdir");
            let blocked_session_dir = blocked_root.path().join("not-a-directory");
            std::fs::write(&blocked_session_dir, b"blocked").expect("write path blocker");
            let session = Session::create_with_dir(Some(blocked_session_dir));
            let (events, session, metrics_before, provider_admission) =
                run_auto_compaction_persistence_case(session, true, runtime_handle).await;
            let end = events
                .iter()
                .find(|event| event["type"] == "auto_compaction_end")
                .expect("auto_compaction_end");
            assert!(
                end.get("result").is_none(),
                "a failed durable write must not expose a successful result: {end}"
            );
            assert!(
                end["errorMessage"]
                    .as_str()
                    .is_some_and(|message| message.contains(
                        "RPC auto-compaction persistence remained indeterminate"
                    )),
                "persistence failure must be explicit and terminal: {end}"
            );
            assert!(
                provider_admission.reason().is_some_and(|reason| reason
                    .contains("RPC auto-compaction persistence remained indeterminate")),
                "an indeterminate durable transition must quarantine provider re-entry"
            );

            let cx = asupersync::Cx::for_testing();
            let session = session.lock(&cx).await.expect("lock failed-save session");
            assert_eq!(
                provider_compaction_count(&session),
                0,
                "failed persistence must not install the private candidate into the live session"
            );
            let metrics_after = session.autosave_metrics();
            assert_eq!(
                metrics_after.pending_mutations,
                metrics_before.pending_mutations,
                "failed private-candidate persistence must not alter the live autosave queue"
            );
            assert_eq!(
                metrics_after.coalesced_mutations,
                metrics_before.coalesced_mutations,
                "failed private-candidate persistence must not alter live mutation counters"
            );
            assert_eq!(
                metrics_after.flush_started, metrics_before.flush_started,
                "failed private-candidate persistence must not alter live flush counters"
            );
            assert_eq!(
                metrics_after.flush_failed, metrics_before.flush_failed,
                "failed private-candidate persistence must not leak candidate failures into live metrics"
            );
        });
    }

    #[test]
    fn auto_compaction_excludes_overlapping_rpc_turn_entry() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let temp = tempfile::tempdir().expect("tempdir");
            let (compaction_entered, mut wait_for_compaction) =
                asupersync::channel::oneshot::channel::<()>();
            let (release_compaction, compaction_gate) =
                asupersync::channel::oneshot::channel::<()>();
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let provider: Arc<dyn Provider> = Arc::new(GatedAutoCompactionProvider {
                calls: Arc::clone(&calls),
                compaction_entered: Mutex::new(Some(compaction_entered)),
                compaction_gate: Mutex::new(Some(compaction_gate)),
            });
            let tools = ToolRegistry::new(&[], temp.path(), None);
            let agent = Agent::new(
                provider,
                tools,
                AgentConfig {
                    stream_options: crate::provider::StreamOptions {
                        api_key: Some("test-key".to_string()),
                        ..crate::provider::StreamOptions::default()
                    },
                    ..AgentConfig::default()
                },
            );
            let session = Arc::new(asupersync::sync::Mutex::new(
                seed_auto_compaction_session(Session::in_memory()),
            ));
            let agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            let mut options =
                build_test_rpc_options(&runtime_handle, temp.path().join("auth.json"));
            let mut model = dummy_entry("test-model", false);
            model.model.provider = "test-provider".to_string();
            options.available_models = vec![model];
            options.config.compaction = Some(crate::config::CompactionSettings {
                enabled: Some(true),
                reserve_tokens: Some(2),
                keep_recent_tokens: Some(1),
                mode: None,
            });

            let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);
            let out_rx = Arc::new(Mutex::new(out_rx));
            let server = runtime_handle
                // Boxed: clippy::large_futures.
                .spawn(async move { Box::pin(run(agent_session, options, in_rx, out_tx)).await });

            let first = send_recv(
                &in_tx,
                &out_rx,
                r#"{"id":"1","type":"prompt","message":"trigger compaction"}"#,
                "compaction trigger",
            )
            .await;
            assert_ok(&first, "prompt");

            let entered_cx = AgentCx::for_request();
            wait_for_compaction
                .recv(entered_cx.cx())
                .await
                .expect("auto-compaction provider entered");

            for (id, command, expected_command) in [
                (
                    "2",
                    r#"{"id":"2","type":"prompt","message":"overlap"}"#,
                    "prompt",
                ),
                (
                    "3",
                    r#"{"id":"3","type":"steer","message":"overlap"}"#,
                    "steer",
                ),
                (
                    "4",
                    r#"{"id":"4","type":"follow_up","message":"overlap"}"#,
                    "follow_up",
                ),
                ("5", r#"{"id":"5","type":"retry"}"#, "retry"),
                ("6", r#"{"id":"6","type":"compact"}"#, "compact"),
                (
                    "7",
                    r#"{"id":"7","type":"new_session"}"#,
                    "new_session",
                ),
                (
                    "8",
                    r#"{"id":"8","type":"switch_session","sessionPath":"unused"}"#,
                    "switch_session",
                ),
                (
                    "9",
                    r#"{"id":"9","type":"fork","entryId":"unused"}"#,
                    "fork",
                ),
            ] {
                let response = send_recv(&in_tx, &out_rx, command, "compaction overlap").await;
                assert_err(&response, expected_command);
                assert_eq!(response["id"], id);
                assert!(
                    response["error"]
                        .as_str()
                        .is_some_and(|error| error.contains("compacting") || error.contains("busy")),
                    "turn entry must fail specifically because compaction owns the phase: {response}"
                );
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "rejected turns must not dispatch another provider request"
            );

            let release_cx = AgentCx::for_request();
            release_compaction
                .send(release_cx.cx(), ())
                .expect("release auto-compaction provider");
            loop {
                let line = recv_line(&out_rx, "auto-compaction completion")
                    .await
                    .expect("receive auto-compaction event");
                if parse_response(&line)["type"] == "auto_compaction_end" {
                    break;
                }
            }

            drop(in_tx);
            let server_result = server.await;
            assert!(server_result.is_ok(), "RPC server error: {server_result:?}");
            let cx = asupersync::Cx::for_testing();
            let session = session.lock(&cx).await.expect("lock compacted session");
            let gated_compactions = session
                .entries_for_current_path()
                .iter()
                .filter(|entry| {
                    // The split-turn strategy appends a "Turn Context (split
                    // turn)" section after the gated summary, so match the
                    // prefix rather than the whole string.
                    matches!(
                        entry,
                        crate::session::SessionEntry::Compaction(compaction)
                            if compaction.summary.starts_with("gated auto-compaction summary")
                    )
                })
                .count();
            assert_eq!(
                gated_compactions, 1,
                "exactly one compaction mutation may survive the exclusion window"
            );
            // One turn call plus the split-turn compaction, which issues two
            // provider calls (history summary + turn-prefix summary; see
            // `compaction::generate_llm_summary`). A duplicate compaction
            // slipping through the exclusion window would add two more.
            assert_eq!(
                calls.load(Ordering::SeqCst),
                3,
                "the completed exclusion window must use one turn call and one split-turn compaction (two provider calls)"
            );
        });
    }

    #[test]
    fn auto_compaction_rejects_stale_session_snapshot_with_paired_end_event() {
        // Watchdog: on 2026-09-01 this test hung forever (the session swap
        // below took the agent-session lock that auto-compaction holds across
        // the gated provider call) and stalled the whole lib test binary,
        // which blocked the DSR test lane twice. The body runs on its own
        // thread so a regression fails after 120 s instead of hanging every
        // test scheduled after it.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let body = std::thread::spawn(move || {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime build");
            let runtime_handle = runtime.handle();

            runtime.block_on(async move {
                let temp = tempfile::tempdir().expect("tempdir");
                let (compaction_entered, mut wait_for_compaction) =
                    asupersync::channel::oneshot::channel::<()>();
                let (release_compaction, compaction_gate) =
                    asupersync::channel::oneshot::channel::<()>();
                let provider: Arc<dyn Provider> = Arc::new(GatedAutoCompactionProvider {
                    calls: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
                    compaction_entered: Mutex::new(Some(compaction_entered)),
                    compaction_gate: Mutex::new(Some(compaction_gate)),
                });
                let agent = Agent::new(
                    provider,
                    ToolRegistry::new(&[], temp.path(), None),
                    AgentConfig {
                        stream_options: crate::provider::StreamOptions {
                            api_key: Some("test-key".to_string()),
                            ..crate::provider::StreamOptions::default()
                        },
                        ..AgentConfig::default()
                    },
                );
                let session = Arc::new(asupersync::sync::Mutex::new(seed_auto_compaction_session(
                    Session::in_memory(),
                )));
                let agent_session = Arc::new(asupersync::sync::Mutex::new(AgentSession::new(
                    agent,
                    Arc::clone(&session),
                    false,
                    crate::compaction::ResolvedCompactionSettings::default(),
                )));
                let mut options =
                    build_test_rpc_options(&runtime_handle, temp.path().join("auth.json"));
                let mut model = dummy_entry("test-model", false);
                model.model.provider = "test-provider".to_string();
                options.available_models = vec![model];
                options.config.compaction = Some(crate::config::CompactionSettings {
                    enabled: Some(true),
                    reserve_tokens: Some(2),
                    keep_recent_tokens: Some(1),
                    mode: None,
                });

                let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(16);
                let compacting = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                    &options.config,
                )));
                let compact_task = runtime_handle.spawn(maybe_auto_compact(
                    Arc::clone(&agent_session),
                    shared_state,
                    options,
                    Arc::clone(&compacting),
                    out_tx,
                ));
                let entered_cx = AgentCx::for_request();
                wait_for_compaction
                    .recv(entered_cx.cx())
                    .await
                    .expect("auto-compaction provider entered");

                // Auto-compaction holds the agent-session lock across the
                // provider call, so the swap must go through the shared inner
                // session handle. Taking the agent-session lock here deadlocked
                // against the gated provider (the cause of the 2026-09-01 hang).
                let replacement_id = {
                    let mut inner = session.lock(&entered_cx).await.expect("session lock");
                    let replacement = Session::in_memory();
                    let replacement_id = replacement.header.id.clone();
                    *inner = replacement;
                    replacement_id
                };
                release_compaction
                    .send(entered_cx.cx(), ())
                    .expect("release auto-compaction provider");
                compact_task.await;

                let events = out_rx
                    .try_iter()
                    .map(|line| serde_json::from_str::<Value>(&line).expect("event json"))
                    .collect::<Vec<_>>();
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| event["type"] == "auto_compaction_start")
                        .count(),
                    1
                );
                let ends = events
                    .iter()
                    .filter(|event| event["type"] == "auto_compaction_end")
                    .collect::<Vec<_>>();
                assert_eq!(
                    ends.len(),
                    1,
                    "start must have exactly one terminal end: {events:?}"
                );
                assert!(
                    ends[0]["errorMessage"]
                        .as_str()
                        .is_some_and(|error| error.contains("Active session changed")),
                    "stale snapshot must fail closed: {ends:?}"
                );
                assert!(ends[0].get("result").is_none());
                assert!(!compacting.load(Ordering::SeqCst));

                let inner = session
                    .lock(&entered_cx)
                    .await
                    .expect("replacement session lock");
                assert_eq!(inner.header.id, replacement_id);
                assert_eq!(provider_compaction_count(&inner), 0);
            });
            let _ = done_tx.send(());
        });
        match done_rx.recv_timeout(Duration::from_secs(120)) {
            Ok(()) => body.join().expect("test body thread panicked"),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // The body panicked before signalling: surface that panic.
                body.join().expect("test body thread panicked");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!(
                "auto_compaction_rejects_stale_session_snapshot_with_paired_end_event hung for 120s: \
                 the compaction provider was never entered or the RPC loop never finished \
                 (see bd-x8mn7 cluster B)"
            ),
        }
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
    fn parse_prompt_images_sanitizes_mime_type_at_ingress() {
        let val = json!([{
            "type": "image",
            "source": {
                "type": "base64",
                "mediaType": " image/jpeg\u{001b}]2;owned\u{0007}",
                "data": "abc"
            }
        }]);
        let images = parse_prompt_images(Some(&val)).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/jpeg");
        assert!(!images[0].mime_type.chars().any(char::is_control));
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
    fn resumed_session_normalization_keeps_header_history_and_runtime_target_coherent() {
        let mut entry = dummy_entry("gpt-4o", false);
        entry.model.provider = "openai".to_string();
        let mut session = Session::in_memory();
        session.header.provider = Some("OpenAI".to_string());
        session.header.model_id = Some("GPT-4O".to_string());
        session.header.thinking_level = Some("high".to_string());

        let (thinking, changed) = normalize_resumed_session_model(&mut session, &entry);

        assert!(changed);
        assert_eq!(thinking, ThinkingLevel::Off);
        assert_eq!(session.header.provider.as_deref(), Some("openai"));
        assert_eq!(session.header.model_id.as_deref(), Some("gpt-4o"));
        assert_eq!(session.header.thinking_level.as_deref(), Some("off"));
        assert_eq!(
            session.effective_model_for_current_path(),
            Some(("openai".to_string(), "gpt-4o".to_string()))
        );
        assert_eq!(session_thinking_level(&session), Some(ThinkingLevel::Off));
    }

    #[test]
    fn replayed_plan_mode_uses_latest_session_transition_and_preserves_fail_closed_state() {
        let mut session = Session::in_memory();
        session.append_custom_entry("plan_mode".to_string(), Some(json!({"mode": "planning"})));
        session.append_custom_entry("plan_mode".to_string(), Some(json!({"mode": "approved"})));
        assert_eq!(
            replayed_plan_mode(&session),
            crate::plan::PlanMode::Approved
        );

        session.append_custom_entry("plan_mode".to_string(), Some(json!({"mode": "off"})));
        assert_eq!(replayed_plan_mode(&session), crate::plan::PlanMode::Off);

        session.append_custom_entry(
            "plan_mode".to_string(),
            Some(json!({"mode": "pending_approval"})),
        );
        assert_eq!(
            replayed_plan_mode(&session),
            crate::plan::PlanMode::PendingApproval
        );
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
            let mut shared_state = RpcSharedState::new(&options.config);
            let err = cycle_model_for_rpc(&mut agent_session, &mut shared_state, &options)
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
    fn cycle_model_for_rpc_quarantines_failed_persistence_without_live_mutation() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 1)
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let mut current = dummy_entry("current-model", true);
            current.api_key = Some("current-key".to_string());
            current.model.input = vec![InputType::Text, InputType::Image];
            current.model.context_window = 16_384;
            current.model.max_tokens = 1_536;
            let mut next = dummy_entry("next-model", false);
            next.api_key = Some("next-key".to_string());
            next.model.context_window = 4_096;
            next.model.max_tokens = 2_048;
            next.compat = Some(crate::models::CompatConfig {
                tool_call_dialect: Some(crate::dialects::Dialect::Xmlish),
                ..Default::default()
            });

            let provider =
                crate::providers::create_provider(&current, None).expect("create current provider");
            let mut agent = Agent::new(
                provider,
                ToolRegistry::new(&[], Path::new("."), None),
                AgentConfig::default(),
            );
            agent.stream_options_mut().api_key = Some("current-key".to_string());
            agent.stream_options_mut().max_tokens = Some(current.model.max_tokens);
            agent.stream_options_mut().thinking_level = Some(ThinkingLevel::High);
            agent.set_model_accepts_images(true);

            let temp = tempfile::tempdir().expect("tempdir");
            let blocked_path = temp.path().join("blocked.jsonl");
            std::fs::create_dir_all(&blocked_path).expect("create blocking directory");
            let mut session = Session::in_memory();
            session.path = Some(blocked_path);
            session.set_model_header(
                Some(current.model.provider.clone()),
                Some(current.model.id.clone()),
                Some(ThinkingLevel::High.to_string()),
            );
            let session_store = Arc::new(asupersync::sync::Mutex::new(session));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session_store),
                true,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            agent_session.set_compaction_context_window(current.model.context_window);
            let options = rpc_options_with_models(vec![current.clone(), next]);
            let mut shared_state = RpcSharedState::new(&options.config);

            let err = cycle_model_for_rpc(&mut agent_session, &mut shared_state, &options)
                .await
                .expect_err("unwritable model-selection candidate must fail closed");
            assert!(err.is_session_persistence(), "unexpected error: {err}");
            assert!(shared_state.provider_admission.reason().is_some());
            assert_eq!(
                agent_session.agent.provider().name(),
                current.model.provider
            );
            assert_eq!(agent_session.agent.provider().model_id(), current.model.id);
            assert_eq!(
                agent_session.agent.stream_options().api_key.as_deref(),
                Some("current-key")
            );
            assert_eq!(agent_session.agent.stream_options().max_tokens, Some(1_536));
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                Some(ThinkingLevel::High)
            );
            assert!(agent_session.agent.model_accepts_images());
            assert_eq!(
                agent_session.compaction_settings().context_window_tokens,
                16_384
            );
            let cx = AgentCx::for_request();
            let session = session_store.lock(cx.cx()).await.expect("Session lock");
            assert_eq!(
                session.header.provider.as_deref(),
                Some(current.model.provider.as_str())
            );
            assert_eq!(
                session.header.model_id.as_deref(),
                Some(current.model.id.as_str())
            );
            assert_eq!(session.header.thinking_level.as_deref(), Some("high"));
            assert!(
                session
                    .entries_for_current_path()
                    .iter()
                    .all(|entry| !matches!(entry, SessionEntry::ModelChange(_)))
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
            let mut shared_state = RpcSharedState::new(&options.config);
            shared_state.failover_primary = Some(RpcFailoverPrimary {
                provider: "test-provider".to_string(),
                model_id: "test-model".to_string(),
                requested_thinking_level: ThinkingLevel::High,
            });
            shared_state.active_failover_model =
                Some(("anthropic".to_string(), "stale-fallback".to_string()));
            shared_state.failover_chain_position = Some(2);
            let result = cycle_model_for_rpc(&mut agent_session, &mut shared_state, &options)
                .await
                .expect("cycle should succeed")
                .expect("should choose next model");

            assert_eq!(result.0.model.provider, next.model.provider);
            assert_eq!(result.0.model.id, next.model.id);
            assert_eq!(agent_session.agent.provider().name(), next.model.provider);
            assert_eq!(agent_session.agent.provider().model_id(), next.model.id);
            assert!(shared_state.failover_primary.is_none());
            assert!(shared_state.active_failover_model.is_none());
            assert!(shared_state.failover_chain_position.is_none());

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
            let mut shared_state = RpcSharedState::new(&options.config);
            let result = cycle_model_for_rpc(&mut agent_session, &mut shared_state, &options)
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
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));
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
                let apply = apply_thinking_level(
                    Arc::clone(&session_handle),
                    Arc::clone(&shared_state),
                    ThinkingLevel::High,
                );
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
            let shared_state = Arc::new(asupersync::sync::Mutex::new(RpcSharedState::new(
                &Config::default(),
            )));

            apply_thinking_level(
                Arc::clone(&session_handle),
                shared_state,
                ThinkingLevel::High,
            )
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
                // Boxed: clippy::large_futures.
                handle.spawn(async move { Box::pin(run(agent_session, options, in_rx, out_tx)).await });

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
            let foreign_session_id = format!("foreign-rpc-{}", uuid::Uuid::new_v4().simple());
            crate::jobs::push_completion_notice(
                &foreign_session_id,
                "must remain outside this RPC session",
            )
            .expect("foreign notice");

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
                // Boxed: clippy::large_futures.
                futures::future::join(Box::pin(run(agent_session, options, in_rx, out_tx)), client)
                    .await;
            assert!(server_result.is_ok(), "rpc server error: {server_result:?}");
            let calls = state.calls.load(Ordering::SeqCst);
            assert_eq!(
                calls, 1,
                "a completion notice owned by another session must not inject an RPC follow-up"
            );
            let deadlines = state
                .observed_deadlines
                .lock()
                .expect("lock rpc deadline probe")
                .clone();
            assert_eq!(deadlines.len(), calls);
            for deadline in deadlines {
                assert_eq!(deadline, Some(expected_deadline));
            }
            let foreign_notices = crate::jobs::take_completion_notices(&foreign_session_id);
            assert_eq!(foreign_notices.len(), 1);
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
            let mut shared_state = RpcSharedState::new(&options.config);
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
                let cycle = cycle_model_for_rpc(&mut agent_session, &mut shared_state, &options);
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

    /// bd-cv653.3.8: the ask_request frame carries the request id, the
    /// serialized questions (camelCase wire), and the wait budget.
    #[test]
    fn ask_request_frame_shape() {
        let request: crate::ask::AskRequest = serde_json::from_value(json!({
            "questions": [{
                "id": "q1",
                "question": "Pick?",
                "recommended": 1,
                "options": [{"label": "A"}, {"label": "B", "description": "beta"}]
            }]
        }))
        .expect("ask request");
        let frame = ask_request_rpc_event(&crate::ask::AskUiRequest {
            id: "ask-1".to_string(),
            request,
        });
        assert_eq!(frame["type"], "ask_request");
        assert_eq!(frame["id"], "ask-1");
        assert_eq!(frame["timeoutMs"], crate::ask::ASK_UI_TIMEOUT_MS);
        assert_eq!(frame["questions"][0]["question"], "Pick?");
        assert_eq!(frame["questions"][0]["recommended"], 1);
        assert_eq!(frame["questions"][0]["options"][1]["description"], "beta");
    }

    /// bd-cv653.3.8: ask_response parsing — answers, dismissal, aliases,
    /// and the malformed cases.
    #[test]
    fn ask_response_parse_matrix() {
        let (id, response) = rpc_parse_ask_response(&json!({
            "type": "ask_response",
            "requestId": "ask-1",
            "answers": [{"questionId": "q1", "selected": ["B"]}]
        }))
        .expect("valid answers");
        assert_eq!(id, "ask-1");
        assert!(!response.dismissed);
        assert_eq!(response.answers[0].question_id, "q1");
        assert_eq!(response.answers[0].selected, vec!["B"]);

        // `id` alias + Other free text.
        let (_, response) = rpc_parse_ask_response(&json!({
            "id": "ask-2",
            "answers": [{"questionId": "q1", "selected": [], "other": "free text"}]
        }))
        .expect("other answer");
        assert_eq!(response.answers[0].other.as_deref(), Some("free text"));

        // Dismissal needs no answers.
        let (_, response) = rpc_parse_ask_response(&json!({
            "requestId": "ask-3",
            "dismissed": true
        }))
        .expect("dismissed");
        assert!(response.dismissed);

        for (label, bad) in [
            ("missing id", json!({"answers": []})),
            ("missing answers", json!({"requestId": "x"})),
            ("empty answers", json!({"requestId": "x", "answers": []})),
            (
                "malformed answer",
                json!({"requestId": "x", "answers": [{"selected": "not-a-list"}]}),
            ),
        ] {
            assert!(
                rpc_parse_ask_response(&bad).is_err(),
                "must reject: {label}"
            );
        }
    }

    /// bd-m83oo: bash RPC execution preserves command results and never returns
    /// a retryable error on persistence failure, while structured persistence
    /// states remain serializable. Production-branch coverage lives in
    /// `tests/e2e_rpc.rs`.
    #[test]
    fn bash_rpc_persistence_surfaces_state_without_command_failure() {
        let success_payload = json!({
            "output": "hello world\n",
            "exitCode": 0,
            "cancelled": false,
            "truncated": false,
            "fullOutputPath": null,
            "persisted": true,
            "persistenceStatus": {
                "event": "session.persistence.healthy",
                "severity": "ok",
                "summary": "Session history persisted.",
                "action": "No action required.",
                "sliIds": ["sli_resume_ready_p95_ms"],
                "pendingMessageCount": 0,
            }
        });
        let ok_resp = response_ok(Some("cmd-1".to_string()), "bash", Some(success_payload));
        assert!(ok_resp.contains("\"success\":true"));
        assert!(ok_resp.contains("\"persisted\":true"));
        assert!(ok_resp.contains("\"event\":\"session.persistence.healthy\""));
        assert!(!ok_resp.contains("persistenceWarning"));

        let disabled_payload = json!({
            "output": "memory only\n",
            "exitCode": 0,
            "cancelled": false,
            "truncated": false,
            "fullOutputPath": null,
            "persisted": false,
            "persistenceStatus": {
                "event": "session.persistence.disabled",
                "severity": "info",
                "summary": "Session persistence is disabled; bash history is retained in memory only.",
                "action": "Enable session saving to make command history durable.",
                "sliIds": [],
                "pendingMessageCount": 1,
            }
        });
        let disabled_resp = response_ok(
            Some("cmd-disabled".to_string()),
            "bash",
            Some(disabled_payload),
        );
        assert!(disabled_resp.contains("\"success\":true"));
        assert!(disabled_resp.contains("\"persisted\":false"));
        assert!(disabled_resp.contains("\"event\":\"session.persistence.disabled\""));
        assert!(!disabled_resp.contains("persistenceWarning"));

        let mut warning_payload = json!({
            "output": "side effect executed\n",
            "exitCode": 0,
            "cancelled": false,
            "truncated": false,
            "fullOutputPath": null,
            "persisted": false,
            "persistenceStatus": {
                "event": "session.persistence.backlog",
                "severity": "warning",
                "summary": "Session history persistence failed after bash execution.",
                "action": "Trigger manual save or verify session storage permissions.",
                "sliIds": ["sli_resume_ready_p95_ms", "sli_failure_recovery_success_rate"],
                "pendingMessageCount": 1,
                "errorMessage": "Disk full",
            }
        });
        warning_payload["persistenceWarning"] =
            json!("Failed to persist bash execution to session: Disk full");

        let warn_resp = response_ok(Some("cmd-2".to_string()), "bash", Some(warning_payload));
        assert!(
            warn_resp.contains("\"success\":true"),
            "command must remain success to prevent unsafe retry"
        );
        assert!(warn_resp.contains("\"persisted\":false"));
        assert!(warn_resp.contains(
            "\"persistenceWarning\":\"Failed to persist bash execution to session: Disk full\""
        ));
        assert!(warn_resp.contains("\"event\":\"session.persistence.backlog\""));
    }

    /// bd-m83oo: auto-compaction must not emit an unqualified successful durable
    /// completion event when persistence fails.
    #[test]
    fn auto_compaction_end_event_emits_error_when_persistence_fails() {
        let success_event = agent_event(AgentEvent::AutoCompactionEnd {
            result: Some(json!({
                "summary": "compacted summary",
                "firstKeptEntryId": "entry-10",
                "tokensBefore": 5000,
                "tokensAfter": 1200,
                "details": {},
                "persisted": true,
                "persistenceStatus": {
                    "event": "session.persistence.healthy",
                    "severity": "ok",
                    "pendingMessageCount": 0,
                },
            })),
            aborted: false,
            will_retry: false,
            error_message: None,
        });
        assert!(success_event.contains("\"type\":\"auto_compaction_end\""));
        assert!(success_event.contains("\"summary\":\"compacted summary\""));
        assert!(success_event.contains("\"persisted\":true"));
        assert!(success_event.contains("\"event\":\"session.persistence.healthy\""));
        assert!(!success_event.contains("\"errorMessage\""));

        let failure_event = agent_event(AgentEvent::AutoCompactionEnd {
            result: None,
            aborted: false,
            will_retry: false,
            error_message: Some(
                "Failed to persist compaction to session: disk write error".to_string(),
            ),
        });
        assert!(failure_event.contains("\"type\":\"auto_compaction_end\""));
        assert!(!failure_event.contains("\"summary\""));
        assert!(failure_event.contains(
            "\"errorMessage\":\"Failed to persist compaction to session: disk write error\""
        ));
    }
}
