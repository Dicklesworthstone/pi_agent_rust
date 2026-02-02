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
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ToolCall,
    ToolResultMessage, UserContent, UserMessage,
};
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use serde::Serialize;
use crate::session::Session;
use crate::session_index::SessionIndex;
use crate::tools::ToolRegistry;
use chrono::Utc;
use futures::StreamExt;
use std::sync::Arc;

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

// ============================================================================
// Agent Event
// ============================================================================

/// Events emitted by the agent during execution.
#[derive(Debug, Clone, Serialize)]
pub enum AgentEvent {
    /// Starting a new LLM request.
    RequestStart,

    /// Streaming text delta from the assistant.
    TextDelta { text: String },

    /// Streaming thinking delta from the assistant.
    ThinkingDelta { text: String },

    /// Tool call starting.
    ToolCallStart { name: String, id: String },

    /// Tool execution starting.
    ToolExecuteStart { name: String, id: String },

    /// Tool execution completed.
    ToolExecuteEnd {
        name: String,
        id: String,
        is_error: bool,
    },

    /// Tool execution update (streaming output).
    ToolUpdate {
        name: String,
        id: String,
        content: Vec<ContentBlock>,
        details: Option<serde_json::Value>,
    },

    /// Assistant message completed.
    AssistantDone { message: AssistantMessage },

    /// Error during execution.
    Error { error: String },

    /// Agent loop completed.
    Done { final_message: AssistantMessage },
}

// ============================================================================
// Agent
// ============================================================================

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
}

impl Agent {
    /// Create a new agent with the given provider and tools.
    pub fn new(provider: Arc<dyn Provider>, tools: ToolRegistry, config: AgentConfig) -> Self {
        Self {
            provider,
            tools,
            config,
            messages: Vec::new(),
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
        // Add user message
        let user_message = UserMessage {
            content: UserContent::Text(user_input.into()),
            timestamp: Utc::now().timestamp_millis(),
        };
        self.messages.push(Message::User(user_message));

        // Run the agent loop
        self.run_loop(Arc::new(on_event)).await
    }

    /// Run the agent with structured content (text + images).
    pub async fn run_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        // Add user message
        let user_message = UserMessage {
            content: UserContent::Blocks(content),
            timestamp: Utc::now().timestamp_millis(),
        };
        self.messages.push(Message::User(user_message));

        // Run the agent loop
        self.run_loop(Arc::new(on_event)).await
    }

    /// The main agent loop.
    async fn run_loop(
        &mut self,
        on_event: Arc<dyn Fn(AgentEvent) + Send + Sync>,
    ) -> Result<AssistantMessage> {
        let mut iterations = 0;

        loop {
            iterations += 1;
            if iterations > self.config.max_tool_iterations {
                return Err(Error::api(format!(
                    "Maximum tool iterations ({}) exceeded",
                    self.config.max_tool_iterations
                )));
            }

            on_event(AgentEvent::RequestStart);

            // Build context and stream completion
            let context = self.build_context();
            let mut stream = self
                .provider
                .stream(&context, &self.config.stream_options)
                .await?;

            // Process stream events
            let assistant_message = self.process_stream(&mut stream, &on_event).await?;

            // Add assistant message to history
            self.messages
                .push(Message::Assistant(assistant_message.clone()));

            on_event(AgentEvent::AssistantDone {
                message: assistant_message.clone(),
            });

            // Check if we need to execute tools
            let tool_calls = extract_tool_calls(&assistant_message.content);

            if tool_calls.is_empty() || assistant_message.stop_reason != StopReason::ToolUse {
                // No tool calls or not a tool use stop - we're done
                on_event(AgentEvent::Done {
                    final_message: assistant_message.clone(),
                });
                return Ok(assistant_message);
            }

            // Execute tool calls
            for tool_call in tool_calls {
                on_event(AgentEvent::ToolExecuteStart {
                    name: tool_call.name.clone(),
                    id: tool_call.id.clone(),
                });

                let tool_result = self.execute_tool(tool_call, &on_event).await;

                let is_error = tool_result.is_error;
                on_event(AgentEvent::ToolExecuteEnd {
                    name: tool_call.name.clone(),
                    id: tool_call.id.clone(),
                    is_error,
                });

                // Add tool result to history
                self.messages.push(Message::ToolResult(tool_result));
            }

            // Continue loop to get next assistant response
        }
    }

    /// Process a stream of events into an assistant message.
    async fn process_stream(
        &self,
        stream: &mut std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>,
        on_event: &Arc<dyn Fn(AgentEvent) + Send + Sync>,
    ) -> Result<AssistantMessage> {
        let mut final_message: Option<AssistantMessage> = None;

        while let Some(event_result) = stream.next().await {
            let event = event_result?;

            match event {
                StreamEvent::TextDelta { delta, .. } => {
                    on_event(AgentEvent::TextDelta { text: delta });
                }
                StreamEvent::ThinkingDelta { delta, .. } => {
                    on_event(AgentEvent::ThinkingDelta { text: delta });
                }
                StreamEvent::ToolCallStart { partial, .. } => {
                    // Find the tool call being started
                    if let Some(ContentBlock::ToolCall(tool_call)) = partial.content.last() {
                        on_event(AgentEvent::ToolCallStart {
                            name: tool_call.name.clone(),
                            id: tool_call.id.clone(),
                        });
                    }
                }
                StreamEvent::Done { message, .. } => {
                    final_message = Some(message);
                }
                StreamEvent::Error { error, .. } => {
                    let error_msg = error
                        .error_message
                        .unwrap_or_else(|| "Unknown error".to_string());
                    on_event(AgentEvent::Error {
                        error: error_msg.clone(),
                    });
                    return Err(Error::api(error_msg));
                }
                _ => {
                    // Other events (Start, TextStart, TextEnd, etc.) are handled
                    // by the partial message accumulation in the provider
                }
            }
        }

        final_message.ok_or_else(|| Error::api("Stream ended without Done event"))
    }

    /// Execute a tool call.
    async fn execute_tool(
        &self,
        tool_call: &ToolCall,
        on_event: &Arc<dyn Fn(AgentEvent) + Send + Sync>,
    ) -> ToolResultMessage {
        let timestamp = Utc::now().timestamp_millis();

        // Find the tool
        let Some(tool) = self.tools.get(&tool_call.name) else {
            return ToolResultMessage {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                content: vec![ContentBlock::Text(TextContent::new(format!(
                    "Error: Tool '{}' not found",
                    tool_call.name
                )))],
                details: None,
                is_error: true,
                timestamp,
            };
        };

        // Execute the tool
        let tool_name = tool_call.name.clone();
        let tool_id = tool_call.id.clone();
        let on_event = Arc::clone(on_event);

        let update_callback = {
            move |update: crate::tools::ToolUpdate| {
                on_event(AgentEvent::ToolUpdate {
                    name: tool_name.clone(),
                    id: tool_id.clone(),
                    content: update.content,
                    details: update.details,
                });
            }
        };

        match tool
            .execute(
                &tool_call.id,
                tool_call.arguments.clone(),
                Some(Box::new(update_callback)),
            )
            .await
        {
            Ok(output) => ToolResultMessage {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                content: output.content,
                details: output.details,
                is_error: false,
                timestamp,
            },
            Err(e) => ToolResultMessage {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                content: vec![ContentBlock::Text(TextContent::new(format!("Error: {e}")))],
                details: None,
                is_error: true,
                timestamp,
            },
        }
    }
}

// ============================================================================
// Agent Session (Agent + Session persistence)
// ============================================================================

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

    pub async fn run_text(
        &mut self,
        input: String,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let start_len = self.agent.messages().len();
        let result = self.agent.run(input, on_event).await?;
        self.persist_new_messages(start_len).await?;
        Ok(result)
    }

    pub async fn run_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let start_len = self.agent.messages().len();
        let result = self.agent.run_with_content(content, on_event).await?;
        self.persist_new_messages(start_len).await?;
        Ok(result)
    }

    async fn persist_new_messages(&mut self, start_len: usize) -> Result<()> {
        if !self.save_enabled {
            return Ok(());
        }
        let new_messages = self.agent.messages()[start_len..].to_vec();
        for message in new_messages {
            self.session.append_model_message(message);
        }
        self.session.save().await?;
        if let Some(index) = &self.session_index {
            index.index_session(&self.session)?;
        }
        Ok(())
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Extract tool calls from content blocks.
fn extract_tool_calls(content: &[ContentBlock]) -> Vec<&ToolCall> {
    content
        .iter()
        .filter_map(|block| {
            if let ContentBlock::ToolCall(tc) = block {
                Some(tc)
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
