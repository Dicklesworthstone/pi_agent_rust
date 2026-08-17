//! FrankenTUI migration stack (bd-cv653.9.1) — feature-gated seed.
//!
//! This module hosts the ftui-runtime port of the interactive front-end. It is
//! compiled only with `--features ftui` (default OFF) so the charmed_rust
//! stack in [`crate::interactive`] stays the shipped TUI while the port
//! proceeds module by module. The bubbletea stack is deleted at cutover — no
//! permanent duality.
//!
//! What is real today:
//! - [`PiFtuiMsg`]: the typed Elm message for the ftui `Model`, wrapping
//!   terminal events and the existing [`PiMsg`](crate::interactive::PiMsg)
//!   agent-event enum (which already models every async event the current TUI
//!   handles — the port reuses it verbatim instead of inventing a parallel
//!   vocabulary).
//! - [`AgentEventSubscription`]: the async→UI bridge as an ftui
//!   `Subscription`, replacing bubbletea's `with_input_receiver`. The agent
//!   side keeps sending `PiMsg` over a std mpsc channel exactly as it does
//!   today; the subscription drains it on a runtime-managed background thread
//!   with clean stop semantics.
//! - [`PiFtuiModel`]: a deliberately minimal `Model` proving the
//!   init/update/view/subscriptions shape end to end, testable headlessly via
//!   `ftui::runtime::simulator::ProgramSimulator`. All agent/tool-originated
//!   text passes through `ftui::render::sanitize` before it can reach a frame
//!   (the content-safety upgrade the migration is required to switch on).
//!
//! What is deliberately NOT here yet: the full view port (conversation
//! rendering, editor, footer, overlays) — that is the mechanical bulk of the
//! migration and lands incrementally against this skeleton.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ftui::core::geometry::Rect;
use ftui::render::sanitize::sanitize;
use ftui::runtime::subscription::{StopSignal, SubId, Subscription};
use ftui::text::Text;
use ftui::widgets::Widget;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::spinner::{DOTS, SpinnerState};
use ftui::widgets::textarea::TextArea;
use ftui::{Cmd, Event, Frame, KeyCode, Model, Modifiers, MouseEventKind};

use crate::ask::{AskAnswer, AskResponse, AskUiRequest, QuestionReply};
use crate::interactive::PiMsg;
use crate::keybindings::{AppAction, KeyBinding, KeyBindings};

/// Typed message for the ftui model: terminal events plus bridged agent events.
///
/// `Model::Message` must be `From<Event>`, so terminal input arrives through
/// [`PiFtuiMsg::Term`]; everything async arrives through [`PiFtuiMsg::Agent`]
/// via [`AgentEventSubscription`].
#[derive(Debug)]
pub enum PiFtuiMsg {
    /// A raw terminal event (key, mouse, resize, paste, focus, ...).
    Term(Event),
    /// An agent/system event bridged from the async side.
    Agent(PiMsg),
}

impl From<Event> for PiFtuiMsg {
    fn from(event: Event) -> Self {
        Self::Term(event)
    }
}

/// Stable subscription id for the agent-event bridge. There is exactly one
/// agent-event stream per interactive session, so a constant id is correct:
/// the runtime deduplicates by id across update cycles and must treat the
/// bridge as the same long-lived source every time.
const AGENT_EVENTS_SUB_ID: SubId = 0x5049_4147; // "PIAG"

/// Bridges the existing async agent-event channel (`std::sync::mpsc` carrying
/// [`PiMsg`]) into the ftui runtime as a `Subscription`.
///
/// The runtime calls [`Subscription::run`] once on a background thread it
/// owns; the receiver is handed over via interior mutability because `run`
/// takes `&self`. The loop wakes every 50ms to observe `StopSignal`, matching
/// the runtime's bounded-join teardown.
///
/// The receiver slot is an `Arc` shared with [`PiFtuiModel`]:
/// `Model::subscriptions()` is called after every update and returns fresh
/// boxes each cycle, but the runtime deduplicates by [`Subscription::id`] and
/// only ever starts one instance — the started instance takes the receiver,
/// and the never-run duplicates see an empty slot.
pub struct AgentEventSubscription {
    rx: Arc<Mutex<Option<Receiver<PiMsg>>>>,
}

impl AgentEventSubscription {
    pub fn new(rx: Receiver<PiMsg>) -> Self {
        Self::from_shared(Arc::new(Mutex::new(Some(rx))))
    }

    const fn from_shared(rx: Arc<Mutex<Option<Receiver<PiMsg>>>>) -> Self {
        Self { rx }
    }
}

const AGENT_EVENT_POLL: Duration = Duration::from_millis(50);

/// Spinner animation cadence while the agent works.
const SPINNER_INTERVAL: Duration = Duration::from_millis(120);

/// Key hint shown in the footer while a picker overlay is open.
const PICKER_HINT: &str = "↑/↓ j/k navigate · Enter apply · Esc close";

/// Drain loop shared by [`Subscription::run`] and unit tests. `stopped` is
/// polled between receives; `StopSignal` has no public constructor, so tests
/// pass a plain closure and terminate via channel disconnect instead.
fn drain_agent_events(
    rx: &Receiver<PiMsg>,
    sender: &Sender<PiFtuiMsg>,
    stopped: impl Fn() -> bool,
) {
    loop {
        if stopped() {
            return;
        }
        match rx.recv_timeout(AGENT_EVENT_POLL) {
            Ok(msg) => {
                if sender.send(PiFtuiMsg::Agent(msg)).is_err() {
                    // Runtime dropped its receiver: program is exiting.
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // Agent side hung up (bridge shutdown). Nothing more to
                // forward; let the runtime reap the thread.
                return;
            }
        }
    }
}

impl Subscription<PiFtuiMsg> for AgentEventSubscription {
    fn id(&self) -> SubId {
        AGENT_EVENTS_SUB_ID
    }

    fn run(&self, sender: Sender<PiFtuiMsg>, stop: StopSignal) {
        let Some(rx) = self.rx.lock().ok().and_then(|mut slot| slot.take()) else {
            // Already consumed (or poisoned): nothing to drain. The runtime
            // only calls run() once per running subscription, so this is a
            // defensive no-op rather than an expected path.
            return;
        };
        drain_agent_events(&rx, &sender, || stop.is_stopped());
    }
}

/// Resolved color palette for the ftui stack.
///
/// Converted from pi's [`Theme`](crate::theme::Theme) hex colors so
/// `pi --ftui` honors the user's configured theme. Colors that fail to parse
/// fall back to the built-in palette per-field.
#[derive(Debug, Clone, Copy)]
pub struct FtuiPalette {
    accent: ftui::PackedRgba,
    muted: ftui::PackedRgba,
    error: ftui::PackedRgba,
    warning: ftui::PackedRgba,
}

impl Default for FtuiPalette {
    fn default() -> Self {
        Self {
            accent: ftui::PackedRgba::rgb(97, 175, 239),
            muted: ftui::PackedRgba::rgb(130, 137, 151),
            error: ftui::PackedRgba::rgb(220, 80, 80),
            warning: ftui::PackedRgba::rgb(229, 192, 123),
        }
    }
}

impl FtuiPalette {
    #[must_use]
    pub fn from_theme(theme: &crate::theme::Theme) -> Self {
        let fallback = Self::default();
        let parse = |hex: &str, fallback: ftui::PackedRgba| {
            crate::theme::parse_hex_color(hex)
                .map_or(fallback, |(r, g, b)| ftui::PackedRgba::rgb(r, g, b))
        };
        Self {
            accent: parse(&theme.colors.accent, fallback.accent),
            muted: parse(&theme.colors.muted, fallback.muted),
            error: parse(&theme.colors.error, fallback.error),
            warning: parse(&theme.colors.warning, fallback.warning),
        }
    }
}

/// Who produced a transcript entry. Drives the prefix and style each role
/// gets in the conversation view (the seed of the real message rendering —
/// markdown/tool cards layer onto this).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryRole {
    User,
    Assistant,
    System,
    Error,
    Ask,
}

impl EntryRole {
    /// Prefix for the entry's first rendered line.
    const fn prefix(self) -> &'static str {
        match self {
            Self::User => "› ",
            Self::System => "· ",
            Self::Error => "✗ ",
            Self::Assistant | Self::Ask => "",
        }
    }

    fn style(self, palette: &FtuiPalette) -> ftui::Style {
        match self {
            Self::User => ftui::Style::new().bold().fg(palette.accent),
            Self::Assistant => ftui::Style::new(),
            Self::System | Self::Ask => ftui::Style::new().dim().fg(palette.muted),
            Self::Error => ftui::Style::new().bold().fg(palette.error),
        }
    }
}

/// One sanitized conversation entry (message, note, card, or error).
#[derive(Debug)]
struct TranscriptEntry {
    role: EntryRole,
    text: String,
}

/// An ask-tool card being answered (bd-cv653.3.8), mirroring the inline flow
/// of the bubbletea stack: the card renders into the transcript and the
/// editor collects the reply (`1`/label to select, comma-separated for multi,
/// free text for Other, `cancel` to dismiss).
struct ActiveAsk {
    request: AskUiRequest,
    question_index: usize,
    answers: Vec<AskAnswer>,
}

/// A completed ask interaction, ready for `AskTool::respond_ui`. The launch
/// path receives these over the reply channel and resolves the pending tool
/// call; tests read the channel directly.
#[derive(Debug)]
pub struct AskUiReply {
    pub request_id: String,
    pub response: AskResponse,
}

/// Modal list picker rendered over the conversation body. All pickers of the
/// bubbletea stack (theme, model, session, branch) share this shape; while
/// open it captures every key (Up/Down/j/k navigate, Enter confirms, Esc
/// closes), matching the modal-capture chain in `update_inner`.
struct PickerOverlay {
    title: String,
    items: Vec<String>,
    selected: usize,
    kind: PickerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    /// Built-in theme picker (`/theme`): applies the palette UI-side.
    Theme,
}

/// Command from the UI to the agent driver.
///
/// The seed of the bubbletea stack's input-routing chain: prompts run agent
/// turns; slash commands that need the session act here (`/model`),
/// everything else is still unported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    /// Run an agent turn with this prompt.
    Prompt(String),
    /// Switch the session's active model (`/model provider/model`).
    SetModel { provider: String, model: String },
}

/// Agent activity as the UI sees it. Drives which surfaces accept input:
/// the editor only receives keys while `Ready` (matching
/// `editor_input_is_available()` in the bubbletea stack).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentUiState {
    Ready,
    Working,
}

impl AgentUiState {
    const fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Working => "working",
        }
    }
}

/// Seed ftui model: proves the Elm loop shape against real pi message types.
///
/// Covers init/update/view/subscriptions end to end but holds only what its
/// tests assert on; the real conversation state migrates here from
/// `interactive::state` as the view port proceeds.
pub struct PiFtuiModel {
    /// What the agent is doing right now (drives header + input routing).
    state: AgentUiState,
    /// Sanitized transcript lines (completed messages / system notes).
    transcript: Vec<TranscriptEntry>,
    /// Sanitized in-flight assistant text (streaming deltas accumulate here).
    streaming: String,
    /// Running tool (name shown in the status region while active).
    current_tool: Option<String>,
    /// Compact todo footer summary (`settled/total · current task`).
    todo_summary: Option<String>,
    /// Sanitized in-flight thinking text (drives the `thinking…` status).
    thinking: String,
    /// Spinner animation state; advanced by `Event::Tick` while working.
    spinner: SpinnerState,
    /// Usage summary from the last completed turn, shown in the footer.
    usage_line: Option<String>,
    /// Theme-derived colors for chrome and role styling.
    palette: FtuiPalette,
    /// Modal picker overlay; captures all keys while open.
    picker: Option<PickerOverlay>,
    /// Keybinding catalog (defaults now; user config once the launch path
    /// wires `KeyBindings::load_from_user_config`). Shared naming with the
    /// bubbletea stack via `KeyBinding::from_ftui_key`.
    keybindings: KeyBindings,
    /// Ask-tool card currently collecting answers via the editor.
    active_ask: Option<ActiveAsk>,
    /// Where completed ask interactions go (launch path calls respond_ui).
    ask_reply_tx: Option<Sender<AskUiReply>>,
    /// Terminal size, tracked from `Event::Resize` (cols, rows).
    term: (u16, u16),
    /// Conversation scroll, measured in lines UP from the tail. 0 means
    /// follow-the-stream (stick to bottom as new content arrives) — the same
    /// semantics as `follow_stream_tail` in the bubbletea stack, but derived
    /// instead of stored so update() never needs the rendered line count.
    scroll_from_tail: usize,
    /// The input editor (ftui-widgets TextArea replaces bubbles TextArea).
    input: TextArea,
    /// Where submitted user input goes. The launch path hands the sending
    /// half of the channel its agent loop consumes; tests read the receiver
    /// directly. `None` falls back to echoing into the transcript only.
    submit_tx: Option<Sender<UiCommand>>,
    /// Shared slot for the agent-event receiver: `subscriptions()` re-declares
    /// the bridge each cycle, and the one instance the runtime actually starts
    /// takes the receiver out of this slot (see [`AgentEventSubscription`]).
    agent_rx: Arc<Mutex<Option<Receiver<PiMsg>>>>,
}

/// Vertical frame regions, top to bottom. The clamp/normalize string hacks of
/// the bubbletea view are gone: the render kernel owns the cell grid, so the
/// layout solver is the only place heights are decided.
struct Regions {
    header: Rect,
    body: Rect,
    status: Rect,
    input: Rect,
    footer: Rect,
}

/// Rows of single-line chrome around the conversation body: header, status,
/// footer. The input region's height is dynamic (see
/// [`PiFtuiModel::input_rows`]), so total chrome = this + input rows.
const FIXED_CHROME_ROWS: u16 = 3;

/// The input editor grows with its content up to this many rows.
const MAX_INPUT_ROWS: u16 = 5;

fn layout_regions(area: Rect, input_rows: u16) -> Regions {
    use ftui::layout::{Constraint, Flex};
    let rects = Flex::vertical()
        .constraints([
            Constraint::Fixed(1),          // header
            Constraint::Fill,              // conversation body
            Constraint::Fixed(1),          // status line (tool/todo/messages)
            Constraint::Fixed(input_rows), // input editor
            Constraint::Fixed(1),          // footer (usage)
        ])
        .split(area);
    Regions {
        header: rects[0],
        body: rects[1],
        status: rects[2],
        input: rects[3],
        footer: rects[4],
    }
}

impl PiFtuiModel {
    pub fn new(agent_rx: Receiver<PiMsg>) -> Self {
        Self {
            state: AgentUiState::Ready,
            transcript: Vec::new(),
            streaming: String::new(),
            current_tool: None,
            todo_summary: None,
            thinking: String::new(),
            spinner: SpinnerState::default(),
            usage_line: None,
            palette: FtuiPalette::default(),
            picker: None,
            keybindings: KeyBindings::default(),
            active_ask: None,
            ask_reply_tx: None,
            term: (80, 24),
            scroll_from_tail: 0,
            input: TextArea::new()
                .with_placeholder("Type a message (Enter to send, Alt+Enter for newline)")
                .with_focus(true)
                .with_soft_wrap(true),
            submit_tx: None,
            agent_rx: Arc::new(Mutex::new(Some(agent_rx))),
        }
    }

    /// Route submitted input to the agent loop via this channel. The launch
    /// path calls this before starting the program.
    #[must_use]
    pub fn with_submit_channel(mut self, tx: Sender<UiCommand>) -> Self {
        self.submit_tx = Some(tx);
        self
    }

    /// Route completed ask-tool interactions to the launch path, which pairs
    /// them back to the pending tool call via `AskTool::respond_ui`.
    #[must_use]
    pub fn with_ask_reply_channel(mut self, tx: Sender<AskUiReply>) -> Self {
        self.ask_reply_tx = Some(tx);
        self
    }

    /// Apply a theme-derived palette (defaults to the built-in colors).
    #[must_use]
    pub const fn with_palette(mut self, palette: FtuiPalette) -> Self {
        self.palette = palette;
        self
    }

    /// Rows the input editor currently needs (content-driven, clamped).
    fn input_rows(&self) -> u16 {
        let lines = if self.input.is_empty() {
            1
        } else {
            self.input.text().lines().count().max(1)
        };
        u16::try_from(lines)
            .unwrap_or(MAX_INPUT_ROWS)
            .min(MAX_INPUT_ROWS)
    }

    /// Visible conversation rows given the tracked terminal size.
    fn body_height(&self) -> usize {
        usize::from(
            self.term
                .1
                .saturating_sub(FIXED_CHROME_ROWS + self.input_rows()),
        )
        .max(1)
    }

    /// Total rendered conversation lines (transcript + in-flight stream).
    fn conversation_line_count(&self) -> usize {
        let transcript: usize = self
            .transcript
            .iter()
            .map(|e| e.text.lines().count().max(1))
            .sum();
        let streaming = if self.streaming.is_empty() {
            0
        } else {
            self.streaming.lines().count().max(1)
        };
        transcript + streaming
    }

    fn push_entry(&mut self, role: EntryRole, text: String) {
        self.transcript.push(TranscriptEntry { role, text });
    }

    /// Cap for `scroll_from_tail`: can't scroll further up than the content.
    fn max_scroll_from_tail(&self) -> usize {
        self.conversation_line_count()
            .saturating_sub(self.body_height())
    }

    fn scroll_up(&mut self, lines: usize) {
        self.scroll_from_tail = self
            .scroll_from_tail
            .saturating_add(lines)
            .min(self.max_scroll_from_tail());
    }

    const fn scroll_down(&mut self, lines: usize) {
        self.scroll_from_tail = self.scroll_from_tail.saturating_sub(lines);
    }

    fn handle_agent(&mut self, msg: PiMsg) -> Cmd<PiFtuiMsg> {
        match msg {
            PiMsg::AgentStart => {
                self.state = AgentUiState::Working;
                // Start the spinner tick chain; it dies naturally once the
                // agent goes idle (Tick reschedules only while Working —
                // same self-limiting pattern as the bubbletea spinner gate).
                return Cmd::tick(SPINNER_INTERVAL);
            }
            PiMsg::TextDelta(delta) => {
                // Adversarial-content safety: agent/tool text is sanitized
                // before it can ever reach a frame.
                self.streaming.push_str(&sanitize(&delta));
            }
            PiMsg::ThinkingDelta(delta) => {
                self.thinking.push_str(&sanitize(&delta));
            }
            PiMsg::ToolStart { name, .. } => {
                self.current_tool = Some(sanitize(&name).into_owned());
            }
            PiMsg::ToolEnd { .. } => {
                self.current_tool = None;
            }
            PiMsg::TodoSummary { summary } => {
                self.todo_summary = summary.map(|s| sanitize(&s).into_owned());
            }
            PiMsg::AgentDone {
                usage,
                error_message,
                ..
            } => {
                if !self.streaming.is_empty() {
                    let text = std::mem::take(&mut self.streaming);
                    self.push_entry(EntryRole::Assistant, text);
                }
                if let Some(err) = error_message {
                    let text = sanitize(&err).into_owned();
                    self.push_entry(EntryRole::Error, text);
                }
                if let Some(usage) = usage {
                    self.usage_line = Some(format!(
                        "tokens {}↑ {}↓ · total {}",
                        usage.input, usage.output, usage.total_tokens
                    ));
                }
                self.state = AgentUiState::Ready;
                self.current_tool = None;
                self.thinking.clear();
            }
            PiMsg::AgentError(err) => {
                let text = sanitize(&err).into_owned();
                self.push_entry(EntryRole::Error, text);
                self.state = AgentUiState::Ready;
                self.current_tool = None;
                self.thinking.clear();
            }
            PiMsg::System(text) | PiMsg::SystemNote(text) => {
                let text = sanitize(&text).into_owned();
                self.push_entry(EntryRole::System, text);
            }
            PiMsg::AskUiRequest(request) => {
                if request.request.questions.is_empty() {
                    // Defensive: an empty card resolves immediately as
                    // dismissed rather than deadlocking the pending tool.
                    self.send_ask_reply(request.id, Vec::new(), true);
                } else {
                    self.push_ask_card(&request, 0);
                    self.active_ask = Some(ActiveAsk {
                        request,
                        question_index: 0,
                        answers: Vec::new(),
                    });
                }
            }
            PiMsg::UiShutdown => return Cmd::quit(),
            // Remaining variants are wired up as their owning surfaces are
            // ported (tools panel, ask cards, OAuth flows, pickers, ...).
            _ => {}
        }
        Cmd::none()
    }

    /// Render one ask question card into the transcript (sanitized — the
    /// question text originates from the model/tool side).
    fn push_ask_card(&mut self, request: &AskUiRequest, index: usize) {
        let total = request.request.questions.len();
        let card =
            crate::ask::format_question_card(&request.request.questions[index], index, total);
        let text = sanitize(card.trim_end()).into_owned();
        self.push_entry(EntryRole::Ask, text);
        self.scroll_from_tail = 0;
    }

    fn send_ask_reply(&self, request_id: String, answers: Vec<AskAnswer>, dismissed: bool) {
        if let Some(tx) = &self.ask_reply_tx {
            let _ = tx.send(AskUiReply {
                request_id,
                response: AskResponse { answers, dismissed },
            });
        }
    }

    /// Consume the editor content as the reply to the active ask question.
    fn submit_ask_answer(&mut self) {
        let Some(mut ask) = self.active_ask.take() else {
            return;
        };
        let raw = self.input.text();
        self.input.set_text("");
        let index = ask.question_index;
        let question = &ask.request.request.questions[index];
        match crate::ask::parse_question_reply(question, &raw) {
            Err(err) => {
                let text = format!("  ! {}", sanitize(&err));
                self.push_entry(EntryRole::Ask, text);
                self.scroll_from_tail = 0;
                self.active_ask = Some(ask); // same question again
            }
            Ok(QuestionReply::Cancel) => {
                self.push_entry(EntryRole::Ask, String::from("  (dismissed)"));
                self.scroll_from_tail = 0;
                self.send_ask_reply(ask.request.id, Vec::new(), true);
            }
            Ok(reply) => {
                let (selected, other) = match reply {
                    QuestionReply::Selected(labels) => (labels, None),
                    QuestionReply::Other(text) => (Vec::new(), Some(text)),
                    QuestionReply::Cancel => unreachable!("handled above"),
                };
                let echo = other.as_ref().map_or_else(
                    || format!("  → {}", selected.join(", ")),
                    |text| format!("  → {text}"),
                );
                let echo = sanitize(&echo).into_owned();
                self.push_entry(EntryRole::Ask, echo);
                let question_id = question.id.clone().unwrap_or_else(|| index.to_string());
                ask.answers.push(AskAnswer {
                    question_id,
                    selected,
                    other,
                });
                let next = index + 1;
                if next < ask.request.request.questions.len() {
                    self.push_ask_card(&ask.request, next);
                    ask.question_index = next;
                    self.active_ask = Some(ask);
                } else {
                    self.scroll_from_tail = 0;
                    self.send_ask_reply(ask.request.id, ask.answers, false);
                }
            }
        }
    }

    /// Submit the editor content: echo into the transcript, hand it to the
    /// agent loop (when wired), clear the editor, resume tail follow.
    fn submit_input(&mut self) {
        let text = self.input.text();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        // User input is the one text source the user typed themself, but it
        // still goes through sanitize: paste can smuggle control sequences.
        let clean = sanitize(trimmed).into_owned();
        self.input.set_text("");
        self.scroll_from_tail = 0;
        self.push_entry(EntryRole::User, clean.clone());

        // Slash-command routing seed (mirrors submit_message's chain; only
        // session-affecting commands the preview can honor are wired).
        if let Some(rest) = clean.strip_prefix("/model") {
            let spec = rest.trim();
            if let Some((provider, model)) = spec.split_once('/')
                && !provider.is_empty()
                && !model.is_empty()
            {
                self.push_entry(EntryRole::System, format!("switching model to {spec} ..."));
                self.send_command(UiCommand::SetModel {
                    provider: provider.to_string(),
                    model: model.to_string(),
                });
            } else {
                self.push_entry(
                    EntryRole::Error,
                    String::from("usage: /model <provider>/<model>"),
                );
            }
            return;
        }
        if clean == "/theme" {
            self.picker = Some(PickerOverlay {
                title: String::from("Theme (Enter to apply, Esc to close)"),
                items: vec![String::from("dark"), String::from("light")],
                selected: 0,
                kind: PickerKind::Theme,
            });
            return;
        }
        if clean == "/help" {
            self.push_entry(
                EntryRole::System,
                String::from(
                    "ftui preview commands: /model <provider>/<model>, /theme, /help — \
                     everything else is still on the charmed stack",
                ),
            );
            return;
        }
        if clean.starts_with('/') && !clean.starts_with("/skill:") {
            let command = clean.split_whitespace().next().unwrap_or(&clean);
            self.push_entry(
                EntryRole::Error,
                format!("Unknown command in ftui preview: {command} (try /help)"),
            );
            return;
        }

        self.send_command(UiCommand::Prompt(clean));
    }

    fn handle_picker_key(&mut self, key: &ftui::KeyEvent) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                picker.selected = picker.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                picker.selected = (picker.selected + 1).min(picker.items.len().saturating_sub(1));
            }
            KeyCode::Escape => {
                self.picker = None;
            }
            KeyCode::Enter => {
                let Some(mut picker) = self.picker.take() else {
                    return;
                };
                let choice = picker.items.swap_remove(picker.selected);
                self.apply_picker_choice(picker.kind, &choice);
            }
            _ => {}
        }
    }

    fn apply_picker_choice(&mut self, kind: PickerKind, choice: &str) {
        match kind {
            PickerKind::Theme => {
                let theme = if choice == "light" {
                    crate::theme::Theme::light()
                } else {
                    crate::theme::Theme::dark()
                };
                self.palette = FtuiPalette::from_theme(&theme);
                self.push_entry(EntryRole::System, format!("theme set to {choice}"));
                self.scroll_from_tail = 0;
            }
        }
    }

    fn send_command(&self, command: UiCommand) {
        if let Some(tx) = &self.submit_tx {
            // A dead agent loop is not a UI error; the transcript echo above
            // still shows what was typed.
            let _ = tx.send(command);
        }
    }

    fn handle_term(&mut self, event: &Event) -> Cmd<PiFtuiMsg> {
        match event {
            Event::Tick => {
                // Spinner heartbeat: advance and reschedule only while the
                // agent is working, so idle sessions stay fully parked.
                if self.state == AgentUiState::Working {
                    self.spinner.tick();
                    return Cmd::tick(SPINNER_INTERVAL);
                }
                return Cmd::none();
            }
            Event::Key(key) => {
                // Hard escape hatch independent of the catalog: the preview
                // stack always quits on ctrl+c. (The bubbletea stack's richer
                // ctrl+c semantics — clear input, double-press to exit,
                // abort-turn — arrive with the launch-path integration.)
                let ctrl_c =
                    key.code == KeyCode::Char('c') && key.modifiers.contains(Modifiers::CTRL);
                if ctrl_c {
                    return Cmd::quit();
                }

                // Modal picker captures all input while open (same precedence
                // as the bubbletea modal-capture chain).
                if self.picker.is_some() {
                    self.handle_picker_key(key);
                    return Cmd::none();
                }

                // Resolve through the shared keybinding catalog so user
                // config behaves identically on both stacks. Chords can bind
                // several context-dependent actions (ctrl+d = delete-forward
                // in a non-empty editor, exit otherwise), so resolve the set
                // against UI state.
                let actions = KeyBinding::from_ftui_key(key)
                    .map(|binding| self.keybindings.matching_actions(&binding))
                    .unwrap_or_default();
                let pick = |wanted: AppAction| actions.contains(&wanted).then_some(wanted);
                let action = pick(AppAction::PageUp)
                    .or_else(|| pick(AppAction::PageDown))
                    .or_else(|| pick(AppAction::Submit))
                    .or_else(|| pick(AppAction::NewLine))
                    .or_else(|| pick(AppAction::Interrupt))
                    .or_else(|| pick(AppAction::CursorLineEnd))
                    .or_else(|| {
                        // Exit only wins when the editor is empty; otherwise
                        // the chord falls through to the editor (delete
                        // forward for the default ctrl+d).
                        if self.input.is_empty() {
                            pick(AppAction::Exit)
                        } else {
                            None
                        }
                    });
                let page = self.body_height().saturating_sub(1).max(1);
                match action {
                    Some(AppAction::PageUp) => return self.consume_scroll(|m| m.scroll_up(page)),
                    Some(AppAction::PageDown) => {
                        return self.consume_scroll(|m| m.scroll_down(page));
                    }
                    Some(AppAction::Exit) if self.input.is_empty() => return Cmd::quit(),
                    Some(AppAction::Interrupt) if self.active_ask.is_some() => {
                        // Escape dismisses the pending ask card.
                        if let Some(ask) = self.active_ask.take() {
                            self.push_entry(EntryRole::Ask, String::from("  (dismissed)"));
                            self.scroll_from_tail = 0;
                            self.send_ask_reply(ask.request.id, Vec::new(), true);
                        }
                        return Cmd::none();
                    }
                    Some(AppAction::Submit) if self.input_active() => {
                        if self.active_ask.is_some() {
                            self.submit_ask_answer();
                        } else {
                            self.submit_input();
                        }
                        return Cmd::none();
                    }
                    Some(AppAction::NewLine) if self.input_active() => {
                        self.input.insert_newline();
                        return Cmd::none();
                    }
                    Some(AppAction::CursorLineEnd) if self.input.is_empty() => {
                        // End with an empty editor resumes tail-follow; with
                        // content it falls through to the editor's line-end.
                        self.scroll_from_tail = 0;
                        return Cmd::none();
                    }
                    _ => {}
                }
                if self.input_active() {
                    // Unrouted keys reach the editor (its own emacs-style
                    // bindings cover cursor/delete/kill-ring behavior).
                    self.input.handle_event(event);
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_up(3),
                MouseEventKind::ScrollDown => self.scroll_down(3),
                _ => {}
            },
            Event::Resize { width, height } => {
                self.term = (*width, *height);
                // Re-clamp: a taller window may make the old offset overshoot.
                self.scroll_from_tail = self.scroll_from_tail.min(self.max_scroll_from_tail());
            }
            _ => {
                if self.input_active() {
                    // Paste and other editor-relevant events flow through.
                    self.input.handle_event(event);
                }
            }
        }
        Cmd::none()
    }

    /// Editor accepts input while the agent is idle (matching
    /// `editor_input_is_available()` in the bubbletea stack) or while an
    /// ask card is collecting its reply mid-turn.
    fn input_active(&self) -> bool {
        self.state == AgentUiState::Ready || self.active_ask.is_some()
    }

    fn consume_scroll(&mut self, scroll: impl FnOnce(&mut Self)) -> Cmd<PiFtuiMsg> {
        scroll(self);
        Cmd::none()
    }

    /// Build the styled conversation. Assistant content renders as markdown
    /// (auto-detected; plain text stays plain); other roles get their prefix
    /// on the first line, matching indent on continuations, and role style.
    ///
    /// Note: markdown rendering can change line counts vs the raw text, so
    /// `conversation_line_count()` is an approximation for scroll clamping in
    /// `update()`; the view recomputes offsets against the rendered total.
    fn conversation_text(&self) -> Text<'static> {
        // Assistant output always renders as markdown, matching the glamour
        // treatment in the bubbletea stack (auto-detection would leave short
        // or mostly-plain replies unstyled).
        let md = ftui_extras::markdown::MarkdownRenderer::new(
            ftui_extras::markdown::MarkdownTheme::default(),
        );
        let palette = self.palette;
        let mut lines: Vec<ftui::text::Line<'static>> =
            Vec::with_capacity(self.conversation_line_count());
        let mut push_block = |role: EntryRole, content: &str| {
            if role == EntryRole::Assistant {
                let rendered = md.render(content);
                lines.extend(rendered.lines().iter().cloned());
                return;
            }
            let style = role.style(&palette);
            let prefix = role.prefix();
            let indent = " ".repeat(prefix.chars().count());
            for (i, line) in content.lines().enumerate() {
                let lead = if i == 0 { prefix } else { indent.as_str() };
                let mut rendered = String::with_capacity(lead.len() + line.len());
                rendered.push_str(lead);
                rendered.push_str(line);
                lines.push(ftui::text::Line::styled(rendered, style));
            }
            if content.is_empty() {
                lines.push(ftui::text::Line::styled(prefix.to_string(), style));
            }
        };
        for entry in &self.transcript {
            push_block(entry.role, &entry.text);
        }
        if !self.streaming.is_empty() {
            // Streaming fragments may end mid-construct; the streaming
            // renderer is tolerant of unterminated markdown.
            let rendered = md.render_streaming(&self.streaming);
            lines.extend(rendered.lines().iter().cloned());
        }
        Text::from_lines(lines)
    }
}

impl Model for PiFtuiModel {
    type Message = PiFtuiMsg;

    fn update(&mut self, msg: PiFtuiMsg) -> Cmd<PiFtuiMsg> {
        match msg {
            PiFtuiMsg::Term(event) => self.handle_term(&event),
            PiFtuiMsg::Agent(agent) => self.handle_agent(agent),
        }
    }

    fn view(&self, frame: &mut Frame) {
        let area = Rect::new(0, 0, frame.width(), frame.height());
        let regions = layout_regions(area, self.input_rows());

        // Header: identity + agent state.
        let header = format!("pi · {}", self.state.label());
        let header_style = ftui::Style::new().bold().fg(self.palette.accent);
        Paragraph::new(Text::from_lines([ftui::text::Line::styled(
            header,
            header_style,
        )]))
        .render(regions.header, frame);

        // Modal picker takes over the conversation body while open. Lines
        // borrow the picker's strings — no per-frame allocation.
        if let Some(picker) = &self.picker {
            let mut lines = vec![ftui::text::Line::styled(
                picker.title.as_str(),
                ftui::Style::new().bold().fg(self.palette.accent),
            )];
            for (i, item) in picker.items.iter().enumerate() {
                let (marker, style) = if i == picker.selected {
                    ("▸ ", ftui::Style::new().bold().fg(self.palette.accent))
                } else {
                    ("  ", ftui::Style::new())
                };
                lines.push(ftui::text::Line::from_spans([
                    ftui::text::Span::styled(marker, style),
                    ftui::text::Span::styled(item.as_str(), style),
                ]));
            }
            Paragraph::new(Text::from_lines(lines)).render(regions.body, frame);
            let footer_style = ftui::Style::new().dim().fg(self.palette.muted);
            Paragraph::new(Text::from_lines([ftui::text::Line::styled(
                PICKER_HINT,
                footer_style,
            )]))
            .render(regions.footer, frame);
            return;
        }

        // Conversation body with tail-follow scroll. `scroll_from_tail == 0`
        // sticks to the bottom; scrolling up pins an offset measured from the
        // tail so streaming appends don't yank the view.
        let body_text = self.conversation_text();
        let total_lines = body_text.lines().len();
        let visible = usize::from(regions.body.height).max(1);
        let from_tail = self
            .scroll_from_tail
            .min(total_lines.saturating_sub(visible));
        let offset = total_lines.saturating_sub(visible + from_tail);
        let offset_u16 = u16::try_from(offset).unwrap_or(u16::MAX);
        Paragraph::new(body_text)
            .scroll((offset_u16, 0))
            .render(regions.body, frame);

        // Status region. While working: spinner + activity (tool > thinking >
        // responding). While idle: the todo summary.
        let status_line = if self.state == AgentUiState::Working {
            let spin = DOTS[self.spinner.current_frame % DOTS.len()];
            let activity = self.current_tool.as_ref().map_or_else(
                || {
                    if self.streaming.is_empty() && !self.thinking.is_empty() {
                        String::from("thinking ...")
                    } else {
                        String::from("responding ...")
                    }
                },
                |tool| format!("running {tool} ..."),
            );
            format!("{spin} {activity}")
        } else {
            self.todo_summary
                .as_ref()
                .map_or_else(String::new, |todo| format!("todo {todo}"))
        };
        if !status_line.is_empty() {
            let status_style = if self.state == AgentUiState::Working {
                ftui::Style::new().fg(self.palette.warning)
            } else {
                ftui::Style::new().dim().fg(self.palette.muted)
            };
            Paragraph::new(Text::from_lines([ftui::text::Line::styled(
                status_line,
                status_style,
            )]))
            .render(regions.status, frame);
        }

        // Input editor while idle or answering an ask card; processing note
        // while the agent works uninterruptibly.
        if self.input_active() {
            self.input.render(regions.input, frame);
        } else {
            Paragraph::new(Text::raw("… processing (ctrl+c to quit)")).render(regions.input, frame);
        }

        // Footer: scroll indicator wins; otherwise last-turn usage stats.
        let footer = if from_tail > 0 {
            format!("[{from_tail} lines up] End to follow")
        } else if let Some(usage) = &self.usage_line {
            usage.clone()
        } else {
            String::from("pi — ftui preview")
        };
        let footer_style = ftui::Style::new().dim().fg(self.palette.muted);
        Paragraph::new(Text::from_lines([ftui::text::Line::styled(
            footer,
            footer_style,
        )]))
        .render(regions.footer, frame);
    }

    fn subscriptions(&self) -> Vec<Box<dyn Subscription<PiFtuiMsg>>> {
        // Re-declared every cycle under the stable AGENT_EVENTS_SUB_ID; the
        // runtime dedups by id, so exactly one instance runs and takes the
        // receiver from the shared slot.
        vec![Box::new(AgentEventSubscription::from_shared(Arc::clone(
            &self.agent_rx,
        )))]
    }
}

// ── Launch path ─────────────────────────────────────────────────────────────

/// Translate one [`AgentEvent`](crate::agent::AgentEvent) into the `PiMsg`
/// vocabulary the model consumes. Pure so tests can pin the mapping.
///
/// Deliberately narrow: lifecycle, streaming deltas, tool lifecycle, and
/// error surfacing. Retry/failover/compaction events surface as system notes;
/// everything else is dropped until its surface is ported.
pub fn agent_event_to_pi_msgs(event: &crate::agent::AgentEvent) -> Vec<PiMsg> {
    use crate::agent::AgentEvent as E;
    use crate::model::AssistantMessageEvent as A;

    match event {
        E::AgentStart { .. } => vec![PiMsg::AgentStart],
        E::AgentEnd {
            messages, error, ..
        } => {
            let last_assistant = messages.iter().rev().find_map(|message| match message {
                crate::model::Message::Assistant(assistant) => Some(assistant),
                _ => None,
            });
            vec![PiMsg::AgentDone {
                usage: last_assistant.map(|a| a.usage.clone()),
                stop_reason: last_assistant
                    .map_or(crate::model::StopReason::Stop, |a| a.stop_reason),
                error_message: error.clone(),
            }]
        }
        E::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event {
            A::TextDelta { delta, .. } => vec![PiMsg::TextDelta(delta.clone())],
            A::ThinkingDelta { delta, .. } => vec![PiMsg::ThinkingDelta(delta.clone())],
            _ => Vec::new(),
        },
        E::ToolExecutionStart {
            tool_call_id,
            tool_name,
            ..
        } => vec![PiMsg::ToolStart {
            name: tool_name.clone(),
            tool_id: tool_call_id.clone(),
        }],
        E::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            is_error,
            ..
        } => vec![PiMsg::ToolEnd {
            name: tool_name.clone(),
            tool_id: tool_call_id.clone(),
            is_error: *is_error,
        }],
        E::AutoRetryStart {
            attempt,
            max_attempts,
            error_message,
            ..
        } => vec![PiMsg::SystemNote(format!(
            "retry {attempt}/{max_attempts}: {error_message}"
        ))],
        E::ExtensionError { event, error, .. } => {
            vec![PiMsg::System(format!("extension error ({event}): {error}"))]
        }
        _ => Vec::new(),
    }
}

/// Poll cadence for picking up submitted prompts in the driver loop.
const SUBMIT_POLL: Duration = Duration::from_millis(50);

/// Run the ftui interactive stack against a real in-process agent session
/// (bd-cv653.9.1 rollout: `pi --ftui`). Blocks until the UI exits.
///
/// Architecture: the UI runs the ftui `Program` on the calling thread; a
/// driver thread owns an asupersync runtime plus the
/// [`AgentSessionHandle`](crate::sdk::AgentSessionHandle) and turns submitted
/// prompts into agent turns, translating [`AgentEvent`](crate::agent::AgentEvent)s
/// back through the [`AgentEventSubscription`] channel. Dropping the UI drops
/// the submit sender, which winds down the driver.
///
/// Not yet at parity with the bubbletea stack (slash commands, bash `!`,
/// pickers, extension UIs, ask respond_ui wiring); tracked on the bead.
/// Inline-mode UI height bounds: enough rows for chrome + a few conversation
/// lines at minimum, capped so the shell above stays visible.
const INLINE_MIN_HEIGHT: u16 = 10;
const INLINE_MAX_HEIGHT: u16 = 24;

pub fn run(
    session_options: crate::sdk::SessionOptions,
    theme: &crate::theme::Theme,
    inline: bool,
) -> std::io::Result<()> {
    let (submit_tx, submit_rx) = std::sync::mpsc::channel::<UiCommand>();
    let (agent_tx, agent_rx) = std::sync::mpsc::channel::<PiMsg>();
    let (ask_reply_tx, ask_reply_rx) = std::sync::mpsc::channel::<AskUiReply>();

    let driver = std::thread::Builder::new()
        .name("pi-ftui-agent-driver".into())
        .spawn(move || {
            let runtime = match asupersync::runtime::RuntimeBuilder::new().build() {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = agent_tx.send(PiMsg::AgentError(format!("runtime build: {err}")));
                    return;
                }
            };
            let runtime_handle = runtime.handle();
            runtime.block_on(async move {
                let mut handle = match crate::sdk::create_agent_session(session_options).await {
                    Ok(handle) => handle,
                    Err(err) => {
                        let _ = agent_tx.send(PiMsg::AgentError(format!("session: {err}")));
                        return;
                    }
                };
                // Ask tool: install the channel picker surface so cards reach
                // the UI as PiMsg::AskUiRequest, and pair replies back through
                // respond_ui (same bridge shape as the RPC host). Both pumps
                // are spawned tasks: asks arrive MID-TURN while the driver
                // loop is blocked inside prompt().await, so pumping replies
                // from the loop itself would deadlock the pending tool call.
                if let Some(ask) = handle.ask_tool() {
                    let (ask_ui_tx, mut ask_ui_rx) =
                        asupersync::channel::mpsc::channel::<AskUiRequest>(4);
                    ask.install_channel_ui(ask_ui_tx);
                    let ask_fwd_tx = agent_tx.clone();
                    runtime_handle.spawn(async move {
                        let cx = crate::agent_cx::AgentCx::for_request();
                        while let Ok(request) = ask_ui_rx.recv(&cx).await {
                            let _ = ask_fwd_tx.send(PiMsg::AskUiRequest(request));
                        }
                    });
                    runtime_handle.spawn(async move {
                        loop {
                            match ask_reply_rx.try_recv() {
                                Ok(reply) => {
                                    let _ = ask.respond_ui(&reply.request_id, reply.response);
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => {
                                    asupersync::time::sleep(
                                        asupersync::time::wall_now(),
                                        SUBMIT_POLL,
                                    )
                                    .await;
                                }
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                            }
                        }
                    });
                }
                let _ = agent_tx.send(PiMsg::System(String::from(
                    "ftui preview stack — experimental (bd-cv653.9.1)",
                )));
                loop {
                    match submit_rx.try_recv() {
                        Ok(UiCommand::Prompt(prompt)) => {
                            // ubs:ignore Sender clone per turn — the event callback must own its sender
                            let tx = agent_tx.clone();
                            let result = handle
                                .prompt(prompt, move |event| {
                                    for msg in agent_event_to_pi_msgs(&event) {
                                        let _ = tx.send(msg);
                                    }
                                })
                                .await;
                            if let Err(err) = result {
                                let _ = agent_tx.send(PiMsg::AgentError(err.to_string()));
                            }
                        }
                        Ok(UiCommand::SetModel { provider, model }) => {
                            let msg = match handle.set_model(&provider, &model).await {
                                Ok(()) => PiMsg::System(format!("model set to {provider}/{model}")),
                                Err(err) => PiMsg::AgentError(format!("model switch: {err}")),
                            };
                            let _ = agent_tx.send(msg);
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            asupersync::time::sleep(asupersync::time::wall_now(), SUBMIT_POLL)
                                .await;
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                    }
                }
            });
        })?;

    let model = PiFtuiModel::new(agent_rx)
        .with_submit_channel(submit_tx)
        .with_ask_reply_channel(ask_reply_tx)
        .with_palette(FtuiPalette::from_theme(theme));
    // Inline mode preserves shell scrollback (bead acceptance #2): the UI
    // anchors at the bottom, auto-sized to content within bounds; alt-screen
    // remains the default.
    let app = if inline {
        ftui::App::inline_auto(model, INLINE_MIN_HEIGHT, INLINE_MAX_HEIGHT)
    } else {
        ftui::App::fullscreen(model)
    };
    let result = app.with_mouse().run();

    // The UI (and with it the submit sender) is gone; the driver's next poll
    // sees Disconnected and unwinds. Join briefly so session teardown (saves)
    // completes before process exit paths run.
    let _ = driver.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StopReason;
    use ftui::runtime::simulator::ProgramSimulator;
    use ftui::{KeyEvent, KeyEventKind};
    use std::sync::mpsc;

    fn key(code: KeyCode, modifiers: Modifiers) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
        })
    }

    fn new_model() -> (mpsc::Sender<PiMsg>, PiFtuiModel) {
        let (tx, rx) = mpsc::channel();
        (tx, PiFtuiModel::new(rx))
    }

    #[test]
    fn streaming_deltas_accumulate_and_flush_on_done() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        assert_eq!(sim.model().state, AgentUiState::Working);
        sim.send(PiFtuiMsg::Agent(PiMsg::TextDelta("hello ".into())));
        sim.send(PiFtuiMsg::Agent(PiMsg::TextDelta("world".into())));
        assert_eq!(sim.model().streaming, "hello world");
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
        }));
        assert_eq!(sim.model().state, AgentUiState::Ready);
        let transcript = &sim.model().transcript;
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript[0].text, "hello world");
        assert_eq!(transcript[0].role, EntryRole::Assistant);
        assert!(sim.model().streaming.is_empty());
    }

    #[test]
    fn agent_text_is_sanitized_before_display() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        // Raw ESC and OSC sequences must not survive into model state: a
        // hostile tool result must not be able to retitle the terminal or
        // fake UI. sanitize() strips C0/C1 controls and escape introducers.
        sim.send(PiFtuiMsg::Agent(PiMsg::TextDelta(
            "safe\x1b]0;pwned\x07 text".into(),
        )));
        let streamed = sim.model().streaming.clone();
        assert!(!streamed.contains('\x1b'), "ESC survived: {streamed:?}");
        assert!(!streamed.contains('\x07'), "BEL survived: {streamed:?}");
        assert!(streamed.contains("safe"));
        assert!(streamed.contains("text"));
    }

    #[test]
    fn ctrl_c_quits() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.inject_event(key(KeyCode::Char('c'), Modifiers::CTRL));
        assert!(!sim.is_running());
    }

    /// Flatten a captured frame to plain text, one row per line.
    fn buffer_text(buf: &ftui::Buffer, width: u16, height: u16) -> String {
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                let ch = buf
                    .get(x, y)
                    .and_then(|cell| cell.content.as_char())
                    .unwrap_or(' ');
                out.push(ch);
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn view_renders_transcript_and_status() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::System("session restored".into())));
        let rendered = buffer_text(sim.capture_frame(40, 8), 40, 8);
        assert!(
            rendered.contains("session restored"),
            "frame missing transcript line: {rendered:?}"
        );
        assert!(rendered.contains("pi · ready"), "frame missing header");
        assert!(
            rendered.contains("Type a message"),
            "frame missing input placeholder: {rendered:?}"
        );
    }

    #[test]
    fn typing_and_enter_submits_to_channel_and_transcript() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        for ch in ['h', 'i'] {
            sim.inject_event(key(KeyCode::Char(ch), Modifiers::empty()));
        }
        assert_eq!(sim.model().input.text(), "hi");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("submitted"),
            UiCommand::Prompt("hi".into())
        );
        assert!(sim.model().input.is_empty(), "editor not cleared");
        let transcript = &sim.model().transcript;
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript[0].text, "hi");
        assert_eq!(transcript[0].role, EntryRole::User);
    }

    #[test]
    fn alt_enter_inserts_newline_and_grows_input_region() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        assert_eq!(sim.model().input_rows(), 1);
        sim.inject_event(key(KeyCode::Char('a'), Modifiers::empty()));
        sim.inject_event(key(KeyCode::Enter, Modifiers::ALT));
        sim.inject_event(key(KeyCode::Char('b'), Modifiers::empty()));
        assert_eq!(sim.model().input.text(), "a\nb");
        assert_eq!(sim.model().input_rows(), 2);
    }

    #[test]
    fn empty_submit_is_a_noop() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(sim.model().transcript.is_empty());
    }

    #[test]
    fn editor_ignores_keys_while_agent_works() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.inject_event(key(KeyCode::Char('x'), Modifiers::empty()));
        assert!(
            sim.model().input.is_empty(),
            "editor took input while working"
        );
        let rendered = buffer_text(sim.capture_frame(40, 8), 40, 8);
        assert!(rendered.contains("processing"), "missing processing note");
    }

    #[test]
    fn submitted_text_is_sanitized() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        // Simulate a hostile paste carrying an OSC title change.
        sim.inject_event(Event::Paste(ftui::PasteEvent::new(
            "hello\x1b]0;pwned\x07world",
            true,
        )));
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        let UiCommand::Prompt(submitted) = submit_rx.try_recv().expect("submitted") else {
            panic!("expected a prompt command");
        };
        assert!(!submitted.contains('\x1b'), "ESC survived: {submitted:?}");
        assert!(submitted.contains("hello"));
        assert!(submitted.contains("world"));
    }

    #[test]
    fn slash_model_routes_set_model_and_bad_specs_error() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/model openai/gpt-5");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::SetModel {
                provider: "openai".into(),
                model: "gpt-5".into(),
            }
        );
        // Bad spec: error entry, nothing sent.
        type_str(&mut sim, "/model nonsense");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(submit_rx.try_recv().is_err(), "bad spec reached the driver");
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.role == EntryRole::Error && e.text.contains("usage: /model")),
            "usage error missing"
        );
    }

    #[test]
    fn unknown_slash_command_errors_locally() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/tree");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(
            submit_rx.try_recv().is_err(),
            "unknown command reached driver"
        );
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.role == EntryRole::Error && e.text.contains("/tree")),
            "unknown-command error missing"
        );
        // /help stays local too.
        type_str(&mut sim, "/help");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(submit_rx.try_recv().is_err());
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.role == EntryRole::System && e.text.contains("/model")),
            "help text missing"
        );
    }

    #[test]
    fn tool_status_renders_while_running() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::ToolStart {
            name: "bash".into(),
            tool_id: "t1".into(),
        }));
        let rendered = buffer_text(sim.capture_frame(40, 8), 40, 8);
        assert!(
            rendered.contains("running bash"),
            "missing tool status: {rendered:?}"
        );
        sim.send(PiFtuiMsg::Agent(PiMsg::ToolEnd {
            name: "bash".into(),
            tool_id: "t1".into(),
            is_error: false,
        }));
        let rendered = buffer_text(sim.capture_frame(40, 8), 40, 8);
        assert!(
            !rendered.contains("running bash"),
            "tool status not cleared"
        );
    }

    #[test]
    fn scroll_pins_view_and_end_resumes_tail_follow() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        // 12x24-line transcript on a 10-row terminal: only the tail visible.
        sim.inject_event(Event::Resize {
            width: 30,
            height: 10,
        });
        for i in 0..20 {
            sim.send(PiFtuiMsg::Agent(PiMsg::System(format!("line-{i}"))));
        }
        // Following the tail: newest line visible, oldest not.
        let rendered = buffer_text(sim.capture_frame(30, 10), 30, 10);
        assert!(
            rendered.contains("line-19"),
            "tail not followed: {rendered:?}"
        );
        assert!(
            !rendered.contains("line-0 "),
            "oldest line unexpectedly visible"
        );

        // Page up: view pins away from the tail.
        sim.inject_event(key(KeyCode::PageUp, Modifiers::empty()));
        let rendered = buffer_text(sim.capture_frame(30, 10), 30, 10);
        assert!(!rendered.contains("line-19"), "still at tail after PageUp");
        assert!(
            rendered.contains("lines up"),
            "footer missing scroll indicator"
        );

        // New content while pinned must not yank the view back to the tail.
        sim.send(PiFtuiMsg::Agent(PiMsg::System("line-20".into())));
        let rendered = buffer_text(sim.capture_frame(30, 10), 30, 10);
        assert!(
            !rendered.contains("line-20"),
            "pinned view was yanked to tail"
        );

        // End: back to following the stream.
        sim.inject_event(key(KeyCode::End, Modifiers::empty()));
        let rendered = buffer_text(sim.capture_frame(30, 10), 30, 10);
        assert!(
            rendered.contains("line-20"),
            "End did not resume tail follow"
        );
    }

    #[test]
    fn resize_reclamps_scroll_offset() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.inject_event(Event::Resize {
            width: 30,
            height: 10,
        });
        for i in 0..12 {
            sim.send(PiFtuiMsg::Agent(PiMsg::System(format!("line-{i}"))));
        }
        sim.inject_event(key(KeyCode::PageUp, Modifiers::empty()));
        assert!(sim.model().scroll_from_tail > 0);
        // Grow the window taller than the content: offset must re-clamp to 0.
        sim.inject_event(Event::Resize {
            width: 30,
            height: 40,
        });
        assert_eq!(sim.model().scroll_from_tail, 0);
    }

    #[test]
    fn drain_loop_bridges_agent_channel_until_disconnect() {
        let (agent_tx, agent_rx) = mpsc::channel::<PiMsg>();
        let (msg_tx, msg_rx) = mpsc::channel::<PiFtuiMsg>();
        let handle = std::thread::spawn(move || {
            // Dropping the agent sender terminates the loop via Disconnected,
            // the same teardown path the bridge shutdown uses today.
            drain_agent_events(&agent_rx, &msg_tx, || false);
        });
        agent_tx.send(PiMsg::AgentStart).unwrap();
        let bridged = msg_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("bridged message");
        assert!(matches!(bridged, PiFtuiMsg::Agent(PiMsg::AgentStart)));
        drop(agent_tx);
        handle.join().expect("bridge thread exits cleanly");
    }

    #[test]
    fn drain_loop_honors_stop_predicate() {
        let (_agent_tx, agent_rx) = mpsc::channel::<PiMsg>();
        let (msg_tx, _msg_rx) = mpsc::channel::<PiFtuiMsg>();
        // stop=true up front: must return immediately without receiving.
        drain_agent_events(&agent_rx, &msg_tx, || true);
    }

    #[test]
    fn spinner_ticks_while_working_and_stops_when_idle() {
        use ftui::runtime::simulator::CmdRecord;
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        // AgentStart schedules the first tick.
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        assert!(
            matches!(sim.command_log().last(), Some(CmdRecord::Tick(_))),
            "AgentStart did not schedule a tick: {:?}",
            sim.command_log().last()
        );
        // Ticks advance the spinner and re-arm while working...
        let frame_before = sim.model().spinner.current_frame;
        sim.inject_event(Event::Tick);
        assert_eq!(sim.model().spinner.current_frame, frame_before + 1);
        assert!(matches!(sim.command_log().last(), Some(CmdRecord::Tick(_))));
        let spin = DOTS[sim.model().spinner.current_frame % DOTS.len()];
        let rendered = buffer_text(sim.capture_frame(40, 8), 40, 8);
        assert!(
            rendered.contains(spin),
            "status missing spinner frame {spin:?}: {rendered:?}"
        );
        // ...but the chain dies once the agent is idle.
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
        }));
        let frame_after_done = sim.model().spinner.current_frame;
        sim.inject_event(Event::Tick);
        assert_eq!(sim.model().spinner.current_frame, frame_after_done);
        assert!(matches!(sim.command_log().last(), Some(CmdRecord::None)));
    }

    #[test]
    fn thinking_status_then_responding_then_usage_footer() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::ThinkingDelta(
            "mull it over".into(),
        )));
        let rendered = buffer_text(sim.capture_frame(44, 8), 44, 8);
        assert!(
            rendered.contains("thinking ..."),
            "missing thinking: {rendered:?}"
        );
        sim.send(PiFtuiMsg::Agent(PiMsg::TextDelta("answer".into())));
        let rendered = buffer_text(sim.capture_frame(44, 8), 44, 8);
        assert!(
            rendered.contains("responding ..."),
            "missing responding: {rendered:?}"
        );
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentDone {
            usage: Some(crate::model::Usage {
                input: 120,
                output: 45,
                total_tokens: 165,
                ..Default::default()
            }),
            stop_reason: StopReason::Stop,
            error_message: None,
        }));
        let rendered = buffer_text(sim.capture_frame(44, 8), 44, 8);
        assert!(
            rendered.contains("tokens 120↑ 45↓ · total 165"),
            "missing usage footer: {rendered:?}"
        );
        assert!(sim.model().thinking.is_empty(), "thinking not cleared");
    }

    fn ask_request(id: &str, questions: Vec<crate::ask::AskQuestion>) -> AskUiRequest {
        AskUiRequest {
            id: id.to_string(),
            request: crate::ask::AskRequest { questions },
        }
    }

    fn question(q: &str, options: &[&str], multi: bool) -> crate::ask::AskQuestion {
        crate::ask::AskQuestion {
            id: None,
            question: q.to_string(),
            header: None,
            options: options
                .iter()
                .map(|label| crate::ask::AskOption {
                    label: (*label).to_string(),
                    description: None,
                })
                .collect(),
            multi,
            recommended: None,
        }
    }

    fn type_str(sim: &mut ProgramSimulator<PiFtuiModel>, s: &str) {
        for ch in s.chars() {
            sim.inject_event(key(KeyCode::Char(ch), Modifiers::empty()));
        }
    }

    #[test]
    fn ask_card_collects_answers_across_questions() {
        let (agent_tx, agent_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel::<AskUiReply>();
        let model = PiFtuiModel::new(agent_rx).with_ask_reply_channel(reply_tx);
        drop(agent_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        // Mid-turn: agent working, ask arrives with two questions.
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::AskUiRequest(ask_request(
            "ask-1",
            vec![
                question("Pick a color?", &["red", "blue"], false),
                question("Pick tools?", &["hammer", "saw"], true),
            ],
        ))));
        let rendered = buffer_text(sim.capture_frame(50, 12), 50, 12);
        assert!(
            rendered.contains("Pick a color?"),
            "card not rendered: {rendered:?}"
        );
        // Editor is active mid-turn for the reply; select by number.
        type_str(&mut sim, "2");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        // Second card renders; multi-select by labels.
        let rendered = buffer_text(sim.capture_frame(50, 14), 50, 14);
        assert!(
            rendered.contains("Pick tools?"),
            "second card missing: {rendered:?}"
        );
        type_str(&mut sim, "hammer, saw");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        let reply = reply_rx.try_recv().expect("ask reply sent");
        assert_eq!(reply.request_id, "ask-1");
        assert!(!reply.response.dismissed);
        assert_eq!(reply.response.answers.len(), 2);
        assert_eq!(reply.response.answers[0].selected, vec!["blue".to_string()]);
        assert_eq!(
            reply.response.answers[1].selected,
            vec!["hammer".to_string(), "saw".to_string()]
        );
        assert!(sim.model().active_ask.is_none(), "ask not cleared");
    }

    #[test]
    fn ask_cancel_dismisses() {
        let (agent_tx, agent_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel::<AskUiReply>();
        let model = PiFtuiModel::new(agent_rx).with_ask_reply_channel(reply_tx);
        drop(agent_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::AskUiRequest(ask_request(
            "ask-2",
            vec![question("Sure?", &["yes", "no"], false)],
        ))));
        type_str(&mut sim, "cancel");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        let reply = reply_rx.try_recv().expect("dismissal sent");
        assert!(reply.response.dismissed);
        assert!(reply.response.answers.is_empty());
        assert!(sim.model().active_ask.is_none());
    }

    #[test]
    fn ask_free_text_becomes_other_answer() {
        let (agent_tx, agent_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel::<AskUiReply>();
        let model = PiFtuiModel::new(agent_rx).with_ask_reply_channel(reply_tx);
        drop(agent_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::AskUiRequest(ask_request(
            "ask-3",
            vec![question("Which env?", &["dev", "prod"], false)],
        ))));
        type_str(&mut sim, "staging with canary");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        let reply = reply_rx.try_recv().expect("reply sent");
        assert_eq!(
            reply.response.answers[0].other.as_deref(),
            Some("staging with canary")
        );
        assert!(reply.response.answers[0].selected.is_empty());
    }

    #[test]
    fn catalog_routes_shift_enter_newline_and_ctrl_d_exit() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        // shift+enter → NewLine action via the catalog.
        sim.inject_event(key(KeyCode::Char('a'), Modifiers::empty()));
        sim.inject_event(key(KeyCode::Enter, Modifiers::SHIFT));
        sim.inject_event(key(KeyCode::Char('b'), Modifiers::empty()));
        assert_eq!(sim.model().input.text(), "a\nb");
        // ctrl+d with content → editor delete-forward (no exit).
        sim.inject_event(key(KeyCode::Char('d'), Modifiers::CTRL));
        assert!(sim.is_running(), "ctrl+d exited despite editor content");
        // Drain the editor, then ctrl+d → Exit.
        sim.model_mut().input.set_text("");
        sim.inject_event(key(KeyCode::Char('d'), Modifiers::CTRL));
        assert!(!sim.is_running(), "ctrl+d on empty editor did not exit");
    }

    #[test]
    fn escape_dismisses_active_ask() {
        let (agent_tx, agent_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel::<AskUiReply>();
        let model = PiFtuiModel::new(agent_rx).with_ask_reply_channel(reply_tx);
        drop(agent_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::AskUiRequest(ask_request(
            "ask-esc",
            vec![question("Continue?", &["yes", "no"], false)],
        ))));
        sim.inject_event(key(KeyCode::Escape, Modifiers::empty()));
        let reply = reply_rx.try_recv().expect("dismissal sent");
        assert!(reply.response.dismissed);
        assert!(sim.model().active_ask.is_none());
    }

    #[test]
    fn agent_event_translation_covers_lifecycle_stream_and_tools() {
        use crate::agent::AgentEvent as E;
        use crate::model::{AssistantMessage, AssistantMessageEvent as A, Message, Usage};
        use std::sync::Arc;

        let msgs = agent_event_to_pi_msgs(&E::AgentStart {
            session_id: Arc::from("s1"),
        });
        assert!(matches!(msgs.as_slice(), [PiMsg::AgentStart]));

        let assistant = Arc::new(AssistantMessage {
            usage: Usage {
                input: 10,
                output: 5,
                total_tokens: 15,
                ..Default::default()
            },
            stop_reason: StopReason::Stop,
            ..Default::default()
        });
        let partial = Arc::clone(&assistant);
        let msgs = agent_event_to_pi_msgs(&E::MessageUpdate {
            message: Message::Assistant(Arc::clone(&assistant)),
            assistant_message_event: A::TextDelta {
                content_index: 0,
                delta: "hi".into(),
                partial,
            },
        });
        assert!(matches!(msgs.as_slice(), [PiMsg::TextDelta(d)] if d == "hi"));

        let msgs = agent_event_to_pi_msgs(&E::ToolExecutionStart {
            tool_call_id: "t1".into(),
            tool_name: "bash".into(),
            args: serde_json::json!({}),
        });
        assert!(
            matches!(msgs.as_slice(), [PiMsg::ToolStart { name, tool_id }] if name == "bash" && tool_id == "t1")
        );

        let msgs = agent_event_to_pi_msgs(&E::AgentEnd {
            session_id: Arc::from("s1"),
            messages: vec![Message::Assistant(assistant)],
            error: None,
        });
        match msgs.as_slice() {
            [
                PiMsg::AgentDone {
                    usage: Some(usage),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                },
            ] => assert_eq!(usage.total_tokens, 15),
            // ubs:ignore panic in #[cfg(test)] match-else is an assertion failure, not library code
            other => panic!("unexpected translation: {other:?}"),
        }
    }

    #[test]
    fn assistant_markdown_renders_without_markers() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::TextDelta(
            "# Release Notes\n\nplain body".into(),
        )));
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
        }));
        let rendered = buffer_text(sim.capture_frame(50, 10), 50, 10);
        assert!(
            rendered.contains("Release Notes"),
            "heading text missing: {rendered:?}"
        );
        assert!(
            !rendered.contains("# Release Notes"),
            "markdown marker leaked into frame: {rendered:?}"
        );
        assert!(
            rendered.contains("plain body"),
            "body missing: {rendered:?}"
        );
    }

    #[test]
    fn theme_picker_opens_navigates_applies_and_captures_keys() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        let dark_accent = sim.model().palette.accent;
        type_str(&mut sim, "/theme");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(sim.model().picker.is_some(), "picker did not open");
        let rendered = buffer_text(sim.capture_frame(50, 10), 50, 10);
        assert!(
            rendered.contains("Theme"),
            "picker title missing: {rendered:?}"
        );
        assert!(rendered.contains("▸ dark"), "selection marker missing");
        // Keys go to the picker, not the editor.
        sim.inject_event(key(KeyCode::Char('j'), Modifiers::empty()));
        assert!(sim.model().input.is_empty(), "picker leaked keys to editor");
        assert_eq!(sim.model().picker.as_ref().unwrap().selected, 1);
        // Enter applies light and closes.
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(sim.model().picker.is_none(), "picker did not close");
        assert_ne!(
            sim.model().palette.accent,
            dark_accent,
            "palette unchanged after applying light theme"
        );
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.text.contains("theme set to light")),
            "confirmation note missing"
        );
    }

    #[test]
    fn theme_picker_escape_closes_without_change() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        let accent_before = sim.model().palette.accent;
        type_str(&mut sim, "/theme");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        sim.inject_event(key(KeyCode::Escape, Modifiers::empty()));
        assert!(sim.model().picker.is_none());
        assert_eq!(sim.model().palette.accent, accent_before);
    }

    #[test]
    fn subscription_id_is_stable() {
        let (_tx, rx) = mpsc::channel::<PiMsg>();
        let sub = AgentEventSubscription::new(rx);
        assert_eq!(sub.id(), AGENT_EVENTS_SUB_ID);
    }
}
