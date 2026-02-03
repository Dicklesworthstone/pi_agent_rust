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
use tokio::sync::{Mutex, mpsc};

use std::fmt::Write as _;
use std::sync::Arc;

use crate::agent::{Agent, AgentEvent};
use crate::config::Config;
use crate::model::Usage;
use crate::session::Session;

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
  /model, /m [name]  - Show or change the current model
  /thinking, /t [level] - Set thinking level (none/low/medium/high)
  /history, /hist    - Show input history
  /export [path]     - Export conversation to HTML
  /exit, /quit, /q   - Exit Pi

Tips:
  • Use ↑/↓ arrows or Ctrl+P/N to navigate input history
  • Use Ctrl+Enter to submit multi-line input
  • Use PageUp/PageDown to scroll conversation history
  • Use Escape to cancel current input"
    }
}

/// Custom message types for async agent events.
#[derive(Debug, Clone)]
pub enum PiMsg {
    /// Agent started processing.
    AgentStart,
    /// Text delta from assistant.
    TextDelta(String),
    /// Thinking delta from assistant.
    ThinkingDelta(String),
    /// Tool execution started.
    ToolStart { name: String, tool_id: String },
    /// Tool execution ended.
    ToolEnd {
        name: String,
        tool_id: String,
        is_error: bool,
    },
    /// Agent finished with final message.
    AgentDone { usage: Option<Usage> },
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
    /// Multi-line input mode (activated with Ctrl+Enter or \).
    MultiLine,
}

/// The main interactive TUI application model.
#[derive(bubbletea::Model)]
pub struct PiApp {
    // Input state
    input: TextArea,
    input_history: Vec<String>,
    history_index: Option<usize>,
    input_mode: InputMode,

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

    // Session and config
    session: Arc<Mutex<Session>>,
    config: Config,
    model: String,
    agent: Arc<Mutex<Agent>>,

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
    pub fn new(
        agent: Agent,
        session: Session,
        config: Config,
        model: String,
        event_tx: mpsc::UnboundedSender<PiMsg>,
    ) -> Self {
        // Get terminal size
        let (term_width, term_height) =
            terminal::size().map_or((80, 24), |(w, h)| (w as usize, h as usize));

        // Configure text area for input
        let mut input = TextArea::new();
        input.placeholder =
            "Type your message... (Enter to send, Ctrl+Enter for multi-line, Esc to quit)"
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

        Self {
            input,
            input_history: Vec::new(),
            history_index: None,
            input_mode: InputMode::SingleLine,
            conversation_viewport,
            spinner,
            agent_state: AgentState::Idle,
            term_width,
            term_height,
            messages: Vec::new(),
            current_response: String::new(),
            current_thinking: String::new(),
            current_tool: None,
            session: Arc::new(Mutex::new(session)),
            config,
            model,
            agent: Arc::new(Mutex::new(agent)),
            total_usage: Usage::default(),
            event_tx,
            status_message: None,
        }
    }

    /// Initialize the application.
    fn init(&self) -> Option<Cmd> {
        // Start text input cursor blink and spinner
        let input_cmd = BubbleteaModel::init(&self.input);
        let spinner_cmd = BubbleteaModel::init(&self.spinner);

        // Batch commands
        batch(vec![input_cmd, spinner_cmd])
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
                // Ctrl+Enter: Toggle multi-line mode or submit in multi-line mode
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
                KeyType::CtrlC => return Some(quit()),
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
            PiMsg::TextDelta(text) => {
                self.current_response.push_str(&text);
            }
            PiMsg::ThinkingDelta(text) => {
                self.current_thinking.push_str(&text);
            }
            PiMsg::ToolStart { name, .. } => {
                self.agent_state = AgentState::ToolRunning;
                self.current_tool = Some(name);
            }
            PiMsg::ToolEnd { .. } => {
                self.agent_state = AgentState::Processing;
                self.current_tool = None;
            }
            PiMsg::AgentDone { usage } => {
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

                // Re-focus input
                self.input.focus();
            }
            PiMsg::AgentError(error) => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: format!("Error: {error}"),
                    thinking: None,
                });
                self.agent_state = AgentState::Idle;
                self.current_tool = None;
                self.input.focus();
            }
        }
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
        let message_for_agent = message_owned.clone();
        let event_tx = self.event_tx.clone();
        let agent = Arc::clone(&self.agent);
        let session = Arc::clone(&self.session);

        // Add to history
        self.input_history.push(message_owned.clone());
        self.history_index = None;

        // Add user message to display
        self.messages.push(ConversationMessage {
            role: MessageRole::User,
            content: message_owned,
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
                .run(message_for_agent, move |event| {
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
                        AgentEvent::Done { final_message } => Some(PiMsg::AgentDone {
                            usage: Some(final_message.usage),
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
            let save_result = session_guard.save().await;
            drop(session_guard);
            if let Err(err) = save_result {
                let _ = event_tx.send(PiMsg::AgentError(format!("Failed to save session: {err}")));
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
                if args.is_empty() {
                    self.status_message = Some(format!("Current model: {}", self.model));
                } else {
                    self.model = args.to_string();
                    self.status_message = Some(format!("Model changed to: {args}"));
                }
            }
            SlashCommand::Thinking => {
                let level_info = if args.is_empty() {
                    "Thinking levels: none, low, medium, high"
                } else {
                    match args.to_lowercase().as_str() {
                        "none" | "off" | "0" => "Thinking disabled",
                        "low" | "1" => "Thinking level: low",
                        "medium" | "med" | "2" => "Thinking level: medium",
                        "high" | "3" => "Thinking level: high",
                        _ => "Unknown thinking level. Use: none, low, medium, high",
                    }
                };
                self.status_message = Some(level_info.to_string());
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
                            let truncated = if h.len() > 60 {
                                format!("{}...", &h[..57])
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
pub async fn run_interactive(
    agent: Agent,
    session: Session,
    config: Config,
    model: String,
) -> anyhow::Result<()> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<PiMsg>();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<Message>();

    tokio::spawn(async move {
        while let Some(msg) = event_rx.recv().await {
            let _ = ui_tx.send(Message::new(msg));
        }
    });

    let app = PiApp::new(agent, session, config, model, event_tx);

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
