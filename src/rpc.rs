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

use crate::agent::{AbortHandle, AgentEvent, AgentSession};
use crate::auth::AuthStorage;
use crate::compaction::{
    ResolvedCompactionSettings, compact, compaction_details_to_value, prepare_compaction,
};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{
    ContentBlock, ImageContent, Message, StopReason, TextContent, UserContent, UserMessage,
};
use crate::models::ModelEntry;
use crate::providers;
use crate::resources::ResourceLoader;
use crate::session::SessionMessage;
use crate::tools::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

#[derive(Debug, Clone)]
pub struct RpcOptions {
    pub config: Config,
    pub resources: ResourceLoader,
    pub available_models: Vec<ModelEntry>,
    pub scoped_models: Vec<RpcScopedModel>,
    pub auth: AuthStorage,
}

#[derive(Debug, Clone)]
pub struct RpcScopedModel {
    pub model: ModelEntry,
    pub thinking_level: Option<crate::model::ThinkingLevel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueMode {
    All,
    OneAtATime,
}

impl QueueMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::OneAtATime => "one-at-a-time",
        }
    }
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

fn parse_queue_mode(mode: Option<&str>) -> Option<QueueMode> {
    match mode {
        Some("all") => Some(QueueMode::All),
        Some("one-at-a-time") => Some(QueueMode::OneAtATime),
        _ => None,
    }
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
        "follow-up" | "followUp" => Ok(Some(StreamingBehavior::FollowUp)),
        _ => Err(Error::validation(format!("Invalid streamingBehavior: {s}"))),
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
    let mut blocks = vec![ContentBlock::Text(TextContent::new(text.to_string()))];
    for image in images {
        blocks.push(ContentBlock::Image(image.clone()));
    }
    Message::User(UserMessage {
        content: UserContent::Blocks(blocks),
        timestamp,
    })
}

fn is_extension_command(message: &str, expanded: &str) -> bool {
    // Extension commands start with `/` but are not expanded by the resource loader
    // (skills and prompt templates are expanded before queueing/sending).
    message.trim_start().starts_with('/') && message == expanded
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

impl RpcSharedState {
    fn new(config: &Config) -> Self {
        Self {
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            steering_mode: parse_queue_mode(config.steering_mode.as_deref())
                .unwrap_or(QueueMode::OneAtATime),
            follow_up_mode: parse_queue_mode(config.follow_up_mode.as_deref())
                .unwrap_or(QueueMode::OneAtATime),
            auto_compaction_enabled: config.compaction_enabled(),
            auto_retry_enabled: config.retry_enabled(),
        }
    }

    fn pending_count(&self) -> usize {
        self.steering.len() + self.follow_up.len()
    }

    fn push_steering(&mut self, message: Message) {
        self.steering.push_back(message);
    }

    fn push_follow_up(&mut self, message: Message) {
        self.follow_up.push_back(message);
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
    abort_tx: oneshot::Sender<()>,
}

pub async fn run_stdio(session: AgentSession, options: RpcOptions) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    run(
        session,
        options,
        tokio::io::BufReader::new(stdin),
        tokio::io::BufWriter::new(stdout),
    )
    .await
}

#[allow(clippy::too_many_lines)]
#[allow(
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee
)]
pub async fn run<R, W>(
    session: AgentSession,
    options: RpcOptions,
    mut reader: R,
    writer: W,
) -> Result<()>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let session = Arc::new(Mutex::new(session));
    let shared_state = Arc::new(Mutex::new(RpcSharedState::new(&options.config)));
    let is_streaming = Arc::new(AtomicBool::new(false));
    let is_compacting = Arc::new(AtomicBool::new(false));
    let abort_handle: Arc<Mutex<Option<AbortHandle>>> = Arc::new(Mutex::new(None));
    let bash_state: Arc<Mutex<Option<RunningBash>>> = Arc::new(Mutex::new(None));
    let (retry_abort_tx, _) = watch::channel(false);

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

    {
        use futures::future::BoxFuture;
        let steering_state = Arc::clone(&shared_state);
        let follow_state = Arc::clone(&shared_state);
        let mut guard = session.lock().await;
        let steering_fetcher = move || -> BoxFuture<'static, Vec<Message>> {
            let steering_state = Arc::clone(&steering_state);
            Box::pin(async move {
                let mut state = steering_state.lock().await;
                state.pop_steering()
            })
        };
        let follow_fetcher = move || -> BoxFuture<'static, Vec<Message>> {
            let follow_state = Arc::clone(&follow_state);
            Box::pin(async move {
                let mut state = follow_state.lock().await;
                state.pop_follow_up()
            })
        };
        guard.agent.set_message_fetchers(
            Some(Arc::new(steering_fetcher)),
            Some(Arc::new(follow_fetcher)),
        );
    }

    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(line) = out_rx.recv().await {
            writer.write_all(line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    });

    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            break; // EOF
        }
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

        let Some(command_type) = parsed.get("type").and_then(Value::as_str) else {
            let resp = response_error(None, "parse", "Missing command type".to_string());
            let _ = out_tx.send(resp);
            continue;
        };

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
                        let resp = response_error(id, "prompt", err.to_string());
                        let _ = out_tx.send(resp);
                        continue;
                    }
                };

                let streaming_behavior =
                    match parse_streaming_behavior(parsed.get("streamingBehavior")) {
                        Ok(value) => value,
                        Err(err) => {
                            let resp = response_error(id, "prompt", err.to_string());
                            let _ = out_tx.send(resp);
                            continue;
                        }
                    };

                let expanded = options.resources.expand_input(&message);

                if is_streaming.load(Ordering::SeqCst) {
                    let queued = {
                        let mut state = shared_state.lock().await;
                        match streaming_behavior {
                            Some(StreamingBehavior::Steer) => {
                                state.push_steering(build_user_message(&expanded, &images));
                                true
                            }
                            Some(StreamingBehavior::FollowUp) => {
                                state.push_follow_up(build_user_message(&expanded, &images));
                                true
                            }
                            None => false,
                        }
                    };

                    if !queued {
                        let resp = response_error(
                            id,
                            "prompt",
                            "Agent is currently streaming; specify streamingBehavior".to_string(),
                        );
                        let _ = out_tx.send(resp);
                        continue;
                    }

                    let _ = out_tx.send(response_ok(id, "prompt", None));
                    continue;
                }

                // Ack immediately.
                let _ = out_tx.send(response_ok(id, "prompt", None));

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let shared_state = Arc::clone(&shared_state);
                let is_streaming = Arc::clone(&is_streaming);
                let is_compacting = Arc::clone(&is_compacting);
                let abort_handle_slot = Arc::clone(&abort_handle);
                let retry_abort_tx = retry_abort_tx.clone();
                let options = options.clone();
                let expanded = expanded.clone();
                tokio::spawn(async move {
                    run_prompt_with_retry(
                        session,
                        shared_state,
                        is_streaming,
                        is_compacting,
                        abort_handle_slot,
                        out_tx,
                        retry_abort_tx,
                        options,
                        expanded,
                        images,
                    )
                    .await;
                });
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

                let expanded = options.resources.expand_input(&message);
                if is_extension_command(&message, &expanded) {
                    let resp = response_error(
                        id,
                        "steer",
                        "Extension commands are not allowed with steer".to_string(),
                    );
                    let _ = out_tx.send(resp);
                    continue;
                }

                if is_streaming.load(Ordering::SeqCst) {
                    shared_state
                        .lock()
                        .await
                        .push_steering(build_user_message(&expanded, &[]));
                    let _ = out_tx.send(response_ok(id, "steer", None));
                    continue;
                }

                let _ = out_tx.send(response_ok(id, "steer", None));

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let shared_state = Arc::clone(&shared_state);
                let is_streaming = Arc::clone(&is_streaming);
                let is_compacting = Arc::clone(&is_compacting);
                let abort_handle_slot = Arc::clone(&abort_handle);
                let retry_abort_tx = retry_abort_tx.clone();
                let options = options.clone();
                let expanded = expanded.clone();
                tokio::spawn(async move {
                    run_prompt_with_retry(
                        session,
                        shared_state,
                        is_streaming,
                        is_compacting,
                        abort_handle_slot,
                        out_tx,
                        retry_abort_tx,
                        options,
                        expanded,
                        Vec::new(),
                    )
                    .await;
                });
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

                let expanded = options.resources.expand_input(&message);
                if is_extension_command(&message, &expanded) {
                    let resp = response_error(
                        id,
                        "follow_up",
                        "Extension commands are not allowed with follow_up".to_string(),
                    );
                    let _ = out_tx.send(resp);
                    continue;
                }

                if is_streaming.load(Ordering::SeqCst) {
                    shared_state
                        .lock()
                        .await
                        .push_follow_up(build_user_message(&expanded, &[]));
                    let _ = out_tx.send(response_ok(id, "follow_up", None));
                    continue;
                }

                let _ = out_tx.send(response_ok(id, "follow_up", None));

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let shared_state = Arc::clone(&shared_state);
                let is_streaming = Arc::clone(&is_streaming);
                let is_compacting = Arc::clone(&is_compacting);
                let abort_handle_slot = Arc::clone(&abort_handle);
                let retry_abort_tx = retry_abort_tx.clone();
                let options = options.clone();
                let expanded = expanded.clone();
                tokio::spawn(async move {
                    run_prompt_with_retry(
                        session,
                        shared_state,
                        is_streaming,
                        is_compacting,
                        abort_handle_slot,
                        out_tx,
                        retry_abort_tx,
                        options,
                        expanded,
                        Vec::new(),
                    )
                    .await;
                });
            }

            "abort" => {
                let handle = abort_handle.lock().await.clone();
                if let Some(handle) = handle {
                    handle.abort();
                }
                let _ = out_tx.send(response_ok(id, "abort", None));
            }

            "get_state" => {
                let snapshot = {
                    let state = shared_state.lock().await;
                    RpcStateSnapshot::from(&*state)
                };
                let data = {
                    let guard = session.lock().await;
                    session_state(
                        &guard,
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
                    let guard = session.lock().await;
                    session_stats(&guard.session)
                };
                let _ = out_tx.send(response_ok(id, "get_session_stats", Some(data)));
            }

            "get_messages" => {
                let messages = {
                    let guard = session.lock().await;
                    guard
                        .session
                        .entries_for_current_path()
                        .iter()
                        .filter_map(|entry| match entry {
                            crate::session::SessionEntry::Message(msg) => match msg.message {
                                SessionMessage::User { .. }
                                | SessionMessage::Assistant { .. }
                                | SessionMessage::ToolResult { .. }
                                | SessionMessage::BashExecution { .. } => Some(msg.message.clone()),
                                _ => None,
                            },
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                };
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
                    .find(|m| m.model.provider == provider && m.model.id == model_id)
                    .cloned()
                else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_model",
                        format!("Model not found: {provider}/{model_id}"),
                    ));
                    continue;
                };

                let api_key = resolve_model_api_key(&options.auth, &entry).ok_or_else(|| {
                    Error::auth(format!(
                        "No API key for {}/{}",
                        entry.model.provider, entry.model.id
                    ))
                });
                let api_key = match api_key {
                    Ok(key) => key,
                    Err(err) => {
                        let _ = out_tx.send(response_error(id, "set_model", err.to_string()));
                        continue;
                    }
                };

                {
                    let mut guard = session.lock().await;
                    guard
                        .agent
                        .set_provider(providers::create_provider(&entry)?);
                    guard.agent.stream_options_mut().api_key = Some(api_key);
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
                    let clamped = clamp_thinking_level(current_thinking, &entry);
                    if clamped != current_thinking {
                        apply_thinking_level(&mut guard, clamped).await?;
                    }
                }

                let _ = out_tx.send(response_ok(
                    id,
                    "set_model",
                    Some(rpc_model_from_entry(&entry)),
                ));
            }

            "cycle_model" => {
                let (entry, thinking_level, is_scoped) = {
                    let mut guard = session.lock().await;
                    let Some(result) = cycle_model_for_rpc(&mut guard, &options).await? else {
                        let _ =
                            out_tx.send(response_ok(id.clone(), "cycle_model", Some(Value::Null)));
                        continue;
                    };
                    result
                };

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
                            out_tx.send(response_error(id, "set_thinking_level", err.to_string()));
                        continue;
                    }
                };

                {
                    let mut guard = session.lock().await;
                    let level = current_model_entry(&guard.session, &options)
                        .map_or(level, |entry| clamp_thinking_level(level, entry));
                    if let Err(err) = apply_thinking_level(&mut guard, level).await {
                        let _ = out_tx.send(response_error(
                            id.clone(),
                            "set_thinking_level",
                            err.to_string(),
                        ));
                        continue;
                    }
                }
                let _ = out_tx.send(response_ok(id, "set_thinking_level", None));
            }

            "cycle_thinking_level" => {
                let next = {
                    let mut guard = session.lock().await;
                    let Some(entry) = current_model_entry(&guard.session, &options) else {
                        let _ =
                            out_tx.send(response_ok(id, "cycle_thinking_level", Some(Value::Null)));
                        continue;
                    };
                    if !entry.model.reasoning {
                        let _ =
                            out_tx.send(response_ok(id, "cycle_thinking_level", Some(Value::Null)));
                        continue;
                    }

                    let levels = available_thinking_levels(entry);
                    let current = guard
                        .agent
                        .stream_options()
                        .thinking_level
                        .unwrap_or_default();
                    let current_index = levels
                        .iter()
                        .position(|level| *level == current)
                        .unwrap_or(0);
                    let next = levels[(current_index + 1) % levels.len()];
                    apply_thinking_level(&mut guard, next).await?;
                    next
                };
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
                let mut state = shared_state.lock().await;
                state.steering_mode = mode;
                drop(state);
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
                let mut state = shared_state.lock().await;
                state.follow_up_mode = mode;
                drop(state);
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
                let mut state = shared_state.lock().await;
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
                let mut state = shared_state.lock().await;
                state.auto_retry_enabled = enabled;
                drop(state);
                let _ = out_tx.send(response_ok(id, "set_auto_retry", None));
            }

            "abort_retry" => {
                let _ = retry_abort_tx.send(true);
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
                {
                    let mut guard = session.lock().await;
                    guard.session.append_session_info(Some(name.to_string()));
                    guard.persist_session().await?;
                }
                let _ = out_tx.send(response_ok(id, "set_session_name", None));
            }

            "get_last_assistant_text" => {
                let text = {
                    let guard = session.lock().await;
                    last_assistant_text(&guard.session)
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
                let path = {
                    let guard = session.lock().await;
                    export_html(&guard.session, output_path.as_deref()).await?
                };
                let _ = out_tx.send(response_ok(
                    id,
                    "export_html",
                    Some(json!({ "path": path })),
                ));
            }

            "bash" => {
                let Some(command) = parsed.get("command").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(id, "bash", "Missing command".to_string()));
                    continue;
                };

                let mut running = bash_state.lock().await;
                if running.is_some() {
                    let _ = out_tx.send(response_error(
                        id,
                        "bash",
                        "Bash command already running".to_string(),
                    ));
                    continue;
                }

                let (abort_tx, abort_rx) = oneshot::channel();
                *running = Some(RunningBash { abort_tx });

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let bash_state = Arc::clone(&bash_state);
                let command = command.to_string();
                let id_clone = id.clone();

                tokio::spawn(async move {
                    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                    let result = run_bash_rpc(&cwd, &command, abort_rx).await;

                    let response = match result {
                        Ok(result) => {
                            let mut guard = session.lock().await;
                            guard.session.append_message(SessionMessage::BashExecution {
                                command: command.clone(),
                                output: result.output.clone(),
                                exit_code: result.exit_code,
                                cancelled: Some(result.cancelled),
                                truncated: Some(result.truncated),
                                full_output_path: result.full_output_path.clone(),
                                timestamp: Some(chrono::Utc::now().timestamp_millis()),
                                extra: std::collections::HashMap::default(),
                            });
                            let _ = guard.persist_session().await;
                            drop(guard);

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
                        Err(err) => response_error(id_clone, "bash", err.to_string()),
                    };

                    let _ = out_tx.send(response);
                    let mut running = bash_state.lock().await;
                    *running = None;
                });
            }

            "abort_bash" => {
                let mut running = bash_state.lock().await;
                if let Some(running_bash) = running.take() {
                    let _ = running_bash.abort_tx.send(());
                }
                let _ = out_tx.send(response_ok(id, "abort_bash", None));
            }

            "compact" => {
                let custom_instructions = parsed
                    .get("customInstructions")
                    .and_then(Value::as_str)
                    .map(str::to_string);

                let data = {
                    let mut guard = session.lock().await;
                    guard.session.ensure_entry_ids();

                    let api_key = guard
                        .agent
                        .stream_options()
                        .api_key
                        .as_deref()
                        .ok_or_else(|| Error::auth("Missing API key for compaction"))?;

                    let provider = guard.agent.provider();

                    let settings = ResolvedCompactionSettings {
                        enabled: options.config.compaction_enabled(),
                        reserve_tokens: options.config.compaction_reserve_tokens(),
                        keep_recent_tokens: options.config.compaction_keep_recent_tokens(),
                    };

                    let path_entries = guard
                        .session
                        .entries_for_current_path()
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>();

                    let prep = prepare_compaction(&path_entries, settings).ok_or_else(|| {
                        Error::session(
                            "Compaction not available (already compacted or missing IDs)",
                        )
                    })?;

                    is_compacting.store(true, Ordering::SeqCst);
                    let result =
                        compact(prep, provider, api_key, custom_instructions.as_deref()).await?;
                    is_compacting.store(false, Ordering::SeqCst);
                    let details_value = compaction_details_to_value(&result.details)?;

                    guard.session.append_compaction(
                        result.summary.clone(),
                        result.first_kept_entry_id.clone(),
                        result.tokens_before,
                        Some(details_value.clone()),
                        None,
                    );
                    guard.persist_session().await?;
                    let messages = guard.session.to_messages_for_current_path();
                    guard.agent.replace_messages(messages);

                    json!({
                        "summary": result.summary,
                        "firstKeptEntryId": result.first_kept_entry_id,
                        "tokensBefore": result.tokens_before,
                        "details": details_value,
                    })
                };

                let _ = out_tx.send(response_ok(id, "compact", Some(data)));
            }

            "new_session" => {
                let parent = parsed
                    .get("parentSession")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                {
                    let mut guard = session.lock().await;
                    let session_dir = guard.session.session_dir.clone();
                    let mut new_session = if guard.save_enabled() {
                        crate::session::Session::create_with_dir(session_dir)
                    } else {
                        crate::session::Session::in_memory()
                    };
                    new_session.header.parent_session = parent;
                    // Keep model fields in header for clients.
                    new_session
                        .header
                        .provider
                        .clone_from(&guard.session.header.provider);
                    new_session
                        .header
                        .model_id
                        .clone_from(&guard.session.header.model_id);
                    new_session
                        .header
                        .thinking_level
                        .clone_from(&guard.session.header.thinking_level);

                    guard.session = new_session;
                    guard.agent.clear_messages();
                    guard.agent.stream_options_mut().session_id =
                        Some(guard.session.header.id.clone());
                }
                {
                    let mut state = shared_state.lock().await;
                    state.steering.clear();
                    state.follow_up.clear();
                }
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

                let loaded = crate::session::Session::open(session_path).await;
                match loaded {
                    Ok(new_session) => {
                        let mut guard = session.lock().await;
                        guard.session = new_session;
                        let messages = guard.session.to_messages_for_current_path();
                        guard.agent.replace_messages(messages);
                        guard.agent.stream_options_mut().session_id =
                            Some(guard.session.header.id.clone());
                        let _ = out_tx.send(response_ok(
                            id,
                            "switch_session",
                            Some(json!({ "cancelled": false })),
                        ));
                        let mut state = shared_state.lock().await;
                        state.steering.clear();
                        state.follow_up.clear();
                    }
                    Err(err) => {
                        let _ = out_tx.send(response_error(id, "switch_session", err.to_string()));
                    }
                }
            }

            "fork" => {
                let Some(entry_id) = parsed.get("entryId").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(id, "fork", "Missing entryId".to_string()));
                    continue;
                };

                let (selected_text, cancelled) = {
                    let mut guard = session.lock().await;
                    fork_session(&mut guard, entry_id)?
                };

                {
                    let mut state = shared_state.lock().await;
                    state.steering.clear();
                    state.follow_up.clear();
                }

                let _ = out_tx.send(response_ok(
                    id,
                    "fork",
                    Some(json!({ "text": selected_text, "cancelled": cancelled })),
                ));
            }

            "get_fork_messages" => {
                let messages = {
                    let guard = session.lock().await;
                    fork_messages(&guard.session)
                };
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
                let _ = out_tx.send(response_ok(id, "extension_ui_response", None));
            }

            _ => {
                let _ = out_tx.send(response_error(
                    id,
                    command_type,
                    format!("Unknown command: {command_type}"),
                ));
            }
        }
    }

    drop(out_tx);
    let _ = writer_task.await;
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
    out_tx: mpsc::UnboundedSender<String>,
    retry_abort_tx: watch::Sender<bool>,
    options: RpcOptions,
    message: String,
    images: Vec<ImageContent>,
) {
    let _ = retry_abort_tx.send(false);
    is_streaming.store(true, Ordering::SeqCst);

    let max_retries = options.config.retry_max_retries();
    let mut retry_count: u32 = 0;
    let mut success = false;
    let mut final_error: Option<String> = None;

    loop {
        let (abort_handle, abort_signal) = AbortHandle::new();
        *abort_handle_slot.lock().await = Some(abort_handle);

        let event_tx = out_tx.clone();
        let event_handler = move |event: AgentEvent| {
            if let Ok(serialized) = serde_json::to_string(&event) {
                let _ = event_tx.send(serialized);
            }
        };

        let result = {
            let mut guard = session.lock().await;
            if images.is_empty() {
                guard
                    .run_text_with_abort(message.clone(), Some(abort_signal), event_handler)
                    .await
            } else {
                let mut blocks = vec![ContentBlock::Text(TextContent::new(message.clone()))];
                for image in &images {
                    blocks.push(ContentBlock::Image(image.clone()));
                }
                guard
                    .run_with_content_with_abort(blocks, Some(abort_signal), event_handler)
                    .await
            }
        };

        *abort_handle_slot.lock().await = None;

        match result {
            Ok(message) => {
                if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                    final_error = message
                        .error_message
                        .clone()
                        .or_else(|| Some("Request error".to_string()));
                    if message.stop_reason == StopReason::Aborted {
                        break;
                    }
                } else {
                    success = true;
                    break;
                }
            }
            Err(err) => {
                final_error = Some(err.to_string());
            }
        }

        let retry_enabled = { shared_state.lock().await.auto_retry_enabled };
        if !retry_enabled || retry_count >= max_retries {
            break;
        }

        retry_count += 1;
        let delay_ms = retry_delay_ms(&options.config, retry_count);
        let error_message = final_error
            .clone()
            .unwrap_or_else(|| "Request error".to_string());
        let _ = out_tx.send(event(&json!({
            "type": "auto_retry_start",
            "attempt": retry_count,
            "maxAttempts": max_retries,
            "delayMs": delay_ms,
            "errorMessage": error_message,
        })));

        let mut abort_rx = retry_abort_tx.subscribe();
        let delay = Duration::from_millis(delay_ms as u64);
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = abort_rx.changed() => {}
        }

        if *abort_rx.borrow() {
            final_error = Some("Retry aborted".to_string());
            break;
        }
    }

    if retry_count > 0 {
        let _ = out_tx.send(event(&json!({
            "type": "auto_retry_end",
            "success": success,
            "attempt": retry_count,
            "finalError": if success { Value::Null } else { json!(final_error.clone()) },
        })));
    }

    is_streaming.store(false, Ordering::SeqCst);

    if !success {
        if let Some(err) = final_error {
            let _ = out_tx.send(event(&json!({
                "type": "agent_end",
                "messages": [],
                "error": err
            })));
        }
        return;
    }

    let auto_compaction_enabled = { shared_state.lock().await.auto_compaction_enabled };
    if auto_compaction_enabled {
        maybe_auto_compact(session, options, is_compacting, out_tx).await;
    }
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

fn event(value: &Value) -> String {
    value.to_string()
}

fn retry_delay_ms(config: &Config, attempt: u32) -> u32 {
    let base = u64::from(config.retry_base_delay_ms());
    let max = u64::from(config.retry_max_delay_ms());
    let shift = attempt.saturating_sub(1);
    let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let delay = base.saturating_mul(multiplier).min(max);
    u32::try_from(delay).unwrap_or(u32::MAX)
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
    out_tx: mpsc::UnboundedSender<String>,
) {
    let (path_entries, context_window, reserve_tokens, settings) = {
        let mut guard = session.lock().await;
        guard.session.ensure_entry_ids();
        let Some(entry) = current_model_entry(&guard.session, &options) else {
            return;
        };

        let reserve_tokens = options.config.compaction_reserve_tokens();
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens,
            keep_recent_tokens: options.config.compaction_keep_recent_tokens(),
        };

        let path_entries = guard
            .session
            .entries_for_current_path()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();

        (
            path_entries,
            entry.model.context_window,
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

    let _ = out_tx.send(event(&json!({
        "type": "auto_compaction_start",
        "reason": "threshold",
    })));
    is_compacting.store(true, Ordering::SeqCst);

    let (provider, api_key) = {
        let guard = session.lock().await;
        let Some(api_key) = guard.agent.stream_options().api_key.clone() else {
            is_compacting.store(false, Ordering::SeqCst);
            let _ = out_tx.send(event(&json!({
                "type": "auto_compaction_end",
                "result": Value::Null,
                "aborted": false,
                "willRetry": false,
                "errorMessage": "Missing API key for compaction",
            })));
            return;
        };
        (guard.agent.provider(), api_key)
    };

    let result = compact(prep, provider, &api_key, None).await;
    is_compacting.store(false, Ordering::SeqCst);

    match result {
        Ok(result) => {
            let details_value = match compaction_details_to_value(&result.details) {
                Ok(value) => value,
                Err(err) => {
                    let _ = out_tx.send(event(&json!({
                        "type": "auto_compaction_end",
                        "result": Value::Null,
                        "aborted": false,
                        "willRetry": false,
                        "errorMessage": err.to_string(),
                    })));
                    return;
                }
            };

            let mut guard = session.lock().await;
            guard.session.append_compaction(
                result.summary.clone(),
                result.first_kept_entry_id.clone(),
                result.tokens_before,
                Some(details_value.clone()),
                None,
            );
            let _ = guard.persist_session().await;
            let messages = guard.session.to_messages_for_current_path();
            guard.agent.replace_messages(messages);
            drop(guard);

            let _ = out_tx.send(event(&json!({
                "type": "auto_compaction_end",
                "result": {
                    "summary": result.summary,
                    "firstKeptEntryId": result.first_kept_entry_id,
                    "tokensBefore": result.tokens_before,
                    "details": details_value,
                },
                "aborted": false,
                "willRetry": false,
            })));
        }
        Err(err) => {
            let _ = out_tx.send(event(&json!({
                "type": "auto_compaction_end",
                "result": Value::Null,
                "aborted": false,
                "willRetry": false,
                "errorMessage": err.to_string(),
            })));
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
    session: &AgentSession,
    options: &RpcOptions,
    snapshot: &RpcStateSnapshot,
    is_streaming: bool,
    is_compacting: bool,
) -> Value {
    let model = session
        .session
        .header
        .provider
        .as_deref()
        .zip(session.session.header.model_id.as_deref())
        .and_then(|(provider, model_id)| {
            options
                .available_models
                .iter()
                .find(|m| m.model.provider == provider && m.model.id == model_id)
        })
        .map(rpc_model_from_entry);

    let message_count = session
        .session
        .entries_for_current_path()
        .iter()
        .filter(|entry| matches!(entry, crate::session::SessionEntry::Message(_)))
        .count();

    let session_name = session
        .session
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
                .session
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
    if let Some(path) = session.session.path.as_ref() {
        state.insert(
            "sessionFile".to_string(),
            Value::String(path.display().to_string()),
        );
    }
    state.insert(
        "sessionId".to_string(),
        Value::String(session.session.header.id.clone()),
    );
    if let Some(session_name) = session_name {
        state.insert("sessionName".to_string(), Value::String(session_name));
    }
    state.insert(
        "autoCompactionEnabled".to_string(),
        Value::Bool(snapshot.auto_compaction_enabled),
    );
    state.insert(
        "messageCount".to_string(),
        Value::Number(message_count.into()),
    );
    state.insert(
        "pendingMessageCount".to_string(),
        Value::Number(snapshot.pending_count().into()),
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
            Message::User(_) => user_messages += 1,
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

    let mut data = serde_json::Map::new();
    if let Some(path) = session.path.as_ref().map(|p| p.display().to_string()) {
        data.insert("sessionFile".to_string(), Value::String(path));
    }
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
        "tokens".to_string(),
        json!({
            "input": total_input,
            "output": total_output,
            "cacheRead": total_cache_read,
            "cacheWrite": total_cache_write,
            "total": total_tokens,
        }),
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

async fn export_html(
    session: &crate::session::Session,
    output_path: Option<&str>,
) -> Result<String> {
    let html = session.to_html();

    let path = output_path.map_or_else(
        || {
            session.path.as_ref().map_or_else(
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
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, html).await?;
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

async fn run_bash_rpc(
    cwd: &std::path::Path,
    command: &str,
    mut abort_rx: oneshot::Receiver<()>,
) -> Result<BashRpcResult> {
    let shell = if std::path::Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "sh"
    };

    let mut child = tokio::process::Command::new(shell)
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::tool("bash", format!("Failed to spawn shell: {e}")))?;

    let child_pid = child.id();
    let mut output_fut = Box::pin(child.wait_with_output());
    let (cancelled, output) = tokio::select! {
        output = &mut output_fut => (false, output),
        _ = &mut abort_rx => {
            kill_process_tree(child_pid);
            (true, output_fut.await)
        }
    };
    let output = output.map_err(|e| Error::tool("bash", format!("Failed to wait for process: {e}")))?;

    let mut combined = Vec::new();
    combined.extend_from_slice(&output.stdout);
    combined.extend_from_slice(&output.stderr);

    let full_output = String::from_utf8_lossy(&combined).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    let truncation = truncate_tail(&full_output, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let truncated = truncation.truncated;

    let (output_text, full_output_path) = if truncated {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let path = std::env::temp_dir().join(format!("pi-rpc-bash-{id}.log"));
        tokio::fs::write(&path, &full_output).await?;
        (truncation.content, Some(path.display().to_string()))
    } else {
        (truncation.content, None)
    };

    let output_text = if output_text.is_empty() {
        "(no output)".to_string()
    } else {
        output_text
    };

    Ok(BashRpcResult {
        output: output_text,
        exit_code,
        cancelled,
        truncated,
        full_output_path,
    })
}

fn kill_process_tree(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    let root = sysinfo::Pid::from_u32(pid);

    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let mut children_map: HashMap<sysinfo::Pid, Vec<sysinfo::Pid>> = HashMap::new();
    for (p, proc_) in sys.processes() {
        if let Some(parent) = proc_.parent() {
            children_map.entry(parent).or_default().push(*p);
        }
    }

    let mut to_kill = Vec::new();
    collect_process_tree(root, &children_map, &mut to_kill);

    // Kill children first.
    for pid in to_kill.into_iter().rev() {
        if let Some(proc_) = sys.process(pid) {
            let _ = proc_.kill();
        }
    }
}

fn collect_process_tree(
    pid: sysinfo::Pid,
    children_map: &HashMap<sysinfo::Pid, Vec<sysinfo::Pid>>,
    out: &mut Vec<sysinfo::Pid>,
) {
    out.push(pid);
    if let Some(children) = children_map.get(&pid) {
        for child in children {
            collect_process_tree(*child, children_map, out);
        }
    }
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

fn resolve_model_api_key(auth: &AuthStorage, entry: &ModelEntry) -> Option<String> {
    auth.resolve_api_key(&entry.model.provider, None)
        .or_else(|| entry.api_key.clone())
}

fn parse_thinking_level(level: &str) -> Result<crate::model::ThinkingLevel> {
    let normalized = level.trim().to_lowercase();
    match normalized.as_str() {
        "off" | "none" | "0" => Ok(crate::model::ThinkingLevel::Off),
        "minimal" | "min" => Ok(crate::model::ThinkingLevel::Minimal),
        "low" | "1" => Ok(crate::model::ThinkingLevel::Low),
        "medium" | "med" | "2" => Ok(crate::model::ThinkingLevel::Medium),
        "high" | "3" => Ok(crate::model::ThinkingLevel::High),
        "xhigh" | "4" => Ok(crate::model::ThinkingLevel::XHigh),
        _ => Err(Error::validation(format!(
            "Invalid thinking level: {level}"
        ))),
    }
}

fn current_model_entry<'a>(
    session: &crate::session::Session,
    options: &'a RpcOptions,
) -> Option<&'a ModelEntry> {
    let provider = session.header.provider.as_deref()?;
    let model_id = session.header.model_id.as_deref()?;
    options
        .available_models
        .iter()
        .find(|m| m.model.provider == provider && m.model.id == model_id)
}

fn clamp_thinking_level(
    thinking: crate::model::ThinkingLevel,
    model_entry: &ModelEntry,
) -> crate::model::ThinkingLevel {
    if !model_entry.model.reasoning {
        return crate::model::ThinkingLevel::Off;
    }
    if thinking == crate::model::ThinkingLevel::XHigh && !supports_xhigh(&model_entry.model.id) {
        return crate::model::ThinkingLevel::High;
    }
    thinking
}

fn supports_xhigh(model_id: &str) -> bool {
    matches!(model_id, "gpt-5.1-codex-max" | "gpt-5.2" | "gpt-5.2-codex")
}

async fn apply_thinking_level(
    guard: &mut AgentSession,
    level: crate::model::ThinkingLevel,
) -> Result<()> {
    guard.session.header.thinking_level = Some(level.to_string());
    guard
        .session
        .append_thinking_level_change(level.to_string());
    guard.agent.stream_options_mut().thinking_level = Some(level);
    guard.persist_session().await
}

async fn apply_model_change(guard: &mut AgentSession, entry: &ModelEntry) -> Result<()> {
    guard.session.header.provider = Some(entry.model.provider.clone());
    guard.session.header.model_id = Some(entry.model.id.clone());
    guard
        .session
        .append_model_change(entry.model.provider.clone(), entry.model.id.clone());
    guard.persist_session().await
}

fn fork_session(guard: &mut AgentSession, entry_id: &str) -> Result<(Option<String>, bool)> {
    let entry = guard
        .session
        .get_entry(entry_id)
        .ok_or_else(|| Error::session("Entry not found"))?;

    let crate::session::SessionEntry::Message(message_entry) = entry else {
        return Err(Error::session("Entry is not a message"));
    };

    let SessionMessage::User { content, .. } = &message_entry.message else {
        return Err(Error::session("Entry is not a user message"));
    };

    let selected_text = extract_user_text(content);
    let parent_id = message_entry.base.parent_id.clone();

    let session_dir = guard.session.session_dir.clone();
    let mut new_session = if guard.save_enabled() {
        crate::session::Session::create_with_dir(session_dir)
    } else {
        crate::session::Session::in_memory()
    };
    new_session.header.parent_session =
        guard.session.path.as_ref().map(|p| p.display().to_string());
    new_session
        .header
        .provider
        .clone_from(&guard.session.header.provider);
    new_session
        .header
        .model_id
        .clone_from(&guard.session.header.model_id);
    new_session
        .header
        .thinking_level
        .clone_from(&guard.session.header.thinking_level);

    if let Some(parent_id) = parent_id {
        let path_ids = guard.session.get_path_to_entry(&parent_id);
        let path_set: HashSet<&str> = path_ids.iter().map(String::as_str).collect();
        new_session.entries = guard
            .session
            .entries
            .iter()
            .filter(|entry| {
                entry
                    .base_id()
                    .is_some_and(|id| path_set.contains(id.as_str()))
            })
            .cloned()
            .collect();
        new_session.leaf_id = Some(parent_id);
    }

    guard.session = new_session;
    guard
        .agent
        .replace_messages(guard.session.to_messages_for_current_path());
    guard.agent.stream_options_mut().session_id = Some(guard.session.header.id.clone());

    Ok((selected_text, false))
}

fn fork_messages(session: &crate::session::Session) -> Vec<Value> {
    let entries = session.entries_for_current_path();
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
        if supports_xhigh(&entry.model.id) {
            levels.push(ThinkingLevel::XHigh);
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

    let current_provider = guard.session.header.provider.as_deref();
    let current_model_id = guard.session.header.model_id.as_deref();

    let current_index = candidates.iter().position(|entry| {
        current_provider == Some(entry.model.provider.as_str())
            && current_model_id == Some(entry.model.id.as_str())
    });

    let next_index = current_index.map_or(0, |idx| (idx + 1) % candidates.len());

    let next_entry = candidates[next_index].clone();
    let provider_impl = crate::providers::create_provider(&next_entry)?;
    guard.agent.set_provider(provider_impl);

    let api_key = resolve_model_api_key(&options.auth, &next_entry).ok_or_else(|| {
        Error::auth(format!(
            "No API key for {}/{}",
            next_entry.model.provider, next_entry.model.id
        ))
    })?;
    guard.agent.stream_options_mut().api_key = Some(api_key);
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

    let next_thinking = clamp_thinking_level(desired_thinking, &next_entry);
    apply_thinking_level(guard, next_thinking).await?;

    Ok(Some((next_entry, next_thinking, is_scoped)))
}
