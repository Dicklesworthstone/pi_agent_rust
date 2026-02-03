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
use crate::session_index::SessionIndex;
use crate::tools::{ToolOutput, ToolRegistry, ToolUpdate};
use chrono::Utc;
use futures::StreamExt;
use futures::future::BoxFuture;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::watch;

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
    tx: watch::Sender<bool>,
}

/// Signal for observing abort requests.
#[derive(Debug, Clone)]
pub struct AbortSignal {
    rx: watch::Receiver<bool>,
}

impl AbortHandle {
    /// Create a new abort handle + signal pair.
    #[must_use]
    pub fn new() -> (Self, AbortSignal) {
        let (tx, rx) = watch::channel(false);
        (Self { tx }, AbortSignal { rx })
    }

    /// Trigger an abort.
    pub fn abort(&self) {
        let _ = self.tx.send(true);
    }
}

impl AbortSignal {
    /// Check if an abort has already been requested.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        *self.rx.borrow()
    }

    async fn wait(&mut self) {
        if *self.rx.borrow() {
            return;
        }
        loop {
            if self.rx.changed().await.is_err() {
                return;
            }
            if *self.rx.borrow() {
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

        let mut pending_messages = self.fetch_messages(self.steering_fetcher.as_ref()).await;
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
                    pending_messages = self.fetch_messages(self.steering_fetcher.as_ref()).await;
                }
            }

            let follow_up = self.fetch_messages(self.follow_up_fetcher.as_ref()).await;
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

    /// Stream an assistant response and emit message events.
    #[allow(clippy::too_many_lines)]
    async fn stream_assistant_response(
        &mut self,
        on_event: &Arc<dyn Fn(AgentEvent) + Send + Sync>,
        mut abort: Option<AbortSignal>,
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
            let event_result = if let Some(signal) = abort.as_mut() {
                tokio::select! {
                    () = signal.wait() => {
                        let abort_message = self.build_abort_message(partial_message.take());
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(abort_message.clone()),
                            assistant_message_event: Box::new(AssistantMessageEvent::Error {
                                reason: StopReason::Aborted,
                                error: abort_message.clone(),
                            }),
                        });
                        return Ok(self.finalize_assistant_message(abort_message, on_event, added_partial));
                    }
                    event = stream.next() => event,
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
                    self.update_partial_message(&mut partial_message, &partial, added_partial);
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
                    self.update_partial_message(&mut partial_message, &partial, added_partial);
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
                    self.update_partial_message(&mut partial_message, &partial, added_partial);
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
                    self.update_partial_message(&mut partial_message, &partial, added_partial);
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
                    self.update_partial_message(&mut partial_message, &partial, added_partial);
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
                    self.update_partial_message(&mut partial_message, &partial, added_partial);
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
                    self.update_partial_message(&mut partial_message, &partial, added_partial);
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
                    self.update_partial_message(&mut partial_message, &partial, added_partial);
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
                    self.update_partial_message(&mut partial_message, &partial, added_partial);
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(partial.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::ToolCallEnd {
                            content_index,
                            tool_call,
                            partial,
                        }),
                    });
                }
                StreamEvent::Done { reason, message } => {
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(message.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::Done {
                            reason,
                            message: message.clone(),
                        }),
                    });
                    return Ok(self.finalize_assistant_message(message, on_event, added_partial));
                }
                StreamEvent::Error { reason, error } => {
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(error.clone()),
                        assistant_message_event: Box::new(AssistantMessageEvent::Error {
                            reason,
                            error: error.clone(),
                        }),
                    });
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
        added_partial: bool,
    ) {
        *partial_message = Some(partial.clone());
        if added_partial {
            if let Some(last) = self.messages.last_mut() {
                *last = Message::Assistant(partial.clone());
            }
        } else {
            self.messages.push(Message::Assistant(partial.clone()));
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

            let (output, is_error) = self.execute_tool(tool_call, on_event).await;

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

            // Check for steering messages after each tool
            let steering = self.fetch_messages(self.steering_fetcher.as_ref()).await;
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
            Ok(output) => (output, false),
            Err(e) => (
                ToolOutput {
                    content: vec![ContentBlock::Text(TextContent::new(format!("Error: {e}")))],
                    details: None,
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
    session_index: Option<SessionIndex>,
    save_enabled: bool,
}

impl AgentSession {
    pub fn new(agent: Agent, session: Session, save_enabled: bool) -> Self {
        let session_index = if save_enabled {
            Some(SessionIndex::new())
        } else {
            None
        };
        Self {
            agent,
            session,
            session_index,
            save_enabled,
        }
    }

    pub const fn save_enabled(&self) -> bool {
        self.save_enabled
    }

    pub async fn save_and_index(&mut self) -> Result<()> {
        if self.save_enabled {
            self.session.save().await?;
            if let Some(index) = &self.session_index {
                index.index_session(&self.session)?;
            }
        }
        Ok(())
    }

    pub async fn persist_session(&mut self) -> Result<()> {
        if !self.save_enabled {
            return Ok(());
        }
        self.session.save().await?;
        if let Some(index) = &self.session_index {
            index.index_session(&self.session)?;
        }
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
            if let Some(index) = &self.session_index {
                index.index_session(&self.session)?;
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
}
