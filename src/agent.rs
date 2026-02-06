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

use crate::error::{Error, Result};
use crate::extension_events::{InputEventOutcome, apply_input_event_response};
use crate::extension_tools::collect_extension_tool_wrappers;
use crate::extensions::{
    EXTENSION_EVENT_TIMEOUT_MS, ExtensionDeliverAs, ExtensionEventName, ExtensionHostActions,
    ExtensionLoadSpec, ExtensionManager, ExtensionRegion, ExtensionSendMessage,
    ExtensionSendUserMessage, JsExtensionRuntimeHandle, resolve_extension_load_spec,
};
#[cfg(feature = "wasm-host")]
use crate::extensions::{ExtensionPolicy, WasmExtensionHost, WasmExtensionLoadSpec};
use crate::extensions_js::PiJsRuntimeConfig;
use crate::model::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, CustomMessage, ImageContent, Message,
    StopReason, StreamEvent, TextContent, ToolCall, ToolResultMessage, Usage, UserContent,
    UserMessage,
};
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use crate::session::{Session, SessionHandle};
use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
use asupersync::sync::{Mutex, Notify};
use async_trait::async_trait;
use chrono::Utc;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

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
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            max_tool_iterations: 50,
            stream_options: StreamOptions::default(),
        }
    }
}

/// Async fetcher for queued messages (steering or follow-up).
pub type MessageFetcher = Arc<dyn Fn() -> BoxFuture<'static, Vec<Message>> + Send + Sync + 'static>;

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
            QueueKind::Steering => self.steering.push_back(entry),
            QueueKind::FollowUp => self.follow_up.push_back(entry),
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
        session_id: String,
    },
    /// Agent lifecycle end with all new messages.
    AgentEnd {
        #[serde(rename = "sessionId")]
        session_id: String,
        messages: Vec<Message>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Turn lifecycle start (assistant response + tool calls).
    TurnStart {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "turnIndex")]
        turn_index: usize,
        timestamp: i64,
    },
    /// Turn lifecycle end with tool results.
    TurnEnd {
        #[serde(rename = "sessionId")]
        session_id: String,
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
        assistant_message_event: Box<AssistantMessageEvent>,
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

    async fn wait(&self) {
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

    /// Steering message fetcher (interrupt).
    steering_fetcher: Option<MessageFetcher>,

    /// Follow-up message fetcher (after idle).
    follow_up_fetcher: Option<MessageFetcher>,

    /// Internal queue for steering/follow-up messages.
    message_queue: MessageQueue,
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
            steering_fetcher: None,
            follow_up_fetcher: None,
            message_queue: MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime),
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

    /// Configure async fetchers for queued steering/follow-up messages.
    pub fn set_message_fetchers(
        &mut self,
        steering: Option<MessageFetcher>,
        follow_up: Option<MessageFetcher>,
    ) {
        self.steering_fetcher = steering;
        self.follow_up_fetcher = follow_up;
    }

    /// Extend the tool registry with additional tools (e.g. extension-registered tools).
    pub fn extend_tools<I>(&mut self, tools: I)
    where
        I: IntoIterator<Item = Box<dyn Tool>>,
    {
        self.tools.extend(tools);
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

    /// Build tool definitions for the API.
    fn build_tool_defs(&self) -> Vec<ToolDef> {
        self.tools
            .tools()
            .iter()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect()
    }

    /// Build context for a completion request.
    fn build_context(&self) -> Context {
        Context {
            system_prompt: self.config.system_prompt.clone(),
            messages: self.messages.clone(),
            tools: self.build_tool_defs(),
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

    /// Continue the agent loop without adding a new prompt message (used for retries).
    pub async fn run_continue_with_abort(
        &mut self,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_loop(Vec::new(), Arc::new(on_event), abort).await
    }

    fn build_abort_message(&self, partial: Option<AssistantMessage>) -> AssistantMessage {
        let mut message = partial.unwrap_or_else(|| AssistantMessage {
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

    /// The main agent loop.
    #[allow(clippy::too_many_lines)]
    async fn run_loop(
        &mut self,
        prompts: Vec<Message>,
        on_event: Arc<dyn Fn(AgentEvent) + Send + Sync>,
        abort: Option<AbortSignal>,
    ) -> Result<AssistantMessage> {
        let session_id = self
            .config
            .stream_options
            .session_id
            .clone()
            .unwrap_or_default();
        let mut iterations = 0usize;
        let mut turn_index: usize = 0;
        let mut new_messages: Vec<Message> = Vec::new();
        let mut last_assistant: Option<AssistantMessage> = None;

        let agent_start_event = AgentEvent::AgentStart {
            session_id: session_id.clone(),
        };
        on_event(agent_start_event.clone());
        self.dispatch_extension_lifecycle_event(&agent_start_event)
            .await;

        for prompt in prompts {
            self.messages.push(prompt.clone());
            new_messages.push(prompt.clone());
            on_event(AgentEvent::MessageStart {
                message: prompt.clone(),
            });
            on_event(AgentEvent::MessageEnd { message: prompt });
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
                on_event(turn_start_event.clone());
                self.dispatch_extension_lifecycle_event(&turn_start_event)
                    .await;

                for message in std::mem::take(&mut pending_messages) {
                    self.messages.push(message.clone());
                    new_messages.push(message.clone());
                    on_event(AgentEvent::MessageStart {
                        message: message.clone(),
                    });
                    on_event(AgentEvent::MessageEnd { message });
                }

                if abort.as_ref().is_some_and(AbortSignal::is_aborted) {
                    let abort_message = self.build_abort_message(last_assistant.clone());
                    let message = Message::Assistant(abort_message.clone());
                    if !matches!(self.messages.last(), Some(Message::Assistant(_))) {
                        self.messages.push(message.clone());
                        new_messages.push(message.clone());
                        on_event(AgentEvent::MessageStart {
                            message: message.clone(),
                        });
                    }
                    on_event(AgentEvent::MessageEnd {
                        message: message.clone(),
                    });
                    let turn_end_event = AgentEvent::TurnEnd {
                        session_id: session_id.clone(),
                        turn_index: current_turn_index,
                        message,
                        tool_results: Vec::new(),
                    };
                    on_event(turn_end_event.clone());
                    self.dispatch_extension_lifecycle_event(&turn_end_event)
                        .await;
                    let agent_end_event = AgentEvent::AgentEnd {
                        session_id: session_id.clone(),
                        messages: new_messages.clone(),
                        error: Some(
                            abort_message
                                .error_message
                                .clone()
                                .unwrap_or_else(|| "Aborted".to_string()),
                        ),
                    };
                    on_event(agent_end_event.clone());
                    self.dispatch_extension_lifecycle_event(&agent_end_event)
                        .await;
                    return Ok(abort_message);
                }

                let assistant_message = self
                    .stream_assistant_response(&on_event, abort.clone())
                    .await?;
                last_assistant = Some(assistant_message.clone());

                let assistant_event_message = Message::Assistant(assistant_message.clone());
                new_messages.push(assistant_event_message.clone());

                if matches!(
                    assistant_message.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    let turn_end_event = AgentEvent::TurnEnd {
                        session_id: session_id.clone(),
                        turn_index: current_turn_index,
                        message: assistant_event_message.clone(),
                        tool_results: Vec::new(),
                    };
                    on_event(turn_end_event.clone());
                    self.dispatch_extension_lifecycle_event(&turn_end_event)
                        .await;
                    let agent_end_event = AgentEvent::AgentEnd {
                        session_id: session_id.clone(),
                        messages: new_messages.clone(),
                        error: assistant_message.error_message.clone(),
                    };
                    on_event(agent_end_event.clone());
                    self.dispatch_extension_lifecycle_event(&agent_end_event)
                        .await;
                    return Ok(assistant_message);
                }

                let tool_calls = extract_tool_calls(&assistant_message.content);
                has_more_tool_calls = !tool_calls.is_empty();

                let mut tool_results: Vec<ToolResultMessage> = Vec::new();
                if has_more_tool_calls {
                    iterations += 1;
                    if iterations > self.config.max_tool_iterations {
                        let error_message = format!(
                            "Maximum tool iterations ({}) exceeded",
                            self.config.max_tool_iterations
                        );
                        let mut stop_message = assistant_message.clone();
                        stop_message.stop_reason = StopReason::Error;
                        stop_message.error_message = Some(error_message.clone());

                        let turn_end_event = AgentEvent::TurnEnd {
                            session_id: session_id.clone(),
                            turn_index: current_turn_index,
                            message: assistant_event_message.clone(),
                            tool_results: Vec::new(),
                        };
                        on_event(turn_end_event.clone());
                        self.dispatch_extension_lifecycle_event(&turn_end_event)
                            .await;

                        let agent_end_event = AgentEvent::AgentEnd {
                            session_id: session_id.clone(),
                            messages: new_messages.clone(),
                            error: Some(error_message),
                        };
                        on_event(agent_end_event.clone());
                        self.dispatch_extension_lifecycle_event(&agent_end_event)
                            .await;

                        return Ok(stop_message);
                    }

                    let outcome = self
                        .execute_tool_calls(
                            &tool_calls,
                            &on_event,
                            &mut new_messages,
                            abort.clone(),
                        )
                        .await?;
                    tool_results = outcome.tool_results;
                    steering_after_tools = outcome.steering_messages;
                }

                let tool_messages = tool_results
                    .iter()
                    .cloned()
                    .map(Message::ToolResult)
                    .collect::<Vec<_>>();

                let turn_end_event = AgentEvent::TurnEnd {
                    session_id: session_id.clone(),
                    turn_index: current_turn_index,
                    message: assistant_event_message.clone(),
                    tool_results: tool_messages,
                };
                on_event(turn_end_event.clone());
                self.dispatch_extension_lifecycle_event(&turn_end_event)
                    .await;

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

        let Some(final_message) = last_assistant else {
            return Err(Error::api("Agent completed without assistant message"));
        };

        let agent_end_event = AgentEvent::AgentEnd {
            session_id: session_id.clone(),
            messages: new_messages.clone(),
            error: None,
        };
        on_event(agent_end_event.clone());
        self.dispatch_extension_lifecycle_event(&agent_end_event)
            .await;
        Ok(final_message)
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

    async fn drain_steering_messages(&mut self) -> Vec<Message> {
        let mut messages = self.message_queue.pop_steering();
        messages.extend(self.fetch_messages(self.steering_fetcher.as_ref()).await);
        messages
    }

    async fn drain_follow_up_messages(&mut self) -> Vec<Message> {
        let mut messages = self.message_queue.pop_follow_up();
        messages.extend(self.fetch_messages(self.follow_up_fetcher.as_ref()).await);
        messages
    }

    /// Stream an assistant response and emit message events.
    #[allow(clippy::too_many_lines)]
    async fn stream_assistant_response(
        &mut self,
        on_event: &Arc<dyn Fn(AgentEvent) + Send + Sync>,
        abort: Option<AbortSignal>,
    ) -> Result<AssistantMessage> {
        // Build context and stream completion
        let context = self.build_context();
        let mut stream = self
            .provider
            .stream(&context, &self.config.stream_options)
            .await?;

        let mut added_partial = false;

        loop {
            let event_result = if let Some(signal) = abort.as_ref() {
                let abort_fut = signal.wait().fuse();
                let event_fut = stream.next().fuse();
                futures::pin_mut!(abort_fut, event_fut);

                match futures::future::select(abort_fut, event_fut).await {
                    futures::future::Either::Left(((), _event_fut)) => {
                        let last_partial = if added_partial {
                            match self.messages.last() {
                                Some(Message::Assistant(a)) => Some(a.clone()),
                                _ => None,
                            }
                        } else {
                            None
                        };
                        let abort_message = self.build_abort_message(last_partial);
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(abort_message.clone()),
                            assistant_message_event: Box::new(AssistantMessageEvent::Error {
                                reason: StopReason::Aborted,
                                error: abort_message.clone(),
                            }),
                        });
                        return Ok(self.finalize_assistant_message(
                            abort_message,
                            on_event,
                            added_partial,
                        ));
                    }
                    futures::future::Either::Right((event, _abort_fut)) => event,
                }
            } else {
                stream.next().await
            };

            let Some(event_result) = event_result else {
                break;
            };
            let event = event_result?;

            match event {
                StreamEvent::Start { partial } => {
                    let ev = partial.clone();
                    self.update_partial_message(partial, &mut added_partial);
                    on_event(AgentEvent::MessageStart {
                        message: Message::Assistant(ev.clone()),
                    });
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(ev.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::Start {
                            partial: ev,
                        }),
                    });
                }
                StreamEvent::TextStart {
                    content_index,
                    partial,
                } => {
                    let ev = partial.clone();
                    let started_now = self.update_partial_message(partial, &mut added_partial);
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(ev.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(ev.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::TextStart {
                            content_index,
                            partial: ev,
                        }),
                    });
                }
                StreamEvent::TextDelta {
                    content_index,
                    delta,
                    partial,
                } => {
                    let ev = partial.clone();
                    let started_now = self.update_partial_message(partial, &mut added_partial);
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(ev.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(ev.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::TextDelta {
                            content_index,
                            delta,
                            partial: ev,
                        }),
                    });
                }
                StreamEvent::TextEnd {
                    content_index,
                    content,
                    partial,
                } => {
                    let ev = partial.clone();
                    let started_now = self.update_partial_message(partial, &mut added_partial);
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(ev.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(ev.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::TextEnd {
                            content_index,
                            content,
                            partial: ev,
                        }),
                    });
                }
                StreamEvent::ThinkingStart {
                    content_index,
                    partial,
                } => {
                    let ev = partial.clone();
                    let started_now = self.update_partial_message(partial, &mut added_partial);
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(ev.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(ev.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::ThinkingStart {
                            content_index,
                            partial: ev,
                        }),
                    });
                }
                StreamEvent::ThinkingDelta {
                    content_index,
                    delta,
                    partial,
                } => {
                    let ev = partial.clone();
                    let started_now = self.update_partial_message(partial, &mut added_partial);
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(ev.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(ev.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::ThinkingDelta {
                            content_index,
                            delta,
                            partial: ev,
                        }),
                    });
                }
                StreamEvent::ThinkingEnd {
                    content_index,
                    content,
                    partial,
                } => {
                    let ev = partial.clone();
                    let started_now = self.update_partial_message(partial, &mut added_partial);
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(ev.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(ev.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::ThinkingEnd {
                            content_index,
                            content,
                            partial: ev,
                        }),
                    });
                }
                StreamEvent::ToolCallStart {
                    content_index,
                    partial,
                } => {
                    let ev = partial.clone();
                    let started_now = self.update_partial_message(partial, &mut added_partial);
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(ev.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(ev.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::ToolCallStart {
                            content_index,
                            partial: ev,
                        }),
                    });
                }
                StreamEvent::ToolCallDelta {
                    content_index,
                    delta,
                    partial,
                } => {
                    let ev = partial.clone();
                    let started_now = self.update_partial_message(partial, &mut added_partial);
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(ev.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(ev.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::ToolCallDelta {
                            content_index,
                            delta,
                            partial: ev,
                        }),
                    });
                }
                StreamEvent::ToolCallEnd {
                    content_index,
                    tool_call,
                    partial,
                } => {
                    let ev = partial.clone();
                    let started_now = self.update_partial_message(partial, &mut added_partial);
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(ev.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(ev.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::ToolCallEnd {
                            content_index,
                            tool_call,
                            partial: ev,
                        }),
                    });
                }
                StreamEvent::Done { message, .. } => {
                    return Ok(self.finalize_assistant_message(message, on_event, added_partial));
                }
                StreamEvent::Error { error, .. } => {
                    return Ok(self.finalize_assistant_message(error, on_event, added_partial));
                }
            }
        }

        Err(Error::api("Stream ended without Done event"))
    }

    /// Update the partial assistant message in `self.messages`.
    ///
    /// Takes ownership of `partial` and moves it into the message list (one move,
    /// zero deep-copies). The caller should clone *before* calling if it needs the
    /// partial for event emission.
    fn update_partial_message(
        &mut self,
        partial: AssistantMessage,
        added_partial: &mut bool,
    ) -> bool {
        if *added_partial {
            if let Some(last) = self.messages.last_mut() {
                *last = Message::Assistant(partial);
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
        if added_partial {
            if let Some(last) = self.messages.last_mut() {
                *last = Message::Assistant(message.clone());
            }
        } else {
            self.messages.push(Message::Assistant(message.clone()));
            on_event(AgentEvent::MessageStart {
                message: Message::Assistant(message.clone()),
            });
        }

        on_event(AgentEvent::MessageEnd {
            message: Message::Assistant(message.clone()),
        });
        message
    }

    async fn execute_tool_calls(
        &mut self,
        tool_calls: &[ToolCall],
        on_event: &Arc<dyn Fn(AgentEvent) + Send + Sync>,
        new_messages: &mut Vec<Message>,
        abort: Option<AbortSignal>,
    ) -> Result<ToolExecutionOutcome> {
        let mut results = Vec::new();
        let mut steering_messages: Option<Vec<Message>> = None;

        for (index, tool_call) in tool_calls.iter().enumerate() {
            if abort.as_ref().is_some_and(AbortSignal::is_aborted) {
                break;
            }

            on_event(AgentEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            });

            let tool_execution = self.execute_tool(tool_call, on_event);

            let (output, is_error) = if let Some(signal) = abort.as_ref() {
                use futures::future::{Either, select};

                let tool_fut = tool_execution.fuse();
                let abort_fut = signal.wait().fuse();
                futures::pin_mut!(tool_fut, abort_fut);

                match select(tool_fut, abort_fut).await {
                    Either::Left((result, _)) => result,
                    Either::Right(_) => {
                        // Aborted
                        let output = ToolOutput {
                            content: vec![ContentBlock::Text(TextContent::new(
                                "Tool execution aborted",
                            ))],
                            details: None,
                            is_error: true,
                        };
                        (output, true)
                    }
                }
            } else {
                tool_execution.await
            };

            // Emit a final update so UIs can render tool output even if the tool
            // doesn't stream incremental updates.
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
                is_error,
            });

            let tool_result = ToolResultMessage {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                content: output.content.clone(),
                details: output.details.clone(),
                is_error,
                timestamp: Utc::now().timestamp_millis(),
            };

            self.messages.push(Message::ToolResult(tool_result.clone()));
            new_messages.push(Message::ToolResult(tool_result.clone()));

            on_event(AgentEvent::MessageStart {
                message: Message::ToolResult(tool_result.clone()),
            });
            on_event(AgentEvent::MessageEnd {
                message: Message::ToolResult(tool_result.clone()),
            });

            results.push(tool_result);

            if abort.as_ref().is_some_and(AbortSignal::is_aborted) {
                break;
            }

            // Delivery boundary: after tool completion (steering interrupts).
            let steering = self.drain_steering_messages().await;
            if !steering.is_empty() {
                steering_messages = Some(steering);

                // Skip remaining tool calls
                for skipped in tool_calls.iter().skip(index + 1) {
                    let skipped_result = self.skip_tool_call(skipped, on_event, new_messages);
                    results.push(skipped_result);
                }
                break;
            }
        }

        Ok(ToolExecutionOutcome {
            tool_results: results,
            steering_messages,
        })
    }

    async fn execute_tool(
        &self,
        tool_call: &ToolCall,
        on_event: &Arc<dyn Fn(AgentEvent) + Send + Sync>,
    ) -> (ToolOutput, bool) {
        let extensions = self.extensions.clone();

        let (mut output, is_error) = if let Some(extensions) = &extensions {
            match Self::dispatch_tool_call_hook(extensions, tool_call).await {
                Some(blocked_output) => (blocked_output, true),
                None => self.execute_tool_without_hooks(tool_call, on_event).await,
            }
        } else {
            self.execute_tool_without_hooks(tool_call, on_event).await
        };

        if let Some(extensions) = &extensions {
            Self::apply_tool_result_hook(extensions, tool_call, &mut output, is_error).await;
        }

        (output, is_error)
    }

    async fn execute_tool_without_hooks(
        &self,
        tool_call: &ToolCall,
        on_event: &Arc<dyn Fn(AgentEvent) + Send + Sync>,
    ) -> (ToolOutput, bool) {
        // Find the tool
        let Some(tool) = self.tools.get(&tool_call.name) else {
            return (Self::tool_not_found_output(&tool_call.name), true);
        };

        let tool_name = tool_call.name.clone();
        let tool_id = tool_call.id.clone();
        let tool_args = tool_call.arguments.clone();
        let on_event = Arc::clone(on_event);

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
                tracing::warn!("tool_call extension hook failed (fail-open): {err}");
                None
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
    ) -> ToolResultMessage {
        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(
                "Skipped due to queued user message.",
            ))],
            details: None,
            is_error: true,
        };

        on_event(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        });
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

        let tool_result = ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            content: output.content,
            details: output.details,
            is_error: true,
            timestamp: Utc::now().timestamp_millis(),
        };

        self.messages.push(Message::ToolResult(tool_result.clone()));
        new_messages.push(Message::ToolResult(tool_result.clone()));

        on_event(AgentEvent::MessageStart {
            message: Message::ToolResult(tool_result.clone()),
        });
        on_event(AgentEvent::MessageEnd {
            message: Message::ToolResult(tool_result.clone()),
        });

        tool_result
    }
}

// ============================================================================
// Agent Session (Agent + Session persistence)
// ============================================================================

struct ToolExecutionOutcome {
    tool_results: Vec<ToolResultMessage>,
    steering_messages: Option<Vec<Message>>,
}

pub struct AgentSession {
    pub agent: Agent,
    pub session: Arc<Mutex<Session>>,
    save_enabled: bool,
    /// Extension lifecycle region — ensures the JS runtime thread is shut
    /// down when the session ends.
    pub extensions: Option<ExtensionRegion>,
    extensions_is_streaming: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
struct ExtensionInjectedQueue {
    steering: VecDeque<Message>,
    follow_up: VecDeque<Message>,
}

impl ExtensionInjectedQueue {
    fn push_steering(&mut self, message: Message) {
        self.steering.push_back(message);
    }

    fn push_follow_up(&mut self, message: Message) {
        self.follow_up.push_back(message);
    }

    fn pop_steering(&mut self) -> Vec<Message> {
        self.steering.drain(..).collect()
    }

    fn pop_follow_up(&mut self) -> Vec<Message> {
        self.follow_up.drain(..).collect()
    }
}

#[derive(Clone)]
struct AgentSessionHostActions {
    session: Arc<Mutex<Session>>,
    injected: Arc<StdMutex<ExtensionInjectedQueue>>,
    is_streaming: Arc<AtomicBool>,
}

impl AgentSessionHostActions {
    fn enqueue(&self, deliver_as: Option<ExtensionDeliverAs>, message: Message) {
        let deliver_as = deliver_as.unwrap_or(ExtensionDeliverAs::Steer);
        let Ok(mut queue) = self.injected.lock() else {
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
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        session.append_model_message(message);
        Ok(())
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

        // Non-streaming, best-effort: persist to session. Triggering a new turn is handled by the
        // interactive layer; non-interactive modes will pick this up on the next prompt.
        let _ = message.trigger_turn;
        self.append_to_session(custom_message).await
    }

    async fn send_user_message(&self, message: ExtensionSendUserMessage) -> Result<()> {
        let user_message = Message::User(UserMessage {
            content: UserContent::Text(message.text),
            timestamp: Utc::now().timestamp_millis(),
        });

        if self.is_streaming.load(Ordering::SeqCst) {
            self.enqueue(message.deliver_as, user_message);
            return Ok(());
        }

        // Non-streaming, best-effort: persist to session. Interactive mode triggers turns via UI.
        self.append_to_session(user_message).await
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
            _context: &Context,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            Ok(Box::pin(futures::stream::empty()))
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
            _context: &Context,
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
            let mut agent_session = AgentSession::new(agent, session, false);

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
            let mut agent_session = AgentSession::new(agent, Arc::clone(&session), false);

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
            let session_guard = session
                .lock(cx.cx())
                .await
                .expect("lock session");
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
            let mut agent_session = AgentSession::new(agent, Arc::clone(&session), false);

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
            let session_guard = session
                .lock(cx.cx())
                .await
                .expect("lock session");
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
            let mut agent_session = AgentSession::new(agent, session, false);

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let _ = agent_session
                .run_text("go".to_string(), |_| {})
                .await
                .expect("run_text");

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
            let mut agent_session = AgentSession::new(agent, session, false);

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
            let mut agent_session = AgentSession::new(agent, session, false);

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
            let (output, is_error) = agent_session
                .agent
                .execute_tool(&tool_call, &on_event)
                .await;

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
            let mut agent_session = AgentSession::new(agent, session, false);

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
            let (output, is_error) = agent_session
                .agent
                .execute_tool(&tool_call, &on_event)
                .await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
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
            let mut agent_session = AgentSession::new(agent, session, false);

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
            let (output, is_error) = agent_session
                .agent
                .execute_tool(&tool_call, &on_event)
                .await;

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
            let mut agent_session = AgentSession::new(agent, session, false);

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
            let (output, is_error) = agent_session
                .agent
                .execute_tool(&tool_call, &on_event)
                .await;

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
            let mut agent_session = AgentSession::new(agent, session, false);

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
            let (output, is_error) = agent_session
                .agent
                .execute_tool(&tool_call, &on_event)
                .await;

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
            let mut agent_session = AgentSession::new(agent, session, false);

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
            let (output, is_error) = agent_session
                .agent
                .execute_tool(&tool_call, &on_event)
                .await;

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
            let mut agent_session = AgentSession::new(agent, session, false);

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
            let (output, is_error) = agent_session
                .agent
                .execute_tool(&tool_call, &on_event)
                .await;

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
            let mut agent_session = AgentSession::new(agent, session, false);

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
            let (output, is_error) = agent_session
                .agent
                .execute_tool(&tool_call, &on_event)
                .await;

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
            let mut agent_session = AgentSession::new(agent, session, false);

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
            let (output, is_error) = agent_session
                .agent
                .execute_tool(&tool_call, &on_event)
                .await;

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
    use crate::tools::ToolRegistry;
    use asupersync::runtime::RuntimeBuilder;
    use async_trait::async_trait;
    use futures::Stream;
    use std::path::Path;
    use std::pin::Pin;
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
            _context: &Context,
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
        let mut agent_session = AgentSession::new(agent, session, false);

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
}

#[cfg(test)]
mod turn_event_tests {
    use super::*;
    use crate::session::Session;
    use crate::tools::ToolRegistry;
    use asupersync::runtime::RuntimeBuilder;
    use async_trait::async_trait;
    use futures::Stream;
    use std::path::Path;
    use std::pin::Pin;
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
            _context: &Context,
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
        let mut agent_session = AgentSession::new(agent, session, false);

        let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_capture = Arc::clone(&events);

        let join = handle.spawn(async move {
            agent_session
                .run_text("hello".to_string(), move |event| {
                    events_capture.lock().unwrap().push(event);
                })
                .await
                .expect("run_text")
        });

        runtime.block_on(async move {
            let message = join.await;
            assert_eq!(message.stop_reason, StopReason::Stop);

            let events = events.lock().unwrap();
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
}

impl AgentSession {
    pub fn new(agent: Agent, session: Arc<Mutex<Session>>, save_enabled: bool) -> Self {
        Self {
            agent,
            session,
            save_enabled,
            extensions: None,
            extensions_is_streaming: Arc::new(AtomicBool::new(false)),
        }
    }

    pub const fn save_enabled(&self) -> bool {
        self.save_enabled
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn enable_extensions(
        &mut self,
        enabled_tools: &[&str],
        cwd: &std::path::Path,
        config: Option<&crate::config::Config>,
        extension_entries: &[std::path::PathBuf],
    ) -> Result<()> {
        let manager = ExtensionManager::new();
        manager.set_cwd(cwd.display().to_string());
        manager.set_session(Arc::new(SessionHandle(self.session.clone())));

        let injected = Arc::new(StdMutex::new(ExtensionInjectedQueue::default()));
        let host_actions = AgentSessionHostActions {
            session: Arc::clone(&self.session),
            injected: Arc::clone(&injected),
            is_streaming: Arc::clone(&self.extensions_is_streaming),
        };
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
            self.agent.set_message_fetchers(
                Some(Arc::new(steering_fetcher)),
                Some(Arc::new(follow_up_fetcher)),
            );
        }

        let tools = Arc::new(ToolRegistry::new(enabled_tools, cwd, config));
        let js_runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: cwd.display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await?;
        manager.set_js_runtime(js_runtime.clone());

        let mut js_specs = Vec::new();
        #[cfg(feature = "wasm-host")]
        let mut wasm_specs: Vec<WasmExtensionLoadSpec> = Vec::new();

        for entry in extension_entries {
            match resolve_extension_load_spec(entry)? {
                ExtensionLoadSpec::Js(spec) => js_specs.push(spec),
                #[cfg(feature = "wasm-host")]
                ExtensionLoadSpec::Wasm(spec) => wasm_specs.push(spec),
            }
        }

        if !js_specs.is_empty() {
            manager.load_js_extensions(js_specs).await?;
        }

        #[cfg(feature = "wasm-host")]
        if !wasm_specs.is_empty() {
            let host = WasmExtensionHost::new(cwd, ExtensionPolicy::default())?;
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
            session.save().await?;
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
        session.save().await?;
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
        let outcome = self.dispatch_input_event(input, Vec::new()).await?;
        let (text, images) = match outcome {
            InputEventOutcome::Continue { text, images } => (text, images),
            InputEventOutcome::Block { reason } => {
                let message = reason.unwrap_or_else(|| "Input blocked".to_string());
                return Err(Error::extension(message));
            }
        };

        self.dispatch_before_agent_start().await;

        if images.is_empty() {
            self.run_agent_with_text(text, abort, on_event).await
        } else {
            let content = Self::build_content_blocks_for_input(&text, &images);
            self.run_agent_with_content(content, abort, on_event).await
        }
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
        let (text, images) = Self::split_content_blocks_for_input(&content);
        let outcome = self.dispatch_input_event(text, images).await?;
        let (text, images) = match outcome {
            InputEventOutcome::Continue { text, images } => (text, images),
            InputEventOutcome::Block { reason } => {
                let message = reason.unwrap_or_else(|| "Input blocked".to_string());
                return Err(Error::extension(message));
            }
        };

        self.dispatch_before_agent_start().await;

        let content_for_agent = Self::build_content_blocks_for_input(&text, &images);
        self.run_agent_with_content(content_for_agent, abort, on_event)
            .await
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
        let payload = json!({
            "text": text,
            "images": images_value,
            "source": "user",
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

    async fn dispatch_before_agent_start(&self) {
        if let Some(region) = &self.extensions {
            if let Err(err) = region
                .manager()
                .dispatch_event(ExtensionEventName::BeforeAgentStart, None)
                .await
            {
                tracing::warn!("before_agent_start extension hook failed (fail-open): {err}");
            }
        }
    }

    fn split_content_blocks_for_input(blocks: &[ContentBlock]) -> (String, Vec<ImageContent>) {
        let mut text = String::new();
        let mut images = Vec::new();
        for block in blocks {
            match block {
                ContentBlock::Text(text_block) => {
                    if !text_block.text.trim().is_empty() {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&text_block.text);
                    }
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

    pub(crate) async fn run_agent_with_text(
        &mut self,
        input: String,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
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
        self.extensions_is_streaming.store(true, Ordering::SeqCst);
        let result = self.agent.run_with_abort(input, abort, on_event).await;
        self.extensions_is_streaming.store(false, Ordering::SeqCst);
        let result = result?;
        self.persist_new_messages(start_len).await?;
        Ok(result)
    }

    pub(crate) async fn run_agent_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
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
        self.extensions_is_streaming.store(true, Ordering::SeqCst);
        let result = self
            .agent
            .run_with_content_with_abort(content, abort, on_event)
            .await;
        self.extensions_is_streaming.store(false, Ordering::SeqCst);
        let result = result?;
        self.persist_new_messages(start_len).await?;
        Ok(result)
    }

    async fn persist_new_messages(&self, start_len: usize) -> Result<()> {
        let new_messages = self.agent.messages()[start_len..].to_vec();
        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            for message in new_messages {
                session.append_model_message(message);
            }
            if self.save_enabled {
                session.save().await?;
            }
        }
        Ok(())
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

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
}
