//! Interactive TUI mode using charmed_rust (bubbletea/lipgloss/bubbles/glamour).
//!
//! This module provides the full interactive terminal interface for Pi,
//! implementing the Elm Architecture for state management.
//!
//! ## Features
//!
//! - **Multi-line editor**: Full text area with line wrapping and history
//! - **Viewport scrolling**: Scrollable conversation history with keyboard navigation
//! - **Slash commands**: Built-in commands like /help, /clear, /model, /exit
//! - **Token tracking**: Real-time cost and token usage display
//! - **Markdown rendering**: Assistant responses rendered with syntax highlighting

use bubbles::spinner::{SpinnerModel, spinners};
use bubbles::textarea::TextArea;
use bubbles::viewport::Viewport;
use bubbletea::{Cmd, KeyMsg, KeyType, Message, Model as BubbleteaModel, Program, batch, quit};
use crossterm::terminal;
use glamour::{Renderer as MarkdownRenderer, Style as GlamourStyle};
use lipgloss::Style;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Arc;

use crate::agent::{AbortHandle, Agent, AgentEvent};
use crate::config::Config;
use crate::model::{ContentBlock, StopReason, ThinkingLevel, Usage, UserContent};
use crate::models::ModelEntry;
use crate::providers;
use crate::resources::ResourceLoader;
use crate::session::{Session, SessionEntry, SessionMessage};
use crate::session_index::SessionIndex;

// ============================================================================
// Slash Commands
// ============================================================================

/// Available slash commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Clear,
    Model,
    Thinking,
    Exit,
    History,
    Export,
}

impl SlashCommand {
    /// Parse a slash command from input.
    pub fn parse(input: &str) -> Option<(Self, &str)> {
        let input = input.trim();
        if !input.starts_with('/') {
            return None;
        }

        let (cmd, args) = input.split_once(char::is_whitespace).unwrap_or((input, ""));

        let command = match cmd.to_lowercase().as_str() {
            "/help" | "/h" | "/?" => Self::Help,
            "/clear" | "/cls" => Self::Clear,
            "/model" | "/m" => Self::Model,
            "/thinking" | "/think" | "/t" => Self::Thinking,
            "/exit" | "/quit" | "/q" => Self::Exit,
            "/history" | "/hist" => Self::History,
            "/export" => Self::Export,
            _ => return None,
        };

        Some((command, args.trim()))
    }

    /// Get help text for all commands.
    pub const fn help_text() -> &'static str {
        r"Available commands:
  /help, /h, /?      - Show this help message
  /clear, /cls       - Clear conversation history
  /model, /m [id|provider/id] - Show or change the current model
  /thinking, /t [level] - Set thinking level (off/minimal/low/medium/high/xhigh)
  /history, /hist    - Show input history
  /export [path]     - Export conversation to HTML
  /exit, /quit, /q   - Exit Pi

Tips:
  • Use ↑/↓ arrows or Ctrl+P/N to navigate input history
  • Use Alt+Enter to submit multi-line input
  • Use PageUp/PageDown to scroll conversation history
  • Use Escape to cancel current input
  • Use /skill:name or /template to expand resources"
    }
}

/// Custom message types for async agent events.
#[derive(Debug, Clone)]
pub enum PiMsg {
    /// Agent started processing.
    AgentStart,
    /// Trigger processing of the next queued input (CLI startup messages).
    RunPending,
    /// Text delta from assistant.
    TextDelta(String),
    /// Thinking delta from assistant.
    ThinkingDelta(String),
    /// Tool execution started.
    ToolStart { name: String, tool_id: String },
    /// Tool execution update (streaming output).
    ToolUpdate {
        name: String,
        tool_id: String,
        content: Vec<ContentBlock>,
        details: Option<Value>,
    },
    /// Tool execution ended.
    ToolEnd {
        name: String,
        tool_id: String,
        is_error: bool,
    },
    /// Agent finished with final message.
    AgentDone {
        usage: Option<Usage>,
        stop_reason: StopReason,
    },
    /// Agent error.
    AgentError(String),
}

/// State of the agent processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Ready for input.
    Idle,
    /// Processing user request.
    Processing,
    /// Executing a tool.
    ToolRunning,
}

/// Input mode for the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Single-line input mode (default).
    SingleLine,
    /// Multi-line input mode (activated with Alt+Enter or \).
    MultiLine,
}

#[derive(Debug, Clone)]
pub enum PendingInput {
    Text(String),
    Content(Vec<ContentBlock>),
}

/// The main interactive TUI application model.
#[derive(bubbletea::Model)]
pub struct PiApp {
    // Input state
    input: TextArea,
    input_history: Vec<String>,
    history_index: Option<usize>,
    input_mode: InputMode,
    pending_inputs: VecDeque<PendingInput>,

    // Display state - viewport for scrollable conversation
    conversation_viewport: Viewport,
    spinner: SpinnerModel,
    agent_state: AgentState,

    // Terminal dimensions
    term_width: usize,
    term_height: usize,

    // Conversation state
    messages: Vec<ConversationMessage>,
    current_response: String,
    current_thinking: String,
    current_tool: Option<String>,
    pending_tool_output: Option<String>,

    // Session and config
    session: Arc<Mutex<Session>>,
    config: Config,
    resources: ResourceLoader,
    model_entry: ModelEntry,
    model_scope: Vec<ModelEntry>,
    available_models: Vec<ModelEntry>,
    model: String,
    agent: Arc<Mutex<Agent>>,
    save_enabled: bool,
    abort_handle: Option<AbortHandle>,

    // Token tracking
    total_usage: Usage,

    // Async channel for agent events
    event_tx: mpsc::UnboundedSender<PiMsg>,

    // Status message (for slash command feedback)
    status_message: Option<String>,
}

/// A message in the conversation history.
#[derive(Debug, Clone)]
pub struct ConversationMessage {
    pub role: MessageRole,
    pub content: String,
    pub thinking: Option<String>,
}

/// Role of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

impl PiApp {
    /// Create a new Pi application.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent: Agent,
        session: Session,
        config: Config,
        resources: ResourceLoader,
        model_entry: ModelEntry,
        model_scope: Vec<ModelEntry>,
        available_models: Vec<ModelEntry>,
        pending_inputs: Vec<PendingInput>,
        event_tx: mpsc::UnboundedSender<PiMsg>,
        save_enabled: bool,
    ) -> Self {
        // Get terminal size
        let (term_width, term_height) =
            terminal::size().map_or((80, 24), |(w, h)| (w as usize, h as usize));

        // Configure text area for input
        let mut input = TextArea::new();
        input.placeholder =
            "Type your message... (Enter to send, Alt+Enter for multi-line, Esc to quit)"
                .to_string();
        input.show_line_numbers = false;
        input.prompt = "> ".to_string();
        input.set_height(3); // Start with 3 lines
        input.set_width(term_width.saturating_sub(4));
        input.max_height = 10; // Allow expansion up to 10 lines
        input.focus();

        let style = Style::new().foreground("212");
        let spinner = SpinnerModel::with_spinner(spinners::dot()).style(style);

        // Configure viewport for conversation history
        // Reserve space for header (2), input (5), footer (2)
        let viewport_height = term_height.saturating_sub(9);
        let mut conversation_viewport =
            Viewport::new(term_width.saturating_sub(2), viewport_height);
        conversation_viewport.mouse_wheel_enabled = true;
        conversation_viewport.mouse_wheel_delta = 3;

        let (messages, total_usage) = load_conversation_from_session(&session);

        let model = format!(
            "{}/{}",
            model_entry.model.provider.as_str(),
            model_entry.model.id.as_str()
        );

        let mut app = Self {
            input,
            input_history: Vec::new(),
            history_index: None,
            input_mode: InputMode::SingleLine,
            pending_inputs: VecDeque::from(pending_inputs),
            conversation_viewport,
            spinner,
            agent_state: AgentState::Idle,
            term_width,
            term_height,
            messages,
            current_response: String::new(),
            current_thinking: String::new(),
            current_tool: None,
            pending_tool_output: None,
            session: Arc::new(Mutex::new(session)),
            config,
            resources,
            model_entry,
            model_scope,
            available_models,
            model,
            agent: Arc::new(Mutex::new(agent)),
            total_usage,
            event_tx,
            status_message: None,
            save_enabled,
            abort_handle: None,
        };

        app.scroll_to_bottom();
        app
    }

    /// Initialize the application.
    fn init(&self) -> Option<Cmd> {
        // Start text input cursor blink and spinner
        let input_cmd = BubbleteaModel::init(&self.input);
        let spinner_cmd = BubbleteaModel::init(&self.spinner);
        let pending_cmd = if self.pending_inputs.is_empty() {
            None
        } else {
            Some(Cmd::new(|| Message::new(PiMsg::RunPending)))
        };

        // Batch commands
        batch(vec![input_cmd, spinner_cmd, pending_cmd])
    }

    /// Handle messages (keyboard input, async events, etc.).
    fn update(&mut self, msg: Message) -> Option<Cmd> {
        // Handle our custom Pi messages
        if let Some(pi_msg) = msg.downcast_ref::<PiMsg>() {
            return self.handle_pi_message(pi_msg.clone());
        }

        // Handle keyboard input
        if let Some(key) = msg.downcast_ref::<KeyMsg>() {
            // Clear status message on any key press
            self.status_message = None;

            match key.key_type {
                // Alt+Enter: Toggle multi-line mode or submit in multi-line mode
                KeyType::Enter if key.alt => {
                    if self.agent_state == AgentState::Idle {
                        if self.input_mode == InputMode::MultiLine {
                            // Submit in multi-line mode
                            let value = self.input.value();
                            if !value.trim().is_empty() {
                                return self.submit_message(value.trim());
                            }
                        } else {
                            // Switch to multi-line mode
                            self.input_mode = InputMode::MultiLine;
                            self.input.set_height(6);
                            self.status_message =
                                Some("Multi-line mode: Alt+Enter to submit".to_string());
                        }
                    }
                    return None;
                }
                // Enter: Submit in single-line mode, newline in multi-line mode
                KeyType::Enter if self.agent_state == AgentState::Idle => {
                    if self.input_mode == InputMode::SingleLine {
                        let value = self.input.value();
                        if !value.trim().is_empty() {
                            return self.submit_message(value.trim());
                        }
                    }
                    // In multi-line mode, let TextArea handle Enter (insert newline)
                }
                KeyType::CtrlC => {
                    if self.agent_state != AgentState::Idle {
                        if let Some(handle) = &self.abort_handle {
                            handle.abort();
                        }
                        self.status_message = Some("Aborting request...".to_string());
                        return None;
                    }
                    return Some(quit());
                }
                KeyType::Esc if self.agent_state == AgentState::Idle => {
                    if self.input_mode == InputMode::MultiLine {
                        // Exit multi-line mode
                        self.input_mode = InputMode::SingleLine;
                        self.input.set_height(3);
                        self.status_message = Some("Single-line mode".to_string());
                        return None;
                    }
                    return Some(quit());
                }
                // History navigation with Ctrl+P/N (works in both modes)
                KeyType::Runes if key.runes == ['p'] && self.agent_state == AgentState::Idle => {
                    // Ctrl+P handled by TextArea as line_previous
                }
                KeyType::Runes if key.runes == ['n'] && self.agent_state == AgentState::Idle => {
                    // Ctrl+N handled by TextArea as line_next
                }
                // Up arrow for history in single-line mode only
                KeyType::Up
                    if self.agent_state == AgentState::Idle
                        && self.input_mode == InputMode::SingleLine =>
                {
                    self.navigate_history_back();
                    return None;
                }
                // Down arrow for history in single-line mode only
                KeyType::Down
                    if self.agent_state == AgentState::Idle
                        && self.input_mode == InputMode::SingleLine =>
                {
                    self.navigate_history_forward();
                    return None;
                }
                // PageUp/PageDown for conversation viewport scrolling
                KeyType::PgUp => {
                    self.conversation_viewport.page_up();
                    return None;
                }
                KeyType::PgDown => {
                    self.conversation_viewport.page_down();
                    return None;
                }
                _ => {}
            }
        }

        // Forward to appropriate component based on state
        if self.agent_state == AgentState::Idle {
            BubbleteaModel::update(&mut self.input, msg)
        } else {
            // While processing, forward to spinner
            self.spinner.update(msg)
        }
    }

    /// Render the view.
    fn view(&self) -> String {
        let mut output = String::new();

        // Header
        output.push_str(&self.render_header());
        output.push('\n');

        // Build conversation content for viewport
        let conversation_content = self.build_conversation_content();

        // Update viewport content (we can't mutate self in view, so we render with current offset)
        // The viewport will be updated in update() when new messages arrive
        let viewport_content = if conversation_content.is_empty() {
            let welcome_style = Style::new().foreground("241").italic();
            welcome_style.render("  Welcome to Pi! Type a message to begin, or /help for commands.")
        } else {
            conversation_content
        };

        // Render conversation area (scrollable)
        let conversation_lines: Vec<&str> = viewport_content.lines().collect();
        let start = self
            .conversation_viewport
            .y_offset()
            .min(conversation_lines.len().saturating_sub(1));
        let end = (start + self.conversation_viewport.height).min(conversation_lines.len());
        let visible_lines = conversation_lines.get(start..end).unwrap_or(&[]);
        output.push_str(&visible_lines.join("\n"));
        output.push('\n');

        // Scroll indicator
        if conversation_lines.len() > self.conversation_viewport.height {
            let total = conversation_lines
                .len()
                .saturating_sub(self.conversation_viewport.height);
            let percent = (start * 100).checked_div(total).map_or(100, |p| p.min(100));
            let scroll_style = Style::new().foreground("241");
            let indicator = format!("  [{percent}%] ↑/↓ PgUp/PgDn to scroll");
            output.push_str(&scroll_style.render(&indicator));
            output.push('\n');
        }

        // Tool status
        if let Some(tool) = &self.current_tool {
            let style = Style::new().foreground("yellow").bold();
            let _ = write!(
                output,
                "\n  {} {} ...\n",
                self.spinner.view(),
                style.render(&format!("Running {tool}"))
            );
        }

        // Status message (slash command feedback)
        if let Some(status) = &self.status_message {
            let status_style = Style::new().foreground("cyan").italic();
            let _ = write!(output, "\n  {}\n", status_style.render(status));
        }

        // Input area (only when idle)
        if self.agent_state == AgentState::Idle {
            output.push_str(&self.render_input());
        } else {
            // Show spinner when processing
            let style = Style::new().foreground("212");
            let _ = write!(
                output,
                "\n  {} {}\n",
                self.spinner.view(),
                style.render("Processing...")
            );
        }

        // Footer with usage stats
        output.push_str(&self.render_footer());

        output
    }

    /// Build the conversation content string for the viewport.
    fn build_conversation_content(&self) -> String {
        let mut output = String::new();

        for msg in &self.messages {
            match msg.role {
                MessageRole::User => {
                    let style = Style::new().bold().foreground("cyan");
                    let _ = write!(output, "\n  {} {}\n", style.render("You:"), msg.content);
                }
                MessageRole::Assistant => {
                    let style = Style::new().bold().foreground("green");
                    let _ = write!(output, "\n  {}\n", style.render("Assistant:"));

                    // Render thinking if present
                    if let Some(thinking) = &msg.thinking {
                        let thinking_style = Style::new().foreground("241").italic();
                        let truncated = truncate(thinking, 100);
                        let _ = writeln!(
                            output,
                            "  {}",
                            thinking_style.render(&format!("Thinking: {truncated}"))
                        );
                    }

                    // Render markdown content
                    let rendered = MarkdownRenderer::new()
                        .with_style(GlamourStyle::Dark)
                        .with_word_wrap(self.term_width.saturating_sub(6).max(40))
                        .render(&msg.content);
                    for line in rendered.lines() {
                        let _ = writeln!(output, "  {line}");
                    }
                }
                MessageRole::System => {
                    let style = Style::new().foreground("yellow");
                    let _ = write!(output, "\n  {}\n", style.render(&msg.content));
                }
            }
        }

        // Add current streaming response
        if !self.current_response.is_empty() || !self.current_thinking.is_empty() {
            let style = Style::new().bold().foreground("green");
            let _ = write!(output, "\n  {}\n", style.render("Assistant:"));

            // Show thinking if present
            if !self.current_thinking.is_empty() {
                let thinking_style = Style::new().foreground("241").italic();
                let truncated = truncate(&self.current_thinking, 100);
                let _ = writeln!(
                    output,
                    "  {}",
                    thinking_style.render(&format!("Thinking: {truncated}"))
                );
            }

            // Show response (no markdown rendering while streaming)
            if !self.current_response.is_empty() {
                for line in self.current_response.lines() {
                    let _ = writeln!(output, "  {line}");
                }
            }
        }

        output
    }

    /// Handle custom Pi messages from the agent.
    fn handle_pi_message(&mut self, msg: PiMsg) -> Option<Cmd> {
        match msg {
            PiMsg::AgentStart => {
                self.agent_state = AgentState::Processing;
                self.current_response.clear();
                self.current_thinking.clear();
            }
            PiMsg::RunPending => {
                return self.run_next_pending();
            }
            PiMsg::TextDelta(text) => {
                self.current_response.push_str(&text);
            }
            PiMsg::ThinkingDelta(text) => {
                self.current_thinking.push_str(&text);
            }
            PiMsg::ToolStart { name, .. } => {
                self.agent_state = AgentState::ToolRunning;
                self.current_tool = Some(name);
                self.pending_tool_output = None;
            }
            PiMsg::ToolUpdate {
                name,
                content,
                details,
                ..
            } => {
                if let Some(output) = format_tool_output(&content, details.as_ref()) {
                    self.pending_tool_output = Some(format!("Tool {name} output:\n{output}"));
                }
            }
            PiMsg::ToolEnd { .. } => {
                self.agent_state = AgentState::Processing;
                self.current_tool = None;
                if let Some(output) = self.pending_tool_output.take() {
                    self.messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: output,
                        thinking: None,
                    });
                    self.scroll_to_bottom();
                }
            }
            PiMsg::AgentDone { usage, stop_reason } => {
                // Finalize the response
                if !self.current_response.is_empty() {
                    self.messages.push(ConversationMessage {
                        role: MessageRole::Assistant,
                        content: std::mem::take(&mut self.current_response),
                        thinking: if self.current_thinking.is_empty() {
                            None
                        } else {
                            Some(std::mem::take(&mut self.current_thinking))
                        },
                    });
                }

                // Update usage
                if let Some(u) = usage {
                    self.total_usage.input += u.input;
                    self.total_usage.output += u.output;
                    self.total_usage.total_tokens += u.total_tokens;
                    self.total_usage.cost.total += u.cost.total;
                }

                self.agent_state = AgentState::Idle;
                self.current_tool = None;
                self.abort_handle = None;

                if stop_reason == StopReason::Aborted {
                    self.status_message = Some("Request aborted".to_string());
                }

                // Re-focus input
                self.input.focus();

                if !self.pending_inputs.is_empty() {
                    return Some(Cmd::new(|| Message::new(PiMsg::RunPending)));
                }
            }
            PiMsg::AgentError(error) => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: format!("Error: {error}"),
                    thinking: None,
                });
                self.agent_state = AgentState::Idle;
                self.current_tool = None;
                self.abort_handle = None;
                self.input.focus();

                if !self.pending_inputs.is_empty() {
                    return Some(Cmd::new(|| Message::new(PiMsg::RunPending)));
                }
            }
        }
        None
    }

    fn run_next_pending(&mut self) -> Option<Cmd> {
        if self.agent_state != AgentState::Idle {
            return None;
        }
        let next = self.pending_inputs.pop_front()?;
        match next {
            PendingInput::Text(text) => self.submit_message(&text),
            PendingInput::Content(content) => self.submit_content(content),
        }
    }

    fn submit_content(&mut self, content: Vec<ContentBlock>) -> Option<Cmd> {
        if content.is_empty() {
            return None;
        }

        let display = content_blocks_to_text(&content);
        if !display.trim().is_empty() {
            self.messages.push(ConversationMessage {
                role: MessageRole::User,
                content: display,
                thinking: None,
            });
        }

        // Clear input and reset to single-line mode
        self.input.reset();
        self.input_mode = InputMode::SingleLine;
        self.input.set_height(3);

        // Start processing
        self.agent_state = AgentState::Processing;

        // Auto-scroll to bottom when new message is added
        self.scroll_to_bottom();

        let content_for_agent = content;
        let event_tx = self.event_tx.clone();
        let agent = Arc::clone(&self.agent);
        let session = Arc::clone(&self.session);
        let save_enabled = self.save_enabled;
        let (abort_handle, abort_signal) = AbortHandle::new();
        self.abort_handle = Some(abort_handle);

        tokio::spawn(async move {
            let mut agent_guard = agent.lock().await;
            let previous_len = agent_guard.messages().len();

            let event_sender = event_tx.clone();
            let result = agent_guard
                .run_with_content_with_abort(content_for_agent, Some(abort_signal), move |event| {
                    let mapped = match event {
                        AgentEvent::RequestStart => Some(PiMsg::AgentStart),
                        AgentEvent::TextDelta { text } => Some(PiMsg::TextDelta(text)),
                        AgentEvent::ThinkingDelta { text } => Some(PiMsg::ThinkingDelta(text)),
                        AgentEvent::ToolExecuteStart { name, id } => {
                            Some(PiMsg::ToolStart { name, tool_id: id })
                        }
                        AgentEvent::ToolExecuteEnd { name, id, is_error } => Some(PiMsg::ToolEnd {
                            name,
                            tool_id: id,
                            is_error,
                        }),
                        AgentEvent::ToolUpdate {
                            name,
                            id,
                            content,
                            details,
                        } => Some(PiMsg::ToolUpdate {
                            name,
                            tool_id: id,
                            content,
                            details,
                        }),
                        AgentEvent::Done { final_message } => Some(PiMsg::AgentDone {
                            usage: Some(final_message.usage),
                            stop_reason: final_message.stop_reason,
                        }),
                        AgentEvent::Error { error } => Some(PiMsg::AgentError(error)),
                        _ => None,
                    };

                    if let Some(msg) = mapped {
                        let _ = event_sender.send(msg);
                    }
                })
                .await;

            let new_messages: Vec<crate::model::Message> =
                agent_guard.messages()[previous_len..].to_vec();
            drop(agent_guard);

            let mut session_guard = session.lock().await;
            for message in new_messages {
                session_guard.append_model_message(message);
            }
            let mut save_error = None;
            let mut index_error = None;

            if save_enabled {
                if let Err(err) = session_guard.save().await {
                    save_error = Some(format!("Failed to save session: {err}"));
                } else {
                    let index = SessionIndex::new();
                    if let Err(err) = index.index_session(&session_guard) {
                        index_error = Some(format!("Failed to index session: {err}"));
                    }
                }
            }
            drop(session_guard);

            if let Some(err) = save_error {
                let _ = event_tx.send(PiMsg::AgentError(err));
            }
            if let Some(err) = index_error {
                let _ = event_tx.send(PiMsg::AgentError(err));
            }

            if let Err(err) = result {
                let _ = event_tx.send(PiMsg::AgentError(err.to_string()));
            }
        });

        None
    }

    /// Submit a message to the agent.
    fn submit_message(&mut self, message: &str) -> Option<Cmd> {
        let message = message.trim();
        if message.is_empty() {
            return None;
        }

        // Check for slash commands
        if let Some((cmd, args)) = SlashCommand::parse(message) {
            return self.handle_slash_command(cmd, args);
        }

        let message_owned = message.to_string();
        let message_for_agent = self.resources.expand_input(&message_owned);
        let event_tx = self.event_tx.clone();
        let agent = Arc::clone(&self.agent);
        let session = Arc::clone(&self.session);
        let save_enabled = self.save_enabled;
        let (abort_handle, abort_signal) = AbortHandle::new();
        self.abort_handle = Some(abort_handle);

        // Add to history
        self.input_history.push(message_owned);
        self.history_index = None;

        // Add user message to display
        self.messages.push(ConversationMessage {
            role: MessageRole::User,
            content: message_for_agent.clone(),
            thinking: None,
        });

        // Clear input and reset to single-line mode
        self.input.reset();
        self.input_mode = InputMode::SingleLine;
        self.input.set_height(3);

        // Start processing
        self.agent_state = AgentState::Processing;

        // Auto-scroll to bottom when new message is added
        self.scroll_to_bottom();

        // Spawn async task to run the agent
        tokio::spawn(async move {
            let mut agent_guard = agent.lock().await;
            let previous_len = agent_guard.messages().len();

            let event_sender = event_tx.clone();
            let result = agent_guard
                .run_with_abort(message_for_agent, Some(abort_signal), move |event| {
                    let mapped = match event {
                        AgentEvent::RequestStart => Some(PiMsg::AgentStart),
                        AgentEvent::TextDelta { text } => Some(PiMsg::TextDelta(text)),
                        AgentEvent::ThinkingDelta { text } => Some(PiMsg::ThinkingDelta(text)),
                        AgentEvent::ToolExecuteStart { name, id } => {
                            Some(PiMsg::ToolStart { name, tool_id: id })
                        }
                        AgentEvent::ToolExecuteEnd { name, id, is_error } => Some(PiMsg::ToolEnd {
                            name,
                            tool_id: id,
                            is_error,
                        }),
                        AgentEvent::ToolUpdate {
                            name,
                            id,
                            content,
                            details,
                        } => Some(PiMsg::ToolUpdate {
                            name,
                            tool_id: id,
                            content,
                            details,
                        }),
                        AgentEvent::Done { final_message } => Some(PiMsg::AgentDone {
                            usage: Some(final_message.usage),
                            stop_reason: final_message.stop_reason,
                        }),
                        AgentEvent::Error { error } => Some(PiMsg::AgentError(error)),
                        _ => None,
                    };

                    if let Some(msg) = mapped {
                        let _ = event_sender.send(msg);
                    }
                })
                .await;

            let new_messages: Vec<crate::model::Message> =
                agent_guard.messages()[previous_len..].to_vec();
            drop(agent_guard);

            let mut session_guard = session.lock().await;
            for message in new_messages {
                session_guard.append_model_message(message);
            }
            let mut save_error = None;
            let mut index_error = None;

            if save_enabled {
                if let Err(err) = session_guard.save().await {
                    save_error = Some(format!("Failed to save session: {err}"));
                } else {
                    let index = SessionIndex::new();
                    if let Err(err) = index.index_session(&session_guard) {
                        index_error = Some(format!("Failed to index session: {err}"));
                    }
                }
            }
            drop(session_guard);

            if let Some(err) = save_error {
                let _ = event_tx.send(PiMsg::AgentError(err));
            }
            if let Some(err) = index_error {
                let _ = event_tx.send(PiMsg::AgentError(err));
            }

            if let Err(err) = result {
                let _ = event_tx.send(PiMsg::AgentError(err.to_string()));
            }
        });

        None
    }

    /// Navigate to previous history entry.
    fn navigate_history_back(&mut self) {
        if self.input_history.is_empty() {
            return;
        }

        let new_index = match self.history_index {
            None => self.input_history.len().saturating_sub(1),
            Some(i) => i.saturating_sub(1),
        };

        if let Some(entry) = self.input_history.get(new_index) {
            self.input.set_value(entry);
            self.history_index = Some(new_index);
        }
    }

    /// Navigate to next history entry.
    fn navigate_history_forward(&mut self) {
        if let Some(index) = self.history_index {
            let next_index = index + 1;
            if let Some(entry) = self.input_history.get(next_index) {
                self.input.set_value(entry);
                self.history_index = Some(next_index);
            } else {
                // Back to empty input
                self.input.reset();
                self.history_index = None;
            }
        }
    }

    /// Handle a slash command.
    #[allow(clippy::too_many_lines)]
    fn handle_slash_command(&mut self, cmd: SlashCommand, args: &str) -> Option<Cmd> {
        // Clear input
        self.input.reset();

        match cmd {
            SlashCommand::Help => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: SlashCommand::help_text().to_string(),
                    thinking: None,
                });
                self.scroll_to_bottom();
            }
            SlashCommand::Clear => {
                self.messages.clear();
                self.current_response.clear();
                self.current_thinking.clear();
                self.status_message = Some("Conversation cleared".to_string());
            }
            SlashCommand::Model => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot change model while processing".to_string());
                } else if args.is_empty() {
                    self.status_message = Some(format!(
                        "Current model: {}/{}",
                        self.model_entry.model.provider, self.model_entry.model.id
                    ));
                } else {
                    let (provider, id) =
                        parse_model_selector(args, &self.model_entry.model.provider);
                    let entry = self
                        .model_scope
                        .iter()
                        .chain(self.available_models.iter())
                        .find(|m| m.model.provider == provider && m.model.id == id)
                        .cloned();

                    let Some(entry) = entry else {
                        self.status_message = Some(format!("Unknown model: {args}"));
                        return None;
                    };

                    let provider_impl = match providers::create_provider(&entry) {
                        Ok(provider_impl) => provider_impl,
                        Err(e) => {
                            self.status_message = Some(format!("Model change failed: {e}"));
                            return None;
                        }
                    };

                    let Some(api_key) = entry.api_key.clone() else {
                        self.status_message =
                            Some(format!("No API key for provider {}", entry.model.provider));
                        return None;
                    };

                    {
                        let mut agent_guard = self.agent.blocking_lock();
                        agent_guard.set_provider(provider_impl);
                        let options = agent_guard.stream_options_mut();
                        options.api_key = Some(api_key);
                        options.headers.clone_from(&entry.headers);
                        drop(agent_guard);
                    }

                    {
                        let mut session_guard = self.session.blocking_lock();
                        session_guard.header.provider = Some(entry.model.provider.clone());
                        session_guard.header.model_id = Some(entry.model.id.clone());
                        session_guard.append_model_change(
                            entry.model.provider.clone(),
                            entry.model.id.clone(),
                        );
                    }

                    self.model_entry = entry;
                    self.model = format!(
                        "{}/{}",
                        self.model_entry.model.provider, self.model_entry.model.id
                    );
                    self.status_message = Some(format!("Model changed to: {}", self.model));
                    self.spawn_save_session();
                }
            }
            SlashCommand::Thinking => {
                if self.agent_state != AgentState::Idle {
                    self.status_message =
                        Some("Cannot change thinking while processing".to_string());
                    return None;
                }

                if args.is_empty() {
                    let current = self
                        .session
                        .blocking_lock()
                        .header
                        .thinking_level
                        .clone()
                        .unwrap_or_else(|| "off".to_string());
                    self.status_message = Some(format!("Current thinking level: {current}"));
                    return None;
                }

                let Some(level) = parse_thinking_level(args) else {
                    self.status_message = Some(
                        "Unknown thinking level. Use: off/minimal/low/medium/high/xhigh"
                            .to_string(),
                    );
                    return None;
                };

                {
                    let mut agent_guard = self.agent.blocking_lock();
                    agent_guard.stream_options_mut().thinking_level = Some(level);
                }

                {
                    let mut session_guard = self.session.blocking_lock();
                    let level_str = thinking_level_to_str(level).to_string();
                    session_guard.header.thinking_level = Some(level_str.clone());
                    session_guard.append_thinking_level_change(level_str);
                }

                self.status_message =
                    Some(format!("Thinking level: {}", thinking_level_to_str(level)));
                self.spawn_save_session();
            }
            SlashCommand::Exit => {
                return Some(quit());
            }
            SlashCommand::History => {
                if self.input_history.is_empty() {
                    self.status_message = Some("No input history".to_string());
                } else {
                    let history_text = self
                        .input_history
                        .iter()
                        .enumerate()
                        .map(|(i, h)| {
                            // Use char count not byte len to avoid panic on multi-byte UTF-8
                            let truncated = if h.chars().count() > 60 {
                                let s: String = h.chars().take(57).collect();
                                format!("{s}...")
                            } else {
                                h.clone()
                            };
                            format!("  {}: {}", i + 1, truncated)
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: format!("Input history:\n{history_text}"),
                        thinking: None,
                    });
                    self.scroll_to_bottom();
                }
            }
            SlashCommand::Export => {
                let path = if args.is_empty() {
                    "conversation.html"
                } else {
                    args
                };
                self.status_message = Some(format!("Export to '{path}' not yet implemented"));
            }
        }

        None
    }

    fn spawn_save_session(&self) {
        if !self.save_enabled {
            return;
        }

        let session = Arc::clone(&self.session);
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            let mut session_guard = session.lock().await;
            if let Err(err) = session_guard.save().await {
                let _ = event_tx.send(PiMsg::AgentError(format!("Failed to save session: {err}")));
                return;
            }

            let index = SessionIndex::new();
            if let Err(err) = index.index_session(&session_guard) {
                let _ = event_tx.send(PiMsg::AgentError(format!("Failed to index session: {err}")));
            }
            drop(session_guard);
        });
    }

    /// Scroll the conversation viewport to the bottom.
    fn scroll_to_bottom(&mut self) {
        // Calculate total lines in conversation
        let content = self.build_conversation_content();
        let line_count = content.lines().count();
        self.conversation_viewport.set_content(&content);
        self.conversation_viewport.goto_bottom();
        let _ = line_count; // Avoid unused warning
    }

    /// Render the header.
    fn render_header(&self) -> String {
        let title_style = Style::new().bold().foreground("212");
        let model_style = Style::new().foreground("241");
        let model = &self.model;
        let model_label = format!("({model})");

        format!(
            "  {} {}\n",
            title_style.render("Pi"),
            model_style.render(&model_label)
        )
    }

    /// Render the conversation messages.
    fn render_messages(&self) -> String {
        let mut output = String::new();

        for msg in &self.messages {
            match msg.role {
                MessageRole::User => {
                    let style = Style::new().bold().foreground("cyan");
                    let _ = write!(output, "\n  {} {}\n", style.render("You:"), msg.content);
                }
                MessageRole::Assistant => {
                    let style = Style::new().bold().foreground("green");
                    let _ = write!(output, "\n  {}\n", style.render("Assistant:"));

                    // Render thinking if present
                    if let Some(thinking) = &msg.thinking {
                        let thinking_style = Style::new().foreground("241").italic();
                        let truncated = truncate(thinking, 100);
                        let _ = writeln!(
                            output,
                            "  {}",
                            thinking_style.render(&format!("Thinking: {truncated}"))
                        );
                    }

                    // Render markdown content
                    let rendered = MarkdownRenderer::new()
                        .with_style(GlamourStyle::Dark)
                        .with_word_wrap(76)
                        .render(&msg.content);
                    for line in rendered.lines() {
                        let _ = writeln!(output, "  {line}");
                    }
                }
                MessageRole::System => {
                    let style = Style::new().foreground("red");
                    let _ = write!(output, "\n  {}\n", style.render(&msg.content));
                }
            }
        }

        output
    }

    /// Render the current streaming response.
    fn render_current_response(&self) -> String {
        let mut output = String::new();

        let style = Style::new().bold().foreground("green");
        let _ = write!(output, "\n  {}\n", style.render("Assistant:"));

        // Show thinking if present
        if !self.current_thinking.is_empty() {
            let thinking_style = Style::new().foreground("241").italic();
            let truncated = truncate(&self.current_thinking, 100);
            let _ = writeln!(
                output,
                "  {}",
                thinking_style.render(&format!("Thinking: {truncated}"))
            );
        }

        // Show response (no markdown rendering while streaming)
        if !self.current_response.is_empty() {
            for line in self.current_response.lines() {
                let _ = writeln!(output, "  {line}");
            }
        }

        output
    }

    /// Render the input area.
    fn render_input(&self) -> String {
        let mut output = String::new();

        // Mode indicator
        let mode_style = Style::new().foreground("241");
        let mode_text = match self.input_mode {
            InputMode::SingleLine => "[single-line] Enter to send",
            InputMode::MultiLine => "[multi-line] Alt+Enter to send, Esc to cancel",
        };
        let _ = writeln!(output, "\n  {}", mode_style.render(mode_text));

        // Input area with textarea view
        output.push_str("  ");
        for line in self.input.view().lines() {
            output.push_str("  ");
            output.push_str(line);
            output.push('\n');
        }

        output
    }

    /// Render the footer with usage stats.
    fn render_footer(&self) -> String {
        let style = Style::new().foreground("241");

        let total_cost = self.total_usage.cost.total;
        let cost_str = if total_cost > 0.0 {
            format!(" (${total_cost:.4})")
        } else {
            String::new()
        };

        let input = self.total_usage.input;
        let output_tokens = self.total_usage.output;
        let mode_hint = match self.input_mode {
            InputMode::SingleLine => "Alt+Enter: multi-line",
            InputMode::MultiLine => "Esc: single-line",
        };
        let footer = format!(
            "Tokens: {input} in / {output_tokens} out{cost_str}  |  {mode_hint}  |  /help  |  Esc: quit"
        );
        format!("\n  {}\n", style.render(&footer))
    }
}

#[allow(clippy::too_many_lines)]
fn load_conversation_from_session(session: &Session) -> (Vec<ConversationMessage>, Usage) {
    let mut messages = Vec::new();
    let mut total_usage = Usage::default();

    for entry in &session.entries {
        match entry {
            SessionEntry::Message(entry) => match &entry.message {
                SessionMessage::User { content, .. } => {
                    let text = user_content_to_text(content);
                    if !text.trim().is_empty() {
                        messages.push(ConversationMessage {
                            role: MessageRole::User,
                            content: text,
                            thinking: None,
                        });
                    }
                }
                SessionMessage::Assistant { message } => {
                    let (text, thinking) = assistant_content_to_text(&message.content);
                    if !text.trim().is_empty() || thinking.is_some() {
                        messages.push(ConversationMessage {
                            role: MessageRole::Assistant,
                            content: text,
                            thinking,
                        });
                    }
                    add_usage(&mut total_usage, &message.usage);
                }
                SessionMessage::ToolResult {
                    tool_name,
                    content,
                    details,
                    is_error,
                    ..
                } => {
                    if let Some(output) = format_tool_output(content, details.as_ref()) {
                        let label = if *is_error {
                            "Tool error"
                        } else {
                            "Tool result"
                        };
                        messages.push(ConversationMessage {
                            role: MessageRole::System,
                            content: format!("{label} ({tool_name}):\n{output}"),
                            thinking: None,
                        });
                    }
                }
                SessionMessage::BashExecution {
                    command,
                    output,
                    exit_code,
                    ..
                } => {
                    let status = if *exit_code == 0 { "ok" } else { "error" };
                    messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: format!("bash ({status}) {command}\n{output}"),
                        thinking: None,
                    });
                }
                SessionMessage::Custom {
                    custom_type,
                    content,
                    display,
                    details,
                    ..
                } => {
                    if *display {
                        let mut combined = content.clone();
                        if let Some(details) = details {
                            let details_text = pretty_json(details);
                            if !details_text.is_empty() {
                                combined.push('\n');
                                combined.push_str(&details_text);
                            }
                        }
                        messages.push(ConversationMessage {
                            role: MessageRole::System,
                            content: format!("{custom_type}: {combined}"),
                            thinking: None,
                        });
                    }
                }
                SessionMessage::BranchSummary { summary, from_id } => {
                    messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: format!("Branch summary from {from_id}: {summary}"),
                        thinking: None,
                    });
                }
                SessionMessage::CompactionSummary {
                    summary,
                    tokens_before,
                } => {
                    messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: format!("Compaction summary ({tokens_before} tokens): {summary}"),
                        thinking: None,
                    });
                }
            },
            SessionEntry::ModelChange(change) => {
                messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: format!("Model set to {}/{}", change.provider, change.model_id),
                    thinking: None,
                });
            }
            SessionEntry::ThinkingLevelChange(change) => {
                messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: format!("Thinking level: {}", change.thinking_level),
                    thinking: None,
                });
            }
            _ => {}
        }
    }

    (messages, total_usage)
}

fn add_usage(total: &mut Usage, usage: &Usage) {
    total.input += usage.input;
    total.output += usage.output;
    total.cache_read += usage.cache_read;
    total.cache_write += usage.cache_write;
    total.total_tokens += usage.total_tokens;
    total.cost.input += usage.cost.input;
    total.cost.output += usage.cost.output;
    total.cost.cache_read += usage.cost.cache_read;
    total.cost.cache_write += usage.cost.cache_write;
    total.cost.total += usage.cost.total;
}

fn user_content_to_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => content_blocks_to_text(blocks),
    }
}

fn assistant_content_to_text(blocks: &[ContentBlock]) -> (String, Option<String>) {
    let mut text = String::new();
    let mut thinking = String::new();

    for block in blocks {
        match block {
            ContentBlock::Text(text_block) => push_line(&mut text, &text_block.text),
            ContentBlock::Thinking(thinking_block) => {
                push_line(&mut thinking, &thinking_block.thinking);
            }
            ContentBlock::Image(image) => {
                push_line(&mut text, &format!("[image: {}]", image.mime_type));
            }
            ContentBlock::ToolCall(call) => {
                push_line(&mut text, &format!("[tool call: {}]", call.name));
            }
        }
    }

    let thinking = if thinking.trim().is_empty() {
        None
    } else {
        Some(thinking)
    };

    (text, thinking)
}

fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    let mut output = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text_block) => push_line(&mut output, &text_block.text),
            ContentBlock::Image(image) => {
                push_line(&mut output, &format!("[image: {}]", image.mime_type));
            }
            ContentBlock::Thinking(thinking_block) => {
                push_line(&mut output, &thinking_block.thinking);
            }
            ContentBlock::ToolCall(call) => {
                push_line(&mut output, &format!("[tool call: {}]", call.name));
            }
        }
    }
    output
}

fn format_tool_output(content: &[ContentBlock], details: Option<&Value>) -> Option<String> {
    let mut output = content_blocks_to_text(content);
    if output.trim().is_empty() {
        if let Some(details) = details {
            output = pretty_json(details);
        }
    }
    if output.trim().is_empty() {
        None
    } else {
        Some(output)
    }
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn parse_model_selector(input: &str, default_provider: &str) -> (String, String) {
    let trimmed = input.trim();
    if let Some((provider, model)) = trimmed.split_once(':') {
        return (provider.trim().to_string(), model.trim().to_string());
    }
    if let Some((provider, model)) = trimmed.split_once('/') {
        return (provider.trim().to_string(), model.trim().to_string());
    }
    (default_provider.to_string(), trimmed.to_string())
}

fn parse_thinking_level(input: &str) -> Option<ThinkingLevel> {
    let normalized = input.trim().to_lowercase();
    match normalized.as_str() {
        "off" | "none" | "0" => Some(ThinkingLevel::Off),
        "minimal" | "min" => Some(ThinkingLevel::Minimal),
        "low" | "1" => Some(ThinkingLevel::Low),
        "medium" | "med" | "2" => Some(ThinkingLevel::Medium),
        "high" | "3" => Some(ThinkingLevel::High),
        "xhigh" | "4" => Some(ThinkingLevel::XHigh),
        _ => None,
    }
}

const fn thinking_level_to_str(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
    }
}

fn push_line(buffer: &mut String, line: &str) {
    if line.is_empty() {
        return;
    }
    if !buffer.is_empty() {
        buffer.push('\n');
    }
    buffer.push_str(line);
}

/// Truncate a string to max_len characters with ellipsis.
fn truncate(s: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }

    let count = s.chars().count();
    if count <= max_len {
        return s.to_string();
    }

    if max_len <= 3 {
        return ".".repeat(max_len);
    }

    let take_len = max_len - 3;
    let mut out = String::with_capacity(max_len);
    out.extend(s.chars().take(take_len));
    out.push_str("...");
    out
}

/// Run the interactive mode.
#[allow(clippy::too_many_arguments)]
pub async fn run_interactive(
    agent: Agent,
    session: Session,
    config: Config,
    model_entry: ModelEntry,
    model_scope: Vec<ModelEntry>,
    available_models: Vec<ModelEntry>,
    pending_inputs: Vec<PendingInput>,
    save_enabled: bool,
    resources: ResourceLoader,
) -> anyhow::Result<()> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<PiMsg>();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<Message>();

    tokio::spawn(async move {
        while let Some(msg) = event_rx.recv().await {
            let _ = ui_tx.send(Message::new(msg));
        }
    });

    let app = PiApp::new(
        agent,
        session,
        config,
        resources,
        model_entry,
        model_scope,
        available_models,
        pending_inputs,
        event_tx,
        save_enabled,
    );

    // Run the TUI program
    Program::new(app)
        .with_alt_screen()
        .with_input_receiver(ui_rx)
        .run()?;

    println!("Goodbye!");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello...");
        assert_eq!(truncate("hi", 5), "hi");
    }

    #[test]
    fn test_message_role() {
        let user = MessageRole::User;
        let assistant = MessageRole::Assistant;
        assert_ne!(user, assistant);
    }
}
