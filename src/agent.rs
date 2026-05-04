//! Agent runtime - the core orchestration loop.
//!
//! The agent coordinates between:
//! - Provider: Makes LLM API calls
//! - Tools: Executes tool calls from the assistant
//! - Session: Persists conversation history
//!
//! The main loop:
//! 1. Receive user input
//! 2. Build context (system prompt + history + tools)
//! 3. Stream completion from provider
//! 4. If tool calls: execute tools, append results, goto 3
//! 5. If done: return final message

use crate::auth::AuthStorage;
use crate::compaction::{self, ResolvedCompactionSettings};
use crate::compaction_worker::{CompactionQuota, CompactionWorkerState};
use crate::error::{Error, Result};
use crate::extension_events::{
    BeforeAgentStartOutcome, InputEventOutcome, SessionBeforeCompactOutcome,
    apply_before_agent_start_response, apply_input_event_response,
    apply_session_before_compact_response,
};
use crate::extension_tools::collect_extension_tool_wrappers;
use crate::extensions::{
    EXTENSION_EVENT_TIMEOUT_MS, ExtensionDeliverAs, ExtensionEventName, ExtensionHostActions,
    ExtensionLoadSpec, ExtensionManager, ExtensionPolicy, ExtensionRegion, ExtensionRuntimeHandle,
    ExtensionSendMessage, ExtensionSendUserMessage, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
    NativeRustExtensionLoadSpec, NativeRustExtensionRuntimeHandle, RepairPolicyMode,
    resolve_extension_load_spec,
};
#[cfg(feature = "wasm-host")]
use crate::extensions::{WasmExtensionHost, WasmExtensionLoadSpec};
use crate::extensions_js::{PiJsRuntimeConfig, RepairMode};
use crate::model::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, CustomMessage, ImageContent, Message,
    StopReason, StreamEvent, TextContent, ThinkingContent, ToolCall, ToolResultMessage, Usage,
    UserContent, UserMessage,
};
use crate::models::{ModelEntry, ModelRegistry, model_requires_configured_credential};
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use crate::session::{AutosaveFlushTrigger, Session, SessionHandle};
use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
use asupersync::runtime::{Runtime, RuntimeBuilder, RuntimeHandle};
use asupersync::sync::{Mutex, Notify};
use async_trait::async_trait;
use chrono::Utc;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream;
use serde::Serialize;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::warn;

const MIN_READ_ONLY_TOOL_PARALLELISM: usize = 8;
const MAX_AUTO_READ_ONLY_TOOL_PARALLELISM: usize = 64;
const MAX_CONFIGURED_READ_ONLY_TOOL_PARALLELISM: usize = 256;
/// Maximum messages in steering queue to prevent unbounded growth
const MAX_STEERING_QUEUE_SIZE: usize = 100;
/// Maximum messages in follow-up queue to prevent unbounded growth
const MAX_FOLLOW_UP_QUEUE_SIZE: usize = 100;
/// Maximum messages in agent history to prevent unbounded growth
const MAX_AGENT_MESSAGES: usize = 10_000;

fn read_only_tool_parallelism_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        let host_parallelism = std::thread::available_parallelism()
            .map_or(MIN_READ_ONLY_TOOL_PARALLELISM, |parallelism| {
                parallelism.get()
            });
        resolve_read_only_tool_parallelism(
            std::env::var("PI_MAX_CONCURRENT_READ_ONLY_TOOLS")
                .ok()
                .as_deref(),
            host_parallelism,
        )
    })
}

fn resolve_read_only_tool_parallelism(
    raw_override: Option<&str>,
    host_parallelism: usize,
) -> usize {
    let host_default = host_parallelism.clamp(
        MIN_READ_ONLY_TOOL_PARALLELISM,
        MAX_AUTO_READ_ONLY_TOOL_PARALLELISM,
    );

    let Some(raw) = raw_override.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return host_default;
    };

    match raw.parse::<usize>() {
        Ok(0) => {
            warn!(
                value = raw,
                "Ignoring PI_MAX_CONCURRENT_READ_ONLY_TOOLS=0; using host-scaled default"
            );
            host_default
        }
        Ok(limit) => limit.clamp(1, MAX_CONFIGURED_READ_ONLY_TOOL_PARALLELISM),
        Err(err) => {
            warn!(
                value = raw,
                error = %err,
                "Ignoring invalid PI_MAX_CONCURRENT_READ_ONLY_TOOLS; using host-scaled default"
            );
            host_default
        }
    }
}

/// Default cap for tool-call iterations per agent turn.
///
/// Override at runtime via the `--max-tool-iterations` CLI flag, the
/// `PI_MAX_TOOL_ITERATIONS` env var, or by setting the
/// [`AgentConfig::max_tool_iterations`] field directly. Clamped to
/// `[1, MAX_TOOL_ITERATIONS_CEILING]`.
const MAX_TOOL_ITERATIONS_DEFAULT: usize = 50;

/// Sanity ceiling for `max_tool_iterations` overrides — guards against
/// runaway loops while still leaving plenty of room for long, multi-step tasks.
const MAX_TOOL_ITERATIONS_CEILING: usize = 1_000;

/// Resolve the effective tool-iteration cap from `PI_MAX_TOOL_ITERATIONS`,
/// falling back to [`MAX_TOOL_ITERATIONS_DEFAULT`].
pub fn max_tool_iterations_default() -> usize {
    resolve_max_tool_iterations(
        std::env::var("PI_MAX_TOOL_ITERATIONS").ok().as_deref(),
    )
}

/// Fraction (in 80ths) of `max_tool_iterations` at which a soft warning is
/// emitted to the agent so it can begin graceful handoff. Hardcoded at 80%
/// for now; expose as a knob if there's demand.
const ITERATION_WARN_NUMERATOR: usize = 4;
const ITERATION_WARN_DENOMINATOR: usize = 5;

/// Minimum cap below which the soft warning is skipped — for tiny caps
/// (e.g., 3-4) the warning would fire on the first iteration and add noise
/// rather than help.
const ITERATION_WARN_MIN_CAP: usize = 5;

/// Pure predicate: should we emit a one-shot iteration-budget warning to the
/// agent at this `current` iteration, given a configured `max`?
///
/// Fires when `current >= (max * 4) / 5` and `max >= 5`. Caller is
/// responsible for tracking fire-once state via a local flag.
fn should_warn_at_iteration_threshold(current: usize, max: usize) -> bool {
    max >= ITERATION_WARN_MIN_CAP
        && current >= (max * ITERATION_WARN_NUMERATOR) / ITERATION_WARN_DENOMINATOR
}

fn resolve_max_tool_iterations(raw_override: Option<&str>) -> usize {
    let Some(raw) = raw_override.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return MAX_TOOL_ITERATIONS_DEFAULT;
    };
    match raw.parse::<usize>() {
        Ok(0) => {
            warn!(
                value = raw,
                "Ignoring PI_MAX_TOOL_ITERATIONS=0; using default {}",
                MAX_TOOL_ITERATIONS_DEFAULT
            );
            MAX_TOOL_ITERATIONS_DEFAULT
        }
        Ok(limit) => limit.clamp(1, MAX_TOOL_ITERATIONS_CEILING),
        Err(err) => {
            warn!(
                value = raw,
                error = %err,
                "Ignoring invalid PI_MAX_TOOL_ITERATIONS; using default {}",
                MAX_TOOL_ITERATIONS_DEFAULT
            );
            MAX_TOOL_ITERATIONS_DEFAULT
        }
    }
}

// ============================================================================
// Agent Configuration
// ============================================================================

/// Configuration for the agent.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// System prompt to use for all requests.
    pub system_prompt: Option<String>,

    /// Maximum tool call iterations before stopping.
    pub max_tool_iterations: usize,

    /// Default stream options.
    pub stream_options: StreamOptions,

    /// Strip image blocks before sending context to providers.
    pub block_images: bool,

    /// Fail closed when extension tool hooks error or time out.
    pub fail_closed_hooks: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            max_tool_iterations: 50,
            stream_options: StreamOptions::default(),
            block_images: false,
            fail_closed_hooks: false,
        }
    }
}

/// Async fetcher for queued messages (steering or follow-up).
pub type MessageFetcher = Arc<dyn Fn() -> BoxFuture<'static, Vec<Message>> + Send + Sync + 'static>;

type AgentEventHandler = Arc<dyn Fn(AgentEvent) + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    All,
    OneAtATime,
}

impl QueueMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::OneAtATime => "one-at-a-time",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSource {
    Interactive,
    Rpc,
    Extension,
}

impl InputSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Rpc => "rpc",
            Self::Extension => "extension",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum QueueKind {
    Steering,
    FollowUp,
}

#[derive(Debug, Clone)]
struct QueuedMessage {
    seq: u64,
    enqueued_at: i64,
    message: Message,
}

#[derive(Debug)]
struct MessageQueue {
    steering: VecDeque<QueuedMessage>,
    follow_up: VecDeque<QueuedMessage>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    next_seq: u64,
}

impl MessageQueue {
    const fn new(steering_mode: QueueMode, follow_up_mode: QueueMode) -> Self {
        Self {
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            steering_mode,
            follow_up_mode,
            next_seq: 0,
        }
    }

    const fn set_modes(&mut self, steering_mode: QueueMode, follow_up_mode: QueueMode) {
        self.steering_mode = steering_mode;
        self.follow_up_mode = follow_up_mode;
    }

    fn pending_count(&self) -> usize {
        self.steering.len() + self.follow_up.len()
    }

    fn push(&mut self, kind: QueueKind, message: Message) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        let entry = QueuedMessage {
            seq,
            enqueued_at: Utc::now().timestamp_millis(),
            message,
        };
        match kind {
            QueueKind::Steering => {
                if self.steering.len() >= MAX_STEERING_QUEUE_SIZE {
                    tracing::warn!(
                        "Steering queue full ({} messages), dropping oldest message",
                        MAX_STEERING_QUEUE_SIZE
                    );
                    self.steering.pop_front();
                }
                self.steering.push_back(entry);
            }
            QueueKind::FollowUp => {
                if self.follow_up.len() >= MAX_FOLLOW_UP_QUEUE_SIZE {
                    tracing::warn!(
                        "Follow-up queue full ({} messages), dropping oldest message",
                        MAX_FOLLOW_UP_QUEUE_SIZE
                    );
                    self.follow_up.pop_front();
                }
                self.follow_up.push_back(entry);
            }
        }
        seq
    }

    fn push_steering(&mut self, message: Message) -> u64 {
        self.push(QueueKind::Steering, message)
    }

    fn push_follow_up(&mut self, message: Message) -> u64 {
        self.push(QueueKind::FollowUp, message)
    }

    fn pop_steering(&mut self) -> Vec<Message> {
        self.pop_kind(QueueKind::Steering)
    }

    fn pop_follow_up(&mut self) -> Vec<Message> {
        self.pop_kind(QueueKind::FollowUp)
    }

    fn pop_kind(&mut self, kind: QueueKind) -> Vec<Message> {
        let (queue, mode) = match kind {
            QueueKind::Steering => (&mut self.steering, self.steering_mode),
            QueueKind::FollowUp => (&mut self.follow_up, self.follow_up_mode),
        };

        match mode {
            QueueMode::All => queue.drain(..).map(|entry| entry.message).collect(),
            QueueMode::OneAtATime => queue
                .pop_front()
                .into_iter()
                .map(|entry| entry.message)
                .collect(),
        }
    }
}

// ============================================================================
// Agent Event
// ============================================================================

/// Events emitted by the agent during execution.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Agent lifecycle start.
    AgentStart {
        #[serde(rename = "sessionId")]
        session_id: Arc<str>,
    },
    /// Agent lifecycle end with all new messages.
    AgentEnd {
        #[serde(rename = "sessionId")]
        session_id: Arc<str>,
        messages: Vec<Message>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Turn lifecycle start (assistant response + tool calls).
    TurnStart {
        #[serde(rename = "sessionId")]
        session_id: Arc<str>,
        #[serde(rename = "turnIndex")]
        turn_index: usize,
        timestamp: i64,
    },
    /// Turn lifecycle end with tool results.
    TurnEnd {
        #[serde(rename = "sessionId")]
        session_id: Arc<str>,
        #[serde(rename = "turnIndex")]
        turn_index: usize,
        message: Message,
        #[serde(rename = "toolResults")]
        tool_results: Vec<Message>,
    },
    /// Message lifecycle start (user, assistant, or tool result).
    MessageStart { message: Message },
    /// Message update (assistant streaming).
    MessageUpdate {
        message: Message,
        #[serde(rename = "assistantMessageEvent")]
        assistant_message_event: AssistantMessageEvent,
    },
    /// Message lifecycle end.
    MessageEnd { message: Message },
    /// Tool execution start.
    ToolExecutionStart {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        args: serde_json::Value,
    },
    /// Tool execution update.
    ToolExecutionUpdate {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        args: serde_json::Value,
        #[serde(rename = "partialResult")]
        partial_result: ToolOutput,
    },
    /// Tool execution end.
    ToolExecutionEnd {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        result: ToolOutput,
        #[serde(rename = "isError")]
        is_error: bool,
    },
    /// Auto-compaction lifecycle start.
    AutoCompactionStart { reason: String },
    /// Auto-compaction lifecycle end.
    AutoCompactionEnd {
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        aborted: bool,
        #[serde(rename = "willRetry")]
        will_retry: bool,
        #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
    },
    /// Auto-retry lifecycle start.
    AutoRetryStart {
        attempt: u32,
        #[serde(rename = "maxAttempts")]
        max_attempts: u32,
        #[serde(rename = "delayMs")]
        delay_ms: u64,
        #[serde(rename = "errorMessage")]
        error_message: String,
    },
    /// Auto-retry lifecycle end.
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        #[serde(rename = "finalError", skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
    },
    /// Extension error during event dispatch or execution.
    ExtensionError {
        #[serde(rename = "extensionId", skip_serializing_if = "Option::is_none")]
        extension_id: Option<String>,
        event: String,
        error: String,
    },
}

// ============================================================================
// Agent
// ============================================================================

/// Handle to request an abort of an in-flight agent run.
#[derive(Debug, Clone)]
pub struct AbortHandle {
    inner: Arc<AbortSignalInner>,
}

/// Signal for observing abort requests.
#[derive(Debug, Clone)]
pub struct AbortSignal {
    inner: Arc<AbortSignalInner>,
}

#[derive(Debug)]
struct AbortSignalInner {
    aborted: AtomicBool,
    notify: Notify,
}

impl AbortHandle {
    /// Create a new abort handle + signal pair.
    #[must_use]
    pub fn new() -> (Self, AbortSignal) {
        let inner = Arc::new(AbortSignalInner {
            aborted: AtomicBool::new(false),
            notify: Notify::new(),
        });
        (
            Self {
                inner: Arc::clone(&inner),
            },
            AbortSignal { inner },
        )
    }

    /// Trigger an abort.
    pub fn abort(&self) {
        if !self.inner.aborted.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }
}

impl AbortSignal {
    /// Check if an abort has already been requested.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.inner.aborted.load(Ordering::SeqCst)
    }

    pub async fn wait(&self) {
        if self.is_aborted() {
            return;
        }

        loop {
            self.inner.notify.notified().await;
            if self.is_aborted() {
                return;
            }
        }
    }
}

/// The agent runtime that orchestrates LLM calls and tool execution.
pub struct Agent {
    /// The LLM provider.
    provider: Arc<dyn Provider>,

    /// Tool registry.
    tools: ToolRegistry,

    /// Agent configuration.
    config: AgentConfig,

    /// Optional extension manager for tool/event hooks.
    extensions: Option<ExtensionManager>,

    /// Message history.
    messages: Vec<Message>,

    /// Fetchers for queued steering messages (interrupts).
    steering_fetchers: Vec<MessageFetcher>,

    /// Fetchers for queued follow-up messages (idle).
    follow_up_fetchers: Vec<MessageFetcher>,

    /// Internal queue for steering/follow-up messages.
    message_queue: MessageQueue,

    /// Cached tool definitions. Invalidated when tools change via `extend_tools`.
    cached_tool_defs: Option<Vec<ToolDef>>,
}

impl Agent {
    /// Create a new agent with the given provider and tools.
    pub fn new(provider: Arc<dyn Provider>, tools: ToolRegistry, config: AgentConfig) -> Self {
        Self {
            provider,
            tools,
            config,
            extensions: None,
            messages: Vec::new(),
            steering_fetchers: Vec::new(),
            follow_up_fetchers: Vec::new(),
            message_queue: MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime),
            cached_tool_defs: None,
        }
    }

    /// Get the current message history.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Clear the message history.
    pub fn clear_messages(&mut self) {
        self.messages.clear();
    }

    /// Add a message to the history.
    pub fn add_message(&mut self, message: Message) {
        if self.messages.len() >= MAX_AGENT_MESSAGES {
            tracing::warn!(
                "Agent message history full ({} messages), dropping oldest message",
                MAX_AGENT_MESSAGES
            );
            self.messages.remove(0);
        }
        self.messages.push(message);
    }

    /// Replace the message history.
    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    /// Replace the provider implementation (used for model/provider switching).
    pub fn set_provider(&mut self, provider: Arc<dyn Provider>) {
        self.provider = provider;
    }

    /// Register async fetchers for queued steering/follow-up messages.
    ///
    /// This is additive: multiple sources (e.g. RPC, extensions) can register
    /// fetchers, and the agent will poll all of them.
    pub fn register_message_fetchers(
        &mut self,
        steering: Option<MessageFetcher>,
        follow_up: Option<MessageFetcher>,
    ) {
        if let Some(fetcher) = steering {
            self.steering_fetchers.push(fetcher);
        }
        if let Some(fetcher) = follow_up {
            self.follow_up_fetchers.push(fetcher);
        }
    }

    /// Extend the tool registry with additional tools (e.g. extension-registered tools).
    pub fn extend_tools<I>(&mut self, tools: I)
    where
        I: IntoIterator<Item = Box<dyn Tool>>,
    {
        self.tools.extend(tools);
        self.cached_tool_defs = None; // Invalidate cache when tools change
    }

    /// Queue a steering message (delivered after tool completion).
    pub fn queue_steering(&mut self, message: Message) -> u64 {
        self.message_queue.push_steering(message)
    }

    /// Queue a follow-up message (delivered when agent becomes idle).
    pub fn queue_follow_up(&mut self, message: Message) -> u64 {
        self.message_queue.push_follow_up(message)
    }

    /// Configure queue delivery modes.
    pub const fn set_queue_modes(&mut self, steering: QueueMode, follow_up: QueueMode) {
        self.message_queue.set_modes(steering, follow_up);
    }

    pub const fn queue_modes(&self) -> (QueueMode, QueueMode) {
        (
            self.message_queue.steering_mode,
            self.message_queue.follow_up_mode,
        )
    }

    /// Count queued messages (steering + follow-up).
    #[must_use]
    pub fn queued_message_count(&self) -> usize {
        self.message_queue.pending_count()
    }

    pub fn provider(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.provider)
    }

    pub const fn stream_options(&self) -> &StreamOptions {
        &self.config.stream_options
    }

    pub const fn stream_options_mut(&mut self) -> &mut StreamOptions {
        &mut self.config.stream_options
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.config.system_prompt.as_deref()
    }

    pub fn set_system_prompt(&mut self, system_prompt: Option<String>) {
        self.config.system_prompt = system_prompt;
    }

    /// Build context for a completion request.
    fn build_context(&mut self) -> Context<'_> {
        let messages: Cow<'_, [Message]> = if self.config.block_images {
            let mut msgs = self.messages.clone();
            // Filter out hidden custom messages.
            msgs.retain(|m| match m {
                Message::Custom(c) => c.display,
                _ => true,
            });
            let stats = filter_images_for_provider(&mut msgs);
            if stats.removed_images > 0 {
                tracing::debug!(
                    filtered_images = stats.removed_images,
                    affected_messages = stats.affected_messages,
                    "Filtered image content from outbound provider context (images.block_images=true)"
                );
            }
            Cow::Owned(msgs)
        } else {
            // Check if we need to filter hidden custom messages to avoid cloning if not needed.
            let has_hidden = self.messages.iter().any(|m| match m {
                Message::Custom(c) => !c.display,
                _ => false,
            });

            if has_hidden {
                let mut msgs = self.messages.clone();
                msgs.retain(|m| match m {
                    Message::Custom(c) => c.display,
                    _ => true,
                });
                Cow::Owned(msgs)
            } else {
                Cow::Borrowed(self.messages.as_slice())
            }
        };

        // Borrow cached tool defs if available; otherwise build + cache + borrow.
        if self.cached_tool_defs.is_none() {
            let defs: Vec<ToolDef> = self
                .tools
                .tools()
                .iter()
                .map(|t| ToolDef {
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    parameters: t.parameters(),
                })
                .collect();
            self.cached_tool_defs = Some(defs);
        }
        let tools = Cow::Borrowed(self.cached_tool_defs.as_deref().unwrap());

        Context {
            system_prompt: self.config.system_prompt.as_deref().map(Cow::Borrowed),
            messages,
            tools,
        }
    }

    /// Run the agent with a user message.
    ///
    /// Returns a stream of events and the final assistant message.
    pub async fn run(
        &mut self,
        user_input: impl Into<String>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_with_abort(user_input, None, on_event).await
    }

    /// Run the agent with a user message and abort support.
    pub async fn run_with_abort(
        &mut self,
        user_input: impl Into<String>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        // Add user message
        let user_message = Message::User(UserMessage {
            content: UserContent::Text(user_input.into()),
            timestamp: Utc::now().timestamp_millis(),
        });

        // Run the agent loop
        self.run_loop(vec![user_message], Arc::new(on_event), abort)
            .await
    }

    /// Run the agent with structured content (text + images).
    pub async fn run_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_with_content_with_abort(content, None, on_event)
            .await
    }

    /// Run the agent with structured content (text + images) and abort support.
    pub async fn run_with_content_with_abort(
        &mut self,
        content: Vec<ContentBlock>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        // Add user message
        let user_message = Message::User(UserMessage {
            content: UserContent::Blocks(content),
            timestamp: Utc::now().timestamp_millis(),
        });

        // Run the agent loop
        self.run_loop(vec![user_message], Arc::new(on_event), abort)
            .await
    }

    /// Run the agent with a pre-constructed user message and abort support.
    pub async fn run_with_message_with_abort(
        &mut self,
        message: Message,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_loop(vec![message], Arc::new(on_event), abort)
            .await
    }

    /// Run the agent with a pre-constructed prompt list and abort support.
    pub async fn run_with_messages_with_abort(
        &mut self,
        messages: Vec<Message>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_loop(messages, Arc::new(on_event), abort).await
    }

    /// Continue the agent loop without adding a new prompt message (used for retries).
    pub async fn run_continue_with_abort(
        &mut self,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_loop(Vec::new(), Arc::new(on_event), abort).await
    }

    fn build_abort_message(&self, partial: Option<&AssistantMessage>) -> AssistantMessage {
        let mut message = partial.cloned().unwrap_or_else(|| AssistantMessage {
            content: Vec::new(),
            api: self.provider.api().to_string(),
            provider: self.provider.name().to_string(),
            model: self.provider.model_id().to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Aborted,
            error_message: Some("Aborted".to_string()),
            timestamp: Utc::now().timestamp_millis(),
        });
        message.stop_reason = StopReason::Aborted;
        message.error_message = Some("Aborted".to_string());
        message.timestamp = Utc::now().timestamp_millis();
        message
    }

    fn build_error_message(
        &self,
        partial: Option<&AssistantMessage>,
        error_message: impl Into<String>,
    ) -> AssistantMessage {
        let error_message = error_message.into();
        let mut message = partial.cloned().unwrap_or_else(|| AssistantMessage {
            content: Vec::new(),
            api: self.provider.api().to_string(),
            provider: self.provider.name().to_string(),
            model: self.provider.model_id().to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some(error_message.clone()),
            timestamp: Utc::now().timestamp_millis(),
        });
        message.stop_reason = StopReason::Error;
        message.error_message = Some(error_message);
        message.timestamp = Utc::now().timestamp_millis();
        message
    }

    /// The main agent loop.
    #[allow(clippy::too_many_lines)]
    async fn run_loop(
        &mut self,
        prompts: Vec<Message>,
        on_event: AgentEventHandler,
        abort: Option<AbortSignal>,
    ) -> Result<AssistantMessage> {
        let loop_cx = crate::agent_cx::AgentCx::for_current_or_request();
        let session_id: Arc<str> = self
            .config
            .stream_options
            .session_id
            .as_deref()
            .unwrap_or("")
            .into();
        let mut iterations = 0usize;
        let mut warned_at_iteration_threshold = false;
        let mut turn_index: usize = 0;
        let mut new_messages: Vec<Message> = Vec::with_capacity(prompts.len() + 8);
        let mut last_assistant: Option<Arc<AssistantMessage>> = None;

        let agent_start_event = AgentEvent::AgentStart {
            session_id: session_id.clone(),
        };
        self.dispatch_extension_lifecycle_event(&agent_start_event)
            .await;
        on_event(agent_start_event);

        for prompt in prompts {
            self.messages.push(prompt.clone());
            on_event(AgentEvent::MessageStart {
                message: prompt.clone(),
            });
            on_event(AgentEvent::MessageEnd {
                message: prompt.clone(),
            });
            new_messages.push(prompt);
        }

        // Delivery boundary: start of turn (steering messages queued while idle).
        let mut pending_messages = self.drain_steering_messages().await;

        loop {
            let mut has_more_tool_calls = true;
            let mut steering_after_tools: Option<Vec<Message>> = None;

            while has_more_tool_calls || !pending_messages.is_empty() {
                let current_turn_index = turn_index;
                let turn_start_event = AgentEvent::TurnStart {
                    session_id: session_id.clone(),
                    turn_index: current_turn_index,
                    timestamp: Utc::now().timestamp_millis(),
                };
                self.dispatch_extension_lifecycle_event(&turn_start_event)
                    .await;
                on_event(turn_start_event);

                for message in std::mem::take(&mut pending_messages) {
                    self.messages.push(message.clone());
                    on_event(AgentEvent::MessageStart {
                        message: message.clone(),
                    });
                    on_event(AgentEvent::MessageEnd {
                        message: message.clone(),
                    });
                    new_messages.push(message);
                }

                if abort.as_ref().is_some_and(AbortSignal::is_aborted) {
                    let abort_message = self.build_abort_message(None);
                    let message = Message::assistant(abort_message.clone());

                    self.messages.push(message.clone());
                    new_messages.push(message.clone());
                    on_event(AgentEvent::MessageStart {
                        message: message.clone(),
                    });
                    on_event(AgentEvent::MessageEnd {
                        message: message.clone(),
                    });

                    let turn_end_event = AgentEvent::TurnEnd {
                        session_id: session_id.clone(),
                        turn_index: current_turn_index,
                        message,
                        tool_results: Vec::new(),
                    };
                    self.dispatch_extension_lifecycle_event(&turn_end_event)
                        .await;
                    on_event(turn_end_event);
                    let agent_end_event = AgentEvent::AgentEnd {
                        session_id: session_id.clone(),
                        messages: std::mem::take(&mut new_messages),
                        error: Some(
                            abort_message
                                .error_message
                                .clone()
                                .unwrap_or_else(|| "Aborted".to_string()),
                        ),
                    };
                    self.dispatch_extension_lifecycle_event(&agent_end_event)
                        .await;
                    on_event(agent_end_event);
                    return Ok(abort_message);
                }

                let assistant_message = match self
                    .stream_assistant_response(Arc::clone(&on_event), abort.clone(), &loop_cx)
                    .await
                {
                    Ok(msg) => msg,
                    Err(err) => {
                        let err_string = err.to_string();
                        let steering_to_add = self.drain_steering_messages().await;
                        for message in steering_to_add {
                            self.messages.push(message.clone());
                            on_event(AgentEvent::MessageStart {
                                message: message.clone(),
                            });
                            on_event(AgentEvent::MessageEnd {
                                message: message.clone(),
                            });
                            new_messages.push(message);
                        }

                        let error_message = self.build_error_message(None, err_string.clone());
                        let assistant_event_message = Message::assistant(error_message.clone());
                        self.messages.push(assistant_event_message.clone());
                        new_messages.push(assistant_event_message.clone());
                        on_event(AgentEvent::MessageStart {
                            message: assistant_event_message.clone(),
                        });
                        on_event(AgentEvent::MessageEnd {
                            message: assistant_event_message.clone(),
                        });

                        let turn_end_event = AgentEvent::TurnEnd {
                            session_id: session_id.clone(),
                            turn_index: current_turn_index,
                            message: assistant_event_message,
                            tool_results: Vec::new(),
                        };
                        self.dispatch_extension_lifecycle_event(&turn_end_event)
                            .await;
                        on_event(turn_end_event);

                        let agent_end_event = AgentEvent::AgentEnd {
                            session_id: session_id.clone(),
                            messages: std::mem::take(&mut new_messages),
                            error: Some(err_string),
                        };
                        self.dispatch_extension_lifecycle_event(&agent_end_event)
                            .await;
                        on_event(agent_end_event);
                        return Err(err);
                    }
                };
                // Wrap in Arc once; share via Arc::clone (O(1)) instead of deep
                // cloning the full AssistantMessage for every consumer.
                let assistant_arc = Arc::new(assistant_message);
                last_assistant = Some(Arc::clone(&assistant_arc));

                let assistant_event_message = Message::Assistant(Arc::clone(&assistant_arc));
                new_messages.push(assistant_event_message.clone());

                if matches!(
                    assistant_arc.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    let steering_to_add = self.drain_steering_messages().await;
                    for message in steering_to_add {
                        self.messages.push(message.clone());
                        on_event(AgentEvent::MessageStart {
                            message: message.clone(),
                        });
                        on_event(AgentEvent::MessageEnd {
                            message: message.clone(),
                        });
                        new_messages.push(message);
                    }

                    let turn_end_event = AgentEvent::TurnEnd {
                        session_id: session_id.clone(),
                        turn_index: current_turn_index,
                        message: assistant_event_message.clone(),
                        tool_results: Vec::new(),
                    };
                    self.dispatch_extension_lifecycle_event(&turn_end_event)
                        .await;
                    on_event(turn_end_event);
                    let agent_end_event = AgentEvent::AgentEnd {
                        session_id: session_id.clone(),
                        messages: std::mem::take(&mut new_messages),
                        error: assistant_arc.error_message.clone(),
                    };
                    self.dispatch_extension_lifecycle_event(&agent_end_event)
                        .await;
                    on_event(agent_end_event);
                    return Ok(Arc::unwrap_or_clone(assistant_arc));
                }

                let tool_calls = extract_tool_calls(&assistant_arc.content);
                has_more_tool_calls = !tool_calls.is_empty();

                let mut tool_results: Vec<Arc<ToolResultMessage>> = Vec::new();
                if has_more_tool_calls {
                    iterations += 1;
                    if !warned_at_iteration_threshold
                        && should_warn_at_iteration_threshold(
                            iterations,
                            self.config.max_tool_iterations,
                        )
                    {
                        warned_at_iteration_threshold = true;
                        let warning = Message::User(UserMessage {
                            content: UserContent::Text(format!(
                                "[runtime] Tool-iteration budget at \u{2265}80% (used {} of {}). \
                                 Per the iteration-aware-handoff protocol in your spec, begin \
                                 graceful handoff now: commit current work, post a one-line \
                                 status note, and write an `incomplete-handoff` envelope with \
                                 what's done / what remains / next-agent starting position. \
                                 Do NOT compress remaining work into the last few iterations.",
                                iterations, self.config.max_tool_iterations,
                            )),
                            timestamp: 0,
                        });
                        self.message_queue.push_steering(warning);
                        tracing::warn!(
                            iterations,
                            max = self.config.max_tool_iterations,
                            "tool-iteration budget at \u{2265}80%; injected handoff steering message"
                        );
                    }
                    if iterations > self.config.max_tool_iterations {
                        let error_message = format!(
                            "Maximum tool iterations ({}) exceeded",
                            self.config.max_tool_iterations
                        );
                        let mut stop_message = (*assistant_arc).clone();
                        stop_message.stop_reason = StopReason::Error;
                        stop_message.error_message = Some(error_message.clone());

                        // Strip dangling tool calls to prevent sequence mismatch on next user prompt.
                        stop_message
                            .content
                            .retain(|b| !matches!(b, crate::model::ContentBlock::ToolCall(_)));

                        let stop_arc = Arc::new(stop_message.clone());
                        let stop_event_message = Message::Assistant(Arc::clone(&stop_arc));

                        // Keep in-memory transcript and event payloads aligned with the
                        // error stop result returned to callers.
                        if let Some(last @ Message::Assistant(_)) = self
                            .messages
                            .iter_mut()
                            .rev()
                            .find(|m| matches!(m, Message::Assistant(_)))
                        {
                            *last = stop_event_message.clone();
                        }
                        if let Some(last @ Message::Assistant(_)) = new_messages.last_mut() {
                            *last = stop_event_message.clone();
                        }

                        let steering_to_add = self.drain_steering_messages().await;
                        for message in steering_to_add {
                            self.messages.push(message.clone());
                            on_event(AgentEvent::MessageStart {
                                message: message.clone(),
                            });
                            on_event(AgentEvent::MessageEnd {
                                message: message.clone(),
                            });
                            new_messages.push(message);
                        }

                        let turn_end_event = AgentEvent::TurnEnd {
                            session_id: session_id.clone(),
                            turn_index: current_turn_index,
                            message: stop_event_message,
                            tool_results: Vec::new(),
                        };
                        self.dispatch_extension_lifecycle_event(&turn_end_event)
                            .await;
                        on_event(turn_end_event);

                        let agent_end_event = AgentEvent::AgentEnd {
                            session_id: session_id.clone(),
                            messages: std::mem::take(&mut new_messages),
                            error: Some(error_message),
                        };
                        self.dispatch_extension_lifecycle_event(&agent_end_event)
                            .await;
                        on_event(agent_end_event);

                        return Ok(stop_message);
                    }

                    let outcome = match self
                        .execute_tool_calls(
                            &tool_calls,
                            Arc::clone(&on_event),
                            &mut new_messages,
                            abort.clone(),
                        )
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            let steering_to_add = self.drain_steering_messages().await;
                            for message in steering_to_add {
                                self.messages.push(message.clone());
                                on_event(AgentEvent::MessageStart {
                                    message: message.clone(),
                                });
                                on_event(AgentEvent::MessageEnd {
                                    message: message.clone(),
                                });
                                new_messages.push(message);
                            }

                            let turn_end_event = AgentEvent::TurnEnd {
                                session_id: session_id.clone(),
                                turn_index: current_turn_index,
                                message: assistant_event_message.clone(),
                                tool_results: Vec::new(),
                            };
                            self.dispatch_extension_lifecycle_event(&turn_end_event)
                                .await;
                            on_event(turn_end_event);

                            let agent_end_event = AgentEvent::AgentEnd {
                                session_id: session_id.clone(),
                                messages: std::mem::take(&mut new_messages),
                                error: Some(err.to_string()),
                            };
                            self.dispatch_extension_lifecycle_event(&agent_end_event)
                                .await;
                            on_event(agent_end_event);
                            return Err(err);
                        }
                    };
                    tool_results = outcome.tool_results;
                    steering_after_tools = outcome.steering_messages;
                }

                let tool_messages = tool_results
                    .iter()
                    .map(|r| Message::ToolResult(Arc::clone(r)))
                    .collect::<Vec<_>>();

                let turn_end_event = AgentEvent::TurnEnd {
                    session_id: session_id.clone(),
                    turn_index: current_turn_index,
                    message: assistant_event_message.clone(),
                    tool_results: tool_messages,
                };
                self.dispatch_extension_lifecycle_event(&turn_end_event)
                    .await;
                on_event(turn_end_event);

                turn_index = turn_index.saturating_add(1);

                if let Some(steering) = steering_after_tools.take() {
                    pending_messages = steering;
                } else {
                    // Delivery boundary: after assistant completion (no tool calls).
                    pending_messages = self.drain_steering_messages().await;
                }
            }

            // Delivery boundary: agent idle (after all tool calls + steering).
            let follow_up = self.drain_follow_up_messages().await;
            if follow_up.is_empty() {
                break;
            }
            pending_messages = follow_up;
        }

        let Some(final_arc) = last_assistant else {
            return Err(Error::api("Agent completed without assistant message"));
        };

        let agent_end_event = AgentEvent::AgentEnd {
            session_id: session_id.clone(),
            messages: new_messages,
            error: None,
        };
        self.dispatch_extension_lifecycle_event(&agent_end_event)
            .await;
        on_event(agent_end_event);
        Ok(Arc::unwrap_or_clone(final_arc))
    }

    async fn fetch_messages(&self, fetcher: Option<&MessageFetcher>) -> Vec<Message> {
        if let Some(fetcher) = fetcher {
            (fetcher)().await
        } else {
            Vec::new()
        }
    }

    async fn dispatch_extension_lifecycle_event(&self, event: &AgentEvent) {
        let Some(extensions) = &self.extensions else {
            return;
        };

        let name = match event {
            AgentEvent::AgentStart { .. } => ExtensionEventName::AgentStart,
            AgentEvent::AgentEnd { .. } => ExtensionEventName::AgentEnd,
            AgentEvent::TurnStart { .. } => ExtensionEventName::TurnStart,
            AgentEvent::TurnEnd { .. } => ExtensionEventName::TurnEnd,
            _ => return,
        };

        let payload = match serde_json::to_value(event) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::warn!("failed to serialize agent lifecycle event (fail-open): {err}");
                return;
            }
        };

        if let Err(err) = extensions.dispatch_event(name, Some(payload)).await {
            tracing::warn!("agent lifecycle extension hook failed (fail-open): {err}");
        }
    }

    async fn dispatch_context_event(&self, messages: &[Message]) -> Option<Vec<Message>> {
        let Some(extensions) = &self.extensions else {
            return None;
        };

        let payload = json!({ "messages": messages });
        let response = extensions
            .dispatch_event_with_response(
                ExtensionEventName::Context,
                Some(payload),
                EXTENSION_EVENT_TIMEOUT_MS,
            )
            .await
            .ok()?;

        let value = response?;

        if value.is_null() {
            return None;
        }

        let messages_value = if let Some(obj) = value.as_object() {
            obj.get("messages").cloned()?
        } else if value.is_array() {
            value
        } else {
            return None;
        };

        if messages_value.is_null() {
            return Some(Vec::new());
        }

        match serde_json::from_value(messages_value) {
            Ok(messages) => Some(messages),
            Err(err) => {
                tracing::warn!("context extension hook returned invalid messages: {err}");
                None
            }
        }
    }

    async fn drain_steering_messages(&mut self) -> Vec<Message> {
        for fetcher in &self.steering_fetchers {
            let fetched = self.fetch_messages(Some(fetcher)).await;
            for message in fetched {
                self.message_queue.push_steering(message);
            }
        }
        self.message_queue.pop_steering()
    }

    async fn drain_follow_up_messages(&mut self) -> Vec<Message> {
        for fetcher in &self.follow_up_fetchers {
            let fetched = self.fetch_messages(Some(fetcher)).await;
            for message in fetched {
                self.message_queue.push_follow_up(message);
            }
        }
        self.message_queue.pop_follow_up()
    }

    /// Stream an assistant response and emit message events.
    #[allow(clippy::too_many_lines)]
    async fn stream_assistant_response(
        &mut self,
        on_event: AgentEventHandler,
        abort: Option<AbortSignal>,
        checkpoint_cx: &crate::agent_cx::AgentCx,
    ) -> Result<AssistantMessage> {
        // Build context and stream completion
        let provider = Arc::clone(&self.provider);
        let stream_options = self.config.stream_options.clone();
        let (system_prompt, tools, base_messages) = {
            let context = self.build_context();
            (
                context.system_prompt.as_deref().map(str::to_string),
                context.tools.to_vec(),
                context.messages.to_vec(),
            )
        };
        let messages = self
            .dispatch_context_event(&base_messages)
            .await
            .unwrap_or(base_messages);
        let context = Context::owned(system_prompt, messages, tools);
        let mut stream = provider.stream(&context, &stream_options).await?;

        let mut added_partial = false;
        // Track whether we've already emitted `MessageStart` for this streaming response.
        // Avoids cloning the full message on every event just to re-emit a redundant start.
        let mut sent_start = false;

        'stream: loop {
            if checkpoint_cx.checkpoint().is_err() {
                let last_partial = if added_partial {
                    match self
                        .messages
                        .iter()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        Some(Message::Assistant(a)) => Some(a.as_ref()),
                        _ => None,
                    }
                } else {
                    None
                };
                let abort_arc = Arc::new(self.build_abort_message(last_partial));
                if !sent_start {
                    on_event(AgentEvent::MessageStart {
                        message: Message::Assistant(Arc::clone(&abort_arc)),
                    });
                    self.messages
                        .push(Message::Assistant(Arc::clone(&abort_arc)));
                    added_partial = true;
                }
                on_event(AgentEvent::MessageUpdate {
                    message: Message::Assistant(Arc::clone(&abort_arc)),
                    assistant_message_event: AssistantMessageEvent::Error {
                        reason: StopReason::Aborted,
                        error: Arc::clone(&abort_arc),
                    },
                });
                return Ok(self.finalize_assistant_message(
                    Arc::try_unwrap(abort_arc).unwrap_or_else(|a| (*a).clone()),
                    &on_event,
                    added_partial,
                ));
            }

            let event_result = if let Some(signal) = abort.as_ref() {
                let abort_fut = signal.wait().fuse();
                let event_fut = stream.next().fuse();
                futures::pin_mut!(abort_fut, event_fut);

                match futures::future::select(abort_fut, event_fut).await {
                    futures::future::Either::Left(((), _event_fut)) => {
                        let last_partial = if added_partial {
                            match self
                                .messages
                                .iter()
                                .rev()
                                .find(|m| matches!(m, Message::Assistant(_)))
                            {
                                Some(Message::Assistant(a)) => Some(a.as_ref()),
                                _ => None,
                            }
                        } else {
                            None
                        };
                        let abort_arc = Arc::new(self.build_abort_message(last_partial));
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&abort_arc)),
                            });
                            self.messages
                                .push(Message::Assistant(Arc::clone(&abort_arc)));
                            added_partial = true;
                            // We do NOT set sent_start = true here because we are returning immediately,
                            // but setting added_partial = true prevents finalize_assistant_message from
                            // emitting a second MessageStart.
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&abort_arc)),
                            assistant_message_event: AssistantMessageEvent::Error {
                                reason: StopReason::Aborted,
                                error: Arc::clone(&abort_arc),
                            },
                        });
                        return Ok(self.finalize_assistant_message(
                            Arc::try_unwrap(abort_arc).unwrap_or_else(|a| (*a).clone()),
                            &on_event,
                            added_partial,
                        ));
                    }
                    futures::future::Either::Right((event, _abort_fut)) => event,
                }
            } else {
                let event_fut = stream.next().fuse();
                futures::pin_mut!(event_fut);
                loop {
                    let now = checkpoint_cx
                        .cx()
                        .timer_driver()
                        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
                    let tick_fut =
                        asupersync::time::sleep(now, std::time::Duration::from_millis(25)).fuse();
                    futures::pin_mut!(tick_fut);

                    match futures::future::select(tick_fut, &mut event_fut).await {
                        futures::future::Either::Left(((), _event_fut)) => {
                            if checkpoint_cx.checkpoint().is_err() {
                                continue 'stream;
                            }
                        }
                        futures::future::Either::Right((result, _tick_fut)) => break result,
                    }
                }
            };

            let Some(event_result) = event_result else {
                break;
            };
            let event = match event_result {
                Ok(e) => e,
                Err(err) => {
                    let partial = if added_partial {
                        match self
                            .messages
                            .iter()
                            .rev()
                            .find(|m| matches!(m, Message::Assistant(_)))
                        {
                            Some(Message::Assistant(a)) => Some(a.as_ref()),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    let msg = self.build_error_message(partial, err.to_string());

                    // If we never sent a Start event, finalize_assistant_message handles it.
                    // But if sent_start is true and added_partial is somehow false,
                    // finalize_assistant_message will emit a second Start. That shouldn't happen.
                    return Ok(self.finalize_assistant_message(msg, &on_event, added_partial));
                }
            };

            match event {
                StreamEvent::Start { partial } => {
                    if added_partial {
                        if let Some(Message::Assistant(msg_arc)) = self
                            .messages
                            .iter_mut()
                            .rev()
                            .find(|m| matches!(m, Message::Assistant(_)))
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if msg.content.is_empty() {
                                *msg = partial;
                            } else {
                                msg.api = partial.api;
                                msg.provider = partial.provider;
                                msg.model = partial.model;
                                msg.usage = partial.usage;
                                msg.stop_reason = partial.stop_reason;
                                msg.error_message = partial.error_message;
                                msg.timestamp = partial.timestamp;
                            }
                            let shared = Arc::clone(msg_arc);
                            if !sent_start {
                                on_event(AgentEvent::MessageStart {
                                    message: Message::Assistant(Arc::clone(&shared)),
                                });
                                sent_start = true;
                            }
                            on_event(AgentEvent::MessageUpdate {
                                message: Message::Assistant(Arc::clone(&shared)),
                                assistant_message_event: AssistantMessageEvent::Start {
                                    partial: shared,
                                },
                            });
                        } else {
                            let shared = Arc::new(partial);
                            self.update_partial_message(Arc::clone(&shared), &mut added_partial);
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                            on_event(AgentEvent::MessageUpdate {
                                message: Message::Assistant(Arc::clone(&shared)),
                                assistant_message_event: AssistantMessageEvent::Start {
                                    partial: shared,
                                },
                            });
                        }
                    } else {
                        let shared = Arc::new(partial);
                        self.update_partial_message(Arc::clone(&shared), &mut added_partial);
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(Arc::clone(&shared)),
                        });
                        sent_start = true;
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::Start {
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::TextStart { content_index, .. } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        let msg = Arc::make_mut(msg_arc);
                        if content_index == msg.content.len() {
                            msg.content.push(ContentBlock::Text(TextContent::new("")));
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::TextStart {
                                content_index,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::TextDelta {
                    content_index,
                    delta,
                    ..
                } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if msg.content.get(content_index).is_none()
                                && content_index == msg.content.len()
                            {
                                msg.content.push(ContentBlock::Text(TextContent::new("")));
                            }
                            if let Some(ContentBlock::Text(text)) =
                                msg.content.get_mut(content_index)
                            {
                                text.text.push_str(&delta);
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::TextDelta {
                                content_index,
                                delta,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::TextEnd {
                    content_index,
                    content,
                    ..
                } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if msg.content.get(content_index).is_none()
                                && content_index == msg.content.len()
                            {
                                msg.content.push(ContentBlock::Text(TextContent::new("")));
                            }
                            if let Some(ContentBlock::Text(text)) =
                                msg.content.get_mut(content_index)
                            {
                                text.text.clone_from(&content);
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::TextEnd {
                                content_index,
                                content,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::ThinkingStart { content_index, .. } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        let msg = Arc::make_mut(msg_arc);
                        if content_index == msg.content.len() {
                            msg.content.push(ContentBlock::Thinking(ThinkingContent {
                                thinking: String::new(),
                                thinking_signature: None,
                            }));
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::ThinkingStart {
                                content_index,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::ThinkingDelta {
                    content_index,
                    delta,
                    ..
                } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if msg.content.get(content_index).is_none()
                                && content_index == msg.content.len()
                            {
                                msg.content.push(ContentBlock::Thinking(ThinkingContent {
                                    thinking: String::new(),
                                    thinking_signature: None,
                                }));
                            }
                            if let Some(ContentBlock::Thinking(thinking)) =
                                msg.content.get_mut(content_index)
                            {
                                thinking.thinking.push_str(&delta);
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                                content_index,
                                delta,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::ThinkingEnd {
                    content_index,
                    content,
                    ..
                } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if msg.content.get(content_index).is_none()
                                && content_index == msg.content.len()
                            {
                                msg.content.push(ContentBlock::Thinking(ThinkingContent {
                                    thinking: String::new(),
                                    thinking_signature: None,
                                }));
                            }
                            if let Some(ContentBlock::Thinking(thinking)) =
                                msg.content.get_mut(content_index)
                            {
                                thinking.thinking.clone_from(&content);
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::ThinkingEnd {
                                content_index,
                                content,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::ToolCallStart { content_index, .. } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        let msg = Arc::make_mut(msg_arc);
                        if content_index == msg.content.len() {
                            msg.content.push(ContentBlock::ToolCall(ToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: serde_json::Value::Null,
                                thought_signature: None,
                            }));
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::ToolCallStart {
                                content_index,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::ToolCallDelta {
                    content_index,
                    delta,
                    ..
                } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        if msg_arc.content.get(content_index).is_none()
                            && content_index == msg_arc.content.len()
                        {
                            let msg = Arc::make_mut(msg_arc);
                            msg.content.push(ContentBlock::ToolCall(ToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: serde_json::Value::Null,
                                thought_signature: None,
                            }));
                        }
                        // No mutation needed for ToolCallDelta – args stay Null until ToolCallEnd.
                        // Just share the current Arc (O(1) refcount bump, zero deep copies).
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::ToolCallDelta {
                                content_index,
                                delta,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::ToolCallEnd {
                    content_index,
                    tool_call,
                    ..
                } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if msg.content.get(content_index).is_none()
                                && content_index == msg.content.len()
                            {
                                msg.content.push(ContentBlock::ToolCall(ToolCall {
                                    id: String::new(),
                                    name: String::new(),
                                    arguments: serde_json::Value::Null,
                                    thought_signature: None,
                                }));
                            }
                            if let Some(ContentBlock::ToolCall(tc)) =
                                msg.content.get_mut(content_index)
                            {
                                *tc = tool_call.clone();
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::ToolCallEnd {
                                content_index,
                                tool_call,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::Done { message, .. } => {
                    return Ok(self.finalize_assistant_message(message, &on_event, added_partial));
                }
                StreamEvent::Error { error, .. } => {
                    return Ok(self.finalize_assistant_message(error, &on_event, added_partial));
                }
            }
        }

        // If the stream ends without a Done/Error event, we may have a partial message.
        // Instead of discarding it, we finalize it with an error state so the user/session
        // retains the partial content.
        if added_partial {
            if let Some(Message::Assistant(last_msg)) = self
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m, Message::Assistant(_)))
            {
                let mut final_msg = (**last_msg).clone();
                final_msg.stop_reason = StopReason::Error;
                final_msg.error_message = Some("Stream ended without Done event".to_string());
                return Ok(self.finalize_assistant_message(final_msg, &on_event, true));
            }
        }
        Err(Error::api("Stream ended without Done event"))
    }

    /// Ensure we have a fresh assistant message for the current stream.
    ///
    /// Some providers/extensions can emit deltas without a Start event; without
    /// this guard we would mutate the previous assistant message instead.
    fn seed_partial_message_if_missing(&mut self, added_partial: &mut bool) {
        if *added_partial {
            return;
        }

        let message = AssistantMessage {
            content: Vec::new(),
            api: self.provider.api().to_string(),
            provider: self.provider.name().to_string(),
            model: self.provider.model_id().to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: Utc::now().timestamp_millis(),
        };
        self.messages.push(Message::Assistant(Arc::new(message)));
        *added_partial = true;
    }

    /// Update the partial assistant message in `self.messages`.
    ///
    /// Takes an `Arc<AssistantMessage>` and moves it into the message list
    /// (one Arc move, zero deep-copies).
    fn update_partial_message(
        &mut self,
        partial: Arc<AssistantMessage>,
        added_partial: &mut bool,
    ) -> bool {
        if *added_partial {
            if let Some(target) = self
                .messages
                .iter_mut()
                .rev()
                .find(|m| matches!(m, Message::Assistant(_)))
            {
                *target = Message::Assistant(partial);
            } else {
                // Defensive: added_partial is true but no Assistant message found.
                // Push as new message rather than silently dropping the update.
                tracing::warn!("update_partial_message: expected an Assistant message in history");
                self.messages.push(Message::Assistant(partial));
            }
            false
        } else {
            self.messages.push(Message::Assistant(partial));
            *added_partial = true;
            true
        }
    }

    fn finalize_assistant_message(
        &mut self,
        message: AssistantMessage,
        on_event: &Arc<dyn Fn(AgentEvent) + Send + Sync>,
        added_partial: bool,
    ) -> AssistantMessage {
        let arc = Arc::new(message);
        if added_partial {
            if let Some(target) = self
                .messages
                .iter_mut()
                .rev()
                .find(|m| matches!(m, Message::Assistant(_)))
            {
                *target = Message::Assistant(Arc::clone(&arc));
            } else {
                // Defensive: added_partial is true but no Assistant message found.
                // Push as new message rather than overwriting an unrelated message.
                tracing::warn!(
                    "finalize_assistant_message: expected an Assistant message in history"
                );
                self.messages.push(Message::Assistant(Arc::clone(&arc)));
                on_event(AgentEvent::MessageStart {
                    message: Message::Assistant(Arc::clone(&arc)),
                });
            }
        } else {
            self.messages.push(Message::Assistant(Arc::clone(&arc)));
            on_event(AgentEvent::MessageStart {
                message: Message::Assistant(Arc::clone(&arc)),
            });
        }

        on_event(AgentEvent::MessageEnd {
            message: Message::Assistant(Arc::clone(&arc)),
        });
        Arc::try_unwrap(arc).unwrap_or_else(|a| (*a).clone())
    }

    async fn execute_parallel_batch(
        &self,
        batch: Vec<(usize, ToolCall)>,
        on_event: AgentEventHandler,
        abort: Option<AbortSignal>,
    ) -> Vec<(usize, (ToolOutput, bool))> {
        let parallelism = read_only_tool_parallelism_limit();
        let futures = batch.into_iter().map(|(idx, tc)| {
            let on_event = Arc::clone(&on_event);
            async move { (idx, self.execute_tool_owned(tc, on_event).await) }
        });

        if let Some(signal) = abort.as_ref() {
            use futures::future::{Either, select};
            let all_fut = stream::iter(futures)
                .buffer_unordered(parallelism)
                .collect::<Vec<_>>()
                .fuse();
            let abort_fut = signal.wait().fuse();
            futures::pin_mut!(all_fut, abort_fut);

            match select(all_fut, abort_fut).await {
                Either::Left((batch_results, _)) => batch_results,
                Either::Right(_) => Vec::new(), // Aborted
            }
        } else {
            stream::iter(futures)
                .buffer_unordered(parallelism)
                .collect::<Vec<_>>()
                .await
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_tool_calls(
        &mut self,
        tool_calls: &[ToolCall],
        on_event: AgentEventHandler,
        new_messages: &mut Vec<Message>,
        abort: Option<AbortSignal>,
    ) -> Result<ToolExecutionOutcome> {
        let mut results = Vec::new();
        let mut steering_messages: Option<Vec<Message>> = None;

        // Phase 1: Emit start events for ALL tools up front.
        for tool_call in tool_calls {
            on_event(AgentEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            });
        }

        // Phase 2: Execute tools with safety barriers.
        let mut pending_parallel: Vec<(usize, ToolCall)> = Vec::new();
        let mut recorded_results: Vec<Option<Arc<ToolResultMessage>>> =
            vec![None; tool_calls.len()];

        // Iterate through tools. If read-only, buffer. If unsafe, flush buffer then run unsafe.
        for (index, tool_call) in tool_calls.iter().enumerate() {
            if abort.as_ref().is_some_and(AbortSignal::is_aborted) {
                break;
            }

            let is_read_only =
                matches!(self.tools.get(&tool_call.name), Some(tool) if tool.is_read_only());

            if is_read_only {
                pending_parallel.push((index, tool_call.clone()));
            } else {
                // Check steering BEFORE flushing parallel or running unsafe.
                let steering = self.drain_steering_messages().await;
                if !steering.is_empty() {
                    steering_messages = Some(steering);
                    break;
                }

                // Barrier: flush parallel buffer first
                if !pending_parallel.is_empty() {
                    let batch = std::mem::take(&mut pending_parallel);
                    let mut results = self
                        .execute_parallel_batch(batch, Arc::clone(&on_event), abort.clone())
                        .await;
                    results.sort_by_key(|(idx, _)| *idx);
                    for (idx, (output, is_error)) in results {
                        recorded_results[idx] = Some(self.record_tool_result(
                            &tool_calls[idx],
                            output,
                            is_error,
                            &on_event,
                            new_messages,
                        ));
                    }
                }

                if abort.as_ref().is_some_and(AbortSignal::is_aborted) {
                    break;
                }

                // Execute unsafe tool sequentially
                // Check steering AGAIN before the potentially expensive unsafe tool
                let steering = self.drain_steering_messages().await;
                if !steering.is_empty() {
                    steering_messages = Some(steering);
                    break;
                }

                // Race tool execution against the abort signal so that a
                // long-running (or hanging) tool is cancelled promptly.
                if let Some(signal) = abort.as_ref() {
                    let maybe_result = {
                        use futures::future::{Either, select};
                        let tool_fut = self
                            .execute_tool(tool_call.clone(), Arc::clone(&on_event))
                            .fuse();
                        let abort_fut = signal.wait().fuse();
                        futures::pin_mut!(tool_fut, abort_fut);
                        match select(tool_fut, abort_fut).await {
                            Either::Left((result, _)) => Some(result),
                            Either::Right(_) => None,
                        }
                    };
                    let Some((output, is_error)) = maybe_result else {
                        // Abort fired; leave this slot unrecorded so Phase 3
                        // emits the aborted tool-result marker in order.
                        break;
                    };
                    recorded_results[index] = Some(self.record_tool_result(
                        tool_call,
                        output,
                        is_error,
                        &on_event,
                        new_messages,
                    ));
                } else {
                    let (output, is_error) = self
                        .execute_tool(tool_call.clone(), Arc::clone(&on_event))
                        .await;
                    recorded_results[index] = Some(self.record_tool_result(
                        tool_call,
                        output,
                        is_error,
                        &on_event,
                        new_messages,
                    ));
                }
            }
        }

        // Flush remaining parallel tools
        if !pending_parallel.is_empty()
            && !abort.as_ref().is_some_and(AbortSignal::is_aborted)
            && steering_messages.is_none()
        {
            let batch = std::mem::take(&mut pending_parallel);
            // Check steering one last time before final flush
            let steering = self.drain_steering_messages().await;
            if steering.is_empty() {
                let mut results = self
                    .execute_parallel_batch(batch, Arc::clone(&on_event), abort.clone())
                    .await;
                results.sort_by_key(|(idx, _)| *idx);
                for (idx, (output, is_error)) in results {
                    recorded_results[idx] = Some(self.record_tool_result(
                        &tool_calls[idx],
                        output,
                        is_error,
                        &on_event,
                        new_messages,
                    ));
                }
            } else {
                steering_messages = Some(steering);
            }
        }

        // Phase 3: Process results sequentially and handle skips.
        for (index, tool_call) in tool_calls.iter().enumerate() {
            // Check for new steering if we haven't already found some.
            // This catches steering messages that arrived during the *last* tool's execution.
            if steering_messages.is_none() && !abort.as_ref().is_some_and(AbortSignal::is_aborted) {
                let steering = self.drain_steering_messages().await;
                if !steering.is_empty() {
                    steering_messages = Some(steering);
                }
            }

            // If a result was recorded during execution, keep outcome ordering
            // without re-emitting lifecycle events or duplicating transcript entries.
            if let Some(tool_result) = recorded_results[index].take() {
                results.push(tool_result);
            } else if steering_messages.is_some() {
                // Skipped due to steering.
                results.push(self.skip_tool_call(tool_call, &on_event, new_messages));
            } else {
                // Aborted or otherwise failed to run (e.g. abort signal).
                let output = ToolOutput {
                    content: vec![ContentBlock::Text(TextContent::new(
                        "Tool execution aborted",
                    ))],
                    details: None,
                    is_error: true,
                };

                on_event(AgentEvent::ToolExecutionUpdate {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    args: tool_call.arguments.clone(),
                    partial_result: ToolOutput {
                        content: output.content.clone(),
                        details: output.details.clone(),
                        is_error: true,
                    },
                });

                on_event(AgentEvent::ToolExecutionEnd {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    result: ToolOutput {
                        content: output.content.clone(),
                        details: output.details.clone(),
                        is_error: true,
                    },
                    is_error: true,
                });

                let tool_result = Arc::new(ToolResultMessage {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    content: output.content,
                    details: output.details,
                    is_error: true,
                    timestamp: Utc::now().timestamp_millis(),
                });

                let msg = Message::ToolResult(Arc::clone(&tool_result));
                self.messages.push(msg.clone());
                on_event(AgentEvent::MessageStart {
                    message: msg.clone(),
                });
                let end_msg = msg.clone();
                new_messages.push(msg);
                on_event(AgentEvent::MessageEnd { message: end_msg });

                results.push(tool_result);
            }
        }

        Ok(ToolExecutionOutcome {
            tool_results: results,
            steering_messages,
        })
    }

    fn record_tool_result(
        &mut self,
        tool_call: &ToolCall,
        output: ToolOutput,
        is_error: bool,
        on_event: &AgentEventHandler,
        new_messages: &mut Vec<Message>,
    ) -> Arc<ToolResultMessage> {
        on_event(AgentEvent::ToolExecutionUpdate {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
            partial_result: ToolOutput {
                content: output.content.clone(),
                details: output.details.clone(),
                is_error,
            },
        });

        let tool_result = Arc::new(ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            content: output.content,
            details: output.details,
            is_error,
            timestamp: Utc::now().timestamp_millis(),
        });

        on_event(AgentEvent::ToolExecutionEnd {
            tool_call_id: tool_result.tool_call_id.clone(),
            tool_name: tool_result.tool_name.clone(),
            result: ToolOutput {
                content: tool_result.content.clone(),
                details: tool_result.details.clone(),
                is_error,
            },
            is_error,
        });

        let msg = Message::ToolResult(Arc::clone(&tool_result));
        self.messages.push(msg.clone());
        on_event(AgentEvent::MessageStart {
            message: msg.clone(),
        });
        new_messages.push(msg.clone());
        on_event(AgentEvent::MessageEnd { message: msg });

        tool_result
    }

    async fn execute_tool(
        &self,
        tool_call: ToolCall,
        on_event: AgentEventHandler,
    ) -> (ToolOutput, bool) {
        let extensions = self.extensions.clone();

        let (mut output, is_error) = if let Some(extensions) = &extensions {
            match Self::dispatch_tool_call_hook(
                extensions,
                &tool_call,
                self.config.fail_closed_hooks,
            )
            .await
            {
                Some(blocked_output) => (blocked_output, true),
                None => {
                    self.execute_tool_without_hooks(&tool_call, Arc::clone(&on_event))
                        .await
                }
            }
        } else {
            self.execute_tool_without_hooks(&tool_call, Arc::clone(&on_event))
                .await
        };

        if let Some(extensions) = &extensions {
            Self::apply_tool_result_hook(extensions, &tool_call, &mut output, is_error).await;
        }

        (output, is_error)
    }

    async fn execute_tool_owned(
        &self,
        tool_call: ToolCall,
        on_event: AgentEventHandler,
    ) -> (ToolOutput, bool) {
        self.execute_tool(tool_call, on_event).await
    }

    async fn execute_tool_without_hooks(
        &self,
        tool_call: &ToolCall,
        on_event: AgentEventHandler,
    ) -> (ToolOutput, bool) {
        // Find the tool
        let Some(tool) = self.tools.get(&tool_call.name) else {
            return (Self::tool_not_found_output(&tool_call.name), true);
        };

        let tool_name = tool_call.name.clone();
        let tool_id = tool_call.id.clone();
        let tool_args = tool_call.arguments.clone();
        let on_event = Arc::clone(&on_event);

        let update_callback = move |update: ToolUpdate| {
            on_event(AgentEvent::ToolExecutionUpdate {
                tool_call_id: tool_id.clone(),
                tool_name: tool_name.clone(),
                args: tool_args.clone(),
                partial_result: ToolOutput {
                    content: update.content,
                    details: update.details,
                    is_error: false,
                },
            });
        };

        match tool
            .execute(
                &tool_call.id,
                tool_call.arguments.clone(),
                Some(Box::new(update_callback)),
            )
            .await
        {
            Ok(output) => {
                let is_error = output.is_error;
                (output, is_error)
            }
            Err(e) => (
                ToolOutput {
                    content: vec![ContentBlock::Text(TextContent::new(format!("Error: {e}")))],
                    details: None,
                    is_error: true,
                },
                true,
            ),
        }
    }

    fn tool_not_found_output(tool_name: &str) -> ToolOutput {
        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "Error: Tool '{tool_name}' not found"
            )))],
            details: None,
            is_error: true,
        }
    }

    async fn dispatch_tool_call_hook(
        extensions: &ExtensionManager,
        tool_call: &ToolCall,
        fail_closed_hooks: bool,
    ) -> Option<ToolOutput> {
        match extensions
            .dispatch_tool_call(tool_call, EXTENSION_EVENT_TIMEOUT_MS)
            .await
        {
            Ok(Some(result)) if result.block => {
                Some(Self::tool_call_blocked_output(result.reason.as_deref()))
            }
            Ok(_) => None,
            Err(err) => {
                if fail_closed_hooks {
                    tracing::warn!(
                        error = ?err,
                        "tool_call extension hook failed (fail-closed)"
                    );
                    Some(Self::tool_call_blocked_output(Some(
                        "extension hook failed",
                    )))
                } else {
                    tracing::warn!("tool_call extension hook failed (fail-open): {err}");
                    None
                }
            }
        }
    }

    fn tool_call_blocked_output(reason: Option<&str>) -> ToolOutput {
        let reason = reason.map(str::trim).filter(|reason| !reason.is_empty());
        let message = reason.map_or_else(
            || "Tool execution was blocked by an extension".to_string(),
            |reason| format!("Tool execution blocked: {reason}"),
        );

        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(message))],
            details: None,
            is_error: true,
        }
    }

    async fn apply_tool_result_hook(
        extensions: &ExtensionManager,
        tool_call: &ToolCall,
        output: &mut ToolOutput,
        is_error: bool,
    ) {
        match extensions
            .dispatch_tool_result(tool_call, &*output, is_error, EXTENSION_EVENT_TIMEOUT_MS)
            .await
        {
            Ok(Some(result)) => {
                if let Some(content) = result.content {
                    output.content = content;
                }
                if let Some(details) = result.details {
                    output.details = Some(details);
                }
            }
            Ok(None) => {}
            Err(err) => tracing::warn!("tool_result extension hook failed (fail-open): {err}"),
        }
    }

    fn skip_tool_call(
        &mut self,
        tool_call: &ToolCall,
        on_event: &Arc<dyn Fn(AgentEvent) + Send + Sync>,
        new_messages: &mut Vec<Message>,
    ) -> Arc<ToolResultMessage> {
        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(
                "Skipped due to queued user message.",
            ))],
            details: None,
            is_error: true,
        };

        // Note: Phase 1 already emitted ToolExecutionStart for all tools,
        // so we only emit Update and End here.
        on_event(AgentEvent::ToolExecutionUpdate {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
            partial_result: output.clone(),
        });
        on_event(AgentEvent::ToolExecutionEnd {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            result: output.clone(),
            is_error: true,
        });

        let tool_result = Arc::new(ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            content: output.content,
            details: output.details,
            is_error: true,
            timestamp: Utc::now().timestamp_millis(),
        });

        let msg = Message::ToolResult(Arc::clone(&tool_result));
        self.messages.push(msg.clone());
        new_messages.push(msg.clone());

        on_event(AgentEvent::MessageStart {
            message: msg.clone(),
        });
        on_event(AgentEvent::MessageEnd { message: msg });

        tool_result
    }
}

// ============================================================================
// Agent Session (Agent + Session persistence)
// ============================================================================

struct ToolExecutionOutcome {
    tool_results: Vec<Arc<ToolResultMessage>>,
    steering_messages: Option<Vec<Message>>,
}

/// Pre-created extension runtime state for overlapping startup I/O.
///
/// By spawning runtime boot as a background task *before* session creation and
/// model selection, expensive runtime startup can overlap with other work.
pub struct PreWarmedExtensionRuntime {
    /// The extension manager (already has `cwd` and risk config set).
    pub manager: ExtensionManager,
    /// The booted runtime handle.
    pub runtime: ExtensionRuntimeHandle,
    /// The tool registry passed to the runtime during boot.
    pub tools: Arc<ToolRegistry>,
}

/// RAII guard that resets an `AtomicBool` to `false` on drop, ensuring the
/// flag is cleared even if the enclosing async task is cancelled.
struct AtomicBoolGuard(Arc<AtomicBool>);

impl AtomicBoolGuard {
    fn activate(flag: &Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::SeqCst);
        Self(Arc::clone(flag))
    }
}

impl Drop for AtomicBoolGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

pub struct AgentSession {
    pub agent: Agent,
    pub session: Arc<Mutex<Session>>,
    save_enabled: bool,
    input_source: InputSource,
    /// Extension lifecycle region — ensures the JS runtime thread is shut
    /// down when the session ends.
    pub extensions: Option<ExtensionRegion>,
    extensions_is_streaming: Arc<AtomicBool>,
    extensions_is_compacting: Arc<AtomicBool>,
    extensions_turn_active: Arc<AtomicBool>,
    extensions_pending_idle_actions: Arc<StdMutex<VecDeque<PendingIdleAction>>>,
    extension_queue_modes: Option<Arc<StdMutex<ExtensionQueueModeState>>>,
    extension_injected_queue: Option<Arc<StdMutex<ExtensionInjectedQueue>>>,
    compaction_settings: ResolvedCompactionSettings,
    compaction_runtime: Option<Runtime>,
    runtime_handle: Option<RuntimeHandle>,
    compaction_worker: CompactionWorkerState,
    model_registry: Option<ModelRegistry>,
    auth_storage: Option<AuthStorage>,
}

#[derive(Debug, Clone, Copy)]
struct ExtensionQueueModeState {
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
}

impl ExtensionQueueModeState {
    const fn new(steering_mode: QueueMode, follow_up_mode: QueueMode) -> Self {
        Self {
            steering_mode,
            follow_up_mode,
        }
    }

    const fn set_modes(&mut self, steering_mode: QueueMode, follow_up_mode: QueueMode) {
        self.steering_mode = steering_mode;
        self.follow_up_mode = follow_up_mode;
    }
}

#[derive(Debug)]
struct ExtensionInjectedQueue {
    steering: VecDeque<Message>,
    follow_up: VecDeque<Message>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
}

impl ExtensionInjectedQueue {
    const fn new(steering_mode: QueueMode, follow_up_mode: QueueMode) -> Self {
        Self {
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            steering_mode,
            follow_up_mode,
        }
    }

    const fn set_modes(&mut self, steering_mode: QueueMode, follow_up_mode: QueueMode) {
        self.steering_mode = steering_mode;
        self.follow_up_mode = follow_up_mode;
    }

    fn push_steering(&mut self, message: Message) {
        if self.steering.len() >= MAX_STEERING_QUEUE_SIZE {
            tracing::warn!(
                "Extension steering queue full ({} messages), dropping oldest message",
                MAX_STEERING_QUEUE_SIZE
            );
            self.steering.pop_front();
        }
        self.steering.push_back(message);
    }

    fn push_follow_up(&mut self, message: Message) {
        if self.follow_up.len() >= MAX_FOLLOW_UP_QUEUE_SIZE {
            tracing::warn!(
                "Extension follow-up queue full ({} messages), dropping oldest message",
                MAX_FOLLOW_UP_QUEUE_SIZE
            );
            self.follow_up.pop_front();
        }
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

impl Default for ExtensionInjectedQueue {
    fn default() -> Self {
        Self::new(QueueMode::OneAtATime, QueueMode::OneAtATime)
    }
}

#[derive(Debug)]
enum PendingIdleAction {
    CustomMessage(Message),
    UserText(String),
}

#[derive(Clone)]
struct AgentSessionHostActions {
    session: Arc<Mutex<Session>>,
    injected: Arc<StdMutex<ExtensionInjectedQueue>>,
    is_streaming: Arc<AtomicBool>,
    is_turn_active: Arc<AtomicBool>,
    pending_idle_actions: Arc<StdMutex<VecDeque<PendingIdleAction>>>,
}

impl AgentSessionHostActions {
    fn enqueue(&self, deliver_as: Option<ExtensionDeliverAs>, message: Message) {
        let deliver_as = deliver_as.unwrap_or(ExtensionDeliverAs::Steer);
        let Ok(mut queue) = self.injected.lock() else {
            tracing::error!("injected queue mutex poisoned; dropping extension message");
            return;
        };
        match deliver_as {
            ExtensionDeliverAs::FollowUp => {
                queue.push_follow_up(message);
            }
            ExtensionDeliverAs::Steer | ExtensionDeliverAs::NextTurn => {
                queue.push_steering(message);
            }
        }
    }

    async fn append_to_session(&self, message: Message) -> Result<()> {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let mut session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        session.append_model_message(message);
        Ok(())
    }

    fn queue_pending_idle_action(&self, action: PendingIdleAction) {
        let Ok(mut actions) = self.pending_idle_actions.lock() else {
            tracing::error!("pending idle actions mutex poisoned; dropping idle action");
            return;
        };
        actions.push_back(action);
    }
}

#[async_trait]
impl ExtensionHostActions for AgentSessionHostActions {
    async fn send_message(&self, message: ExtensionSendMessage) -> Result<()> {
        let custom_message = Message::Custom(CustomMessage {
            content: message.content,
            custom_type: message.custom_type,
            display: message.display,
            details: message.details,
            timestamp: Utc::now().timestamp_millis(),
        });

        if matches!(message.deliver_as, Some(ExtensionDeliverAs::NextTurn)) {
            return self.append_to_session(custom_message).await;
        }

        if self.is_streaming.load(Ordering::SeqCst) {
            self.enqueue(message.deliver_as, custom_message);
            return Ok(());
        }

        if self.is_turn_active.load(Ordering::SeqCst) {
            return self.append_to_session(custom_message).await;
        }

        if message.trigger_turn {
            self.queue_pending_idle_action(PendingIdleAction::CustomMessage(custom_message));
            return Ok(());
        }

        self.append_to_session(custom_message).await
    }

    async fn send_user_message(&self, message: ExtensionSendUserMessage) -> Result<()> {
        let text = message.text;
        let user_message = Message::User(UserMessage {
            content: UserContent::Text(text.clone()),
            timestamp: Utc::now().timestamp_millis(),
        });

        if self.is_streaming.load(Ordering::SeqCst) {
            self.enqueue(message.deliver_as, user_message);
            return Ok(());
        }

        if self.is_turn_active.load(Ordering::SeqCst) {
            return self.append_to_session(user_message).await;
        }

        self.queue_pending_idle_action(PendingIdleAction::UserText(text));
        Ok(())
    }
}

#[cfg(test)]
mod message_queue_tests {
    use super::*;

    fn user_message(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    #[test]
    fn message_queue_one_at_a_time() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(user_message("a"));
        queue.push_steering(user_message("b"));

        let first = queue.pop_steering();
        assert_eq!(first.len(), 1);
        assert!(matches!(
            first.first(),
            Some(Message::User(UserMessage { content, .. }))
                if matches!(content, UserContent::Text(text) if text == "a")
        ));

        let second = queue.pop_steering();
        assert_eq!(second.len(), 1);
        assert!(matches!(
            second.first(),
            Some(Message::User(UserMessage { content, .. }))
                if matches!(content, UserContent::Text(text) if text == "b")
        ));

        assert!(queue.pop_steering().is_empty());
    }

    #[test]
    fn message_queue_all_mode() {
        let mut queue = MessageQueue::new(QueueMode::All, QueueMode::OneAtATime);
        queue.push_steering(user_message("a"));
        queue.push_steering(user_message("b"));

        let drained = queue.pop_steering();
        assert_eq!(drained.len(), 2);
        assert!(queue.pop_steering().is_empty());
    }

    #[test]
    fn message_queue_separates_kinds() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(user_message("steer"));
        queue.push_follow_up(user_message("follow"));

        let steering = queue.pop_steering();
        assert_eq!(steering.len(), 1);
        assert_eq!(queue.pending_count(), 1);

        let follow = queue.pop_follow_up();
        assert_eq!(follow.len(), 1);
        assert_eq!(queue.pending_count(), 0);
    }

    #[test]
    fn message_queue_seq_increments() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        let first = queue.push_steering(user_message("a"));
        let second = queue.push_follow_up(user_message("b"));
        assert!(second > first);
    }

    #[test]
    fn message_queue_seq_saturates_at_u64_max() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.next_seq = u64::MAX;

        let first = queue.push_steering(user_message("a"));
        let second = queue.push_follow_up(user_message("b"));

        assert_eq!(first, u64::MAX);
        assert_eq!(second, u64::MAX);
        assert_eq!(queue.pending_count(), 2);
    }

    #[test]
    fn message_queue_follow_up_all_mode_drains_entire_queue_in_order() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::All);
        queue.push_follow_up(user_message("f1"));
        queue.push_follow_up(user_message("f2"));

        let follow_up = queue.pop_follow_up();
        assert_eq!(follow_up.len(), 2);
        assert!(matches!(
            follow_up.first(),
            Some(Message::User(UserMessage { content, .. }))
                if matches!(content, UserContent::Text(text) if text == "f1")
        ));
        assert!(matches!(
            follow_up.get(1),
            Some(Message::User(UserMessage { content, .. }))
                if matches!(content, UserContent::Text(text) if text == "f2")
        ));
        assert!(queue.pop_follow_up().is_empty());
    }
}

#[cfg(test)]
mod read_only_tool_parallelism_tests {
    use super::*;

    #[test]
    fn read_only_tool_parallelism_preserves_historical_floor() {
        assert_eq!(resolve_read_only_tool_parallelism(None, 1), 8);
        assert_eq!(resolve_read_only_tool_parallelism(None, 8), 8);
    }

    #[test]
    fn read_only_tool_parallelism_scales_on_many_core_hosts() {
        assert_eq!(resolve_read_only_tool_parallelism(None, 32), 32);
        assert_eq!(resolve_read_only_tool_parallelism(None, 64), 64);
        assert_eq!(resolve_read_only_tool_parallelism(None, 128), 64);
    }

    #[test]
    fn read_only_tool_parallelism_accepts_bounded_override() {
        assert_eq!(resolve_read_only_tool_parallelism(Some("16"), 4), 16);
        assert_eq!(resolve_read_only_tool_parallelism(Some("512"), 64), 256);
        assert_eq!(resolve_read_only_tool_parallelism(Some("1"), 64), 1);
    }

    #[test]
    fn read_only_tool_parallelism_ignores_invalid_override() {
        assert_eq!(
            resolve_read_only_tool_parallelism(Some("not-a-number"), 24),
            24
        );
        assert_eq!(resolve_read_only_tool_parallelism(Some("0"), 24), 24);
        assert_eq!(resolve_read_only_tool_parallelism(Some(" "), 24), 24);
    }
}

#[cfg(test)]
mod extensions_integration_tests {
    use super::*;

    use crate::session::Session;
    use asupersync::runtime::RuntimeBuilder;
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::json;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

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
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[derive(Debug)]
    struct IdleCommandProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for IdleCommandProvider {
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
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let partial = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            };
            let done = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(
                    "resumed-response-0".to_string(),
                ))],
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: done,
                }),
            ])))
        }
    }

    #[derive(Debug)]
    struct CountingTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            "count_tool"
        }

        fn label(&self) -> &str {
            "count_tool"
        }

        fn description(&self) -> &str {
            "counting tool"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({ "type": "object" })
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> Result<ToolOutput> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("ok"))],
                details: None,
                is_error: false,
            })
        }
    }

    #[derive(Debug)]
    struct ToolUseProvider {
        stream_calls: AtomicUsize,
    }

    impl ToolUseProvider {
        const fn new() -> Self {
            Self {
                stream_calls: AtomicUsize::new(0),
            }
        }

        fn assistant_message(
            &self,
            stop_reason: StopReason,
            content: Vec<ContentBlock>,
        ) -> AssistantMessage {
            AssistantMessage {
                content,
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason,
                error_message: None,
                timestamp: 0,
            }
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ToolUseProvider {
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
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let call_index = self.stream_calls.fetch_add(1, Ordering::SeqCst);

            let partial = self.assistant_message(StopReason::Stop, Vec::new());

            let (reason, message) = if call_index == 0 {
                let tool_calls = vec![
                    ToolCall {
                        id: "call-1".to_string(),
                        name: "count_tool".to_string(),
                        arguments: json!({}),
                        thought_signature: None,
                    },
                    ToolCall {
                        id: "call-2".to_string(),
                        name: "count_tool".to_string(),
                        arguments: json!({}),
                        thought_signature: None,
                    },
                ];

                (
                    StopReason::ToolUse,
                    self.assistant_message(
                        StopReason::ToolUse,
                        tool_calls
                            .into_iter()
                            .map(ContentBlock::ToolCall)
                            .collect::<Vec<_>>(),
                    ),
                )
            } else {
                (
                    StopReason::Stop,
                    self.assistant_message(
                        StopReason::Stop,
                        vec![ContentBlock::Text(TextContent::new("done"))],
                    ),
                )
            };

            let events = vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done { reason, message }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[test]
    fn agent_session_enable_extensions_registers_extension_tools() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "hello_tool",
                    label: "hello_tool",
                    description: "test tool",
                    parameters: { type: "object", properties: { name: { type: "string" } } },
                    execute: async (_callId, input, _onUpdate, _abort, ctx) => {
                      const who = input && input.name ? String(input.name) : "world";
                      const cwd = ctx && ctx.cwd ? String(ctx.cwd) : "";
                      return {
                        content: [{ type: "text", text: `hello ${who}` }],
                        details: { from: "extension", cwd: cwd },
                        isError: false
                      };
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool = agent_session
                .agent
                .tools
                .get("hello_tool")
                .expect("hello_tool registered");

            let output = tool
                .execute("call-1", json!({ "name": "pi" }), None)
                .await
                .expect("execute tool");

            assert!(!output.is_error);
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected single text content block, got {:?}",
                output.content
            );
            let [ContentBlock::Text(text)] = output.content.as_slice() else {
                return;
            };
            assert_eq!(text.text, "hello pi");

            let details = output.details.expect("details present");
            assert_eq!(
                details.get("from").and_then(serde_json::Value::as_str),
                Some("extension")
            );
        });
    }

    #[test]
    fn agent_session_enable_extensions_with_no_entries_clears_and_is_noop() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            // Manually inject a dummy extension state to verify clearing behavior.
            let dummy_manager = ExtensionManager::new();
            agent_session.extensions = Some(crate::extensions::ExtensionRegion::new(dummy_manager.clone()));
            agent_session.agent.extensions = Some(dummy_manager.clone());
            agent_session.extension_queue_modes = Some(Arc::new(std::sync::Mutex::new(ExtensionQueueModeState::new(
                QueueMode::OneAtATime,
                QueueMode::OneAtATime,
            ))));
            agent_session.extension_injected_queue = Some(Arc::new(std::sync::Mutex::new(ExtensionInjectedQueue::default())));

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[])
                .await
                .expect("empty extension list should be a no-op");

            assert!(
                agent_session.extensions.is_none(),
                "no extension region should be created (and existing should be cleared) for an empty extension list"
            );
            assert!(
                agent_session.agent.extensions.is_none(),
                "agent should not report extensions active when nothing was requested"
            );
            assert!(
                agent_session.extension_queue_modes.is_none(),
                "empty extension list should clear queue mode mirrors"
            );
            assert!(
                agent_session.extension_injected_queue.is_none(),
                "empty extension list should clear injected extension queues"
            );
        });
    }

    #[test]
    fn agent_session_enable_extensions_rejects_mixed_js_and_native_entries() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let js_entry = temp_dir.path().join("ext.mjs");
            let native_entry = temp_dir.path().join("ext.native.json");
            std::fs::write(
                &js_entry,
                r"
                export default function init(_pi) {}
                ",
            )
            .expect("write js extension entry");
            std::fs::write(&native_entry, "{}").expect("write native extension descriptor");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            let err = agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[js_entry, native_entry])
                .await
                .expect_err("mixed extension runtimes should be rejected");
            let msg = err.to_string();
            assert!(
                msg.contains("Mixed extension runtimes are not supported"),
                "unexpected mixed-runtime error message: {msg}"
            );
        });
    }

    #[test]
    fn extension_send_message_persists_custom_message_entry_when_idle() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "emit_message",
                    label: "emit_message",
                    description: "emit a custom message",
                    parameters: { type: "object" },
                    execute: async () => {
                      pi.sendMessage({
                        customType: "note",
                        content: "hello",
                        display: true,
                        details: { from: "test" }
                      }, {});
                      return { content: [{ type: "text", text: "ok" }], isError: false };
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool = agent_session
                .agent
                .tools
                .get("emit_message")
                .expect("emit_message registered");

            let _ = tool
                .execute("call-1", json!({}), None)
                .await
                .expect("execute tool");

            let cx = crate::agent_cx::AgentCx::for_request();
            let session_guard = session.lock(cx.cx()).await.expect("lock session");
            let messages = session_guard.to_messages_for_current_path();

            assert!(
                messages.iter().any(|msg| {
                    matches!(
                        msg,
                        Message::Custom(CustomMessage { custom_type, content, display, details, .. })
                            if custom_type == "note"
                                && content == "hello"
                                && *display
                                && details.as_ref().and_then(|v| v.get("from").and_then(Value::as_str)) == Some("test")
                    )
                }),
                "expected custom message to be persisted, got {messages:?}"
            );
        });
    }

    #[test]
    fn extension_send_message_persists_custom_message_entry_when_idle_after_await() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "emit_message",
                    label: "emit_message",
                    description: "emit a custom message",
                    parameters: { type: "object" },
                    execute: async () => {
                      await Promise.resolve();
                      pi.sendMessage({
                        customType: "note",
                        content: "hello-after-await",
                        display: true,
                        details: { from: "test" }
                      }, {});
                      return { content: [{ type: "text", text: "ok" }], isError: false };
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool = agent_session
                .agent
                .tools
                .get("emit_message")
                .expect("emit_message registered");

            let _ = tool
                .execute("call-1", json!({}), None)
                .await
                .expect("execute tool");

            let cx = crate::agent_cx::AgentCx::for_request();
            let session_guard = session.lock(cx.cx()).await.expect("lock session");
            let messages = session_guard.to_messages_for_current_path();

            assert!(
                messages.iter().any(|msg| {
                    matches!(
                        msg,
                        Message::Custom(CustomMessage { custom_type, content, display, details, .. })
                            if custom_type == "note"
                                && content == "hello-after-await"
                                && *display
                                && details.as_ref().and_then(|v| v.get("from").and_then(Value::as_str)) == Some("test")
                    )
                }),
                "expected custom message to be persisted, got {messages:?}"
            );
        });
    }

    #[test]
    fn agent_host_actions_send_message_inherits_cancelled_context_when_locked() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let actions = AgentSessionHostActions {
                session: Arc::clone(&session),
                injected: Arc::new(StdMutex::new(ExtensionInjectedQueue::default())),
                is_streaming: Arc::new(AtomicBool::new(false)),
                is_turn_active: Arc::new(AtomicBool::new(false)),
                pending_idle_actions: Arc::new(StdMutex::new(VecDeque::new())),
            };

            let hold_cx = crate::agent_cx::AgentCx::for_request();
            let held_guard = session.lock(hold_cx.cx()).await.expect("lock session");

            let ambient_cx = asupersync::Cx::for_testing();
            ambient_cx.set_cancel_requested(true);
            let _current = asupersync::Cx::set_current(Some(ambient_cx));
            let inner = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_millis(100),
                actions.send_message(ExtensionSendMessage {
                    extension_id: Some("ext".to_string()),
                    custom_type: "note".to_string(),
                    content: "blocked".to_string(),
                    display: false,
                    details: None,
                    deliver_as: Some(ExtensionDeliverAs::NextTurn),
                    trigger_turn: false,
                }),
            )
            .await;
            let outcome = inner.expect("cancelled helper should finish before timeout");
            let err = outcome.expect_err("session append should fail under inherited cancellation");
            assert!(
                err.to_string().contains("mutex lock cancelled"),
                "unexpected error: {err}"
            );

            drop(held_guard);

            let cx = crate::agent_cx::AgentCx::for_request();
            let guard = session.lock(cx.cx()).await.expect("lock session");
            assert!(
                guard.to_messages_for_current_path().is_empty(),
                "cancelled send_message should not append a message"
            );
        });
    }

    #[test]
    fn extension_command_send_message_trigger_turn_runs_agent_turn_when_idle() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerCommand("emit-now", {
                    description: "emit a custom message and trigger a turn",
                    handler: async () => {
                      await pi.events("sendMessage", {
                        message: {
                          customType: "note",
                          content: "turn-now",
                          display: true
                        },
                        options: {
                          deliverAs: "steer",
                          triggerTurn: true
                        }
                      });
                      return "queued";
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(IdleCommandProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let value = agent_session
                .execute_extension_command("emit-now", "", 5_000, |_| {})
                .await
                .expect("execute extension command");
            assert_eq!(value.as_str(), Some("queued"));

            let cx = crate::agent_cx::AgentCx::for_request();
            let session_guard = session.lock(cx.cx()).await.expect("lock session");
            let messages = session_guard.to_messages_for_current_path();

            assert!(
                messages.iter().any(|msg| {
                    matches!(
                        msg,
                        Message::Custom(CustomMessage { custom_type, content, .. })
                            if custom_type == "note" && content == "turn-now"
                    )
                }),
                "expected custom message prompt in session, got {messages:?}"
            );
            assert!(
                messages.iter().any(|msg| {
                    matches!(
                        msg,
                        Message::Assistant(assistant)
                            if assistant.content.iter().any(|block| matches!(
                                block,
                                ContentBlock::Text(TextContent { text, .. })
                                    if text == "resumed-response-0"
                            ))
                    )
                }),
                "expected assistant response after triggered turn, got {messages:?}"
            );
        });
    }

    #[test]
    fn agent_extension_session_get_state_reports_agent_runtime_state() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let mut session = Session::in_memory();
            session.set_model_header(
                Some("test-provider".to_string()),
                Some("test-model".to_string()),
                Some("high".to_string()),
            );
            session.append_message(crate::session::SessionMessage::User {
                content: UserContent::Text("hello".to_string()),
                timestamp: Some(1),
            });
            let session = Arc::new(Mutex::new(session));

            let extension_session = AgentExtensionSession {
                handle: SessionHandle(Arc::clone(&session)),
                is_streaming: Arc::new(AtomicBool::new(true)),
                is_compacting: Arc::new(AtomicBool::new(true)),
                queue_modes: Arc::new(StdMutex::new(ExtensionQueueModeState::new(
                    QueueMode::All,
                    QueueMode::OneAtATime,
                ))),
                auto_compaction_enabled: true,
            };

            let state = <AgentExtensionSession as crate::extensions::ExtensionSession>::get_state(
                &extension_session,
            )
            .await;

            assert_eq!(state["model"]["provider"], "test-provider");
            assert_eq!(state["model"]["id"], "test-model");
            assert_eq!(state["thinkingLevel"], "high");
            assert_eq!(state["isStreaming"], true);
            assert_eq!(state["isCompacting"], true);
            assert_eq!(state["steeringMode"], "all");
            assert_eq!(state["followUpMode"], "one-at-a-time");
            assert_eq!(state["autoCompactionEnabled"], true);
            assert_eq!(state["messageCount"], 1);
        });
    }

    #[test]
    fn agent_extension_session_get_state_uses_branch_local_model_and_thinking() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let mut session = Session::in_memory();
            let root_id = session.append_message(crate::session::SessionMessage::User {
                content: UserContent::Text("root".to_string()),
                timestamp: Some(1),
            });
            session.append_model_change("openai".to_string(), "gpt-4o".to_string());
            let branch_a_thinking = session.append_thinking_level_change("low".to_string());
            session.set_model_header(
                Some("openai".to_string()),
                Some("gpt-4o".to_string()),
                Some("low".to_string()),
            );

            assert!(session.create_branch_from(&root_id));
            session.append_model_change("anthropic".to_string(), "claude-sonnet-4-5".to_string());
            session.append_thinking_level_change("high".to_string());
            session.set_model_header(
                Some("anthropic".to_string()),
                Some("claude-sonnet-4-5".to_string()),
                Some("high".to_string()),
            );

            assert!(session.navigate_to(&branch_a_thinking));
            let session = Arc::new(Mutex::new(session));

            let extension_session = AgentExtensionSession {
                handle: SessionHandle(Arc::clone(&session)),
                is_streaming: Arc::new(AtomicBool::new(false)),
                is_compacting: Arc::new(AtomicBool::new(false)),
                queue_modes: Arc::new(StdMutex::new(ExtensionQueueModeState::new(
                    QueueMode::OneAtATime,
                    QueueMode::OneAtATime,
                ))),
                auto_compaction_enabled: false,
            };

            let state = <AgentExtensionSession as crate::extensions::ExtensionSession>::get_state(
                &extension_session,
            )
            .await;

            assert_eq!(state["model"]["provider"], "openai");
            assert_eq!(state["model"]["id"], "gpt-4o");
            assert_eq!(state["thinkingLevel"], "low");
        });
    }

    #[test]
    fn agent_session_set_queue_modes_updates_extension_delivery_state() {
        let provider = Arc::new(NoopProvider);
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let queue_modes = Arc::new(StdMutex::new(ExtensionQueueModeState::new(
            QueueMode::OneAtATime,
            QueueMode::OneAtATime,
        )));
        let injected_queue = Arc::new(StdMutex::new(ExtensionInjectedQueue::new(
            QueueMode::OneAtATime,
            QueueMode::OneAtATime,
        )));
        agent_session.extension_queue_modes = Some(Arc::clone(&queue_modes));
        agent_session.extension_injected_queue = Some(Arc::clone(&injected_queue));

        agent_session.set_queue_modes(QueueMode::All, QueueMode::All);

        assert_eq!(
            agent_session.agent.queue_modes(),
            (QueueMode::All, QueueMode::All)
        );
        let mirrored = queue_modes.lock().expect("lock queue mode mirror");
        assert_eq!(mirrored.steering_mode, QueueMode::All);
        assert_eq!(mirrored.follow_up_mode, QueueMode::All);
        drop(mirrored);

        let queued_follow_up_len = {
            let mut queue = injected_queue.lock().expect("lock injected queue");
            queue.push_follow_up(Message::User(UserMessage {
                content: UserContent::Text("first".to_string()),
                timestamp: 0,
            }));
            queue.push_follow_up(Message::User(UserMessage {
                content: UserContent::Text("second".to_string()),
                timestamp: 0,
            }));
            queue.pop_follow_up().len()
        };
        assert_eq!(
            queued_follow_up_len, 2,
            "updated queue modes should apply to extension-injected follow-ups"
        );
    }

    #[test]
    fn extension_command_send_user_message_runs_agent_turn_when_idle() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerCommand("inject-user", {
                    description: "inject a user message",
                    handler: async () => {
                      await pi.events("sendUserMessage", {
                        text: "Please review the changes"
                      });
                      return "queued";
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(IdleCommandProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let value = agent_session
                .execute_extension_command("inject-user", "", 5_000, |_| {})
                .await
                .expect("execute extension command");
            assert_eq!(value.as_str(), Some("queued"));

            let cx = crate::agent_cx::AgentCx::for_request();
            let session_guard = session.lock(cx.cx()).await.expect("lock session");
            let messages = session_guard.to_messages_for_current_path();

            assert!(
                messages.iter().any(|msg| {
                    matches!(
                        msg,
                        Message::User(UserMessage {
                            content: UserContent::Text(text),
                            ..
                        }) if text == "Please review the changes"
                    )
                }),
                "expected injected user message in session, got {messages:?}"
            );
            assert!(
                messages.iter().any(|msg| {
                    matches!(
                        msg,
                        Message::Assistant(assistant)
                            if assistant.content.iter().any(|block| matches!(
                                block,
                                ContentBlock::Text(TextContent { text, .. })
                                    if text == "resumed-response-0"
                            ))
                    )
                }),
                "expected assistant response after injected user turn, got {messages:?}"
            );
        });
    }

    #[test]
    fn send_user_message_steer_skips_remaining_tools() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  let sent = false;
                  pi.on("tool_call", async (event) => {
                    if (sent) return {};
                    if (event && event.toolName === "count_tool") {
                      sent = true;
                      await pi.events("sendUserMessage", {
                        text: "steer-now",
                        options: { deliverAs: "steer" }
                      });
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(ToolUseProvider::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let _ = agent_session
                .run_text("go".to_string(), |_| {})
                .await
                .expect("run_text");

            // A steer message should short-circuit remaining tool dispatch.
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn send_user_message_follow_up_does_not_skip_tools() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  let sent = false;
                  pi.on("tool_call", async (event) => {
                    if (sent) return {};
                    if (event && event.toolName === "count_tool") {
                      sent = true;
                      await pi.events("sendUserMessage", {
                        text: "follow-up",
                        options: { deliverAs: "followUp" }
                      });
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(ToolUseProvider::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let _ = agent_session
                .run_text("go".to_string(), |_| {})
                .await
                .expect("run_text");

            assert_eq!(calls.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn tool_call_hook_can_block_tool_execution() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (event) => {
                    if (event && event.toolName === "count_tool") {
                      return { block: true, reason: "blocked in test" };
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 0);

            assert_eq!(output.details, None);
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "Tool execution blocked: blocked in test");
            }
        });
    }

    #[test]
    fn tool_call_hook_errors_fail_open() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (_event) => {
                    throw new Error("boom");
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn tool_call_hook_errors_fail_closed_when_configured() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (_event) => {
                    throw new Error("boom");
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(
                provider,
                tools,
                AgentConfig {
                    fail_closed_hooks: true,
                    ..AgentConfig::default()
                },
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            let [ContentBlock::Text(text)] = output.content.as_slice() else {
                return;
            };
            assert_eq!(text.text, "Tool execution blocked: extension hook failed");
        });
    }

    #[test]
    fn tool_call_hook_absent_allows_tool_execution() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r"
                export default function init(_pi) {}
                ",
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn tool_call_hook_returns_empty_allows_tool_execution() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (_event) => ({}));
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn tool_call_hook_can_block_bash_tool_execution() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (event) => {
                    const name = event && event.toolName ? String(event.toolName) : "";
                    if (name === "bash") return { block: true, reason: "blocked bash in test" };
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&["bash"], temp_dir.path(), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&["bash"], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: json!({ "command": "printf 'hi' > blocked.txt" }),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(output.details, None);
            assert!(
                !temp_dir.path().join("blocked.txt").exists(),
                "expected bash command not to run when blocked"
            );
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "Tool execution blocked: blocked bash in test");
            }
        });
    }

    #[test]
    fn tool_result_hook_can_modify_tool_output() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_result", async (event) => {
                    if (event && event.toolName === "count_tool") {
                      return {
                        content: [{ type: "text", text: "modified" }],
                        details: { from: "tool_result" }
                      };
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(output.details, Some(json!({ "from": "tool_result" })));

            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "modified");
            }
        });
    }

    #[test]
    fn tool_result_hook_can_modify_tool_not_found_error() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_result", async (event) => {
                    if (event && event.toolName === "missing_tool" && event.isError) {
                      return {
                        content: [{ type: "text", text: "overridden" }],
                        details: { handled: true }
                      };
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::from_tools(Vec::new());
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "missing_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(output.details, Some(json!({ "handled": true })));

            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "overridden");
            }
        });
    }

    #[test]
    fn tool_result_hook_errors_fail_open() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_result", async (_event) => {
                    throw new Error("boom");
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);

            assert_eq!(output.details, None);
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "ok");
            }
        });
    }

    #[test]
    fn tool_result_hook_runs_on_blocked_tool_call() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (event) => {
                    if (event && event.toolName === "count_tool") {
                      return { block: true, reason: "blocked in test" };
                    }
                    return {};
                  });

                  pi.on("tool_result", async (event) => {
                    if (event && event.toolName === "count_tool" && event.isError) {
                      return { content: [{ type: "text", text: "override" }] };
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 0);

            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "override");
            }
        });
    }
}

#[cfg(test)]
mod abort_tests {
    use super::*;
    use crate::session::Session;
    use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
    use asupersync::runtime::RuntimeBuilder;
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::json;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context as TaskContext, Poll};

    struct StartThenPending {
        start: Option<StreamEvent>,
    }

    impl Stream for StartThenPending {
        type Item = crate::error::Result<StreamEvent>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<Option<Self::Item>> {
            if let Some(event) = self.start.take() {
                return Poll::Ready(Some(Ok(event)));
            }
            Poll::Pending
        }
    }

    #[derive(Debug)]
    struct HangingProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for HangingProvider {
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
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let partial = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            };

            Ok(Box::pin(StartThenPending {
                start: Some(StreamEvent::Start { partial }),
            }))
        }
    }

    #[derive(Debug)]
    struct CountingProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for CountingProvider {
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
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[derive(Debug)]
    struct PhasedProvider {
        pending_calls: usize,
        calls: AtomicUsize,
    }

    impl PhasedProvider {
        const fn new(pending_calls: usize) -> Self {
            Self {
                pending_calls,
                calls: AtomicUsize::new(0),
            }
        }

        fn base_message() -> AssistantMessage {
            AssistantMessage {
                content: Vec::new(),
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            }
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for PhasedProvider {
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
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.pending_calls {
                return Ok(Box::pin(StartThenPending {
                    start: Some(StreamEvent::Start {
                        partial: Self::base_message(),
                    }),
                }));
            }

            let partial = Self::base_message();
            let mut done = Self::base_message();
            done.content = vec![ContentBlock::Text(TextContent::new(format!(
                "resumed-response-{call}"
            )))];

            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: done,
                }),
            ])))
        }
    }

    #[derive(Debug)]
    struct ToolCallProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ToolCallProvider {
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
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let message = AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "call-1".to_string(),
                    name: "hanging_tool".to_string(),
                    arguments: json!({}),
                    thought_signature: None,
                })],
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp: 0,
            };

            Ok(Box::pin(futures::stream::iter(vec![Ok(
                StreamEvent::Done {
                    reason: StopReason::ToolUse,
                    message,
                },
            )])))
        }
    }

    #[derive(Debug)]
    struct HangingTool;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Tool for HangingTool {
        fn name(&self) -> &str {
            "hanging_tool"
        }

        fn label(&self) -> &str {
            "Hanging Tool"
        }

        fn description(&self) -> &str {
            "Never completes unless aborted by the host"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> crate::error::Result<ToolOutput> {
            futures::future::pending::<()>().await;
            unreachable!("hanging tool should be aborted by the agent")
        }
    }

    fn event_tag(event: &AgentEvent) -> &'static str {
        match event {
            AgentEvent::AgentStart { .. } => "agent_start",
            AgentEvent::AgentEnd { error, .. } => {
                if error.as_deref() == Some("Aborted") {
                    "agent_end_aborted"
                } else {
                    "agent_end"
                }
            }
            AgentEvent::TurnStart { .. } => "turn_start",
            AgentEvent::TurnEnd { .. } => "turn_end",
            AgentEvent::MessageStart { .. } => "message_start",
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => match &assistant_message_event {
                AssistantMessageEvent::Error {
                    reason: StopReason::Aborted,
                    ..
                } => "assistant_error_aborted",
                AssistantMessageEvent::Done { .. } => "assistant_done",
                _ => "assistant_update",
            },
            AgentEvent::MessageEnd { .. } => "message_end",
            AgentEvent::ToolExecutionStart { .. } => "tool_start",
            AgentEvent::ToolExecutionUpdate { .. } => "tool_update",
            AgentEvent::ToolExecutionEnd { .. } => "tool_end",
            AgentEvent::AutoCompactionStart { .. } => "auto_compaction_start",
            AgentEvent::AutoCompactionEnd { .. } => "auto_compaction_end",
            AgentEvent::AutoRetryStart { .. } => "auto_retry_start",
            AgentEvent::AutoRetryEnd { .. } => "auto_retry_end",
            AgentEvent::ExtensionError { .. } => "extension_error",
        }
    }

    fn assert_abort_resume_message_sequence(persisted: &[Message]) {
        assert_eq!(
            persisted.len(),
            6,
            "expected three user+assistant pairs, got: {persisted:?}"
        );

        let assistant_states = persisted
            .iter()
            .filter_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant.stop_reason),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            assistant_states,
            vec![StopReason::Aborted, StopReason::Aborted, StopReason::Stop]
        );
    }

    fn assert_abort_resume_timeline_boundaries(timeline: &[String]) {
        assert!(
            timeline
                .iter()
                .any(|event| event == "run0:agent_end_aborted"),
            "missing aborted boundary for first run: {timeline:?}"
        );
        assert!(
            timeline
                .iter()
                .any(|event| event == "run1:agent_end_aborted"),
            "missing aborted boundary for second run: {timeline:?}"
        );
        assert!(
            timeline.iter().any(|event| event == "run2:agent_end"),
            "missing successful boundary for resumed run: {timeline:?}"
        );
    }

    #[test]
    fn abort_interrupts_in_flight_stream() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        let started = Arc::new(Notify::new());
        let started_wait = started.notified();

        let (abort_handle, abort_signal) = AbortHandle::new();

        let provider = Arc::new(HangingProvider);
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let started_tx = Arc::clone(&started);
        let join = handle.spawn(async move {
            agent_session
                .run_text_with_abort("hello".to_string(), Some(abort_signal), move |event| {
                    if matches!(
                        event,
                        AgentEvent::MessageStart {
                            message: Message::Assistant(_)
                        }
                    ) {
                        started_tx.notify_one();
                    }
                })
                .await
        });

        runtime.block_on(async move {
            started_wait.await;
            abort_handle.abort();

            let message = join.await.expect("run_text_with_abort");
            assert_eq!(message.stop_reason, StopReason::Aborted);
            assert_eq!(message.error_message.as_deref(), Some("Aborted"));
        });
    }

    #[test]
    fn ambient_cancellation_interrupts_in_flight_stream() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let (started_tx, started_rx) = std::sync::mpsc::channel();

            let provider = Arc::new(HangingProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            let ambient_cx = asupersync::Cx::for_testing();
            let cancel_cx = ambient_cx.clone();
            let _current = asupersync::Cx::set_current(Some(ambient_cx));

            let cancel_thread = std::thread::spawn(move || {
                started_rx
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .expect("stream start");
                cancel_cx.set_cancel_requested(true);
            });

            let run = agent_session.run_text_with_abort("hello".to_string(), None, move |event| {
                if matches!(
                    event,
                    AgentEvent::MessageStart {
                        message: Message::Assistant(_)
                    }
                ) {
                    let _ = started_tx.send(());
                }
            });
            futures::pin_mut!(run);

            let message = asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_secs(1),
                run,
            )
            .await
            .expect("ambient cancellation should finish before timeout")
            .expect("run_text_with_abort");

            cancel_thread.join().expect("cancel thread");

            assert_eq!(message.stop_reason, StopReason::Aborted);
            assert_eq!(message.error_message.as_deref(), Some("Aborted"));
        });
    }

    #[test]
    fn abort_before_run_skips_provider_stream_call() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = Arc::new(CountingProvider {
            calls: Arc::clone(&calls),
        });
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let (abort_handle, abort_signal) = AbortHandle::new();
        abort_handle.abort();

        runtime.block_on(async move {
            let message = agent_session
                .run_text_with_abort("hello".to_string(), Some(abort_signal), |_| {})
                .await
                .expect("run_text_with_abort");
            assert_eq!(message.stop_reason, StopReason::Aborted);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn abort_then_resume_preserves_session_history() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(PhasedProvider::new(1));
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            let started = Arc::new(Notify::new());
            let (abort_handle, abort_signal) = AbortHandle::new();
            let started_for_abort = Arc::clone(&started);
            let abort_join = handle.spawn(async move {
                started_for_abort.notified().await;
                abort_handle.abort();
            });

            let aborted = agent_session
                .run_text_with_abort("first".to_string(), Some(abort_signal), {
                    let started = Arc::clone(&started);
                    move |event| {
                        if matches!(
                            event,
                            AgentEvent::MessageStart {
                                message: Message::Assistant(_)
                            }
                        ) {
                            started.notify_one();
                        }
                    }
                })
                .await
                .expect("first run");
            abort_join.await;

            assert_eq!(aborted.stop_reason, StopReason::Aborted);
            assert_eq!(aborted.error_message.as_deref(), Some("Aborted"));

            let resumed = agent_session
                .run_text("second".to_string(), |_| {})
                .await
                .expect("resumed run");
            assert_eq!(resumed.stop_reason, StopReason::Stop);
            assert!(resumed.error_message.is_none());

            let cx = crate::agent_cx::AgentCx::for_request();
            let persisted = session
                .lock(cx.cx())
                .await
                .expect("lock session")
                .to_messages_for_current_path();

            assert_eq!(
                persisted.len(),
                4,
                "unexpected message history after abort+resume: {persisted:?}"
            );
            assert!(matches!(persisted.first(), Some(Message::User(_))));
            assert!(matches!(
                persisted.get(1),
                Some(Message::Assistant(assistant)) if assistant.stop_reason == StopReason::Aborted
            ));
            assert!(matches!(persisted.get(2), Some(Message::User(_))));
            assert!(matches!(
                persisted.get(3),
                Some(Message::Assistant(assistant))
                    if assistant.stop_reason == StopReason::Stop && assistant.error_message.is_none()
            ));
        });
    }

    #[test]
    fn repeated_abort_then_resume_has_consistent_timeline_and_state() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(PhasedProvider::new(2));
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            let timeline = Arc::new(StdMutex::new(Vec::<String>::new()));

            for run_idx in 0..2 {
                let started = Arc::new(Notify::new());
                let (abort_handle, abort_signal) = AbortHandle::new();
                let started_for_abort = Arc::clone(&started);
                let abort_join = handle.spawn(async move {
                    started_for_abort.notified().await;
                    abort_handle.abort();
                });

                let run_timeline = Arc::clone(&timeline);
                let aborted = agent_session
                    .run_text_with_abort(format!("abort-run-{run_idx}"), Some(abort_signal), {
                        let started = Arc::clone(&started);
                        move |event| {
                            if let Ok(mut events) = run_timeline.lock() {
                                events.push(format!("run{run_idx}:{}", event_tag(&event)));
                            }
                            if matches!(
                                event,
                                AgentEvent::MessageStart {
                                    message: Message::Assistant(_)
                                }
                            ) {
                                started.notify_one();
                            }
                        }
                    })
                    .await
                    .expect("aborted run");
                abort_join.await;

                assert_eq!(
                    aborted.stop_reason,
                    StopReason::Aborted,
                    "run {run_idx} should abort cleanly"
                );
            }

            let run_timeline = Arc::clone(&timeline);
            let resumed = agent_session
                .run_text("final-run".to_string(), move |event| {
                    if let Ok(mut events) = run_timeline.lock() {
                        events.push(format!("run2:{}", event_tag(&event)));
                    }
                })
                .await
                .expect("final resumed run");
            assert_eq!(resumed.stop_reason, StopReason::Stop);
            assert!(resumed.error_message.is_none());

            let cx = crate::agent_cx::AgentCx::for_request();
            let persisted = session
                .lock(cx.cx())
                .await
                .expect("lock session")
                .to_messages_for_current_path();

            assert_abort_resume_message_sequence(&persisted);

            let timeline = timeline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_abort_resume_timeline_boundaries(&timeline);
        });
    }

    #[test]
    fn abort_during_tool_execution_records_aborted_tool_result() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(ToolCallProvider);
            let tools = ToolRegistry::from_tools(vec![Box::new(HangingTool)]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            let tool_started = Arc::new(Notify::new());
            let (abort_handle, abort_signal) = AbortHandle::new();
            let tool_started_for_abort = Arc::clone(&tool_started);
            let abort_join = handle.spawn(async move {
                tool_started_for_abort.notified().await;
                abort_handle.abort();
            });

            let result = agent_session
                .run_text_with_abort("trigger tool".to_string(), Some(abort_signal), {
                    let tool_started = Arc::clone(&tool_started);
                    move |event| {
                        if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                            tool_started.notify_one();
                        }
                    }
                })
                .await
                .expect("tool-abort run");
            abort_join.await;
            assert_eq!(result.stop_reason, StopReason::Aborted);

            let cx = crate::agent_cx::AgentCx::for_request();
            let persisted = session
                .lock(cx.cx())
                .await
                .expect("lock session")
                .to_messages_for_current_path();

            let tool_result = persisted
                .iter()
                .find_map(|message| match message {
                    Message::ToolResult(result) => Some(result),
                    _ => None,
                })
                .expect("expected tool result message");
            assert!(tool_result.is_error);
            assert!(
                tool_result.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Text(text) if text.text.contains("Tool execution aborted")
                    )
                }),
                "missing aborted tool marker in tool output: {:?}",
                tool_result.content
            );
        });
    }
}

#[cfg(test)]
mod turn_event_tests {
    use super::*;
    use crate::session::Session;
    use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
    use asupersync::runtime::RuntimeBuilder;
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::json;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    // Note: Mutex from super::* is asupersync::sync::Mutex (for Session)
    // Use std::sync::Mutex directly for synchronous event capture

    fn assistant_message(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "test-model".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        }
    }

    struct SingleShotProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for SingleShotProvider {
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
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let partial = assistant_message("");
            let final_message = assistant_message("hello");
            let events = vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: final_message,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    struct StreamSetupErrorProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for StreamSetupErrorProvider {
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
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            Err(Error::api("stream setup failed"))
        }
    }

    #[derive(Debug)]
    struct EchoTool;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }

        fn label(&self) -> &str {
            "echo_tool"
        }

        fn description(&self) -> &str {
            "echo test tool"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({ "type": "object" })
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> Result<ToolOutput> {
            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("tool-ok"))],
                details: None,
                is_error: false,
            })
        }
    }

    #[derive(Debug)]
    struct ToolTurnProvider {
        calls: AtomicUsize,
    }

    impl ToolTurnProvider {
        const fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }

        fn assistant_message_with(
            &self,
            stop_reason: StopReason,
            content: Vec<ContentBlock>,
        ) -> AssistantMessage {
            AssistantMessage {
                content,
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason,
                error_message: None,
                timestamp: 0,
            }
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ToolTurnProvider {
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
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
            let partial = self.assistant_message_with(StopReason::Stop, Vec::new());
            let done = if call_index == 0 {
                self.assistant_message_with(
                    StopReason::ToolUse,
                    vec![ContentBlock::ToolCall(ToolCall {
                        id: "tool-1".to_string(),
                        name: "echo_tool".to_string(),
                        arguments: json!({}),
                        thought_signature: None,
                    })],
                )
            } else {
                self.assistant_message_with(
                    StopReason::Stop,
                    vec![ContentBlock::Text(TextContent::new("final"))],
                )
            };

            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: done.stop_reason,
                    message: done,
                }),
            ])))
        }
    }

    #[test]
    fn turn_events_wrap_assistant_response() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        let provider = Arc::new(SingleShotProvider);
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_capture = Arc::clone(&events);

        let join = handle.spawn(async move {
            agent_session
                .run_text("hello".to_string(), move |event| {
                    events_capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(event);
                })
                .await
                .expect("run_text")
        });

        runtime.block_on(async move {
            let message = join.await;
            assert_eq!(message.stop_reason, StopReason::Stop);

            let events = events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let turn_start_indices = events
                .iter()
                .enumerate()
                .filter_map(|(idx, event)| {
                    matches!(event, AgentEvent::TurnStart { .. }).then_some(idx)
                })
                .collect::<Vec<_>>();
            let turn_end_indices = events
                .iter()
                .enumerate()
                .filter_map(|(idx, event)| {
                    matches!(event, AgentEvent::TurnEnd { .. }).then_some(idx)
                })
                .collect::<Vec<_>>();

            assert_eq!(turn_start_indices.len(), 1);
            assert_eq!(turn_end_indices.len(), 1);
            assert!(turn_start_indices[0] < turn_end_indices[0]);

            let assistant_message_end = events
                .iter()
                .enumerate()
                .find_map(|(idx, event)| match event {
                    AgentEvent::MessageEnd {
                        message: Message::Assistant(_),
                    } => Some(idx),
                    _ => None,
                })
                .expect("assistant message end");

            assert!(assistant_message_end < turn_end_indices[0]);

            let (message_is_assistant, tool_results_empty) = {
                let turn_end_event = &events[turn_end_indices[0]];
                assert!(
                    matches!(turn_end_event, AgentEvent::TurnEnd { .. }),
                    "Expected TurnEnd event, got {turn_end_event:?}"
                );
                match turn_end_event {
                    AgentEvent::TurnEnd {
                        message,
                        tool_results,
                        ..
                    } => (
                        matches!(message, Message::Assistant(_)),
                        tool_results.is_empty(),
                    ),
                    _ => (false, false),
                }
            };
            drop(events);
            assert!(message_is_assistant);
            assert!(tool_results_empty);
        });
    }

    #[test]
    fn stream_setup_errors_still_emit_turn_end_before_agent_end() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        let provider = Arc::new(StreamSetupErrorProvider);
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_capture = Arc::clone(&events);

        let join = handle.spawn(async move {
            agent_session
                .run_text("hello".to_string(), move |event| {
                    events_capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(event);
                })
                .await
                .expect_err("run_text should fail before streaming starts")
        });

        runtime.block_on(async move {
            let err = join.await;
            assert!(
                err.to_string().contains("stream setup failed"),
                "unexpected error: {err}"
            );

            let events = events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let turn_start_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::TurnStart { turn_index: 0, .. }))
                .expect("turn start");
            let turn_end_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::TurnEnd { turn_index: 0, .. }))
                .expect("turn end");
            let agent_end_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::AgentEnd { .. }))
                .expect("agent end");

            assert!(turn_start_idx < turn_end_idx);
            assert!(turn_end_idx < agent_end_idx);

            let assistant_message_end = events
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        AgentEvent::MessageEnd {
                            message: Message::Assistant(_),
                        }
                    )
                })
                .expect("assistant message end");
            assert!(assistant_message_end < turn_end_idx);

            match &events[turn_end_idx] {
                AgentEvent::TurnEnd {
                    message,
                    tool_results,
                    ..
                } => {
                    assert!(tool_results.is_empty());
                    assert!(
                        matches!(message, Message::Assistant(_)),
                        "expected assistant message in TurnEnd, got {message:?}"
                    );
                    let Message::Assistant(message) = message else {
                        return;
                    };
                    assert_eq!(message.stop_reason, StopReason::Error);
                    assert_eq!(
                        message.error_message.as_deref(),
                        Some("API error: stream setup failed")
                    );
                    assert_eq!(message.api, "test-api");
                    assert_eq!(message.provider, "test-provider");
                    assert_eq!(message.model, "test-model");
                }
                other => {
                    assert!(matches!(other, AgentEvent::TurnEnd { .. }));
                    return;
                }
            }

            match &events[agent_end_idx] {
                AgentEvent::AgentEnd { error, .. } => {
                    assert_eq!(error.as_deref(), Some("API error: stream setup failed"));
                }
                other => {
                    assert!(matches!(other, AgentEvent::AgentEnd { .. }));
                }
            }
        });
    }

    #[test]
    fn turn_events_include_tool_execution_and_tool_result_messages() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        let provider = Arc::new(ToolTurnProvider::new());
        let tools = ToolRegistry::from_tools(vec![Box::new(EchoTool)]);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_capture = Arc::clone(&events);

        let join = handle.spawn(async move {
            agent_session
                .run_text("hello".to_string(), move |event| {
                    events_capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(event);
                })
                .await
                .expect("run_text")
        });

        runtime.block_on(async move {
            let message = join.await;
            assert_eq!(message.stop_reason, StopReason::Stop);

            let events = events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let turn_start_count = events
                .iter()
                .filter(|event| matches!(event, AgentEvent::TurnStart { .. }))
                .count();
            let turn_end_count = events
                .iter()
                .filter(|event| matches!(event, AgentEvent::TurnEnd { .. }))
                .count();
            assert_eq!(
                turn_start_count, 2,
                "expected one tool turn and one final turn"
            );
            assert_eq!(
                turn_end_count, 2,
                "expected one tool turn and one final turn"
            );

            let tool_start_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::ToolExecutionStart { .. }))
                .expect("tool execution start event");
            let tool_end_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::ToolExecutionEnd { .. }))
                .expect("tool execution end event");
            assert!(tool_start_idx < tool_end_idx);

            let first_turn_end_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::TurnEnd { turn_index: 0, .. }))
                .expect("first turn end");
            assert!(
                tool_end_idx < first_turn_end_idx,
                "tool execution should complete before first turn end"
            );

            let first_turn_tool_results = events.iter().find_map(|event| match event {
                AgentEvent::TurnEnd {
                    turn_index,
                    tool_results,
                    ..
                } if *turn_index == 0 => Some(tool_results),
                _ => None,
            });

            let first_turn_tool_results =
                first_turn_tool_results.expect("expected tool results for first turn");
            assert_eq!(first_turn_tool_results.len(), 1);
            let first_result = first_turn_tool_results.first().unwrap();
            if let Message::ToolResult(tr) = first_result {
                assert_eq!(tr.tool_name, "echo_tool");
                assert!(!tr.is_error);
            } else {
                unreachable!("expected Message::ToolResult, got {:?}", first_result);
            }
            drop(events);
        });
    }
}

#[derive(Clone)]
struct AgentExtensionSession {
    handle: SessionHandle,
    is_streaming: Arc<AtomicBool>,
    is_compacting: Arc<AtomicBool>,
    queue_modes: Arc<StdMutex<ExtensionQueueModeState>>,
    auto_compaction_enabled: bool,
}

impl AgentExtensionSession {
    fn current_queue_modes(&self) -> (QueueMode, QueueMode) {
        self.queue_modes
            .lock()
            .map_or((QueueMode::OneAtATime, QueueMode::OneAtATime), |state| {
                (state.steering_mode, state.follow_up_mode)
            })
    }

    fn state_fallback(&self) -> Value {
        let (steering_mode, follow_up_mode) = self.current_queue_modes();
        json!({
            "model": null,
            "thinkingLevel": "off",
            "durabilityMode": "balanced",
            "isStreaming": self.is_streaming.load(std::sync::atomic::Ordering::SeqCst),
            "isCompacting": self.is_compacting.load(std::sync::atomic::Ordering::SeqCst),
            "steeringMode": steering_mode.as_str(),
            "followUpMode": follow_up_mode.as_str(),
            "sessionFile": null,
            "sessionId": "",
            "sessionName": null,
            "autoCompactionEnabled": self.auto_compaction_enabled,
            "messageCount": 0,
            "pendingMessageCount": 0,
        })
    }
}

#[async_trait]
impl crate::extensions::ExtensionSession for AgentExtensionSession {
    async fn get_state(&self) -> Value {
        let (steering_mode, follow_up_mode) = self.current_queue_modes();
        let mut state =
            <SessionHandle as crate::extensions::ExtensionSession>::get_state(&self.handle).await;
        let Some(object) = state.as_object_mut() else {
            return self.state_fallback();
        };

        object.insert(
            "isStreaming".to_string(),
            Value::Bool(self.is_streaming.load(std::sync::atomic::Ordering::SeqCst)),
        );
        object.insert(
            "isCompacting".to_string(),
            Value::Bool(self.is_compacting.load(std::sync::atomic::Ordering::SeqCst)),
        );
        object.insert(
            "steeringMode".to_string(),
            Value::String(steering_mode.as_str().to_string()),
        );
        object.insert(
            "followUpMode".to_string(),
            Value::String(follow_up_mode.as_str().to_string()),
        );
        object.insert(
            "autoCompactionEnabled".to_string(),
            Value::Bool(self.auto_compaction_enabled),
        );

        state
    }

    async fn get_messages(&self) -> Vec<crate::session::SessionMessage> {
        <SessionHandle as crate::extensions::ExtensionSession>::get_messages(&self.handle).await
    }

    async fn get_entries(&self) -> Vec<Value> {
        <SessionHandle as crate::extensions::ExtensionSession>::get_entries(&self.handle).await
    }

    async fn get_branch(&self) -> Vec<Value> {
        <SessionHandle as crate::extensions::ExtensionSession>::get_branch(&self.handle).await
    }

    async fn set_name(&self, name: String) -> crate::error::Result<()> {
        <SessionHandle as crate::extensions::ExtensionSession>::set_name(&self.handle, name).await
    }

    async fn append_message(
        &self,
        message: crate::session::SessionMessage,
    ) -> crate::error::Result<()> {
        <SessionHandle as crate::extensions::ExtensionSession>::append_message(
            &self.handle,
            message,
        )
        .await
    }

    async fn append_custom_entry(
        &self,
        custom_type: String,
        data: Option<Value>,
    ) -> crate::error::Result<()> {
        <SessionHandle as crate::extensions::ExtensionSession>::append_custom_entry(
            &self.handle,
            custom_type,
            data,
        )
        .await
    }

    async fn set_model(&self, provider: String, model_id: String) -> crate::error::Result<()> {
        <SessionHandle as crate::extensions::ExtensionSession>::set_model(
            &self.handle,
            provider,
            model_id,
        )
        .await
    }

    async fn get_model(&self) -> (Option<String>, Option<String>) {
        <SessionHandle as crate::extensions::ExtensionSession>::get_model(&self.handle).await
    }

    async fn set_thinking_level(&self, level: String) -> crate::error::Result<()> {
        <SessionHandle as crate::extensions::ExtensionSession>::set_thinking_level(
            &self.handle,
            level,
        )
        .await
    }

    async fn get_thinking_level(&self) -> Option<String> {
        <SessionHandle as crate::extensions::ExtensionSession>::get_thinking_level(&self.handle)
            .await
    }

    async fn set_label(
        &self,
        target_id: String,
        label: Option<String>,
    ) -> crate::error::Result<()> {
        <SessionHandle as crate::extensions::ExtensionSession>::set_label(
            &self.handle,
            target_id,
            label,
        )
        .await
    }
}

impl AgentSession {
    pub const fn runtime_repair_mode_from_policy_mode(mode: RepairPolicyMode) -> RepairMode {
        match mode {
            RepairPolicyMode::Off => RepairMode::Off,
            RepairPolicyMode::Suggest => RepairMode::Suggest,
            RepairPolicyMode::AutoSafe => RepairMode::AutoSafe,
            RepairPolicyMode::AutoStrict => RepairMode::AutoStrict,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_js_extension_runtime(
        stage: &'static str,
        cwd: &std::path::Path,
        tools: Arc<ToolRegistry>,
        manager: ExtensionManager,
        policy: ExtensionPolicy,
        repair_mode: RepairMode,
        memory_limit_bytes: usize,
    ) -> Result<ExtensionRuntimeHandle> {
        let mut config = PiJsRuntimeConfig {
            cwd: cwd.display().to_string(),
            repair_mode,
            ..PiJsRuntimeConfig::default()
        };
        config.limits.memory_limit_bytes = Some(memory_limit_bytes).filter(|bytes| *bytes > 0);

        let runtime =
            JsExtensionRuntimeHandle::start_with_policy(config, tools, manager, policy).await?;
        tracing::info!(
            event = "pi.extension_runtime.engine_decision",
            stage,
            requested = "quickjs",
            selected = "quickjs",
            fallback = false,
            "Extension runtime engine selected (legacy JS/TS)"
        );
        Ok(ExtensionRuntimeHandle::Js(runtime))
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_native_extension_runtime(
        stage: &'static str,
        _cwd: &std::path::Path,
        _tools: Arc<ToolRegistry>,
        _manager: ExtensionManager,
        _policy: ExtensionPolicy,
        _repair_mode: RepairMode,
        _memory_limit_bytes: usize,
    ) -> Result<ExtensionRuntimeHandle> {
        let runtime = NativeRustExtensionRuntimeHandle::start().await?;
        tracing::info!(
            event = "pi.extension_runtime.engine_decision",
            stage,
            requested = "native-rust",
            selected = "native-rust",
            fallback = false,
            "Extension runtime engine selected (native-rust)"
        );
        Ok(ExtensionRuntimeHandle::NativeRust(runtime))
    }

    pub fn new(
        agent: Agent,
        session: Arc<Mutex<Session>>,
        save_enabled: bool,
        compaction_settings: ResolvedCompactionSettings,
    ) -> Self {
        Self {
            agent,
            session,
            save_enabled,
            input_source: InputSource::Interactive,
            extensions: None,
            extensions_is_streaming: Arc::new(AtomicBool::new(false)),
            extensions_is_compacting: Arc::new(AtomicBool::new(false)),
            extensions_turn_active: Arc::new(AtomicBool::new(false)),
            extensions_pending_idle_actions: Arc::new(StdMutex::new(VecDeque::new())),
            extension_queue_modes: None,
            extension_injected_queue: None,
            compaction_settings,
            compaction_runtime: None,
            runtime_handle: None,
            compaction_worker: CompactionWorkerState::new(CompactionQuota::default()),
            model_registry: None,
            auth_storage: None,
        }
    }

    pub const fn set_input_source(&mut self, source: InputSource) {
        self.input_source = source;
    }

    #[must_use]
    pub fn with_runtime_handle(mut self, runtime_handle: RuntimeHandle) -> Self {
        self.compaction_runtime = None;
        self.runtime_handle = Some(runtime_handle);
        self
    }

    #[must_use]
    pub fn with_model_registry(mut self, registry: ModelRegistry) -> Self {
        self.model_registry = Some(registry);
        self
    }

    #[must_use]
    pub fn with_auth_storage(mut self, auth: AuthStorage) -> Self {
        self.auth_storage = Some(auth);
        self
    }

    pub fn set_model_registry(&mut self, registry: ModelRegistry) {
        self.model_registry = Some(registry);
    }

    pub fn set_auth_storage(&mut self, auth: AuthStorage) {
        self.auth_storage = Some(auth);
    }

    pub fn set_queue_modes(&mut self, steering_mode: QueueMode, follow_up_mode: QueueMode) {
        self.agent.set_queue_modes(steering_mode, follow_up_mode);

        if let Some(queue_modes) = &self.extension_queue_modes
            && let Ok(mut state) = queue_modes.lock()
        {
            state.set_modes(steering_mode, follow_up_mode);
        }

        if let Some(injected_queue) = &self.extension_injected_queue
            && let Ok(mut queue) = injected_queue.lock()
        {
            queue.set_modes(steering_mode, follow_up_mode);
        }
    }

    pub const fn set_compaction_context_window(&mut self, context_window_tokens: u32) {
        self.compaction_settings.context_window_tokens = context_window_tokens;
    }

    pub async fn set_provider_model(&mut self, provider_id: &str, model_id: &str) -> Result<()> {
        let already_active = {
            let provider = self.agent.provider();
            provider.name() == provider_id && provider.model_id() == model_id
        };
        let current_thinking = self
            .agent
            .stream_options()
            .thinking_level
            .unwrap_or_default();

        let target_entry = self
            .model_registry
            .as_ref()
            .and_then(|registry| registry.find(provider_id, model_id));
        let next_thinking = if let Some(target_entry) = target_entry {
            let resolved_key = self.resolve_stream_api_key_for_model(&target_entry);
            if !already_active
                && model_requires_configured_credential(&target_entry)
                && resolved_key.is_none()
            {
                return Err(Error::auth(format!(
                    "Missing credentials for {provider_id}/{model_id}"
                )));
            }
            self.clamp_thinking_level_for_model(provider_id, model_id, current_thinking)
        } else if already_active {
            current_thinking
        } else {
            return Err(Error::validation(format!(
                "Unable to switch provider/model to {provider_id}/{model_id}"
            )));
        };

        if !already_active {
            self.apply_session_model_selection(provider_id, model_id)?;
        }
        self.agent.stream_options_mut().thinking_level = Some(next_thinking);

        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            let previous_model = session.effective_model_for_current_path();
            let previous_thinking = session
                .effective_thinking_level_for_current_path()
                .as_deref()
                .and_then(|value| value.parse::<crate::model::ThinkingLevel>().ok());
            if previous_model
                .as_ref()
                .map(|(provider, model_id)| (provider.as_str(), model_id.as_str()))
                != Some((provider_id, model_id))
            {
                session.append_model_change(provider_id.to_string(), model_id.to_string());
            }
            session.set_model_header(
                Some(provider_id.to_string()),
                Some(model_id.to_string()),
                Some(next_thinking.to_string()),
            );
            if previous_thinking != Some(next_thinking) {
                session.append_thinking_level_change(next_thinking.to_string());
            }
        }

        self.persist_session().await
    }

    pub(crate) fn clamp_thinking_level_for_model(
        &self,
        provider_id: &str,
        model_id: &str,
        level: crate::model::ThinkingLevel,
    ) -> crate::model::ThinkingLevel {
        self.model_registry
            .as_ref()
            .and_then(|registry| registry.find(provider_id, model_id))
            .map_or(level, |entry| entry.clamp_thinking_level(level))
    }

    fn resolve_stream_api_key_for_model(&self, entry: &ModelEntry) -> Option<String> {
        let normalize = |key_opt: Option<String>| {
            key_opt.and_then(|key| {
                let trimmed = key.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            })
        };

        self.auth_storage
            .as_ref()
            .and_then(|auth| normalize(auth.resolve_api_key(&entry.model.provider, None)))
            .or_else(|| normalize(entry.api_key.clone()))
    }

    pub(crate) async fn sync_runtime_selection_from_session_header(&mut self) -> Result<()> {
        let session_state = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            (
                session.effective_model_for_current_path(),
                session.effective_thinking_level_for_current_path(),
            )
        };

        let (session_model, session_thinking) = session_state;
        let current_thinking = self
            .agent
            .stream_options()
            .thinking_level
            .unwrap_or_default();

        if let Some((provider_id, model_id)) = session_model.as_ref() {
            self.apply_session_model_selection(provider_id, model_id)?;
        }

        let parsed_session_thinking = session_thinking.as_deref().and_then(|raw| {
            raw.parse::<crate::model::ThinkingLevel>().map_or_else(
                |_| {
                    tracing::warn!("Ignoring invalid session thinking level: {raw}");
                    None
                },
                Some,
            )
        });
        let requested = parsed_session_thinking.unwrap_or(current_thinking);

        let effective = if let Some((provider_id, model_id)) = session_model.as_ref() {
            self.clamp_thinking_level_for_model(provider_id, model_id, requested)
        } else {
            requested
        };

        self.agent.stream_options_mut().thinking_level = Some(effective);

        let thinking_changed = effective != current_thinking;
        let persist_needed = if session_thinking.is_some() {
            parsed_session_thinking != Some(effective)
        } else {
            thinking_changed
        };
        if !persist_needed {
            return Ok(());
        }

        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            let previous_thinking = session
                .header
                .thinking_level
                .as_deref()
                .and_then(|value| value.parse::<crate::model::ThinkingLevel>().ok());
            session.set_model_header(None, None, Some(effective.to_string()));
            if thinking_changed && previous_thinking != Some(effective) {
                session.append_thinking_level_change(effective.to_string());
            }
        }

        self.persist_session().await
    }

    fn apply_session_model_selection(&mut self, provider_id: &str, model_id: &str) -> Result<()> {
        if self.agent.provider().name() == provider_id
            && self.agent.provider().model_id() == model_id
        {
            return Ok(());
        }

        let Some(registry) = &self.model_registry else {
            return Err(Error::validation(format!(
                "Unable to switch provider/model to {provider_id}/{model_id}"
            )));
        };

        let Some(entry) = registry.find(provider_id, model_id) else {
            return Err(Error::validation(format!(
                "Unable to switch provider/model to {provider_id}/{model_id}"
            )));
        };

        let resolved_key = self.resolve_stream_api_key_for_model(&entry);
        if model_requires_configured_credential(&entry) && resolved_key.is_none() {
            return Err(Error::auth(format!(
                "Missing credentials for {provider_id}/{model_id}"
            )));
        }

        match crate::providers::create_provider(
            &entry,
            self.extensions.as_ref().map(ExtensionRegion::manager),
        ) {
            Ok(provider) => {
                tracing::info!("Updating agent provider to {provider_id}/{model_id}");
                self.agent.set_provider(provider);

                let stream_options = self.agent.stream_options_mut();
                stream_options.api_key.clone_from(&resolved_key);
                stream_options.headers.clone_from(&entry.headers);
                Ok(())
            }
            Err(e) => Err(Error::validation(format!(
                "Unable to switch provider/model to {provider_id}/{model_id}: {e}"
            ))),
        }
    }

    pub const fn save_enabled(&self) -> bool {
        self.save_enabled
    }

    /// Force-run compaction synchronously (used by `/compact` slash command).
    pub async fn compact_now(
        &mut self,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<()> {
        self.compact_synchronous(Arc::new(on_event)).await
    }

    pub async fn execute_extension_command(
        &mut self,
        command_name: &str,
        args: &str,
        timeout_ms: u64,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<Value> {
        self.execute_extension_command_with_abort(command_name, args, timeout_ms, None, on_event)
            .await
    }

    pub async fn execute_extension_command_with_abort(
        &mut self,
        command_name: &str,
        args: &str,
        timeout_ms: u64,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<Value> {
        let manager = self
            .extensions
            .as_ref()
            .map(ExtensionRegion::manager)
            .ok_or_else(|| Error::extension("Extensions are disabled"))?
            .clone();
        let on_event: AgentEventHandler = Arc::new(on_event);

        self.run_pending_idle_actions_with_abort(abort.clone(), Arc::clone(&on_event))
            .await?;

        let command_result = manager
            .execute_command(command_name, args, timeout_ms)
            .await;
        let replay_result = self
            .run_pending_idle_actions_with_abort(abort, Arc::clone(&on_event))
            .await;

        match command_result {
            Ok(value) => {
                replay_result?;
                Ok(value)
            }
            Err(err) => {
                if let Err(replay_err) = replay_result {
                    tracing::warn!(
                        "extension command follow-up replay failed after command error: {replay_err}"
                    );
                }
                Err(err)
            }
        }
    }

    /// Two-phase non-blocking compaction.
    ///
    /// **Phase 1** — apply a completed background compaction result (if any).
    /// **Phase 2** — if quotas allow and the session needs compaction, start a
    /// new background compaction task.
    #[allow(clippy::too_many_lines)]
    async fn maybe_compact(&mut self, on_event: AgentEventHandler) -> Result<()> {
        if !self.compaction_settings.enabled {
            return Ok(());
        }

        // Phase 1: apply completed background result.
        if let Some(outcome) = self.compaction_worker.try_recv().await {
            self.extensions_is_compacting
                .store(false, std::sync::atomic::Ordering::SeqCst);
            match outcome {
                Ok(result) => {
                    self.apply_compaction_result(result, Arc::clone(&on_event))
                        .await?;
                }
                Err(e) => {
                    on_event(AgentEvent::AutoCompactionEnd {
                        result: None,
                        aborted: false,
                        will_retry: false,
                        error_message: Some(e.to_string()),
                    });
                }
            }
        }

        // Phase 2: start new background compaction if quotas allow.
        if !self.compaction_worker.can_start() {
            return Ok(());
        }

        let (entries, preparation) = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.ensure_entry_ids();
            let entries = session
                .entries_for_current_path()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            let prep = compaction::prepare_compaction(&entries, self.compaction_settings.clone());
            (entries, prep)
        };

        if let Some(prep) = preparation {
            on_event(AgentEvent::AutoCompactionStart {
                reason: "threshold".to_string(),
            });

            let before_outcome = self.dispatch_before_compact(&prep, &entries, None).await;
            if before_outcome.cancel {
                on_event(AgentEvent::AutoCompactionEnd {
                    result: None,
                    aborted: true,
                    will_retry: false,
                    error_message: None,
                });
                return Ok(());
            }

            if let Some(compaction) = before_outcome.compaction {
                let result_value = Some(Self::auto_compaction_result_payload(
                    compaction.summary.clone(),
                    compaction.first_kept_entry_id.clone(),
                    compaction.tokens_before,
                    compaction.details.clone(),
                ));
                self.extensions_is_compacting
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                let apply_result = self
                    .apply_compaction_entry(
                        compaction.summary,
                        compaction.first_kept_entry_id,
                        compaction.tokens_before,
                        compaction.details,
                        true,
                    )
                    .await;
                self.extensions_is_compacting
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                apply_result?;
                on_event(AgentEvent::AutoCompactionEnd {
                    result: result_value,
                    aborted: false,
                    will_retry: false,
                    error_message: None,
                });
                return Ok(());
            }

            let provider = self.agent.provider();
            let credential = self
                .agent
                .stream_options()
                .api_key
                .clone()
                .unwrap_or_default();

            let runtime_handle = match self.compaction_runtime_handle() {
                Ok(runtime_handle) => runtime_handle,
                Err(e) => {
                    on_event(AgentEvent::AutoCompactionEnd {
                        result: None,
                        aborted: false,
                        will_retry: false,
                        error_message: Some(e.to_string()),
                    });
                    return Ok(());
                }
            };

            self.compaction_worker
                .start(&runtime_handle, prep, provider, credential, None);
            self.extensions_is_compacting
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        Ok(())
    }

    fn compaction_runtime_handle(&mut self) -> Result<RuntimeHandle> {
        if let Some(runtime_handle) = self.runtime_handle.clone() {
            return Ok(runtime_handle);
        }

        let runtime = RuntimeBuilder::new().build().map_err(|e| {
            Error::session(format!("Background compaction runtime init failed: {e}"))
        })?;
        let runtime_handle = runtime.handle();
        self.compaction_runtime = Some(runtime);
        self.runtime_handle = Some(runtime_handle.clone());
        Ok(runtime_handle)
    }

    fn auto_compaction_result_payload(
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        details: Option<Value>,
    ) -> Value {
        let mut payload = serde_json::Map::new();
        payload.insert("summary".to_string(), Value::String(summary));
        payload.insert(
            "firstKeptEntryId".to_string(),
            Value::String(first_kept_entry_id),
        );
        payload.insert("tokensBefore".to_string(), Value::from(tokens_before));
        if let Some(details) = details {
            payload.insert("details".to_string(), details);
        }
        Value::Object(payload)
    }

    async fn apply_compaction_entry(
        &self,
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        details: Option<Value>,
        from_extension: bool,
    ) -> Result<()> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;

        let from_hook = if from_extension { Some(true) } else { None };
        let entry_id = session.append_compaction(
            summary,
            first_kept_entry_id,
            tokens_before,
            details,
            from_hook,
        );

        if self.save_enabled {
            session
                .flush_autosave(AutosaveFlushTrigger::Periodic)
                .await?;
        }

        let compaction_entry = session.get_entry(&entry_id).and_then(|entry| {
            if let crate::session::SessionEntry::Compaction(compaction) = entry {
                Some(compaction.clone())
            } else {
                None
            }
        });
        drop(session);

        if let (Some(region), Some(compaction_entry)) = (&self.extensions, compaction_entry) {
            let payload = json!({
                "compactionEntry": compaction_entry,
                "fromExtension": from_extension,
            });
            if let Err(err) = region
                .manager()
                .dispatch_event(ExtensionEventName::SessionCompact, Some(payload))
                .await
            {
                tracing::warn!("session_compact extension hook failed (fail-open): {err}");
            }
        }

        Ok(())
    }

    /// Apply a completed compaction result to the session.
    async fn apply_compaction_result(
        &self,
        result: compaction::CompactionResult,
        on_event: AgentEventHandler,
    ) -> Result<()> {
        let details = Some(compaction::compaction_details_to_value(&result.details)?);
        let result_value = Some(Self::auto_compaction_result_payload(
            result.summary.clone(),
            result.first_kept_entry_id.clone(),
            result.tokens_before,
            details.clone(),
        ));

        self.apply_compaction_entry(
            result.summary,
            result.first_kept_entry_id,
            result.tokens_before,
            details,
            false,
        )
        .await?;

        on_event(AgentEvent::AutoCompactionEnd {
            result: result_value,
            aborted: false,
            will_retry: false,
            error_message: None,
        });

        Ok(())
    }

    /// Run compaction synchronously (inline), blocking until completion.
    async fn compact_synchronous(&self, on_event: AgentEventHandler) -> Result<()> {
        if !self.compaction_settings.enabled {
            return Ok(());
        }

        let (entries, preparation) = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.ensure_entry_ids();
            let entries = session
                .entries_for_current_path()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            let prep = compaction::prepare_compaction(&entries, self.compaction_settings.clone());
            (entries, prep)
        };

        if let Some(prep) = preparation {
            on_event(AgentEvent::AutoCompactionStart {
                reason: "threshold".to_string(),
            });

            let before_outcome = self.dispatch_before_compact(&prep, &entries, None).await;
            if before_outcome.cancel {
                on_event(AgentEvent::AutoCompactionEnd {
                    result: None,
                    aborted: true,
                    will_retry: false,
                    error_message: None,
                });
                return Err(Error::extension("Compaction cancelled".to_string()));
            }

            if let Some(compaction) = before_outcome.compaction {
                let result_value = Some(Self::auto_compaction_result_payload(
                    compaction.summary.clone(),
                    compaction.first_kept_entry_id.clone(),
                    compaction.tokens_before,
                    compaction.details.clone(),
                ));
                self.extensions_is_compacting
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                let apply_result = self
                    .apply_compaction_entry(
                        compaction.summary,
                        compaction.first_kept_entry_id,
                        compaction.tokens_before,
                        compaction.details,
                        true,
                    )
                    .await;
                self.extensions_is_compacting
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                apply_result?;
                on_event(AgentEvent::AutoCompactionEnd {
                    result: result_value,
                    aborted: false,
                    will_retry: false,
                    error_message: None,
                });
                return Ok(());
            }
            self.extensions_is_compacting
                .store(true, std::sync::atomic::Ordering::SeqCst);

            let provider = self.agent.provider();
            let credential = self
                .agent
                .stream_options()
                .api_key
                .clone()
                .unwrap_or_default();

            let compaction_result = compaction::compact(prep, provider, &credential, None).await;
            self.extensions_is_compacting
                .store(false, std::sync::atomic::Ordering::SeqCst);

            match compaction_result {
                Ok(result) => {
                    self.apply_compaction_result(result, Arc::clone(&on_event))
                        .await?;
                }
                Err(e) => {
                    on_event(AgentEvent::AutoCompactionEnd {
                        result: None,
                        aborted: false,
                        will_retry: false,
                        error_message: Some(e.to_string()),
                    });
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn resolve_extension_policy_for_enable(
        config: Option<&crate::config::Config>,
        policy: Option<ExtensionPolicy>,
    ) -> ExtensionPolicy {
        policy.unwrap_or_else(|| {
            config.map_or_else(
                || crate::config::Config::default().resolve_extension_policy(None),
                |cfg| cfg.resolve_extension_policy(None),
            )
        })
    }

    pub async fn enable_extensions(
        &mut self,
        enabled_tools: &[&str],
        cwd: &std::path::Path,
        config: Option<&crate::config::Config>,
        extension_entries: &[std::path::PathBuf],
    ) -> Result<()> {
        self.enable_extensions_with_policy(
            enabled_tools,
            cwd,
            config,
            extension_entries,
            None,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub async fn enable_extensions_with_policy(
        &mut self,
        enabled_tools: &[&str],
        cwd: &std::path::Path,
        config: Option<&crate::config::Config>,
        extension_entries: &[std::path::PathBuf],
        policy: Option<ExtensionPolicy>,
        repair_policy: Option<RepairPolicyMode>,
        pre_warmed: Option<PreWarmedExtensionRuntime>,
    ) -> Result<()> {
        let mut js_specs: Vec<JsExtensionLoadSpec> = Vec::new();
        let mut native_specs: Vec<NativeRustExtensionLoadSpec> = Vec::new();
        #[cfg(feature = "wasm-host")]
        let mut wasm_specs: Vec<WasmExtensionLoadSpec> = Vec::new();

        for entry in extension_entries {
            match resolve_extension_load_spec(entry)? {
                ExtensionLoadSpec::Js(spec) => js_specs.push(spec),
                ExtensionLoadSpec::NativeRust(spec) => native_specs.push(spec),
                #[cfg(feature = "wasm-host")]
                ExtensionLoadSpec::Wasm(spec) => wasm_specs.push(spec),
            }
        }

        if !js_specs.is_empty() && !native_specs.is_empty() {
            return Err(Error::validation(
                "Mixed extension runtimes are not supported in one session yet. Use either JS/TS extensions (QuickJS) or native-rust descriptors (*.native.json), but not both at once."
                    .to_string(),
            ));
        }

        #[cfg(feature = "wasm-host")]
        if js_specs.is_empty() && native_specs.is_empty() && wasm_specs.is_empty() {
            self.extensions = None;
            self.agent.extensions = None;
            self.extension_queue_modes = None;
            self.extension_injected_queue = None;
            return Ok(());
        }

        #[cfg(not(feature = "wasm-host"))]
        if js_specs.is_empty() && native_specs.is_empty() {
            self.extensions = None;
            self.agent.extensions = None;
            self.extension_queue_modes = None;
            self.extension_injected_queue = None;
            return Ok(());
        }

        let resolved_policy = Self::resolve_extension_policy_for_enable(config, policy);
        let resolved_repair_policy = repair_policy
            .or_else(|| config.map(|cfg| cfg.resolve_repair_policy(None)))
            .unwrap_or(RepairPolicyMode::AutoSafe);
        let runtime_repair_mode =
            Self::runtime_repair_mode_from_policy_mode(resolved_repair_policy);
        let memory_limit_bytes =
            (resolved_policy.max_memory_mb as usize).saturating_mul(1024 * 1024);
        let wants_js_runtime = !js_specs.is_empty();

        // Either use the pre-warmed extension runtime (booted concurrently with startup)
        // or create a fresh runtime inline.
        #[allow(unused_variables)]
        let (manager, tools) = if let Some(pre) = pre_warmed {
            let manager = pre.manager;
            let tools = pre.tools;
            let runtime = match pre.runtime {
                ExtensionRuntimeHandle::NativeRust(runtime) => {
                    if wants_js_runtime {
                        tracing::warn!(
                            event = "pi.extension_runtime.prewarm.mismatch",
                            expected = "quickjs",
                            got = "native-rust",
                            "Pre-warmed runtime mismatched requested JS mode; creating quickjs runtime"
                        );
                        Self::start_js_extension_runtime(
                            "agent_enable_extensions_prewarm_mismatch",
                            cwd,
                            Arc::clone(&tools),
                            manager.clone(),
                            resolved_policy.clone(),
                            runtime_repair_mode,
                            memory_limit_bytes,
                        )
                        .await?
                    } else {
                        tracing::info!(
                            event = "pi.extension_runtime.engine_decision",
                            stage = "agent_enable_extensions_prewarmed",
                            requested = "native-rust",
                            selected = "native-rust",
                            fallback = false,
                            "Using pre-warmed extension runtime"
                        );
                        ExtensionRuntimeHandle::NativeRust(runtime)
                    }
                }
                ExtensionRuntimeHandle::Js(runtime) => {
                    if wants_js_runtime {
                        tracing::info!(
                            event = "pi.extension_runtime.engine_decision",
                            stage = "agent_enable_extensions_prewarmed",
                            requested = "quickjs",
                            selected = "quickjs",
                            fallback = false,
                            "Using pre-warmed extension runtime"
                        );
                        ExtensionRuntimeHandle::Js(runtime)
                    } else {
                        tracing::warn!(
                            event = "pi.extension_runtime.prewarm.mismatch",
                            expected = "native-rust",
                            got = "quickjs",
                            "Pre-warmed runtime mismatched requested native mode; creating native-rust runtime"
                        );
                        Self::start_native_extension_runtime(
                            "agent_enable_extensions_prewarm_mismatch",
                            cwd,
                            Arc::clone(&tools),
                            manager.clone(),
                            resolved_policy.clone(),
                            runtime_repair_mode,
                            memory_limit_bytes,
                        )
                        .await?
                    }
                }
            };
            manager.set_runtime(runtime);
            (manager, tools)
        } else {
            let manager = ExtensionManager::new();
            manager.set_cwd(cwd.display().to_string());
            let tools = Arc::new(ToolRegistry::new(enabled_tools, cwd, config));

            if let Some(cfg) = config {
                let resolved_risk = cfg.resolve_extension_risk_with_metadata();
                tracing::info!(
                    event = "pi.extension_runtime_risk.config",
                    source = resolved_risk.source,
                    enabled = resolved_risk.settings.enabled,
                    alpha = resolved_risk.settings.alpha,
                    window_size = resolved_risk.settings.window_size,
                    ledger_limit = resolved_risk.settings.ledger_limit,
                    fail_closed = resolved_risk.settings.fail_closed,
                    "Resolved extension runtime risk settings"
                );
                manager.set_runtime_risk_config(resolved_risk.settings);
            }

            let runtime = if wants_js_runtime {
                Self::start_js_extension_runtime(
                    "agent_enable_extensions_boot",
                    cwd,
                    Arc::clone(&tools),
                    manager.clone(),
                    resolved_policy.clone(),
                    runtime_repair_mode,
                    memory_limit_bytes,
                )
                .await?
            } else {
                Self::start_native_extension_runtime(
                    "agent_enable_extensions_boot",
                    cwd,
                    Arc::clone(&tools),
                    manager.clone(),
                    resolved_policy.clone(),
                    runtime_repair_mode,
                    memory_limit_bytes,
                )
                .await?
            };
            manager.set_runtime(runtime);
            (manager, tools)
        };

        // Session, host actions, and message fetchers are always set here
        // (after runtime boot) — the JS runtime only needs these when
        // dispatching hostcalls, which happens during extension loading.
        let (steering_mode, follow_up_mode) = self.agent.queue_modes();
        let queue_modes = Arc::new(StdMutex::new(ExtensionQueueModeState::new(
            steering_mode,
            follow_up_mode,
        )));
        manager.set_session(Arc::new(AgentExtensionSession {
            handle: SessionHandle(self.session.clone()),
            is_streaming: Arc::clone(&self.extensions_is_streaming),
            is_compacting: Arc::clone(&self.extensions_is_compacting),
            queue_modes: Arc::clone(&queue_modes),
            auto_compaction_enabled: self.compaction_settings.enabled,
        }));

        let injected = Arc::new(StdMutex::new(ExtensionInjectedQueue::new(
            steering_mode,
            follow_up_mode,
        )));
        let host_actions = AgentSessionHostActions {
            session: Arc::clone(&self.session),
            injected: Arc::clone(&injected),
            is_streaming: Arc::clone(&self.extensions_is_streaming),
            is_turn_active: Arc::clone(&self.extensions_turn_active),
            pending_idle_actions: Arc::clone(&self.extensions_pending_idle_actions),
        };
        self.extension_queue_modes = Some(Arc::clone(&queue_modes));
        self.extension_injected_queue = Some(Arc::clone(&injected));
        manager.set_host_actions(Arc::new(host_actions));
        {
            let steering_queue = Arc::clone(&injected);
            let follow_up_queue = Arc::clone(&injected);
            let steering_fetcher = move || -> BoxFuture<'static, Vec<Message>> {
                let steering_queue = Arc::clone(&steering_queue);
                Box::pin(async move {
                    let Ok(mut queue) = steering_queue.lock() else {
                        return Vec::new();
                    };
                    queue.pop_steering()
                })
            };
            let follow_up_fetcher = move || -> BoxFuture<'static, Vec<Message>> {
                let follow_up_queue = Arc::clone(&follow_up_queue);
                Box::pin(async move {
                    let Ok(mut queue) = follow_up_queue.lock() else {
                        return Vec::new();
                    };
                    queue.pop_follow_up()
                })
            };
            self.agent.register_message_fetchers(
                Some(Arc::new(steering_fetcher)),
                Some(Arc::new(follow_up_fetcher)),
            );
        }

        if !js_specs.is_empty() {
            manager.load_js_extensions(js_specs).await?;
        }

        if !native_specs.is_empty() {
            manager.load_native_extensions(native_specs).await?;
        }

        // Drain and log auto-repair diagnostics (bd-k5q5.8.11).
        if let Some(rt) = manager.runtime() {
            let events = rt.drain_repair_events().await;
            if !events.is_empty() {
                log_repair_diagnostics(&events);
            }
        }

        #[cfg(feature = "wasm-host")]
        if !wasm_specs.is_empty() {
            let host = WasmExtensionHost::new(cwd, resolved_policy.clone())?;
            manager
                .load_wasm_extensions(&host, wasm_specs, Arc::clone(&tools))
                .await?;
        }

        // Fire the `startup` lifecycle hook once extensions are loaded.
        // Fail-open: extension errors must not prevent the agent from running.
        let session_path = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::extension(e.to_string()))?;
            session.path.as_ref().map(|p| p.display().to_string())
        };

        if let Err(err) = manager
            .dispatch_event(
                ExtensionEventName::Startup,
                Some(serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "sessionFile": session_path,
                })),
            )
            .await
        {
            tracing::warn!("startup extension hook failed (fail-open): {err}");
        }

        if let Err(err) = manager
            .dispatch_event(ExtensionEventName::SessionStart, None)
            .await
        {
            tracing::warn!("session_start extension hook failed (fail-open): {err}");
        }

        let ctx_payload = serde_json::json!({ "cwd": cwd.display().to_string() });
        let wrappers = collect_extension_tool_wrappers(&manager, ctx_payload).await?;
        self.agent.extend_tools(wrappers);
        self.agent.extensions = Some(manager.clone());
        self.extensions = Some(ExtensionRegion::new(manager));
        Ok(())
    }

    pub async fn save_and_index(&mut self) -> Result<()> {
        if self.save_enabled {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session
                .flush_autosave(AutosaveFlushTrigger::Periodic)
                .await?;
        }
        Ok(())
    }

    pub async fn persist_session(&mut self) -> Result<()> {
        if !self.save_enabled {
            return Ok(());
        }
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        session
            .flush_autosave(AutosaveFlushTrigger::Periodic)
            .await?;
        Ok(())
    }

    pub async fn run_text(
        &mut self,
        input: String,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_text_with_abort(input, None, on_event).await
    }

    pub async fn run_text_with_abort(
        &mut self,
        input: String,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.extensions_turn_active.store(true, Ordering::SeqCst);
        let result = async {
            let outcome = self.dispatch_input_event(input, Vec::new()).await?;
            let (text, images) = match outcome {
                InputEventOutcome::Continue { text, images } => (text, images),
                InputEventOutcome::Block { reason } => {
                    let message = reason.unwrap_or_else(|| "Input blocked".to_string());
                    return Err(Error::extension(message));
                }
            };

            let base_system_prompt = self.agent.system_prompt().map(str::to_string);
            let BeforeAgentStartOutcome {
                messages: custom_messages,
                system_prompt,
            } = self
                .dispatch_before_agent_start(
                    &text,
                    &images,
                    base_system_prompt.as_deref().unwrap_or(""),
                )
                .await;
            if let Some(prompt) = system_prompt {
                self.agent.set_system_prompt(Some(prompt));
            } else {
                self.agent.set_system_prompt(base_system_prompt.clone());
            }

            let result = if images.is_empty() {
                self.run_agent_with_text(text, abort, on_event, custom_messages)
                    .await
            } else {
                let content = Self::build_content_blocks_for_input(&text, &images);
                self.run_agent_with_content(content, abort, on_event, custom_messages)
                    .await
            };

            self.agent.set_system_prompt(base_system_prompt);
            result
        }
        .await;
        self.extensions_turn_active.store(false, Ordering::SeqCst);
        result
    }

    pub async fn run_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_with_content_with_abort(content, None, on_event)
            .await
    }

    pub async fn run_with_content_with_abort(
        &mut self,
        content: Vec<ContentBlock>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.extensions_turn_active.store(true, Ordering::SeqCst);
        let result = async {
            let (text, images) = Self::split_content_blocks_for_input(&content);
            let outcome = self.dispatch_input_event(text, images).await?;
            let (text, images) = match outcome {
                InputEventOutcome::Continue { text, images } => (text, images),
                InputEventOutcome::Block { reason } => {
                    let message = reason.unwrap_or_else(|| "Input blocked".to_string());
                    return Err(Error::extension(message));
                }
            };

            let base_system_prompt = self.agent.system_prompt().map(str::to_string);
            let BeforeAgentStartOutcome {
                messages: custom_messages,
                system_prompt,
            } = self
                .dispatch_before_agent_start(
                    &text,
                    &images,
                    base_system_prompt.as_deref().unwrap_or(""),
                )
                .await;
            if let Some(prompt) = system_prompt {
                self.agent.set_system_prompt(Some(prompt));
            } else {
                self.agent.set_system_prompt(base_system_prompt.clone());
            }

            let content_for_agent = Self::build_content_blocks_for_input(&text, &images);
            let result = self
                .run_agent_with_content(content_for_agent, abort, on_event, custom_messages)
                .await;

            self.agent.set_system_prompt(base_system_prompt);
            result
        }
        .await;
        self.extensions_turn_active.store(false, Ordering::SeqCst);
        result
    }

    pub async fn revert_last_user_message(&mut self) -> Result<bool> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;

        let reverted = session.revert_last_user_message();
        if reverted {
            let messages = session.to_messages_for_current_path();
            self.agent.replace_messages(messages);
        }
        Ok(reverted)
    }

    async fn dispatch_input_event(
        &self,
        text: String,
        images: Vec<ImageContent>,
    ) -> Result<InputEventOutcome> {
        let Some(region) = &self.extensions else {
            return Ok(InputEventOutcome::Continue { text, images });
        };

        let images_value = serde_json::to_value(&images).unwrap_or(Value::Null);
        let attachments_value = images_value.clone();
        let text_clone = text.clone();
        let payload = json!({
            "text": text,
            "content": text_clone,
            "images": images_value,
            "attachments": attachments_value,
            "source": self.input_source.as_str(),
        });

        let response = region
            .manager()
            .dispatch_event_with_response(
                ExtensionEventName::Input,
                Some(payload),
                EXTENSION_EVENT_TIMEOUT_MS,
            )
            .await?;

        Ok(apply_input_event_response(response, text, images))
    }

    async fn dispatch_before_agent_start(
        &self,
        prompt: &str,
        images: &[ImageContent],
        system_prompt: &str,
    ) -> BeforeAgentStartOutcome {
        let Some(region) = &self.extensions else {
            return BeforeAgentStartOutcome {
                messages: Vec::new(),
                system_prompt: None,
            };
        };

        let images_value = serde_json::to_value(images).unwrap_or(Value::Null);
        let payload = json!({
            "prompt": prompt,
            "images": images_value,
            "systemPrompt": system_prompt,
        });

        let response = region
            .manager()
            .dispatch_event_with_response(
                ExtensionEventName::BeforeAgentStart,
                Some(payload),
                EXTENSION_EVENT_TIMEOUT_MS,
            )
            .await;

        match response {
            Ok(value) => apply_before_agent_start_response(value, Utc::now().timestamp_millis()),
            Err(err) => {
                tracing::warn!("before_agent_start extension hook failed (fail-open): {err}");
                BeforeAgentStartOutcome {
                    messages: Vec::new(),
                    system_prompt: None,
                }
            }
        }
    }

    async fn dispatch_before_compact(
        &self,
        preparation: &compaction::CompactionPreparation,
        branch_entries: &[crate::session::SessionEntry],
        custom_instructions: Option<&str>,
    ) -> SessionBeforeCompactOutcome {
        let Some(region) = &self.extensions else {
            return SessionBeforeCompactOutcome::default();
        };

        let prep_value = compaction::compaction_preparation_to_value(preparation);
        let branch_entries_value =
            serde_json::to_value(branch_entries).unwrap_or(Value::Array(Vec::new()));
        let mut payload = serde_json::Map::new();
        payload.insert("preparation".to_string(), prep_value);
        payload.insert("branchEntries".to_string(), branch_entries_value);
        if let Some(custom_instructions) = custom_instructions {
            payload.insert(
                "customInstructions".to_string(),
                Value::String(custom_instructions.to_string()),
            );
        }

        let response = region
            .manager()
            .dispatch_event_with_response(
                ExtensionEventName::SessionBeforeCompact,
                Some(Value::Object(payload)),
                EXTENSION_EVENT_TIMEOUT_MS,
            )
            .await;

        match response {
            Ok(value) => apply_session_before_compact_response(value, preparation.tokens_before),
            Err(err) => {
                tracing::warn!("session_before_compact extension hook failed (fail-open): {err}");
                SessionBeforeCompactOutcome::default()
            }
        }
    }

    fn split_content_blocks_for_input(blocks: &[ContentBlock]) -> (String, Vec<ImageContent>) {
        let mut text = String::new();
        let mut images = Vec::new();
        for block in blocks {
            match block {
                ContentBlock::Text(text_block) if !text_block.text.trim().is_empty() => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&text_block.text);
                }
                ContentBlock::Image(image) => images.push(image.clone()),
                _ => {}
            }
        }
        (text, images)
    }

    fn build_content_blocks_for_input(text: &str, images: &[ImageContent]) -> Vec<ContentBlock> {
        let mut content = Vec::new();
        if !text.trim().is_empty() {
            content.push(ContentBlock::Text(TextContent::new(text.to_string())));
        }
        for image in images {
            content.push(ContentBlock::Image(image.clone()));
        }
        content
    }

    fn take_pending_idle_actions(&self) -> Vec<PendingIdleAction> {
        let Ok(mut actions) = self.extensions_pending_idle_actions.lock() else {
            return Vec::new();
        };
        actions.drain(..).collect()
    }

    async fn run_pending_idle_actions_with_abort(
        &mut self,
        abort: Option<AbortSignal>,
        on_event: AgentEventHandler,
    ) -> Result<()> {
        let actions = self.take_pending_idle_actions();
        if actions.is_empty() {
            return Ok(());
        }

        let previous_source = self.input_source;
        self.input_source = InputSource::Extension;
        let result = async {
            for action in actions {
                match action {
                    PendingIdleAction::CustomMessage(message) => {
                        let handler = Arc::clone(&on_event);
                        self.run_custom_message_with_abort(message, abort.clone(), move |event| {
                            handler(event);
                        })
                        .await?;
                    }
                    PendingIdleAction::UserText(text) => {
                        let handler = Arc::clone(&on_event);
                        self.run_text_with_abort(text, abort.clone(), move |event| {
                            handler(event);
                        })
                        .await?;
                    }
                }
            }
            Ok(())
        }
        .await;
        self.input_source = previous_source;
        result
    }

    async fn run_custom_message_with_abort(
        &mut self,
        message: Message,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.extensions_turn_active.store(true, Ordering::SeqCst);
        let result = async {
            let base_system_prompt = self.agent.system_prompt().map(str::to_string);
            let BeforeAgentStartOutcome {
                messages: custom_messages,
                system_prompt,
            } = self
                .dispatch_before_agent_start("", &[], base_system_prompt.as_deref().unwrap_or(""))
                .await;
            if let Some(prompt) = system_prompt {
                self.agent.set_system_prompt(Some(prompt));
            } else {
                self.agent.set_system_prompt(base_system_prompt.clone());
            }

            let result = self
                .run_agent_with_prompt_message(message, abort, on_event, custom_messages)
                .await;

            self.agent.set_system_prompt(base_system_prompt);
            result
        }
        .await;
        self.extensions_turn_active.store(false, Ordering::SeqCst);
        result
    }

    async fn run_agent_with_prompt_message(
        &mut self,
        prompt_message: Message,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
        custom_messages: Vec<CustomMessage>,
    ) -> Result<AssistantMessage> {
        let on_event: AgentEventHandler = Arc::new(on_event);
        self.sync_runtime_selection_from_session_header().await?;

        self.maybe_compact(Arc::clone(&on_event)).await?;
        let history = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.to_messages_for_current_path()
        };
        self.agent.replace_messages(history);

        let start_len = self.agent.messages().len();
        let mut prompts = Vec::with_capacity(1 + custom_messages.len());
        prompts.push(prompt_message.clone());
        prompts.extend(custom_messages.into_iter().map(Message::Custom));

        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.append_model_message(prompt_message.clone());
            if self.save_enabled {
                session.flush_autosave(AutosaveFlushTrigger::Manual).await?;
            }
        }

        let streaming_guard = AtomicBoolGuard::activate(&self.extensions_is_streaming);
        let on_event_for_run = Arc::clone(&on_event);
        let result = self
            .agent
            .run_with_messages_with_abort(prompts, abort, move |event| {
                on_event_for_run(event);
            })
            .await;
        drop(streaming_guard);

        let persist_result = self
            .persist_new_messages(start_len + 1, result.is_err())
            .await;

        let result = result?;
        persist_result?;
        Ok(result)
    }

    pub(crate) async fn run_agent_with_text(
        &mut self,
        input: String,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
        custom_messages: Vec<CustomMessage>,
    ) -> Result<AssistantMessage> {
        let on_event: AgentEventHandler = Arc::new(on_event);
        self.sync_runtime_selection_from_session_header().await?;

        self.maybe_compact(Arc::clone(&on_event)).await?;
        let history = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.to_messages_for_current_path()
        };
        self.agent.replace_messages(history);

        let start_len = self.agent.messages().len();

        // Create and persist user message immediately to avoid data loss on API errors
        let user_message = Message::User(UserMessage {
            content: UserContent::Text(input),
            timestamp: Utc::now().timestamp_millis(),
        });
        let mut prompts = Vec::with_capacity(1 + custom_messages.len());
        prompts.push(user_message.clone());
        prompts.extend(custom_messages.into_iter().map(Message::Custom));

        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.append_model_message(user_message.clone());
            if self.save_enabled {
                session.flush_autosave(AutosaveFlushTrigger::Manual).await?;
            }
        }

        let streaming_guard = AtomicBoolGuard::activate(&self.extensions_is_streaming);
        let on_event_for_run = Arc::clone(&on_event);
        let result = self
            .agent
            .run_with_messages_with_abort(prompts, abort, move |event| {
                on_event_for_run(event);
            })
            .await;
        drop(streaming_guard);

        // Persist any NEW messages (assistant/tools) generated before the agent stopped,
        // even if it stopped due to an error, skipping the user message we already saved.
        let persist_result = self
            .persist_new_messages(start_len + 1, result.is_err())
            .await;

        let result = result?;
        persist_result?;
        Ok(result)
    }

    pub(crate) async fn run_agent_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
        custom_messages: Vec<CustomMessage>,
    ) -> Result<AssistantMessage> {
        let on_event: AgentEventHandler = Arc::new(on_event);
        self.sync_runtime_selection_from_session_header().await?;

        self.maybe_compact(Arc::clone(&on_event)).await?;
        let history = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.to_messages_for_current_path()
        };
        self.agent.replace_messages(history);

        let start_len = self.agent.messages().len();

        // Create and persist user message immediately to avoid data loss on API errors
        let user_message = Message::User(UserMessage {
            content: UserContent::Blocks(content),
            timestamp: Utc::now().timestamp_millis(),
        });
        let mut prompts = Vec::with_capacity(1 + custom_messages.len());
        prompts.push(user_message.clone());
        prompts.extend(custom_messages.into_iter().map(Message::Custom));

        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.append_model_message(user_message.clone());
            if self.save_enabled {
                session.flush_autosave(AutosaveFlushTrigger::Manual).await?;
            }
        }

        let streaming_guard = AtomicBoolGuard::activate(&self.extensions_is_streaming);
        let on_event_for_run = Arc::clone(&on_event);
        let result = self
            .agent
            .run_with_messages_with_abort(prompts, abort, move |event| {
                on_event_for_run(event);
            })
            .await;
        drop(streaming_guard);

        // Persist any NEW messages (assistant/tools) generated before the agent stopped,
        // even if it stopped due to an error, skipping the user message we already saved.
        let persist_result = self
            .persist_new_messages(start_len + 1, result.is_err())
            .await;

        let result = result?;
        persist_result?;
        Ok(result)
    }

    async fn persist_new_messages(&self, start_len: usize, run_failed: bool) -> Result<()> {
        let new_messages = self.agent.messages()[start_len..].to_vec();
        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            for message in new_messages {
                if run_failed && is_synthetic_empty_error_assistant(&message) {
                    continue;
                }
                session.append_model_message(message);
            }
            if self.save_enabled {
                session
                    .flush_autosave(AutosaveFlushTrigger::Periodic)
                    .await?;
            }
        }
        Ok(())
    }
}

fn is_synthetic_empty_error_assistant(message: &Message) -> bool {
    matches!(
        message,
        Message::Assistant(assistant)
            if assistant.content.is_empty()
                && assistant.stop_reason == StopReason::Error
                && assistant.error_message.is_some()
    )
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Log a summary of auto-repair events that fired during extension loading.
///
/// Default: one-line summary.  Set `PI_AUTO_REPAIR_VERBOSE=1` for per-extension
/// detail.  Structured tracing events are always emitted regardless of verbosity.
fn log_repair_diagnostics(events: &[crate::extensions_js::ExtensionRepairEvent]) {
    use std::collections::BTreeMap;

    // Always emit structured tracing events for each repair.
    for ev in events {
        tracing::info!(
            event = "extension.auto_repair",
            extension_id = %ev.extension_id,
            pattern = %ev.pattern,
            success = ev.success,
            original_error = %ev.original_error,
            repair_action = %ev.repair_action,
        );
    }

    // Group by pattern for the summary line.
    let mut by_pattern: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for ev in events {
        by_pattern
            .entry(ev.pattern.to_string())
            .or_default()
            .push(&ev.extension_id);
    }

    let verbose = std::env::var("PI_AUTO_REPAIR_VERBOSE")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

    if verbose {
        warn!(
            "[auto-repair] {} extension{} auto-repaired:",
            events.len(),
            if events.len() == 1 { "" } else { "s" }
        );
        for ev in events {
            warn!(
                "  {}: {} ({})",
                ev.pattern, ev.extension_id, ev.repair_action
            );
        }
    } else {
        // Compact one-line summary.
        let patterns: Vec<String> = by_pattern
            .iter()
            .map(|(pat, ids)| format!("{pat}:{}", ids.len()))
            .collect();
        tracing::info!(
            event = "extension.auto_repair.summary",
            count = events.len(),
            patterns = %patterns.join(", "),
            "auto-repaired {} extension(s)",
            events.len(),
        );
    }
}

const BLOCK_IMAGES_PLACEHOLDER: &str = "Image reading is disabled.";

#[derive(Debug, Default, Clone, Copy)]
struct ImageFilterStats {
    removed_images: usize,
    affected_messages: usize,
}

fn filter_images_for_provider(messages: &mut [Message]) -> ImageFilterStats {
    let mut stats = ImageFilterStats::default();
    for message in messages {
        let removed = filter_images_from_message(message);
        if removed > 0 {
            stats.removed_images += removed;
            stats.affected_messages += 1;
        }
    }
    stats
}

fn filter_images_from_message(message: &mut Message) -> usize {
    match message {
        Message::User(user) => match &mut user.content {
            UserContent::Text(_) => 0,
            UserContent::Blocks(blocks) => filter_image_blocks(blocks),
        },
        Message::Assistant(assistant) => {
            let assistant = Arc::make_mut(assistant);
            filter_image_blocks(&mut assistant.content)
        }
        Message::ToolResult(tool_result) => {
            filter_image_blocks(&mut Arc::make_mut(tool_result).content)
        }
        Message::Custom(_) => 0,
    }
}

fn filter_image_blocks(blocks: &mut Vec<ContentBlock>) -> usize {
    let mut removed = 0usize;
    let mut filtered = Vec::with_capacity(blocks.len());

    for block in blocks.drain(..) {
        match block {
            ContentBlock::Image(_) => {
                removed += 1;
                let previous_is_placeholder =
                    filtered
                        .last()
                        .is_some_and(|prev| matches!(prev, ContentBlock::Text(TextContent { text, .. }) if text == BLOCK_IMAGES_PLACEHOLDER));
                if !previous_is_placeholder {
                    filtered.push(ContentBlock::Text(TextContent::new(
                        BLOCK_IMAGES_PLACEHOLDER,
                    )));
                }
            }
            other => filtered.push(other),
        }
    }

    *blocks = filtered;
    removed
}

/// Extract tool calls from content blocks.
fn extract_tool_calls(content: &[ContentBlock]) -> Vec<ToolCall> {
    content
        .iter()
        .filter_map(|block| {
            if let ContentBlock::ToolCall(tc) = block {
                Some(tc.clone())
            } else {
                None
            }
        })
        .collect()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthCredential;
    use crate::provider::{InputType, Model, ModelCost};
    use asupersync::runtime::RuntimeBuilder;
    use async_trait::async_trait;
    use futures::Stream;
    use std::collections::HashMap;
    use std::path::Path;
    use std::pin::Pin;

    fn user_message(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    fn assert_user_text(message: &Message, expected: &str) {
        assert!(
            matches!(
                message,
                Message::User(UserMessage {
                    content: UserContent::Text(_),
                    ..
                })
            ),
            "expected user text message, got {message:?}"
        );
        if let Message::User(UserMessage {
            content: UserContent::Text(text),
            ..
        }) = message
        {
            assert_eq!(text, expected);
        }
    }

    fn sample_image_block() -> ContentBlock {
        ContentBlock::Image(ImageContent {
            data: "aGVsbG8=".to_string(),
            mime_type: "image/png".to_string(),
        })
    }

    fn image_count_in_message(message: &Message) -> usize {
        let count_images = |blocks: &[ContentBlock]| {
            blocks
                .iter()
                .filter(|block| matches!(block, ContentBlock::Image(_)))
                .count()
        };
        match message {
            Message::User(UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            }) => count_images(blocks),
            Message::Assistant(msg) => count_images(&msg.content),
            Message::ToolResult(tool_result) => count_images(&tool_result.content),
            Message::User(UserMessage {
                content: UserContent::Text(_),
                ..
            })
            | Message::Custom(_) => 0,
        }
    }

    fn assistant_message(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "test-model".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        }
    }

    #[derive(Debug)]
    struct SilentProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for SilentProvider {
        fn name(&self) -> &str {
            "silent-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[derive(Debug)]
    struct DeltaOnlyProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for DeltaOnlyProvider {
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
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let final_message = assistant_message("hello");
            let events = vec![
                Ok(StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "hello".to_string(),
                }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: final_message,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[test]
    fn delta_without_start_does_not_mutate_previous_message() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(DeltaOnlyProvider);
            let tools = ToolRegistry::from_tools(Vec::new());
            let mut agent = Agent::new(provider, tools, AgentConfig::default());

            agent.add_message(Message::Assistant(Arc::new(assistant_message("prev"))));

            agent
                .run_with_message_with_abort(user_message("hi"), None, |_| {})
                .await
                .expect("run");

            let assistant_texts = agent
                .messages()
                .iter()
                .filter_map(|message| match message {
                    Message::Assistant(msg)
                        if matches!(msg.content.as_slice(), [ContentBlock::Text(_)]) =>
                    {
                        if let [ContentBlock::Text(text)] = msg.content.as_slice() {
                            Some(text.text.clone())
                        } else {
                            None
                        }
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();

            assert_eq!(
                assistant_texts.as_slice(),
                ["prev".to_string(), "hello".to_string()]
            );
        });
    }

    #[test]
    fn enable_extensions_policy_resolution_defaults_to_permissive() {
        let policy = AgentSession::resolve_extension_policy_for_enable(None, None);
        assert_eq!(
            policy.mode,
            crate::extensions::ExtensionPolicyMode::Permissive
        );
    }

    #[test]
    fn enable_extensions_policy_resolution_respects_config_default_toggle() {
        let config = crate::config::Config {
            extension_policy: Some(crate::config::ExtensionPolicyConfig {
                profile: None,
                default_permissive: Some(false),
                allow_dangerous: None,
            }),
            ..Default::default()
        };
        let policy = AgentSession::resolve_extension_policy_for_enable(Some(&config), None);
        assert_eq!(policy.mode, crate::extensions::ExtensionPolicyMode::Strict);
    }

    #[test]
    fn enable_extensions_policy_resolution_prefers_explicit_policy() {
        let config = crate::config::Config {
            extension_policy: Some(crate::config::ExtensionPolicyConfig {
                profile: None,
                default_permissive: Some(false),
                allow_dangerous: None,
            }),
            ..Default::default()
        };
        let explicit = crate::extensions::PolicyProfile::Permissive.to_policy();
        let policy =
            AgentSession::resolve_extension_policy_for_enable(Some(&config), Some(explicit));
        assert_eq!(
            policy.mode,
            crate::extensions::ExtensionPolicyMode::Permissive
        );
    }

    #[test]
    fn test_extract_tool_calls() {
        let content = vec![
            ContentBlock::Text(TextContent::new("Hello")),
            ContentBlock::ToolCall(ToolCall {
                id: "tc1".to_string(),
                name: "read".to_string(),
                arguments: serde_json::json!({"path": "file.txt"}),
                thought_signature: None,
            }),
            ContentBlock::Text(TextContent::new("World")),
            ContentBlock::ToolCall(ToolCall {
                id: "tc2".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({"command": "ls"}),
                thought_signature: None,
            }),
        ];

        let tool_calls = extract_tool_calls(&content);
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].name, "read");
        assert_eq!(tool_calls[1].name, "bash");
    }

    #[test]
    fn test_agent_config_default() {
        let config = AgentConfig::default();
        assert_eq!(config.max_tool_iterations, 50);
        assert!(config.system_prompt.is_none());
        assert!(!config.block_images);
    }

    #[test]
    fn resolve_max_tool_iterations_unset_uses_default() {
        assert_eq!(resolve_max_tool_iterations(None), MAX_TOOL_ITERATIONS_DEFAULT);
    }

    #[test]
    fn resolve_max_tool_iterations_empty_uses_default() {
        assert_eq!(resolve_max_tool_iterations(Some("")), MAX_TOOL_ITERATIONS_DEFAULT);
        assert_eq!(resolve_max_tool_iterations(Some("   ")), MAX_TOOL_ITERATIONS_DEFAULT);
    }

    #[test]
    fn resolve_max_tool_iterations_zero_falls_back_to_default() {
        assert_eq!(resolve_max_tool_iterations(Some("0")), MAX_TOOL_ITERATIONS_DEFAULT);
    }

    #[test]
    fn resolve_max_tool_iterations_invalid_falls_back_to_default() {
        assert_eq!(resolve_max_tool_iterations(Some("notanumber")), MAX_TOOL_ITERATIONS_DEFAULT);
        assert_eq!(resolve_max_tool_iterations(Some("-5")), MAX_TOOL_ITERATIONS_DEFAULT);
        assert_eq!(resolve_max_tool_iterations(Some("3.14")), MAX_TOOL_ITERATIONS_DEFAULT);
    }

    #[test]
    fn resolve_max_tool_iterations_accepts_valid_overrides() {
        assert_eq!(resolve_max_tool_iterations(Some("1")), 1);
        assert_eq!(resolve_max_tool_iterations(Some("100")), 100);
        assert_eq!(resolve_max_tool_iterations(Some("999")), 999);
    }

    #[test]
    fn resolve_max_tool_iterations_clamps_above_ceiling() {
        assert_eq!(
            resolve_max_tool_iterations(Some("99999")),
            MAX_TOOL_ITERATIONS_CEILING
        );
    }

    #[test]
    fn resolve_max_tool_iterations_trims_whitespace() {
        assert_eq!(resolve_max_tool_iterations(Some(" 200 ")), 200);
    }

    #[test]
    fn iteration_warning_fires_at_80_percent_of_default() {
        // Default cap = 50; 80% threshold = 40.
        assert!(!should_warn_at_iteration_threshold(39, 50));
        assert!(should_warn_at_iteration_threshold(40, 50));
        assert!(should_warn_at_iteration_threshold(41, 50));
        assert!(should_warn_at_iteration_threshold(50, 50));
    }

    #[test]
    fn iteration_warning_fires_at_80_percent_of_custom_caps() {
        // 100 -> 80
        assert!(!should_warn_at_iteration_threshold(79, 100));
        assert!(should_warn_at_iteration_threshold(80, 100));
        // 200 -> 160
        assert!(!should_warn_at_iteration_threshold(159, 200));
        assert!(should_warn_at_iteration_threshold(160, 200));
        // 1000 -> 800
        assert!(!should_warn_at_iteration_threshold(799, 1000));
        assert!(should_warn_at_iteration_threshold(800, 1000));
    }

    #[test]
    fn iteration_warning_skipped_for_tiny_caps() {
        // Cap < ITERATION_WARN_MIN_CAP (5) — never warn, regardless of current.
        for cap in 0..ITERATION_WARN_MIN_CAP {
            for current in 0..=cap {
                assert!(
                    !should_warn_at_iteration_threshold(current, cap),
                    "should not warn at current={} cap={}",
                    current,
                    cap
                );
            }
        }
    }

    #[test]
    fn iteration_warning_handles_minimum_warnable_cap() {
        // Cap == ITERATION_WARN_MIN_CAP (5) — threshold is (5 * 4) / 5 = 4.
        assert!(!should_warn_at_iteration_threshold(3, 5));
        assert!(should_warn_at_iteration_threshold(4, 5));
        assert!(should_warn_at_iteration_threshold(5, 5));
    }

    #[test]
    fn filter_image_blocks_replaces_images_with_deduped_placeholder_text() {
        let mut blocks = vec![
            sample_image_block(),
            sample_image_block(),
            ContentBlock::Text(TextContent::new("tail")),
            sample_image_block(),
        ];

        let removed = filter_image_blocks(&mut blocks);

        assert_eq!(removed, 3);
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Image(_)))
        );
        assert!(matches!(
            blocks.first(),
            Some(ContentBlock::Text(TextContent { text, .. })) if text == BLOCK_IMAGES_PLACEHOLDER
        ));
        assert!(matches!(
            blocks.get(1),
            Some(ContentBlock::Text(TextContent { text, .. })) if text == "tail"
        ));
        assert!(matches!(
            blocks.get(2),
            Some(ContentBlock::Text(TextContent { text, .. })) if text == BLOCK_IMAGES_PLACEHOLDER
        ));
    }

    #[test]
    fn filter_images_for_provider_filters_images_from_all_block_message_types() {
        let mut messages = vec![
            Message::User(UserMessage {
                content: UserContent::Blocks(vec![
                    ContentBlock::Text(TextContent::new("hello")),
                    sample_image_block(),
                ]),
                timestamp: 0,
            }),
            Message::Assistant(Arc::new(AssistantMessage {
                content: vec![sample_image_block()],
                api: "test".to_string(),
                provider: "test".to_string(),
                model: "test".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            })),
            Message::tool_result(ToolResultMessage {
                tool_call_id: "tc1".to_string(),
                tool_name: "read".to_string(),
                content: vec![
                    sample_image_block(),
                    ContentBlock::Text(TextContent::new("ok")),
                ],
                details: None,
                is_error: false,
                timestamp: 0,
            }),
        ];

        let stats = filter_images_for_provider(&mut messages);

        assert_eq!(stats.removed_images, 3);
        assert_eq!(stats.affected_messages, 3);
        assert_eq!(
            messages.iter().map(image_count_in_message).sum::<usize>(),
            0,
            "no images should remain in provider-bound context"
        );
    }

    #[test]
    fn build_context_strips_images_when_block_images_enabled() {
        let mut agent = Agent::new(
            Arc::new(SilentProvider),
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
                stream_options: StreamOptions::default(),
                block_images: true,
                fail_closed_hooks: false,
            },
        );
        agent.add_message(Message::User(UserMessage {
            content: UserContent::Blocks(vec![sample_image_block()]),
            timestamp: 0,
        }));

        let context = agent.build_context();
        assert_eq!(context.messages.len(), 1);
        assert_eq!(image_count_in_message(&context.messages[0]), 0);
        assert!(matches!(
            &context.messages[0],
            Message::User(UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            }) if blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Text(TextContent { text, .. }) if text == BLOCK_IMAGES_PLACEHOLDER))
        ));
    }

    #[test]
    fn build_context_keeps_images_when_block_images_disabled() {
        let mut agent = Agent::new(
            Arc::new(SilentProvider),
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
                stream_options: StreamOptions::default(),
                block_images: false,
                fail_closed_hooks: false,
            },
        );
        agent.add_message(Message::User(UserMessage {
            content: UserContent::Blocks(vec![sample_image_block()]),
            timestamp: 0,
        }));

        let context = agent.build_context();
        assert_eq!(context.messages.len(), 1);
        assert_eq!(image_count_in_message(&context.messages[0]), 1);
    }

    #[test]
    fn auto_compaction_start_serializes_with_pi_mono_compatible_type_tag() {
        let event = AgentEvent::AutoCompactionStart {
            reason: "threshold".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_start");
        assert_eq!(json["reason"], "threshold");
    }

    #[test]
    fn auto_compaction_end_serializes_with_pi_mono_compatible_fields() {
        let event = AgentEvent::AutoCompactionEnd {
            result: Some(serde_json::json!({"tokens_before": 5000, "tokens_after": 2000})),
            aborted: false,
            will_retry: false,
            error_message: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert_eq!(json["aborted"], false);
        assert_eq!(json["willRetry"], false);
        assert!(json.get("errorMessage").is_none()); // skipped when None
        assert!(json["result"].is_object());
    }

    #[test]
    fn auto_compaction_end_includes_error_message_when_present() {
        let event = AgentEvent::AutoCompactionEnd {
            result: None,
            aborted: true,
            will_retry: false,
            error_message: Some("Compaction failed".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert_eq!(json["aborted"], true);
        assert_eq!(json["errorMessage"], "Compaction failed");
    }

    #[test]
    fn auto_retry_start_serializes_with_camel_case_fields() {
        let event = AgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
            error_message: "Rate limited".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_start");
        assert_eq!(json["attempt"], 1);
        assert_eq!(json["maxAttempts"], 3);
        assert_eq!(json["delayMs"], 2000);
        assert_eq!(json["errorMessage"], "Rate limited");
    }

    #[test]
    fn auto_retry_end_serializes_success_and_omits_null_final_error() {
        let event = AgentEvent::AutoRetryEnd {
            success: true,
            attempt: 2,
            final_error: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], true);
        assert_eq!(json["attempt"], 2);
        assert!(json.get("finalError").is_none());
    }

    #[test]
    fn auto_retry_end_includes_final_error_on_failure() {
        let event = AgentEvent::AutoRetryEnd {
            success: false,
            attempt: 3,
            final_error: Some("Max retries exceeded".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], false);
        assert_eq!(json["attempt"], 3);
        assert_eq!(json["finalError"], "Max retries exceeded");
    }

    #[test]
    fn message_queue_push_increments_seq_and_counts_both_queues() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        assert_eq!(queue.pending_count(), 0);

        assert_eq!(queue.push_steering(user_message("s1")), 0);
        assert_eq!(queue.push_follow_up(user_message("f1")), 1);
        assert_eq!(queue.push_steering(user_message("s2")), 2);

        assert_eq!(queue.pending_count(), 3);
    }

    #[test]
    fn message_queue_pop_steering_one_at_a_time_preserves_order() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(user_message("s1"));
        queue.push_steering(user_message("s2"));

        let first = queue.pop_steering();
        assert_eq!(first.len(), 1);
        assert_user_text(&first[0], "s1");
        assert_eq!(queue.pending_count(), 1);

        let second = queue.pop_steering();
        assert_eq!(second.len(), 1);
        assert_user_text(&second[0], "s2");
        assert_eq!(queue.pending_count(), 0);

        let empty = queue.pop_steering();
        assert!(empty.is_empty());
    }

    #[test]
    fn message_queue_pop_respects_queue_modes_per_kind() {
        let mut queue = MessageQueue::new(QueueMode::All, QueueMode::OneAtATime);
        queue.push_steering(user_message("s1"));
        queue.push_steering(user_message("s2"));
        queue.push_follow_up(user_message("f1"));
        queue.push_follow_up(user_message("f2"));

        let steering = queue.pop_steering();
        assert_eq!(steering.len(), 2);
        assert_user_text(&steering[0], "s1");
        assert_user_text(&steering[1], "s2");
        assert_eq!(queue.pending_count(), 2);

        let follow_up = queue.pop_follow_up();
        assert_eq!(follow_up.len(), 1);
        assert_user_text(&follow_up[0], "f1");
        assert_eq!(queue.pending_count(), 1);

        let follow_up = queue.pop_follow_up();
        assert_eq!(follow_up.len(), 1);
        assert_user_text(&follow_up[0], "f2");
        assert_eq!(queue.pending_count(), 0);
    }

    #[test]
    fn message_queue_set_modes_applies_to_existing_messages() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(user_message("s1"));
        queue.push_steering(user_message("s2"));

        let first = queue.pop_steering();
        assert_eq!(first.len(), 1);
        assert_user_text(&first[0], "s1");

        queue.set_modes(QueueMode::All, QueueMode::OneAtATime);
        let remaining = queue.pop_steering();
        assert_eq!(remaining.len(), 1);
        assert_user_text(&remaining[0], "s2");
    }

    fn build_switch_test_session(auth: &AuthStorage) -> AgentSession {
        let registry = ModelRegistry::load(auth, None);
        let current_entry = registry
            .find("anthropic", "claude-sonnet-4-5")
            .expect("anthropic model in registry");
        let provider = crate::providers::create_provider(&current_entry, None)
            .expect("create anthropic provider");
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let mut stream_options = StreamOptions {
            api_key: Some("stale-key".to_string()),
            ..Default::default()
        };
        let _ = stream_options
            .headers
            .insert("x-stale-header".to_string(), "stale-value".to_string());
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
                stream_options,
                block_images: false,
                fail_closed_hooks: false,
            },
        );

        let mut session = Session::in_memory();
        session.header.provider = Some("anthropic".to_string());
        session.header.model_id = Some("claude-sonnet-4-5".to_string());

        let mut agent_session = AgentSession::new(
            agent,
            Arc::new(Mutex::new(session)),
            false,
            ResolvedCompactionSettings::default(),
        );
        agent_session.set_model_registry(registry);
        agent_session.set_auth_storage(auth.clone());
        agent_session
    }

    #[test]
    fn compaction_runtime_handle_creates_fallback_runtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage::load(auth_path).expect("load auth");
        let mut agent_session = build_switch_test_session(&auth);

        assert!(agent_session.compaction_runtime.is_none());
        assert!(agent_session.runtime_handle.is_none());

        let runtime_handle = agent_session
            .compaction_runtime_handle()
            .expect("create fallback compaction runtime");
        let join = runtime_handle.spawn(async { 7_u8 });
        assert_eq!(futures::executor::block_on(join), 7);

        assert!(agent_session.compaction_runtime.is_some());
        assert!(agent_session.runtime_handle.is_some());
    }

    #[test]
    fn apply_session_model_selection_updates_stream_credentials_and_headers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load auth");
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "anthropic-key".to_string(),
            },
        );
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "openai-key".to_string(),
            },
        );

        let mut agent_session = build_switch_test_session(&auth);
        agent_session
            .apply_session_model_selection("openai", "gpt-4o")
            .expect("switch should update stream options");

        assert_eq!(agent_session.agent.provider().name(), "openai");
        assert_eq!(agent_session.agent.provider().model_id(), "gpt-4o");
        assert_eq!(
            agent_session.agent.stream_options().api_key.as_deref(),
            Some("openai-key")
        );
        assert!(
            agent_session.agent.stream_options().headers.is_empty(),
            "stream headers should be refreshed from selected model entry"
        );
    }

    #[test]
    fn apply_session_model_selection_clears_stale_key_for_keyless_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load auth");
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "anthropic-key".to_string(),
            },
        );

        let mut registry = ModelRegistry::load(&auth, None);
        registry.merge_entries(vec![ModelEntry {
            model: Model {
                id: "local-model".to_string(),
                name: "Local Model".to_string(),
                api: "openai-completions".to_string(),
                provider: "acme-local".to_string(),
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
            auth_header: false,
            compat: None,
            oauth_config: None,
        }]);

        let mut agent_session = build_switch_test_session(&auth);
        agent_session.set_model_registry(registry);
        agent_session
            .apply_session_model_selection("acme-local", "local-model")
            .expect("keyless local model should still activate");

        assert_eq!(agent_session.agent.provider().name(), "acme-local");
        assert_eq!(
            agent_session.agent.stream_options().api_key,
            None,
            "stale key must be cleared when target model has no configured key"
        );
    }

    #[test]
    fn apply_session_model_selection_treats_blank_model_key_as_missing_credential() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage::load(auth_path).expect("load auth");

        let mut registry = ModelRegistry::load(&auth, None);
        registry.merge_entries(vec![ModelEntry {
            model: Model {
                id: "blank-model".to_string(),
                name: "Blank Model".to_string(),
                api: "openai-completions".to_string(),
                provider: "acme".to_string(),
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
            api_key: Some("   ".to_string()),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }]);

        let mut agent_session = build_switch_test_session(&auth);
        agent_session.set_model_registry(registry);
        let err = agent_session
            .apply_session_model_selection("acme", "blank-model")
            .expect_err("blank keys must not satisfy credential requirements");

        assert!(
            err.to_string()
                .contains("Missing credentials for acme/blank-model"),
            "unexpected error: {err}"
        );
        assert_eq!(agent_session.agent.provider().name(), "anthropic");
        assert_eq!(
            agent_session.agent.stream_options().api_key,
            Some("stale-key".to_string()),
            "failed switches must preserve the prior runtime credentials"
        );
    }

    #[test]
    fn set_provider_model_preserves_session_header_when_switch_fails() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");
            let mut agent_session = build_switch_test_session(&auth);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("anthropic".to_string());
                session.header.model_id = Some("claude-sonnet-4-5".to_string());
            }

            let err = agent_session
                .set_provider_model("missing-provider", "missing-model")
                .await
                .expect_err("missing model should not switch");
            assert!(
                err.to_string()
                    .contains("Unable to switch provider/model to missing-provider/missing-model"),
                "unexpected error: {err}"
            );
            assert_eq!(agent_session.agent.provider().name(), "anthropic");
            assert_eq!(
                agent_session.agent.provider().model_id(),
                "claude-sonnet-4-5"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("anthropic"));
            assert_eq!(
                session.header.model_id.as_deref(),
                Some("claude-sonnet-4-5")
            );
        });
    }

    #[test]
    fn set_provider_model_rejects_missing_credentials_without_switching() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");
            let mut agent_session = build_switch_test_session(&auth);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("anthropic".to_string());
                session.header.model_id = Some("claude-sonnet-4-5".to_string());
            }

            let err = agent_session
                .set_provider_model("openai", "gpt-4o")
                .await
                .expect_err("missing credentials should abort model switch");
            assert!(
                err.to_string()
                    .contains("Missing credentials for openai/gpt-4o"),
                "unexpected error: {err}"
            );
            assert_eq!(agent_session.agent.provider().name(), "anthropic");
            assert_eq!(
                agent_session.agent.provider().model_id(),
                "claude-sonnet-4-5"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("anthropic"));
            assert_eq!(
                session.header.model_id.as_deref(),
                Some("claude-sonnet-4-5")
            );
        });
    }

    #[test]
    fn set_provider_model_clamps_thinking_for_non_reasoning_targets() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");

            let mut registry = ModelRegistry::load(&auth, None);
            registry.merge_entries(vec![ModelEntry {
                model: Model {
                    id: "plain-model".to_string(),
                    name: "Plain Model".to_string(),
                    api: "openai-completions".to_string(),
                    provider: "acme".to_string(),
                    base_url: "https://example.invalid/v1".to_string(),
                    reasoning: false,
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
                auth_header: false,
                compat: None,
                oauth_config: None,
            }]);

            let mut agent_session = build_switch_test_session(&auth);
            agent_session.set_model_registry(registry);
            agent_session.agent.stream_options_mut().thinking_level =
                Some(crate::model::ThinkingLevel::High);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.thinking_level = Some("high".to_string());
            }

            agent_session
                .set_provider_model("acme", "plain-model")
                .await
                .expect("switch should clamp unsupported thinking");

            assert_eq!(agent_session.agent.provider().name(), "acme");
            assert_eq!(agent_session.agent.provider().model_id(), "plain-model");
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::Off)
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("acme"));
            assert_eq!(session.header.model_id.as_deref(), Some("plain-model"));
            assert_eq!(session.header.thinking_level.as_deref(), Some("off"));
        });
    }

    #[test]
    fn set_provider_model_records_model_change_once() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage::load(auth_path).expect("load auth");
            auth.set(
                "anthropic",
                AuthCredential::ApiKey {
                    key: "anthropic-key".to_string(),
                },
            );
            auth.set(
                "openai",
                AuthCredential::ApiKey {
                    key: "openai-key".to_string(),
                },
            );

            let mut agent_session = build_switch_test_session(&auth);
            agent_session
                .set_provider_model("openai", "gpt-4o")
                .await
                .expect("switch model");
            agent_session
                .set_provider_model("openai", "gpt-4o")
                .await
                .expect("repeat same model");

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            let model_changes = session
                .entries_for_current_path()
                .iter()
                .filter(|entry| matches!(entry, crate::session::SessionEntry::ModelChange(_)))
                .count();
            assert_eq!(model_changes, 1);
        });
    }

    #[test]
    fn sync_runtime_selection_from_session_header_clamps_and_normalizes_thinking() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");

            let mut registry = ModelRegistry::load(&auth, None);
            registry.merge_entries(vec![ModelEntry {
                model: Model {
                    id: "plain-model".to_string(),
                    name: "Plain Model".to_string(),
                    api: "openai-completions".to_string(),
                    provider: "acme".to_string(),
                    base_url: "https://example.invalid/v1".to_string(),
                    reasoning: false,
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
                auth_header: false,
                compat: None,
                oauth_config: None,
            }]);

            let mut agent_session = build_switch_test_session(&auth);
            agent_session.set_model_registry(registry);
            agent_session.agent.stream_options_mut().thinking_level =
                Some(crate::model::ThinkingLevel::High);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("acme".to_string());
                session.header.model_id = Some("plain-model".to_string());
                session.header.thinking_level = Some("high".to_string());
            }

            agent_session
                .sync_runtime_selection_from_session_header()
                .await
                .expect("sync runtime selection");

            assert_eq!(agent_session.agent.provider().name(), "acme");
            assert_eq!(agent_session.agent.provider().model_id(), "plain-model");
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::Off)
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.thinking_level.as_deref(), Some("off"));
            let thinking_changes = session
                .entries_for_current_path()
                .iter()
                .filter(|entry| {
                    matches!(entry, crate::session::SessionEntry::ThinkingLevelChange(_))
                })
                .count();
            assert_eq!(thinking_changes, 1);
        });
    }

    #[test]
    fn sync_runtime_selection_from_session_header_clamps_current_thinking_when_header_omits_it() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");

            let mut registry = ModelRegistry::load(&auth, None);
            registry.merge_entries(vec![ModelEntry {
                model: Model {
                    id: "plain-model".to_string(),
                    name: "Plain Model".to_string(),
                    api: "openai-completions".to_string(),
                    provider: "acme".to_string(),
                    base_url: "https://example.invalid/v1".to_string(),
                    reasoning: false,
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
                auth_header: false,
                compat: None,
                oauth_config: None,
            }]);

            let mut agent_session = build_switch_test_session(&auth);
            agent_session.set_model_registry(registry);
            agent_session.agent.stream_options_mut().thinking_level =
                Some(crate::model::ThinkingLevel::High);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("acme".to_string());
                session.header.model_id = Some("plain-model".to_string());
                session.header.thinking_level = None;
            }

            agent_session
                .sync_runtime_selection_from_session_header()
                .await
                .expect("sync runtime selection");

            assert_eq!(agent_session.agent.provider().name(), "acme");
            assert_eq!(agent_session.agent.provider().model_id(), "plain-model");
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::Off)
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.thinking_level.as_deref(), Some("off"));
            let thinking_changes = session
                .entries_for_current_path()
                .iter()
                .filter(|entry| {
                    matches!(entry, crate::session::SessionEntry::ThinkingLevelChange(_))
                })
                .count();
            assert_eq!(thinking_changes, 1);
        });
    }

    #[test]
    fn sync_runtime_selection_from_session_header_rejects_missing_credentials() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");
            let mut agent_session = build_switch_test_session(&auth);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("openai".to_string());
                session.header.model_id = Some("gpt-4o".to_string());
            }

            let err = agent_session
                .sync_runtime_selection_from_session_header()
                .await
                .expect_err("sync should reject switching to a credentialed target without a key");
            assert!(
                err.to_string()
                    .contains("Missing credentials for openai/gpt-4o"),
                "unexpected error: {err}"
            );
            assert_eq!(agent_session.agent.provider().name(), "anthropic");
            assert_eq!(
                agent_session.agent.provider().model_id(),
                "claude-sonnet-4-5"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("openai"));
            assert_eq!(session.header.model_id.as_deref(), Some("gpt-4o"));
        });
    }

    #[test]
    fn set_provider_model_allows_current_model_without_registry() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");
            let mut agent_session = build_switch_test_session(&auth);
            agent_session.model_registry = None;
            agent_session.agent.stream_options_mut().thinking_level =
                Some(crate::model::ThinkingLevel::High);

            agent_session
                .set_provider_model("anthropic", "claude-sonnet-4-5")
                .await
                .expect("re-persisting the current model should succeed without a registry");

            assert_eq!(agent_session.agent.provider().name(), "anthropic");
            assert_eq!(
                agent_session.agent.provider().model_id(),
                "claude-sonnet-4-5"
            );
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::High)
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("anthropic"));
            assert_eq!(
                session.header.model_id.as_deref(),
                Some("claude-sonnet-4-5")
            );
            assert_eq!(session.header.thinking_level.as_deref(), Some("high"));
        });
    }

    #[test]
    fn auto_compaction_start_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoCompactionStart {
            reason: "threshold".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_start");
        assert_eq!(json["reason"], "threshold");
    }

    #[test]
    fn auto_compaction_end_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoCompactionEnd {
            result: Some(serde_json::json!({
                "summary": "Compacted",
                "firstKeptEntryId": "abc123",
                "tokensBefore": 50000,
                "details": { "readFiles": [], "modifiedFiles": [] }
            })),
            aborted: false,
            will_retry: true,
            error_message: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert!(json["result"].is_object());
        assert_eq!(json["aborted"], false);
        assert_eq!(json["willRetry"], true);
        assert!(json.get("errorMessage").is_none());
    }

    #[test]
    fn auto_compaction_end_with_error_serializes_error_message() {
        let event = AgentEvent::AutoCompactionEnd {
            result: None,
            aborted: false,
            will_retry: false,
            error_message: Some("compaction failed".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert!(json.get("result").is_none());
        assert_eq!(json["errorMessage"], "compaction failed");
    }

    #[test]
    fn apply_compaction_result_emits_structured_result_payload() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(SilentProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&events);
            let on_event: AgentEventHandler = Arc::new(move |event| {
                sink.lock().expect("lock compaction events").push(event);
            });

            let result = compaction::CompactionResult {
                summary: "Compacted 10 messages into 2".to_string(),
                first_kept_entry_id: "entry-5".to_string(),
                tokens_before: 12_000,
                details: compaction::CompactionDetails {
                    read_files: vec!["src/main.rs".to_string()],
                    modified_files: vec!["src/agent.rs".to_string()],
                },
            };

            agent_session
                .apply_compaction_result(result, on_event)
                .await
                .expect("apply compaction result");

            let payload = {
                let guard = events.lock().expect("lock captured events");
                guard
                    .iter()
                    .find_map(|event| match event {
                        AgentEvent::AutoCompactionEnd {
                            result: Some(result),
                            ..
                        } => Some(result.clone()),
                        _ => None,
                    })
                    .expect("auto compaction end payload")
            };

            assert_eq!(payload["summary"], "Compacted 10 messages into 2");
            assert_eq!(payload["firstKeptEntryId"], "entry-5");
            assert_eq!(payload["tokensBefore"], 12_000);
            assert_eq!(payload["details"]["readFiles"], json!(["src/main.rs"]));
            assert_eq!(payload["details"]["modifiedFiles"], json!(["src/agent.rs"]));
        });
    }

    #[test]
    fn auto_retry_start_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoRetryStart {
            attempt: 2,
            max_attempts: 3,
            delay_ms: 4000,
            error_message: "rate limited".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_start");
        assert_eq!(json["attempt"], 2);
        assert_eq!(json["maxAttempts"], 3);
        assert_eq!(json["delayMs"], 4000);
        assert_eq!(json["errorMessage"], "rate limited");
    }

    #[test]
    fn auto_retry_end_success_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoRetryEnd {
            success: true,
            attempt: 2,
            final_error: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], true);
        assert_eq!(json["attempt"], 2);
        assert!(json.get("finalError").is_none());
    }

    #[test]
    fn auto_retry_end_failure_serializes_final_error() {
        let event = AgentEvent::AutoRetryEnd {
            success: false,
            attempt: 3,
            final_error: Some("max retries exceeded".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], false);
        assert_eq!(json["attempt"], 3);
        assert_eq!(json["finalError"], "max retries exceeded");
    }
}
