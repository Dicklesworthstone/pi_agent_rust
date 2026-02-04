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
use crate::model::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Message, StopReason, StreamEvent,
    TextContent, ToolCall, ToolResultMessage, Usage, UserContent, UserMessage,
};
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use crate::session::Session;
use crate::tools::{ToolOutput, ToolRegistry, ToolUpdate};
use asupersync::sync::Notify;
use chrono::Utc;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
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
    AgentStart,
    /// Agent lifecycle end with all new messages.
    AgentEnd {
        messages: Vec<Message>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Turn lifecycle start (assistant response + tool calls).
    TurnStart,
    /// Turn lifecycle end with tool results.
    TurnEnd {
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
        let mut iterations = 0usize;
        let mut new_messages: Vec<Message> = Vec::new();
        let mut last_assistant: Option<AssistantMessage> = None;

        on_event(AgentEvent::AgentStart);
        on_event(AgentEvent::TurnStart);

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
        let mut turn_started = true; // already emitted turn_start

        loop {
            let mut has_more_tool_calls = true;
            let mut steering_after_tools: Option<Vec<Message>> = None;

            while has_more_tool_calls || !pending_messages.is_empty() {
                if turn_started {
                    turn_started = false;
                } else {
                    on_event(AgentEvent::TurnStart);
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
                    on_event(AgentEvent::TurnEnd {
                        message,
                        tool_results: Vec::new(),
                    });
                    on_event(AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                        error: Some(
                            abort_message
                                .error_message
                                .clone()
                                .unwrap_or_else(|| "Aborted".to_string()),
                        ),
                    });
                    return Ok(abort_message);
                }

                for message in std::mem::take(&mut pending_messages) {
                    self.messages.push(message.clone());
                    new_messages.push(message.clone());
                    on_event(AgentEvent::MessageStart {
                        message: message.clone(),
                    });
                    on_event(AgentEvent::MessageEnd { message });
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
                    on_event(AgentEvent::TurnEnd {
                        message: assistant_event_message.clone(),
                        tool_results: Vec::new(),
                    });
                    on_event(AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                        error: assistant_message.error_message.clone(),
                    });
                    return Ok(assistant_message);
                }

                let tool_calls = extract_tool_calls(&assistant_message.content);
                has_more_tool_calls = !tool_calls.is_empty();

                let mut tool_results: Vec<ToolResultMessage> = Vec::new();
                if has_more_tool_calls {
                    iterations += 1;
                    if iterations > self.config.max_tool_iterations {
                        return Err(Error::api(format!(
                            "Maximum tool iterations ({}) exceeded",
                            self.config.max_tool_iterations
                        )));
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

                on_event(AgentEvent::TurnEnd {
                    message: assistant_event_message.clone(),
                    tool_results: tool_messages,
                });

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

        on_event(AgentEvent::AgentEnd {
            messages: new_messages.clone(),
            error: None,
        });
        Ok(final_message)
    }

    async fn fetch_messages(&self, fetcher: Option<&MessageFetcher>) -> Vec<Message> {
        if let Some(fetcher) = fetcher {
            (fetcher)().await
        } else {
            Vec::new()
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

        let mut partial_message: Option<AssistantMessage> = None;
        let mut added_partial = false;

        loop {
            let event_result = if let Some(signal) = abort.as_ref() {
                let abort_fut = signal.wait().fuse();
                let event_fut = stream.next().fuse();
                futures::pin_mut!(abort_fut, event_fut);

                match futures::future::select(abort_fut, event_fut).await {
                    futures::future::Either::Left(((), _event_fut)) => {
                        let abort_message = self.build_abort_message(partial_message.take());
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
                    partial_message = Some(partial.clone());
                    self.messages.push(Message::Assistant(partial.clone()));
                    added_partial = true;
                    on_event(AgentEvent::MessageStart {
                        message: Message::Assistant(partial),
                    });
                    if let Some(partial) = partial_message.clone() {
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(partial.clone()),
                            assistant_message_event: Box::new(AssistantMessageEvent::Start {
                                partial,
                            }),
                        });
                    }
                }
                StreamEvent::TextStart {
                    content_index,
                    partial,
                } => {
                    let started_now = self.update_partial_message(
                        &mut partial_message,
                        &partial,
                        &mut added_partial,
                    );
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(partial.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(partial.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::TextStart {
                            content_index,
                            partial,
                        }),
                    });
                }
                StreamEvent::TextDelta {
                    content_index,
                    delta,
                    partial,
                } => {
                    let started_now = self.update_partial_message(
                        &mut partial_message,
                        &partial,
                        &mut added_partial,
                    );
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(partial.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(partial.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::TextDelta {
                            content_index,
                            delta,
                            partial,
                        }),
                    });
                }
                StreamEvent::TextEnd {
                    content_index,
                    content,
                    partial,
                } => {
                    let started_now = self.update_partial_message(
                        &mut partial_message,
                        &partial,
                        &mut added_partial,
                    );
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(partial.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(partial.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::TextEnd {
                            content_index,
                            content,
                            partial,
                        }),
                    });
                }
                StreamEvent::ThinkingStart {
                    content_index,
                    partial,
                } => {
                    let started_now = self.update_partial_message(
                        &mut partial_message,
                        &partial,
                        &mut added_partial,
                    );
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(partial.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(partial.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::ThinkingStart {
                            content_index,
                            partial,
                        }),
                    });
                }
                StreamEvent::ThinkingDelta {
                    content_index,
                    delta,
                    partial,
                } => {
                    let started_now = self.update_partial_message(
                        &mut partial_message,
                        &partial,
                        &mut added_partial,
                    );
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(partial.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(partial.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::ThinkingDelta {
                            content_index,
                            delta,
                            partial,
                        }),
                    });
                }
                StreamEvent::ThinkingEnd {
                    content_index,
                    content,
                    partial,
                } => {
                    let started_now = self.update_partial_message(
                        &mut partial_message,
                        &partial,
                        &mut added_partial,
                    );
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(partial.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(partial.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::ThinkingEnd {
                            content_index,
                            content,
                            partial,
                        }),
                    });
                }
                StreamEvent::ToolCallStart {
                    content_index,
                    partial,
                } => {
                    let started_now = self.update_partial_message(
                        &mut partial_message,
                        &partial,
                        &mut added_partial,
                    );
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(partial.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(partial.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::ToolCallStart {
                            content_index,
                            partial,
                        }),
                    });
                }
                StreamEvent::ToolCallDelta {
                    content_index,
                    delta,
                    partial,
                } => {
                    let started_now = self.update_partial_message(
                        &mut partial_message,
                        &partial,
                        &mut added_partial,
                    );
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(partial.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(partial.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::ToolCallDelta {
                            content_index,
                            delta,
                            partial,
                        }),
                    });
                }
                StreamEvent::ToolCallEnd {
                    content_index,
                    tool_call,
                    partial,
                } => {
                    let started_now = self.update_partial_message(
                        &mut partial_message,
                        &partial,
                        &mut added_partial,
                    );
                    if started_now {
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(partial.clone()),
                        });
                    }
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(partial.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::ToolCallEnd {
                            content_index,
                            tool_call,
                            partial,
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

    fn update_partial_message(
        &mut self,
        partial_message: &mut Option<AssistantMessage>,
        partial: &AssistantMessage,
        added_partial: &mut bool,
    ) -> bool {
        *partial_message = Some(partial.clone());
        if *added_partial {
            if let Some(last) = self.messages.last_mut() {
                *last = Message::Assistant(partial.clone());
                return false;
            }
        }
        self.messages.push(Message::Assistant(partial.clone()));
        let was_added = !*added_partial;
        *added_partial = true;
        was_added
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
        // Find the tool
        let Some(tool) = self.tools.get(&tool_call.name) else {
            return (
                ToolOutput {
                    content: vec![ContentBlock::Text(TextContent::new(format!(
                        "Error: Tool '{}' not found",
                        tool_call.name
                    )))],
                    details: None,
                    is_error: true,
                },
                true,
            );
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
    pub session: Session,
    save_enabled: bool,
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
        let session = Session::in_memory();
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
                        started_tx.notify_waiters();
                    }
                })
                .await
        });

        runtime.block_on(async move {
            started_wait.await;
            abort_handle.abort();

            let message = join.await.expect("join");
            assert_eq!(message.stop_reason, StopReason::Aborted);
            assert_eq!(message.error_message.as_deref(), Some("Aborted"));
        });
    }
}

impl AgentSession {
    pub const fn new(agent: Agent, session: Session, save_enabled: bool) -> Self {
        Self {
            agent,
            session,
            save_enabled,
        }
    }

    pub const fn save_enabled(&self) -> bool {
        self.save_enabled
    }

    pub async fn save_and_index(&mut self) -> Result<()> {
        if self.save_enabled {
            self.session.save().await?;
        }
        Ok(())
    }

    pub async fn persist_session(&mut self) -> Result<()> {
        if !self.save_enabled {
            return Ok(());
        }
        self.session.save().await?;
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
        self.agent
            .replace_messages(self.session.to_messages_for_current_path());
        let start_len = self.agent.messages().len();
        let result = self.agent.run_with_abort(input, abort, on_event).await?;
        self.persist_new_messages(start_len).await?;
        Ok(result)
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
        self.agent
            .replace_messages(self.session.to_messages_for_current_path());
        let start_len = self.agent.messages().len();
        let result = self
            .agent
            .run_with_content_with_abort(content, abort, on_event)
            .await?;
        self.persist_new_messages(start_len).await?;
        Ok(result)
    }

    async fn persist_new_messages(&mut self, start_len: usize) -> Result<()> {
        let new_messages = self.agent.messages()[start_len..].to_vec();
        for message in new_messages {
            self.session.append_model_message(message);
        }
        if self.save_enabled {
            self.session.save().await?;
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
        match message {
            Message::User(UserMessage {
                content: UserContent::Text(text),
                ..
            }) => assert_eq!(text, expected),
            other => panic!("expected user text message, got {other:?}"),
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
