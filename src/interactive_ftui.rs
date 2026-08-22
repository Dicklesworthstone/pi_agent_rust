//! FrankenTUI migration stack (bd-cv653.9.1) — feature-gated preview.
//!
//! This module hosts the ftui-runtime port of the interactive front-end. It is
//! compiled only with `--features ftui` (default OFF) so the charmed_rust
//! stack in [`crate::interactive`] stays the shipped TUI while the port
//! proceeds module by module. The bubbletea stack is deleted at cutover — no
//! permanent duality. Run it with `pi --ftui` (add `--inline` to keep shell
//! scrollback), or try the fake-agent demo: `cargo run --example ftui_preview
//! --features ftui`.
//!
//! What is real today:
//! - [`PiFtuiMsg`]: the typed Elm message wrapping terminal events and the
//!   existing [`PiMsg`](crate::interactive::PiMsg) agent-event vocabulary.
//! - [`AgentEventSubscription`]: the async→UI bridge as an ftui
//!   `Subscription` (stable-id dedup, shared receiver slot, stop-aware
//!   drain), replacing bubbletea's `with_input_receiver`.
//! - [`PiFtuiModel`]: layout regions (header / markdown conversation /
//!   status / growing `TextArea` editor / footer), tail-follow scroll,
//!   spinner ticks, theme-derived [`FtuiPalette`], the shared keybinding
//!   catalog via `KeyBinding::from_ftui_key`, inline ask cards, a modal
//!   picker overlay (`/theme`), and input routing for `/model`, `/help`, and
//!   display-only `!`/`!!` bash. All agent/tool-originated text passes
//!   through `ftui::render::sanitize` before it can reach a frame.
//! - [`run`]: the `pi --ftui` launch path — a driver thread owns an
//!   asupersync runtime plus an SDK session; prompts become real agent turns
//!   ([`agent_event_to_pi_msgs`] pins the translation), asks pair through
//!   `respond_ui`, sessions persist per the usual CLI flags.
//!
//! Still on the bubbletea stack: the interactive tree/fork selector overlays
//! (bd-cv653.9.8) and the command-palette composer (bd-cv653.9.3). Core
//! session slash commands (/new, /clear, /session, /tree summary,
//! /thinking, /name), bash context-inclusion, extension UIs, and the
//! PTY/e2e acceptance lanes are ported here.

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
use crate::extensions::{ExtensionUiRequest, ExtensionUiResponse};
use crate::interactive::PiMsg;
use crate::interactive::{format_extension_ui_prompt, parse_extension_ui_response};
use crate::keybindings::{AppAction, KeyBinding, KeyBindings};
use std::collections::VecDeque;

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

/// Render one tool-card block: state glyph + name on the head line (the
/// glyph is the SHARED spinner frame while pending), then the folded
/// result detail as dim indented lines.
fn push_card_block(
    lines: &mut Vec<ftui::text::Line<'static>>,
    state: CardState,
    text: &str,
    detail: Option<&String>,
    palette: &FtuiPalette,
    spinner_frame: usize,
) {
    let (glyph, style) = match state {
        CardState::Pending => (
            DOTS[spinner_frame % DOTS.len()],
            ftui::Style::new().dim().fg(palette.accent),
        ),
        CardState::Ok => ("✓", ftui::Style::new().fg(palette.accent)),
        CardState::Err => ("✗", ftui::Style::new().bold().fg(palette.error)),
    };
    let mut rendered = String::with_capacity(text.len() + 2);
    rendered.push_str(glyph);
    rendered.push(' ');
    rendered.push_str(text);
    lines.push(ftui::text::Line::styled(rendered, style));
    if let Some(detail) = detail {
        let dim = ftui::Style::new().dim().fg(palette.muted);
        for line in detail.lines() {
            lines.push(ftui::text::Line::styled(format!("  {line}"), dim));
        }
    }
}

/// Render one role block: assistant content as markdown, everything else
/// with the role prefix on the first line and role style throughout.
fn push_role_block(
    lines: &mut Vec<ftui::text::Line<'static>>,
    role: EntryRole,
    content: &str,
    palette: &FtuiPalette,
    md: &ftui_extras::markdown::MarkdownRenderer,
) {
    if role == EntryRole::Assistant {
        let rendered = md.render(content);
        lines.extend(rendered.lines().iter().cloned());
        return;
    }
    let style = role.style(palette);
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
}

/// Live state of a tool-execution card (bd-cv653.9.2): a pending card
/// flips to its terminal state IN PLACE when the tool ends, mirroring
/// omp's state-tinted tool boxes. Bordered widget chrome lands with the
/// widget-grade card framework slice; the seed renders tinted head line +
/// dim folded detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardState {
    Pending,
    Ok,
    Err,
}

/// One sanitized conversation entry (message, note, card, or error).
#[derive(Debug)]
struct TranscriptEntry {
    role: EntryRole,
    text: String,
    /// Set for tool-execution cards; `None` renders as a plain role block.
    card: Option<CardState>,
    /// Folded result preview for tool cards (sanitized, size-capped).
    detail: Option<String>,
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
    /// Selection values when they differ from the display items (e.g. the
    /// session picker shows names but selects paths). Empty → items are the
    /// values.
    values: Vec<String>,
    selected: usize,
    kind: PickerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    /// Built-in theme picker (`/theme`): applies the palette UI-side.
    Theme,
    /// Model picker (`/model` with no arguments): items are
    /// `provider/model-id` strings; selection routes `UiCommand::SetModel`.
    Model,
    /// Session picker (`/resume`): items are display labels, values are
    /// session file paths; selection routes `UiCommand::ResumeSession`.
    Session,
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
    /// Run a shell command. `!cmd` (exclude=false) shows the output AND
    /// submits it to the agent as a turn — the bubbletea semantics; `!!cmd`
    /// (exclude=true) is display-only.
    Bash { command: String, exclude: bool },
    /// Resume a saved session (`/resume` picker): the driver swaps its
    /// session handle and replays the conversation into the transcript.
    ResumeSession { path: String },
    /// Compact the conversation (`/compact`): the driver runs compaction and
    /// replays the rewritten history into the transcript.
    Compact,
    /// Roll back (`/undo`) or re-apply (`/redo`) recorded agent file edits
    /// (bd-cv653.3.13).
    Undo {
        count: usize,
        force: bool,
        redo: bool,
    },
    /// Show provider usage/quota state (`/usage`, bd-cv653.7.4).
    Usage { refresh: bool },
    /// Dispatch a non-built-in slash command to the extension runtime; the
    /// driver checks registration and reports unknown commands.
    ExtensionCommand { name: String, args: String },
    /// Start a fresh session (`/new`): the driver builds a new session from
    /// the launch template with the current provider/model selection and a
    /// reset thinking level, swaps it in, and replays the (empty) history.
    NewSession,
    /// Show session info (`/session`): file, id, name, model, thinking
    /// level, and message count — a read-only snapshot of the live session.
    SessionInfo,
    /// Print a textual branch-tree summary (`/tree`). The interactive tree
    /// selector overlay arrives with bd-cv653.9.8; until then /tree reports
    /// branches/entries instead of falling through to extension dispatch.
    TreeSummary,
    /// Show (`None`) or set (`Some`) the thinking level (`/thinking`).
    /// The UI validates the level against `ThinkingLevel::from_str` before
    /// sending; invalid levels never reach the driver.
    SetThinking(Option<crate::model::ThinkingLevel>),
    /// Set the session display name (`/name <name>`).
    SetName(String),
}

/// Match `input` against a slash command name: returns the argument tail for
/// exactly `name` or `name<space>args`, and `None` for prefixes of longer
/// commands (`/undocumented` must not hit `/undo`).
fn strip_command<'a>(input: &'a str, name: &str) -> Option<&'a str> {
    // Case-insensitive command tokens (SlashCommand::parse parity, a11a0cda);
    // the argument tail keeps its original case.
    if input.len() < name.len() || !input.is_char_boundary(name.len()) {
        return None;
    }
    let (head, rest) = input.split_at(name.len());
    if !head.eq_ignore_ascii_case(name) {
        return None;
    }
    if rest.is_empty() {
        Some("")
    } else if rest.starts_with(' ') {
        Some(rest.trim_start())
    } else {
        None
    }
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
    /// Pinned error banner above the editor (bd-cv653.9.2): set by
    /// AgentError, dismissed on the next sent input.
    error_banner: Option<String>,
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
    /// `provider/model-id` entries for the `/model` picker (from the launch
    /// path's model registry; empty when unset).
    available_models: Vec<String>,
    /// Set by `/exit`//`/quit`; the update loop turns it into `Cmd::quit()`.
    pending_quit: bool,
    /// `(display label, session path)` entries for the `/resume` picker.
    available_sessions: Vec<(String, String)>,
    /// Keybinding catalog (defaults now; user config once the launch path
    /// wires `KeyBindings::load_from_user_config`). Shared naming with the
    /// bubbletea stack via `KeyBinding::from_ftui_key`.
    keybindings: KeyBindings,
    /// Ask-tool card currently collecting answers via the editor.
    active_ask: Option<ActiveAsk>,
    /// Extension UI prompt currently collecting a reply (bd-1eoh4); extras
    /// queue behind it, mirroring the bubbletea active/queue pair.
    active_ext: Option<ExtensionUiRequest>,
    ext_queue: VecDeque<ExtensionUiRequest>,
    /// Where completed extension UI replies go (driver pairs them back to the
    /// pending request via `FtuiExtensionUiHandler::resolve`).
    ext_reply_tx: Option<Sender<ExtensionUiResponse>>,
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
    /// Pinned error banner row (present only while an error is undissmissed).
    banner: Rect,
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

fn layout_regions(area: Rect, input_rows: u16, banner_rows: u16) -> Regions {
    use ftui::layout::{Constraint, Flex};
    let rects = Flex::vertical()
        .constraints([
            Constraint::Fixed(1),          // header
            Constraint::Fill,              // conversation body
            Constraint::Fixed(banner_rows), // pinned error banner (0 = none)
            Constraint::Fixed(1),          // status line (tool/todo/messages)
            Constraint::Fixed(input_rows), // input editor
            Constraint::Fixed(1),          // footer (usage)
        ])
        .split(area);
    Regions {
        header: rects[0],
        body: rects[1],
        banner: rects[2],
        status: rects[3],
        input: rects[4],
        footer: rects[5],
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
            error_banner: None,
            thinking: String::new(),
            spinner: SpinnerState::default(),
            usage_line: None,
            palette: FtuiPalette::default(),
            picker: None,
            available_models: Vec::new(),
            pending_quit: false,
            available_sessions: Vec::new(),
            keybindings: KeyBindings::default(),
            active_ask: None,
            active_ext: None,
            ext_queue: VecDeque::new(),
            ext_reply_tx: None,
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

    /// Route completed extension UI replies to the driver (bd-1eoh4).
    #[must_use]
    pub fn with_ext_reply_channel(mut self, tx: Sender<ExtensionUiResponse>) -> Self {
        self.ext_reply_tx = Some(tx);
        self
    }

    /// Apply a theme-derived palette (defaults to the built-in colors).
    #[must_use]
    pub const fn with_palette(mut self, palette: FtuiPalette) -> Self {
        self.palette = palette;
        self
    }

    /// Provide the `provider/model-id` list backing the `/model` picker.
    #[must_use]
    pub fn with_available_models(mut self, models: Vec<String>) -> Self {
        self.available_models = models;
        self
    }

    /// Provide `(display label, session path)` entries for `/resume`.
    #[must_use]
    pub fn with_available_sessions(mut self, sessions: Vec<(String, String)>) -> Self {
        self.available_sessions = sessions;
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
        let banner = u16::from(self.error_banner.is_some());
        usize::from(
            self.term
                .1
                .saturating_sub(FIXED_CHROME_ROWS + banner + self.input_rows()),
        )
        .max(1)
    }

    /// Total rendered conversation lines (transcript + in-flight stream).
    fn conversation_line_count(&self) -> usize {
        let transcript: usize = self
            .transcript
            .iter()
            .map(|e| {
                e.text.lines().count().max(1) + e.detail.as_ref().map_or(0, |d| d.lines().count())
            })
            .sum();
        let streaming = if self.streaming.is_empty() {
            0
        } else {
            self.streaming.lines().count().max(1)
        };
        transcript + streaming
    }

    fn push_entry(&mut self, role: EntryRole, text: String) {
        self.transcript.push(TranscriptEntry {
            role,
            text,
            card: None,
            detail: None,
        });
    }

    /// Push a pending tool-execution card; the matching [`PiMsg::ToolEnd`]
    /// flips the LAST pending card with the same name in place. `name` is
    /// already sanitized (single-sanitize contract: each event path
    /// sanitizes exactly once so start/end forms always pair).
    fn push_tool_card(&mut self, sanitized_name: &str) {
        self.transcript.push(TranscriptEntry {
            role: EntryRole::System,
            text: sanitized_name.to_string(),
            card: Some(CardState::Pending),
            detail: None,
        });
    }

    /// Close the last pending tool card named `sanitized_name`, falling
    /// back to a plain trace line when no matching open card exists.
    fn finish_tool_card(&mut self, sanitized_name: &str, ok: bool) {
        if let Some(entry) = self
            .transcript
            .iter_mut()
            .rev()
            .find(|e| e.card == Some(CardState::Pending) && e.text == sanitized_name)
        {
            entry.card = Some(if ok { CardState::Ok } else { CardState::Err });
            return;
        }
        let mark = if ok { "✓" } else { "✗" };
        self.push_entry(EntryRole::System, format!("{mark} {sanitized_name}"));
    }

    /// Fold a bash result preview into the still-pending bash card
    /// (driver emits BashResult between ToolStart and ToolEnd). Caps the
    /// preview at 8 lines with an elision counter. Returns false when no
    /// open bash card exists (caller falls back to a plain block).
    fn fold_bash_detail(&mut self, sanitized_display: &str) -> bool {
        const MAX_DETAIL_LINES: usize = 8;
        let Some(entry) = self
            .transcript
            .iter_mut()
            .rev()
            .find(|e| e.card == Some(CardState::Pending) && e.text == "bash")
        else {
            return false;
        };
        let total = sanitized_display.lines().count();
        let mut collected: String = sanitized_display
            .lines()
            .take(MAX_DETAIL_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        if total > MAX_DETAIL_LINES {
            collected.push_str(&format!("\n… +{} more lines", total - MAX_DETAIL_LINES));
        }
        entry.detail = Some(collected);
        true
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
                let name = sanitize(&name).into_owned();
                self.current_tool = Some(name.clone());
                self.push_tool_card(&name);
            }
            PiMsg::ToolEnd { name, is_error, .. } => {
                // The tool card flips to its terminal state in place
                // (bd-cv653.9.2 card framework). Sanitize ONCE here,
                // matching ToolStart, so start/end names always pair.
                let name = sanitize(&name).into_owned();
                self.finish_tool_card(&name, !is_error);
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
                // Pinned above the editor (bd-cv653.9.2), dismiss-on-send —
                // not duplicated into the transcript.
                self.error_banner = Some(sanitize(&err).into_owned());
                self.state = AgentUiState::Ready;
                self.current_tool = None;
                self.thinking.clear();
            }
            PiMsg::System(text) | PiMsg::SystemNote(text) => {
                let text = sanitize(&text).into_owned();
                self.push_entry(EntryRole::System, text);
            }
            PiMsg::ConversationReset {
                messages, status, ..
            } => {
                self.apply_conversation_reset(messages, status);
            }
            PiMsg::BashResult { display, .. } => {
                let text = sanitize(&display).into_owned();
                if !self.fold_bash_detail(&text) {
                    self.push_entry(EntryRole::System, text);
                }
                self.current_tool = None;
                self.scroll_from_tail = 0;
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
            PiMsg::ExtensionUiRequest(request) => {
                if self.active_ext.is_none() && self.active_ask.is_none() {
                    self.activate_ext_request(request);
                } else {
                    self.ext_queue.push_back(request);
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
                self.maybe_activate_queued_ext();
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
                    self.maybe_activate_queued_ext();
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
        // Sending anything dismisses the pinned error banner
        // (bd-cv653.9.2 dismiss-on-send semantics).
        self.error_banner = None;
        // User input is the one text source the user typed themself, but it
        // still goes through sanitize: paste can smuggle control sequences.
        let clean = sanitize(trimmed).into_owned();
        self.input.set_text("");
        self.scroll_from_tail = 0;
        self.push_entry(EntryRole::User, clean.clone());

        // Bash routing comes before slash commands, matching submit_message:
        // `!cmd` shows output and submits it to the agent, `!!cmd` shows only.
        let bang = clean
            .strip_prefix("!!")
            .map(|rest| (rest.trim(), true))
            .or_else(|| clean.strip_prefix('!').map(|rest| (rest.trim(), false)));
        if let Some((command, exclude)) = bang {
            if command.is_empty() {
                self.push_entry(EntryRole::Error, String::from("usage: !<command>"));
            } else {
                self.send_command(UiCommand::Bash {
                    command: command.to_string(),
                    exclude,
                });
            }
            return;
        }

        if clean.starts_with('/') && self.route_slash_command(&clean) {
            return;
        }

        self.send_command(UiCommand::Prompt(clean));
    }

    /// Slash-command routing seed (mirrors submit_message's chain; only
    /// commands the preview can honor are wired). Returns true when the
    /// input was consumed as a command (including local errors).
    fn route_slash_command(&mut self, clean: &str) -> bool {
        // Case-insensitive like SlashCommand::parse in the bubbletea stack.
        // Token-exact: /model and /m route here; /mode or /modelx fall
        // through to the tail (extension dispatch), matching bubbletea.
        let (token, rest) = clean.split_once(char::is_whitespace).unwrap_or((clean, ""));
        if token.eq_ignore_ascii_case("/model") || token.eq_ignore_ascii_case("/m") {
            self.route_model_command(rest.trim());
            return true;
        }
        self.route_slash_command_tail(clean)
    }

    /// `/model` handling: bare opens the picker, `provider/model` switches.
    fn route_model_command(&mut self, spec: &str) {
        {
            if spec.is_empty() {
                // Bare /model opens the picker over the registry list.
                if self.available_models.is_empty() {
                    self.push_entry(
                        EntryRole::Error,
                        String::from("no models available; use /model <provider>/<model>"),
                    );
                } else {
                    self.picker = Some(PickerOverlay {
                        title: String::from("Model (Enter to switch, Esc to close)"),
                        items: self.available_models.clone(),
                        values: Vec::new(),
                        selected: 0,
                        kind: PickerKind::Model,
                    });
                }
            } else if let Some((provider, model)) = spec.split_once('/')
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
        }
    }

    /// `/undo [n] [force]` and `/redo [n] [force]` (bd-cv653.3.13).
    fn route_undo_command(&mut self, args: &str, redo: bool) -> bool {
        let verb = if redo { "redo" } else { "undo" };
        let mut count = 1_usize;
        let mut force = false;
        for token in args.split_whitespace() {
            if token.eq_ignore_ascii_case("force") {
                force = true;
            } else if let Ok(n) = token.parse::<usize>() {
                count = n.max(1);
            } else {
                self.push_entry(EntryRole::Error, format!("usage: /{verb} [n] [force]")); // ubs:ignore loop returns immediately after; cold error path
                return true;
            }
        }
        self.send_command(UiCommand::Undo { count, force, redo });
        true
    }

    /// Remaining slash routing after `/model`.
    fn route_slash_command_tail(&mut self, clean: &str) -> bool {
        // Case-insensitive tokens (SlashCommand::parse parity): compare on
        // an ASCII-lowercased copy; args keep their original case.
        let canon = clean.to_ascii_lowercase();
        if canon == "/exit" || canon == "/quit" || canon == "/q" {
            self.pending_quit = true;
            return true;
        }
        if canon == "/compact" {
            self.push_entry(
                EntryRole::System,
                String::from("compacting conversation ..."),
            );
            self.send_command(UiCommand::Compact);
            return true;
        }
        if let Some(rest) = strip_command(clean, "/undo") {
            return self.route_undo_command(rest, false);
        }
        if let Some(rest) = strip_command(clean, "/redo") {
            return self.route_undo_command(rest, true);
        }
        if let Some(rest) = strip_command(clean, "/usage") {
            let refresh = rest.trim().eq_ignore_ascii_case("refresh");
            self.push_entry(
                EntryRole::System,
                String::from("fetching provider usage ..."),
            );
            self.send_command(UiCommand::Usage { refresh });
            return true;
        }
        if canon == "/theme" {
            self.picker = Some(PickerOverlay {
                title: String::from("Theme (Enter to apply, Esc to close)"),
                items: vec![String::from("dark"), String::from("light")],
                values: Vec::new(),
                selected: 0,
                kind: PickerKind::Theme,
            });
            return true;
        }
        if canon == "/resume" || canon == "/r" {
            if self.available_sessions.is_empty() {
                self.push_entry(EntryRole::Error, String::from("no saved sessions found"));
            } else {
                let (items, values) = self
                    .available_sessions
                    .iter()
                    .map(|(label, path)| (label.clone(), path.clone()))
                    .unzip();
                self.picker = Some(PickerOverlay {
                    title: String::from("Resume session (Enter to load, Esc to close)"),
                    items,
                    values,
                    selected: 0,
                    kind: PickerKind::Session,
                });
            }
            return true;
        }
        if canon == "/help" || canon == "/h" || canon == "/?" {
            self.push_entry(
                EntryRole::System,
                String::from(
                    "ftui preview commands: /model [provider/model], /resume, /compact, \
                     /theme, /new, /clear, /session, /tree, /thinking [level], \
                     /name <name>, /exit, /help, !<cmd> (runs + sends output to the \
                     agent), !!<cmd> (display-only)",
                ),
            );
            return true;
        }
        let (cmd_name, cmd_args) = clean.split_once(char::is_whitespace).unwrap_or((clean, ""));
        match cmd_name.to_ascii_lowercase().as_str() {
            "/new" => {
                self.send_command(UiCommand::NewSession);
                return true;
            }
            "/clear" | "/cls" => {
                // Display-only clear (SlashCommand::Clear parity): the
                // session file and its history stay untouched. Unreachable
                // mid-turn — the editor gate (`input_active`) already blocks
                // input while the agent works.
                self.transcript.clear();
                self.streaming.clear();
                self.thinking.clear();
                self.current_tool = None;
                self.scroll_from_tail = 0;
                self.push_entry(EntryRole::System, String::from("Conversation cleared"));
                return true;
            }
            "/session" | "/info" => {
                self.send_command(UiCommand::SessionInfo);
                return true;
            }
            "/tree" => {
                self.send_command(UiCommand::TreeSummary);
                return true;
            }
            "/thinking" | "/think" | "/t" => {
                let value = cmd_args.trim();
                if value.is_empty() {
                    self.send_command(UiCommand::SetThinking(None));
                    return true;
                }
                match value.parse::<crate::model::ThinkingLevel>() {
                    Ok(level) => self.send_command(UiCommand::SetThinking(Some(level))),
                    Err(err) => self.push_entry(EntryRole::Error, err),
                }
                return true;
            }
            "/name" => {
                let name = cmd_args.trim();
                if name.is_empty() {
                    self.push_entry(EntryRole::Error, String::from("Usage: /name <name>"));
                } else {
                    self.send_command(UiCommand::SetName(name.to_string()));
                }
                return true;
            }
            _ => {}
        }
        if !clean.starts_with("/skill:") {
            // Anything else may be an extension-registered command; the
            // driver checks registration and reports unknown ones.
            let body = clean.trim_start_matches('/');
            let (name, args) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
            if name.is_empty() {
                self.push_entry(EntryRole::Error, String::from("Unknown command: /"));
            } else {
                self.send_command(UiCommand::ExtensionCommand {
                    name: name.to_string(),
                    args: args.trim().to_string(),
                });
            }
            return true;
        }
        // /skill: inputs flow through to the agent as prompts.
        false
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
                let choice = if picker.values.is_empty() {
                    picker.items.swap_remove(picker.selected)
                } else {
                    picker.values.swap_remove(picker.selected)
                };
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
            PickerKind::Model => {
                if let Some((provider, model)) = choice.split_once('/') {
                    self.push_entry(
                        EntryRole::System,
                        format!("switching model to {choice} ..."),
                    );
                    self.scroll_from_tail = 0;
                    self.send_command(UiCommand::SetModel {
                        provider: provider.to_string(),
                        model: model.to_string(),
                    });
                } else {
                    self.push_entry(EntryRole::Error, format!("malformed model entry: {choice}"));
                }
            }
            PickerKind::Session => {
                self.push_entry(EntryRole::System, String::from("resuming session ..."));
                self.scroll_from_tail = 0;
                self.send_command(UiCommand::ResumeSession {
                    path: choice.to_string(),
                });
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
                            self.maybe_activate_queued_ext();
                        }
                        return Cmd::none();
                    }
                    Some(AppAction::Interrupt) if self.active_ext.is_some() => {
                        // Escape cancels the pending extension prompt.
                        self.cancel_active_ext();
                        return Cmd::none();
                    }
                    Some(AppAction::Submit) if self.input_active() => {
                        if self.active_ask.is_some() {
                            self.submit_ask_answer();
                        } else if self.active_ext.is_some() {
                            self.submit_ext_answer();
                        } else {
                            self.submit_input();
                            if self.pending_quit {
                                return Cmd::quit();
                            }
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
    /// ask card / extension UI prompt is collecting its reply mid-turn.
    fn input_active(&self) -> bool {
        self.state == AgentUiState::Ready || self.active_ask.is_some() || self.active_ext.is_some()
    }

    /// Rebuild the transcript from a resumed/forked/compacted session.
    fn apply_conversation_reset(
        &mut self,
        messages: Vec<crate::interactive::ConversationMessage>,
        status: Option<String>,
    ) {
        self.transcript.clear();
        self.streaming.clear();
        for message in messages {
            let role = match message.role {
                crate::interactive::MessageRole::User => EntryRole::User,
                crate::interactive::MessageRole::Assistant => EntryRole::Assistant,
                crate::interactive::MessageRole::Tool | crate::interactive::MessageRole::System => {
                    EntryRole::System
                }
            };
            let text = sanitize(&message.content).into_owned();
            self.push_entry(role, text);
        }
        if let Some(status) = status {
            let text = sanitize(&status).into_owned();
            self.push_entry(EntryRole::System, text);
        }
        self.scroll_from_tail = 0;
    }

    /// Render an extension UI prompt into the transcript and make it the
    /// active reply target.
    fn activate_ext_request(&mut self, request: ExtensionUiRequest) {
        let card = format_extension_ui_prompt(&request);
        let text = sanitize(card.trim_end()).into_owned();
        self.push_entry(EntryRole::Ask, text);
        self.scroll_from_tail = 0;
        self.active_ext = Some(request);
    }

    fn send_ext_reply(&self, response: ExtensionUiResponse) {
        if let Some(tx) = &self.ext_reply_tx {
            let _ = tx.send(response);
        }
    }

    /// Consume the editor content as the reply to the active extension UI
    /// prompt; parse errors re-prompt, `cancel` dismisses.
    fn submit_ext_answer(&mut self) {
        let Some(request) = self.active_ext.take() else {
            return;
        };
        let raw = self.input.text();
        self.input.set_text("");
        match parse_extension_ui_response(&request, &raw) {
            Err(err) => {
                let text = format!("  ! {}", sanitize(&err));
                self.push_entry(EntryRole::Ask, text);
                self.scroll_from_tail = 0;
                self.active_ext = Some(request);
            }
            Ok(response) => {
                let echo = if response.cancelled {
                    String::from("  (cancelled)")
                } else {
                    format!("  → {}", sanitize(raw.trim()))
                };
                self.push_entry(EntryRole::Ask, echo);
                self.scroll_from_tail = 0;
                self.send_ext_reply(response);
                if let Some(next) = self.ext_queue.pop_front() {
                    self.activate_ext_request(next);
                }
            }
        }
    }

    /// Activate a queued extension prompt once no ask card or prompt is
    /// holding the input line.
    fn maybe_activate_queued_ext(&mut self) {
        if self.active_ask.is_none()
            && self.active_ext.is_none()
            && let Some(next) = self.ext_queue.pop_front()
        {
            self.activate_ext_request(next);
        }
    }

    /// Cancel the active extension prompt (escape path).
    fn cancel_active_ext(&mut self) {
        if let Some(request) = self.active_ext.take() {
            self.push_entry(EntryRole::Ask, String::from("  (cancelled)"));
            self.scroll_from_tail = 0;
            self.send_ext_reply(ExtensionUiResponse {
                id: request.id,
                value: None,
                cancelled: true,
            });
            if let Some(next) = self.ext_queue.pop_front() {
                self.activate_ext_request(next);
            }
        }
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
        for entry in &self.transcript {
            if let Some(state) = entry.card {
                push_card_block(
                    &mut lines,
                    state,
                    &entry.text,
                    entry.detail.as_ref(),
                    &palette,
                    self.spinner.current_frame,
                );
                continue;
            }
            push_role_block(&mut lines, entry.role, &entry.text, &palette, &md);
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
        E::AutoCompactionStart { reason } => {
            vec![PiMsg::SystemNote(format!("compacting context: {reason}"))]
        }
        E::AutoCompactionEnd {
            aborted,
            error_message,
            ..
        } => {
            let note = if *aborted {
                String::from("compaction aborted")
            } else if let Some(err) = error_message {
                format!("compaction failed: {err}")
            } else {
                String::from("compaction complete")
            };
            vec![PiMsg::SystemNote(note)]
        }
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
/// lines at minimum, capped so the shell above stays visible. The cap must
/// stay well under common terminal heights (24 rows): an inline UI as tall
/// as the screen erases the very scrollback the mode exists to preserve
/// (proven by the e2e_ftui scrollback capture lane).
const INLINE_MIN_HEIGHT: u16 = 10;
const INLINE_MAX_HEIGHT: u16 = 15;

/// Default budget for an extension UI prompt when the request carries none.
const EXT_UI_TIMEOUT_MS: u64 = 300_000;

/// Driver-side extension UI surface (bd-1eoh4): forwards requests to the UI
/// as `PiMsg::ExtensionUiRequest` and awaits the typed reply routed back over
/// the extension reply channel — the same oneshot-pending shape as
/// `AskTool::install_channel_ui`.
struct FtuiExtensionUiHandler {
    agent_tx: Sender<PiMsg>,
    pending: Mutex<
        std::collections::HashMap<
            String,
            asupersync::channel::oneshot::Sender<ExtensionUiResponse>,
        >,
    >,
}

impl FtuiExtensionUiHandler {
    fn new(agent_tx: Sender<PiMsg>) -> Self {
        Self {
            agent_tx,
            pending: Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn resolve(&self, response: ExtensionUiResponse) {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let sender = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&response.id);
        if let Some(sender) = sender {
            let _ = sender.send(cx.cx(), response);
        }
    }

    fn drop_pending(&self, id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }
}

#[async_trait::async_trait]
impl crate::sdk::ExtensionUiHandler for FtuiExtensionUiHandler {
    async fn request_ui(
        &self,
        request: ExtensionUiRequest,
    ) -> crate::error::Result<Option<ExtensionUiResponse>> {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let id = request.id.clone();
        let timeout_ms = request.timeout_ms.unwrap_or(EXT_UI_TIMEOUT_MS);
        let (reply_tx, mut reply_rx) = asupersync::channel::oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), reply_tx);
        if self
            .agent_tx
            .send(PiMsg::ExtensionUiRequest(request))
            .is_err()
        {
            self.drop_pending(&id);
            return Ok(None);
        }
        let waited = asupersync::time::timeout(
            asupersync::time::wall_now(),
            std::time::Duration::from_millis(timeout_ms),
            reply_rx.recv(cx.cx()),
        )
        .await;
        if let Ok(Ok(response)) = waited {
            Ok(Some(response))
        } else {
            // UI gone or user never answered: report a cancel so the
            // extension gets a definitive answer instead of hanging.
            self.drop_pending(&id);
            Ok(Some(ExtensionUiResponse {
                id,
                value: None,
                cancelled: true,
            }))
        }
    }
}

/// Long-lived pump pairing UI extension replies back to their pending
/// requests (same spawned-task rationale as the ask reply pump).
fn spawn_ext_reply_pump(
    handler: Arc<FtuiExtensionUiHandler>,
    ext_reply_rx: Receiver<ExtensionUiResponse>,
    runtime_handle: &asupersync::runtime::RuntimeHandle,
) {
    runtime_handle.spawn(async move {
        loop {
            match ext_reply_rx.try_recv() {
                Ok(response) => handler.resolve(response),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    asupersync::time::sleep(asupersync::time::wall_now(), SUBMIT_POLL).await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
    });
}

/// Install the ask bridge pair for a fresh driver: per-handle forwarder plus
/// the long-lived reply pump against the CURRENT tool (same shape as the RPC
/// host), so `/resume` handle swaps keep replies pairable.
fn install_ask_bridges(
    handle: &crate::sdk::AgentSessionHandle,
    agent_tx: &Sender<PiMsg>,
    ask_reply_rx: Receiver<AskUiReply>,
    runtime_handle: &asupersync::runtime::RuntimeHandle,
) -> CurrentAsk {
    let current_ask: CurrentAsk = Arc::new(Mutex::new(handle.ask_tool()));
    if let Some(ask) = handle.ask_tool() {
        install_ask_forwarder(&ask, agent_tx, runtime_handle);
    }
    spawn_ask_reply_pump(Arc::clone(&current_ask), ask_reply_rx, runtime_handle);
    current_ask
}

/// Shared slot for the CURRENT ask tool: `/resume` swaps the session handle
/// (and with it the ask tool), so the long-lived reply pump resolves against
/// whatever tool is current when the reply arrives.
type CurrentAsk = Arc<Mutex<Option<crate::ask::AskTool>>>;

/// Install the per-handle half of the ask bridge: a channel picker surface on
/// the tool plus a forwarder task that turns cards into `PiMsg::AskUiRequest`.
/// The forwarder dies naturally when the handle (and its ask tool clones)
/// drop. Spawned, not inline: asks arrive MID-TURN while the driver loop is
/// blocked inside `prompt().await`.
fn install_ask_forwarder(
    ask: &crate::ask::AskTool,
    agent_tx: &Sender<PiMsg>,
    runtime_handle: &asupersync::runtime::RuntimeHandle,
) {
    let (ask_ui_tx, mut ask_ui_rx) = asupersync::channel::mpsc::channel::<AskUiRequest>(4);
    ask.install_channel_ui(ask_ui_tx);
    let ask_fwd_tx = agent_tx.clone();
    runtime_handle.spawn(async move {
        let cx = crate::agent_cx::AgentCx::for_request();
        while let Ok(request) = ask_ui_rx.recv(&cx).await {
            let _ = ask_fwd_tx.send(PiMsg::AskUiRequest(request));
        }
    });
}

/// Spawn the long-lived reply pump: answered cards pair back through the
/// CURRENT ask tool's `respond_ui` (see [`CurrentAsk`]).
fn spawn_ask_reply_pump(
    current_ask: CurrentAsk,
    ask_reply_rx: Receiver<AskUiReply>,
    runtime_handle: &asupersync::runtime::RuntimeHandle,
) {
    runtime_handle.spawn(async move {
        loop {
            match ask_reply_rx.try_recv() {
                Ok(reply) => {
                    let guard = current_ask
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(ask) = guard.as_ref() {
                        let _ = ask.respond_ui(&reply.request_id, reply.response);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    asupersync::time::sleep(asupersync::time::wall_now(), SUBMIT_POLL).await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
    });
}

/// Run one agent turn for a submitted prompt, translating events back to the
/// UI and surfacing turn errors as transcript entries.
async fn run_prompt_turn(
    handle: &mut crate::sdk::AgentSessionHandle,
    prompt: String,
    agent_tx: &Sender<PiMsg>,
) {
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

/// Template for `/resume`: a resumed session keeps the launch selection
/// (provider/model/key/cwd) but swaps the session file.
fn resume_template_from(options: &crate::sdk::SessionOptions) -> crate::sdk::SessionOptions {
    crate::sdk::SessionOptions {
        provider: options.provider.clone(),
        model: options.model.clone(),
        api_key: options.api_key.clone(),
        working_directory: options.working_directory.clone(),
        session_dir: options.session_dir.clone(),
        extension_paths: options.extension_paths.clone(),
        extension_policy: options.extension_policy.clone(),
        no_session: false,
        ..Default::default()
    }
}

/// Match the bubbletea stack's interactive extension-command budget.
const EXT_COMMAND_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

/// Dispatch a slash command to the extension runtime (bd-1eoh4): unknown or
/// unavailable commands report the same way the bubbletea stack does.
async fn run_extension_command(
    handle: &crate::sdk::AgentSessionHandle,
    cwd: &std::path::Path,
    name: &str,
    args: &str,
    agent_tx: &Sender<PiMsg>,
) {
    let manager = handle
        .session()
        .extensions
        .as_ref()
        .map(|region| region.manager().clone());
    let Some(manager) = manager else {
        let _ = agent_tx.send(PiMsg::System(format!(
            "Unknown command: /{name} (extensions disabled; try /help)"
        )));
        return;
    };
    if !manager.has_command(name) {
        let _ = agent_tx.send(PiMsg::System(format!(
            "Unknown command: /{name} (try /help)"
        )));
        return;
    }
    let Some(runtime) = manager.runtime() else {
        let _ = agent_tx.send(PiMsg::System(format!(
            "Extension command '/{name}' is not available (runtime not enabled)"
        )));
        return;
    };
    let _ = agent_tx.send(PiMsg::ToolStart {
        name: format!("/{name}"),
        tool_id: String::from("ftui-ext-command"),
    });
    let ctx_payload = serde_json::json!({
        "cwd": cwd.display().to_string(),
        "hasUI": true,
    });
    let result = runtime
        .execute_command(
            name.to_string(),
            args.to_string(),
            Arc::new(ctx_payload),
            EXT_COMMAND_TIMEOUT_MS,
        )
        .await;
    let msg = match result {
        Ok(value) if value.is_null() => PiMsg::SystemNote(format!("/{name} done")),
        Ok(value) => PiMsg::SystemNote(format!("/{name} → {value}")),
        Err(err) => PiMsg::AgentError(format!("/{name}: {err}")),
    };
    let _ = agent_tx.send(msg);
    let _ = agent_tx.send(PiMsg::ToolEnd {
        name: format!("/{name}"),
        tool_id: String::from("ftui-ext-command"),
        is_error: false,
    });
}

/// Handle a model switch in the driver, reporting the outcome to the UI.
async fn run_set_model_command(
    handle: &mut crate::sdk::AgentSessionHandle,
    provider: &str,
    model: &str,
    agent_tx: &Sender<PiMsg>,
) {
    let msg = match handle.set_model(provider, model).await {
        Ok(()) => PiMsg::System(format!("model set to {provider}/{model}")),
        Err(err) => PiMsg::AgentError(format!("model switch: {err}")),
    };
    let _ = agent_tx.send(msg);
}

/// Handle `/compact` in the driver: run compaction with events translated to
/// the UI, then replay the rewritten history into the transcript.
async fn run_compact_command(
    handle: &mut crate::sdk::AgentSessionHandle,
    agent_tx: &Sender<PiMsg>,
) {
    // ubs:ignore Sender clone per command — the event callback must own its sender
    let tx = agent_tx.clone();
    let result = handle
        .compact(move |event| {
            for msg in agent_event_to_pi_msgs(&event) {
                let _ = tx.send(msg);
            }
        })
        .await;
    match result {
        Ok(()) => {
            send_conversation_reset(handle, agent_tx, "conversation compacted").await;
        }
        Err(err) => {
            let _ = agent_tx.send(PiMsg::AgentError(format!("compact: {err}")));
        }
    }
}

/// Handle `/new` in the driver: build a fresh session from the launch
/// template with the CURRENT provider/model selection preserved and thinking
/// reset to off (SlashCommand::New parity), then swap it in exactly like
/// `/resume`. Returns the replacement handle on success; failures surface as
/// UI errors and keep the current session.
async fn new_session_command(
    template: &crate::sdk::SessionOptions,
    handle: &crate::sdk::AgentSessionHandle,
    current_ask: &CurrentAsk,
    ext_handler: &Arc<FtuiExtensionUiHandler>,
    agent_tx: &Sender<PiMsg>,
    runtime_handle: &asupersync::runtime::RuntimeHandle,
) -> Option<crate::sdk::AgentSessionHandle> {
    let (provider, model_id) = handle.model();
    let options = crate::sdk::SessionOptions {
        provider: Some(provider.clone()),
        model: Some(model_id.clone()),
        api_key: template.api_key.clone(),
        working_directory: template.working_directory.clone(),
        session_dir: template.session_dir.clone(),
        extension_paths: template.extension_paths.clone(),
        extension_policy: template.extension_policy.clone(),
        extension_ui_handler: Some(
            Arc::clone(ext_handler) as Arc<dyn crate::sdk::ExtensionUiHandler>
        ),
        thinking: Some(crate::model::ThinkingLevel::Off),
        no_session: false,
        ..Default::default()
    };
    match crate::sdk::create_agent_session(options).await {
        Ok(new_handle) => {
            *current_ask
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = new_handle.ask_tool();
            if let Some(ask) = new_handle.ask_tool() {
                install_ask_forwarder(&ask, agent_tx, runtime_handle);
            }
            send_conversation_reset(
                &new_handle,
                agent_tx,
                &format!(
                    "Started new session\nModel set to {provider}/{model_id}\nThinking level: off"
                ),
            )
            .await;
            Some(new_handle)
        }
        Err(err) => {
            let _ = agent_tx.send(PiMsg::AgentError(format!("new session: {err}")));
            None
        }
    }
}

/// Handle `/session`: report the live session's file/id/name/model/thinking/
/// message count. Token/cost totals are omitted deliberately — the ftui
/// stack tracks only last-turn usage today, and fabricated zeros would be
/// worse than absent lines.
async fn run_session_info_command(
    handle: &crate::sdk::AgentSessionHandle,
    agent_tx: &Sender<PiMsg>,
) {
    let state = match handle.state().await {
        Ok(state) => state,
        Err(err) => {
            let _ = agent_tx.send(PiMsg::AgentError(format!("session info: {err}")));
            return;
        }
    };
    let info = handle
        .with_session(|session| {
            let file = session.path.as_ref().map_or_else(
                || String::from("(not saved yet)"),
                |p| p.display().to_string(),
            );
            let name = session.get_name().unwrap_or_else(|| String::from("-"));
            format!(
                "Session info:\n  file: {file}\n  id: {id}\n  name: {name}\n  model: {provider}/{model_id}\n  thinking: {thinking}\n  messageCount: {message_count}",
                id = state.session_id.as_deref().unwrap_or("-"),
                provider = state.provider,
                model_id = state.model_id,
                thinking = state
                    .thinking_level
                    .as_ref()
                    .map_or_else(|| String::from("off"), ToString::to_string),
                message_count = state.message_count,
            )
        })
        .await;
    match info {
        Ok(text) => {
            let _ = agent_tx.send(PiMsg::System(text));
        }
        Err(err) => {
            let _ = agent_tx.send(PiMsg::AgentError(format!("session info: {err}")));
        }
    }
}

/// Handle `/tree`: print a textual branch-tree summary. The interactive
/// tree selector overlay is bd-cv653.9.8 scope; this keeps `/tree`
/// functional during the runtime-migration phase instead of letting it fall
/// through to extension dispatch and report "Unknown command".
async fn run_tree_summary_command(
    handle: &crate::sdk::AgentSessionHandle,
    agent_tx: &Sender<PiMsg>,
) {
    let summary = handle.with_session(|session| {
        let leaves = session.list_leaves();
        let entry_count = session.entries.len();
        if leaves.is_empty() {
            return format!("Session tree: no branches, {entry_count} entries");
        }
        let rendered = leaves
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n  ");
        format!(
            "Session tree: {} branch(es), {entry_count} entries\nLeaves:\n  {rendered}",
            leaves.len()
        )
    });
    match summary.await {
        Ok(text) => {
            let _ = agent_tx.send(PiMsg::System(text));
        }
        Err(err) => {
            let _ = agent_tx.send(PiMsg::AgentError(format!("tree: {err}")));
        }
    }
}

/// Handle `/thinking`: bare shows the effective level, a parsed level sets
/// it on the live session (`set_thinking_level` persists the header change).
async fn run_set_thinking_command(
    handle: &mut crate::sdk::AgentSessionHandle,
    level: Option<crate::model::ThinkingLevel>,
    agent_tx: &Sender<PiMsg>,
) {
    let msg = match level {
        None => match handle.state().await {
            Ok(state) => PiMsg::System(format!(
                "Thinking level: {}",
                state
                    .thinking_level
                    .as_ref()
                    .map_or_else(|| String::from("off"), ToString::to_string)
            )),
            Err(err) => PiMsg::AgentError(format!("thinking: {err}")),
        },
        Some(level) => match handle.set_thinking_level(level).await {
            Ok(()) => PiMsg::System(format!("Thinking level: {level}")),
            Err(err) => PiMsg::AgentError(format!("thinking: {err}")),
        },
    };
    let _ = agent_tx.send(msg);
}

/// Handle `/name <name>`: set the session display name.
async fn run_set_name_command(
    handle: &mut crate::sdk::AgentSessionHandle,
    name: &str,
    agent_tx: &Sender<PiMsg>,
) {
    let msg = match handle.set_session_name(name).await {
        Ok(()) => PiMsg::System(format!("Session name: {name}")),
        Err(err) => PiMsg::AgentError(format!("name: {err}")),
    };
    let _ = agent_tx.send(msg);
}

/// Handle `/undo` and `/redo` in the driver (bd-cv653.3.13): apply through
/// the session agent's mutation recorder and report the shared outcome text.
fn run_undo_command(
    handle: &crate::sdk::AgentSessionHandle,
    count: usize,
    force: bool,
    redo: bool,
    agent_tx: &Sender<PiMsg>,
) {
    let verb = if redo { "redo" } else { "undo" };
    let Some(recorder) = handle.session().agent.mutation_recorder() else {
        let _ = agent_tx.send(PiMsg::AgentError(format!(
            "/{verb} unavailable: no mutation recorder in this session"
        )));
        return;
    };
    let outcome = if redo {
        recorder.redo(count, force)
    } else {
        recorder.undo(count, force)
    };
    let _ = agent_tx.send(PiMsg::System(crate::undo::render_outcome_text(
        &outcome, redo, count,
    )));
}

/// Handle `/usage` in the driver (bd-cv653.7.4): read-only quota table.
async fn run_usage_command(refresh: bool, agent_tx: &Sender<PiMsg>) {
    let message = match crate::auth::AuthStorage::load(crate::config::Config::auth_path()) {
        Ok(auth) => {
            let rows = crate::usage::gather_usage(&auth, refresh).await;
            crate::usage::render_usage_text(&rows)
        }
        Err(err) => format!("failed to load credentials: {err}"),
    };
    let _ = agent_tx.send(PiMsg::System(message));
}

/// Handle `/resume` in the driver: open the chosen session file with the
/// launch selection preserved, rewire the ask bridge to the new handle, and
/// replay the conversation into the UI. Returns the replacement handle on
/// success (the caller swaps it in); failures surface as UI errors and keep
/// the current session.
async fn resume_session_command(
    path: &str,
    template: &crate::sdk::SessionOptions,
    current_ask: &CurrentAsk,
    ext_handler: &Arc<FtuiExtensionUiHandler>,
    agent_tx: &Sender<PiMsg>,
    runtime_handle: &asupersync::runtime::RuntimeHandle,
) -> Option<crate::sdk::AgentSessionHandle> {
    let options = crate::sdk::SessionOptions {
        session_path: Some(std::path::PathBuf::from(path)),
        provider: template.provider.clone(),
        model: template.model.clone(),
        api_key: template.api_key.clone(),
        working_directory: template.working_directory.clone(),
        session_dir: template.session_dir.clone(),
        extension_paths: template.extension_paths.clone(),
        extension_policy: template.extension_policy.clone(),
        extension_ui_handler: Some(
            Arc::clone(ext_handler) as Arc<dyn crate::sdk::ExtensionUiHandler>
        ),
        no_session: false,
        ..Default::default()
    };
    match crate::sdk::create_agent_session(options).await {
        Ok(handle) => {
            *current_ask
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = handle.ask_tool();
            if let Some(ask) = handle.ask_tool() {
                install_ask_forwarder(&ask, agent_tx, runtime_handle);
            }
            send_conversation_reset(&handle, agent_tx, "session resumed").await;
            Some(handle)
        }
        Err(err) => {
            let _ = agent_tx.send(PiMsg::AgentError(format!("resume: {err}")));
            None
        }
    }
}

/// Snapshot the handle's conversation and reset the UI transcript from it.
async fn send_conversation_reset(
    handle: &crate::sdk::AgentSessionHandle,
    agent_tx: &Sender<PiMsg>,
    status: &str,
) {
    match handle
        .with_session(crate::interactive::conversation_from_session)
        .await
    {
        Ok((messages, usage)) => {
            let _ = agent_tx.send(PiMsg::ConversationReset {
                messages,
                usage,
                status: Some(status.to_string()),
            });
        }
        Err(err) => {
            let _ = agent_tx.send(PiMsg::AgentError(format!("conversation snapshot: {err}")));
        }
    }
}

/// Run a `!command` for the driver loop: tool-status blips around the shared
/// bash runner, result rendered via the session display formatter. Returns
/// the display text on success so the caller can submit it as a turn
/// (`!` context-inclusion); `!!` gets the exclusion note appended.
async fn run_bash_ui_command(
    cwd: &std::path::Path,
    command: &str,
    exclude: bool,
    agent_tx: &Sender<PiMsg>,
) -> Option<String> {
    // Bracket the run with AgentStart/AgentDone (submit_bash_command
    // parity: bubbletea flips to ToolRunning so the status region shows the
    // running tool and the editor gates input). Without AgentStart the
    // model stays Ready and "running bash" never renders.
    let _ = agent_tx.send(PiMsg::AgentStart);
    let _ = agent_tx.send(PiMsg::ToolStart {
        name: String::from("bash"),
        tool_id: String::from("ftui-bash"),
    });
    let result = crate::tools::run_bash_command(cwd, None, None, command, None, None).await;
    let output = match result {
        Ok(result) => {
            let display = crate::session::bash_execution_to_text(
                command,
                &result.output,
                result.exit_code,
                result.cancelled,
                result.truncated,
                result.full_output_path.as_deref(),
            );
            let mut shown = display.clone();
            if exclude {
                shown.push_str("\n\n[Output excluded from model context]");
            }
            let _ = agent_tx.send(PiMsg::BashResult {
                display: shown,
                content_for_agent: None,
            });
            Some(display)
        }
        Err(err) => {
            let _ = agent_tx.send(PiMsg::AgentError(format!("bash: {err}")));
            None
        }
    };
    let _ = agent_tx.send(PiMsg::ToolEnd {
        name: String::from("bash"),
        tool_id: String::from("ftui-bash"),
        is_error: false,
    });
    let _ = agent_tx.send(PiMsg::AgentDone {
        usage: None,
        stop_reason: crate::model::StopReason::Stop,
        error_message: None,
    });
    output
}

/// Create the driver's agent session with the extension UI surface
/// (bd-1eoh4) installed on the options BEFORE creation so extension init
/// prompts work too. Errors surface to the UI and yield `None`.
async fn create_driver_session(
    mut session_options: crate::sdk::SessionOptions,
    agent_tx: &Sender<PiMsg>,
    ext_reply_rx: std::sync::mpsc::Receiver<ExtensionUiResponse>,
    runtime_handle: &asupersync::runtime::RuntimeHandle,
) -> Option<(crate::sdk::AgentSessionHandle, Arc<FtuiExtensionUiHandler>)> {
    let ext_handler = Arc::new(FtuiExtensionUiHandler::new(agent_tx.clone()));
    session_options.extension_ui_handler =
        Some(Arc::clone(&ext_handler) as Arc<dyn crate::sdk::ExtensionUiHandler>);
    spawn_ext_reply_pump(Arc::clone(&ext_handler), ext_reply_rx, runtime_handle);
    match crate::sdk::create_agent_session(session_options).await {
        Ok(handle) => Some((handle, ext_handler)),
        Err(err) => {
            let _ = agent_tx.send(PiMsg::AgentError(format!("session: {err}")));
            None
        }
    }
}

/// Working directory for `!` bash commands in the driver.
fn driver_bash_cwd(session_options: &crate::sdk::SessionOptions) -> std::path::PathBuf {
    session_options
        .working_directory
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

pub fn run(
    session_options: crate::sdk::SessionOptions,
    theme: &crate::theme::Theme,
    inline: bool,
    available_models: Vec<String>,
    available_sessions: Vec<(String, String)>,
) -> std::io::Result<()> {
    let (submit_tx, submit_rx) = std::sync::mpsc::channel::<UiCommand>();
    let (agent_tx, agent_rx) = std::sync::mpsc::channel::<PiMsg>();
    let (ask_reply_tx, ask_reply_rx) = std::sync::mpsc::channel::<AskUiReply>();
    let (ext_reply_tx, ext_reply_rx) = std::sync::mpsc::channel::<ExtensionUiResponse>();
    let bash_cwd = driver_bash_cwd(&session_options);
    let resume_template = resume_template_from(&session_options);

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
                let Some((mut handle, ext_handler)) = create_driver_session(
                    session_options,
                    &agent_tx,
                    ext_reply_rx,
                    &runtime_handle,
                )
                .await
                else {
                    return;
                };
                let current_ask =
                    install_ask_bridges(&handle, &agent_tx, ask_reply_rx, &runtime_handle);
                let _ = agent_tx.send(PiMsg::System(String::from(
                    "ftui preview stack — experimental (bd-cv653.9.1)",
                )));
                loop {
                    match submit_rx.try_recv() {
                        Ok(UiCommand::Prompt(prompt)) => {
                            run_prompt_turn(&mut handle, prompt, &agent_tx).await;
                        }
                        Ok(UiCommand::SetModel { provider, model }) => {
                            run_set_model_command(&mut handle, &provider, &model, &agent_tx).await;
                        }
                        Ok(UiCommand::Bash { command, exclude }) => {
                            // `!` semantics: the output becomes the next
                            // turn's user content (submit_content parity).
                            if let Some(output) =
                                run_bash_ui_command(&bash_cwd, &command, exclude, &agent_tx).await
                                && !exclude
                            {
                                run_prompt_turn(&mut handle, output, &agent_tx).await;
                            }
                        }
                        Ok(UiCommand::Compact) => {
                            run_compact_command(&mut handle, &agent_tx).await;
                        }
                        Ok(UiCommand::Undo { count, force, redo }) => {
                            run_undo_command(&handle, count, force, redo, &agent_tx);
                        }
                        Ok(UiCommand::Usage { refresh }) => {
                            run_usage_command(refresh, &agent_tx).await;
                        }
                        Ok(UiCommand::ExtensionCommand { name, args }) => {
                            run_extension_command(&handle, &bash_cwd, &name, &args, &agent_tx)
                                .await;
                        }
                        Ok(UiCommand::ResumeSession { path }) => {
                            if let Some(new_handle) = resume_session_command(
                                &path,
                                &resume_template,
                                &current_ask,
                                &ext_handler,
                                &agent_tx,
                                &runtime_handle,
                            )
                            .await
                            {
                                handle = new_handle;
                            }
                        }
                        Ok(UiCommand::NewSession) => {
                            if let Some(new_handle) = new_session_command(
                                &resume_template,
                                &handle,
                                &current_ask,
                                &ext_handler,
                                &agent_tx,
                                &runtime_handle,
                            )
                            .await
                            {
                                handle = new_handle;
                            }
                        }
                        Ok(UiCommand::SessionInfo) => {
                            run_session_info_command(&handle, &agent_tx).await;
                        }
                        Ok(UiCommand::TreeSummary) => {
                            run_tree_summary_command(&handle, &agent_tx).await;
                        }
                        Ok(UiCommand::SetThinking(level)) => {
                            run_set_thinking_command(&mut handle, level, &agent_tx).await;
                        }
                        Ok(UiCommand::SetName(name)) => {
                            run_set_name_command(&mut handle, &name, &agent_tx).await;
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
        .with_palette(FtuiPalette::from_theme(theme))
        .with_available_models(available_models)
        .with_available_sessions(available_sessions)
        .with_ext_reply_channel(ext_reply_tx);
    // Inline mode preserves shell scrollback (bead acceptance #2): the UI
    // anchors at the bottom, auto-sized to content within bounds; alt-screen
    // remains the default.
    let app = if inline {
        ftui::App::inline_auto(model, INLINE_MIN_HEIGHT, INLINE_MAX_HEIGHT)
    } else {
        ftui::App::fullscreen(model)
    };
    // Divert tracing output away from the terminal while the TUI owns it
    // (bd-trkef); restored on drop.
    let log_guard = crate::tui::TuiLogRedirectGuard::begin();
    let result = app.with_mouse().run();
    drop(log_guard);

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
    fn non_builtin_slash_commands_route_to_extension_dispatch() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/deploy --force");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::ExtensionCommand {
                name: "deploy".into(),
                args: "--force".into(),
            }
        );
        // /help stays local.
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
        assert!(
            rendered.contains("✓ bash"),
            "durable tool trace missing: {rendered:?}"
        );
        // Errored tools leave an ✗ trace.
        sim.send(PiFtuiMsg::Agent(PiMsg::ToolEnd {
            name: "edit".into(),
            tool_id: "t2".into(),
            is_error: true,
        }));
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.text.contains("✗ edit")),
            "error trace missing"
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

    fn ext_request(id: &str, method: &str, payload: serde_json::Value) -> ExtensionUiRequest {
        ExtensionUiRequest {
            id: id.to_string(),
            method: method.to_string(),
            payload,
            timeout_ms: None,
            extension_id: Some(String::from("demo-ext")),
        }
    }

    #[test]
    fn extension_confirm_prompt_renders_and_reply_routes() {
        let (_agent_tx, rx) = mpsc::channel();
        let (ext_tx, ext_rx) = mpsc::channel::<ExtensionUiResponse>();
        let model = PiFtuiModel::new(rx).with_ext_reply_channel(ext_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::ExtensionUiRequest(ext_request(
            "ext-1",
            "confirm",
            serde_json::json!({"title": "Deploy?", "message": "Ship to prod?"}),
        ))));
        let rendered = buffer_text(sim.capture_frame(50, 12), 50, 12);
        assert!(rendered.contains("Deploy?"), "prompt missing: {rendered:?}");
        assert!(
            rendered.contains("demo-ext"),
            "provenance missing: {rendered:?}"
        );
        // Mid-turn input works for the reply; 'yes' confirms.
        type_str(&mut sim, "yes");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        let reply = ext_rx.try_recv().expect("reply routed");
        assert_eq!(reply.id, "ext-1");
        assert!(!reply.cancelled);
        assert_eq!(reply.value, Some(serde_json::Value::Bool(true)));
        assert!(sim.model().active_ext.is_none());
    }

    #[test]
    fn extension_prompt_escape_cancels_and_queue_advances() {
        let (_agent_tx, rx) = mpsc::channel();
        let (ext_tx, ext_rx) = mpsc::channel::<ExtensionUiResponse>();
        let model = PiFtuiModel::new(rx).with_ext_reply_channel(ext_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::ExtensionUiRequest(ext_request(
            "ext-a",
            "confirm",
            serde_json::json!({"title": "First?"}),
        ))));
        sim.send(PiFtuiMsg::Agent(PiMsg::ExtensionUiRequest(ext_request(
            "ext-b",
            "confirm",
            serde_json::json!({"title": "Second?"}),
        ))));
        assert_eq!(sim.model().ext_queue.len(), 1, "second request not queued");
        sim.inject_event(key(KeyCode::Escape, Modifiers::empty()));
        let reply = ext_rx.try_recv().expect("cancel routed");
        assert_eq!(reply.id, "ext-a");
        assert!(reply.cancelled);
        // Queue advanced: the second prompt is now active.
        assert_eq!(
            sim.model().active_ext.as_ref().map(|r| r.id.as_str()),
            Some("ext-b")
        );
    }

    #[test]
    fn extension_prompt_queues_behind_active_ask() {
        let (_agent_tx, rx) = mpsc::channel();
        let (ask_tx, _ask_rx) = mpsc::channel::<AskUiReply>();
        let (ext_tx, _ext_rx) = mpsc::channel::<ExtensionUiResponse>();
        let model = PiFtuiModel::new(rx)
            .with_ask_reply_channel(ask_tx)
            .with_ext_reply_channel(ext_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::AskUiRequest(ask_request(
            "ask-hold",
            vec![question("Pick?", &["a", "b"], false)],
        ))));
        sim.send(PiFtuiMsg::Agent(PiMsg::ExtensionUiRequest(ext_request(
            "ext-waiting",
            "confirm",
            serde_json::json!({"title": "Later?"}),
        ))));
        assert!(sim.model().active_ext.is_none(), "ext jumped the ask");
        assert_eq!(sim.model().ext_queue.len(), 1);
        // Answer the ask; the queued extension prompt activates.
        type_str(&mut sim, "1");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            sim.model().active_ext.as_ref().map(|r| r.id.as_str()),
            Some("ext-waiting")
        );
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
    fn bare_model_command_opens_picker_and_selection_routes_set_model() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx)
            .with_submit_channel(submit_tx)
            .with_available_models(vec![
                String::from("openai/gpt-5"),
                String::from("anthropic/claude-opus-5"),
            ]);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/model");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(sim.model().picker.is_some(), "picker did not open");
        let rendered = buffer_text(sim.capture_frame(50, 10), 50, 10);
        assert!(
            rendered.contains("▸ openai/gpt-5"),
            "first entry not selected: {rendered:?}"
        );
        // Down + Enter selects the anthropic entry and routes SetModel.
        sim.inject_event(key(KeyCode::Down, Modifiers::empty()));
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::SetModel {
                provider: "anthropic".into(),
                model: "claude-opus-5".into(),
            }
        );
        assert!(sim.model().picker.is_none());
    }

    /// bd-cv653.3.13/7.4 parity: /undo //redo //usage route driver commands.
    #[test]
    fn slash_undo_redo_usage_route_commands() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();

        type_str(&mut sim, "/undo 3 force");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::Undo {
                count: 3,
                force: true,
                redo: false
            }
        );

        type_str(&mut sim, "/redo");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::Undo {
                count: 1,
                force: false,
                redo: true
            }
        );

        type_str(&mut sim, "/usage refresh");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::Usage { refresh: true }
        );

        // Bad argument reports usage instead of sending a command.
        type_str(&mut sim, "/undo everything");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(submit_rx.try_recv().is_err(), "no command for bad args");
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.text.contains("usage: /undo")),
            "usage error shown"
        );
    }

    /// A longer command must not be captured by a shorter prefix.
    #[test]
    fn strip_command_requires_exact_name_or_space() {
        assert_eq!(strip_command("/undo", "/undo"), Some(""));
        assert_eq!(strip_command("/UNDO 2", "/undo"), Some("2"));
        assert_eq!(strip_command("/undo 2", "/undo"), Some("2"));
        assert_eq!(strip_command("/undocumented", "/undo"), None);
        assert_eq!(strip_command("/usage", "/usage"), Some(""));
    }

    #[test]
    fn slash_compact_routes_command() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/compact");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(submit_rx.try_recv().expect("routed"), UiCommand::Compact);
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.text.contains("compacting")),
            "compact note missing"
        );
    }

    #[test]
    fn slash_exit_quits() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/exit");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(!sim.is_running(), "/exit did not quit");
    }

    #[test]
    fn bare_model_command_errors_without_registry() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/model");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(sim.model().picker.is_none());
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.role == EntryRole::Error && e.text.contains("no models available")),
            "empty-registry error missing"
        );
    }

    #[test]
    fn resume_picker_shows_labels_and_routes_paths() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx)
            .with_submit_channel(submit_tx)
            .with_available_sessions(vec![
                (
                    String::from("fix parser · 12 msgs"),
                    String::from("/tmp/sessions/a.jsonl"),
                ),
                (
                    String::from("older run · 3 msgs"),
                    String::from("/tmp/sessions/b.jsonl"),
                ),
            ]);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/resume");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        let rendered = buffer_text(sim.capture_frame(50, 10), 50, 10);
        assert!(
            rendered.contains("▸ fix parser · 12 msgs"),
            "labels not shown: {rendered:?}"
        );
        assert!(
            !rendered.contains("/tmp/sessions"),
            "paths leaked into display: {rendered:?}"
        );
        sim.inject_event(key(KeyCode::Char('j'), Modifiers::empty()));
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::ResumeSession {
                path: "/tmp/sessions/b.jsonl".into()
            }
        );
    }

    #[test]
    fn conversation_reset_rebuilds_transcript() {
        use crate::interactive::{ConversationMessage, MessageRole};
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        // Preexisting content is replaced wholesale.
        sim.send(PiFtuiMsg::Agent(PiMsg::System("old line".into())));
        sim.send(PiFtuiMsg::Agent(PiMsg::ConversationReset {
            messages: vec![
                ConversationMessage {
                    role: MessageRole::User,
                    content: "restore me".into(),
                    thinking: None,
                    collapsed: false,
                },
                ConversationMessage {
                    role: MessageRole::Assistant,
                    content: "restored reply".into(),
                    thinking: None,
                    collapsed: false,
                },
            ],
            usage: crate::model::Usage::default(),
            status: Some("session resumed".into()),
        }));
        let transcript = &sim.model().transcript;
        assert!(
            !transcript.iter().any(|e| e.text.contains("old line")),
            "stale transcript survived reset"
        );
        assert!(
            transcript
                .iter()
                .any(|e| e.role == EntryRole::User && e.text == "restore me")
        );
        assert!(
            transcript
                .iter()
                .any(|e| e.role == EntryRole::Assistant && e.text == "restored reply")
        );
        assert!(
            transcript
                .iter()
                .any(|e| e.text.contains("session resumed"))
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
    fn bang_routes_bash_command_and_result_renders() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "!echo hi");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::Bash {
                command: "echo hi".into(),
                exclude: false,
            }
        );
        // `!!` runs display-only (excluded from model context).
        type_str(&mut sim, "!!ls");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::Bash {
                command: "ls".into(),
                exclude: true,
            }
        );
        // Bare `!` errors locally.
        type_str(&mut sim, "!");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(submit_rx.try_recv().is_err());
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.role == EntryRole::Error && e.text.contains("usage: !")),
            "bare-bang usage error missing"
        );
        // A BashResult renders into the transcript as a system entry.
        sim.send(PiFtuiMsg::Agent(PiMsg::BashResult {
            display: "$ echo hi\nhi".into(),
            content_for_agent: None,
        }));
        let rendered = buffer_text(sim.capture_frame(40, 10), 40, 10);
        assert!(
            rendered.contains("echo hi"),
            "bash display missing: {rendered:?}"
        );
    }

    #[test]
    fn subscription_id_is_stable() {
        let (_tx, rx) = mpsc::channel::<PiMsg>();
        let sub = AgentEventSubscription::new(rx);
        assert_eq!(sub.id(), AGENT_EVENTS_SUB_ID);
    }
    #[test]
    fn session_slash_commands_route_to_driver() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/new");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(submit_rx.try_recv().expect("routed"), UiCommand::NewSession);
        type_str(&mut sim, "/session");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::SessionInfo
        );
        type_str(&mut sim, "/tree deep --all");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::TreeSummary
        );
        type_str(&mut sim, "/thinking medium");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::SetThinking(Some(crate::model::ThinkingLevel::Medium))
        );
        // Numeric and abbreviated aliases parse like the bubbletea stack.
        type_str(&mut sim, "/t 3");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::SetThinking(Some(crate::model::ThinkingLevel::High))
        );
        // Bare /thinking asks the driver for the current level.
        type_str(&mut sim, "/think");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::SetThinking(None)
        );
        // Invalid levels error locally without reaching the driver.
        type_str(&mut sim, "/thinking bogus");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(submit_rx.try_recv().is_err());
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.role == EntryRole::Error && e.text.contains("Invalid thinking level")),
            "invalid-level error missing"
        );
        // /name requires an argument; a provided one routes through.
        type_str(&mut sim, "/name");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(submit_rx.try_recv().is_err());
        type_str(&mut sim, "/name ship-it");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::SetName(String::from("ship-it"))
        );
    }

    #[test]
    fn slash_input_is_gated_while_working() {
        // The editor only accepts input while the agent is idle
        // (`input_active` parity), so mid-turn /new and /tree neither reach
        // the driver nor fabricate error entries — the gate IS the busy
        // guard.
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        type_str(&mut sim, "/new");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        type_str(&mut sim, "/tree");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(submit_rx.try_recv().is_err());
        assert!(sim.model().transcript.is_empty());
    }

    #[test]
    fn clear_resets_transcript_locally() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::System(String::from(
            "earlier note",
        ))));
        type_str(&mut sim, "/cls");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        let transcript = &sim.model().transcript;
        assert!(!transcript.iter().any(|e| e.text.contains("earlier note")));
        assert!(transcript.iter().any(|e| e.text == "Conversation cleared"));
    }
    #[test]
    fn slash_commands_are_case_insensitive_with_aliases() {
        // Token matching lowercases like SlashCommand::parse; aliases /q,
        // /r, /h, /? and /m ride along for free. /Q LAST: Cmd::quit ends
        // simulated input processing.
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        // Bare /M with no models errors locally instead of reaching a driver.
        type_str(&mut sim, "/M");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.text.contains("no models available")),
            "uppercase /M must hit the model path"
        );
        type_str(&mut sim, "/H");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.text.contains("ftui preview commands")),
            "uppercase /H must show help"
        );
        type_str(&mut sim, "/Q");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(sim.model().pending_quit, "uppercase /Q must quit");
    }

    #[test]
    fn tool_card_transitions_and_bash_detail_folding() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        // !bash flow: ToolStart opens a pending card, BashResult folds an
        // 8-line-capped preview into it, ToolEnd flips it to Ok in place.
        sim.send(PiFtuiMsg::Agent(PiMsg::ToolStart {
            name: "bash".into(),
            tool_id: "t1".into(),
        }));
        assert!(
            sim.model()
                .transcript
                .last()
                .and_then(|e| e.card.as_ref())
                .is_some_and(|c| *c == CardState::Pending),
            "ToolStart must open a pending card"
        );
        let output = "line-one\nline-two";
        sim.send(PiFtuiMsg::Agent(PiMsg::BashResult {
            display: format!("$ demo\n{output}"),
            content_for_agent: None,
        }));
        let card = sim
            .model()
            .transcript
            .iter()
            .rev()
            .find(|e| e.text == "bash")
            .expect("bash card exists");
        assert!(
            card.detail
                .as_deref()
                .is_some_and(|d| d.contains("line-one") && d.contains("line-two")),
            "BashResult must fold its preview into the pending card"
        );
        sim.send(PiFtuiMsg::Agent(PiMsg::ToolEnd {
            name: "bash".into(),
            tool_id: "t1".into(),
            is_error: false,
        }));
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.card == Some(CardState::Ok))
        );
        // An errored run opens and closes its own Err card.
        sim.send(PiFtuiMsg::Agent(PiMsg::ToolStart {
            name: "edit".into(),
            tool_id: "t2".into(),
        }));
        sim.send(PiFtuiMsg::Agent(PiMsg::ToolEnd {
            name: "edit".into(),
            tool_id: "t2".into(),
            is_error: true,
        }));
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.card == Some(CardState::Err))
        );
        let rendered = buffer_text(sim.capture_frame(60, 16), 60, 16);
        assert!(
            rendered.contains("✓ bash"),
            "ok glyph missing: {rendered:?}"
        );
        assert!(
            rendered.contains("✗ edit"),
            "error glyph missing: {rendered:?}"
        );
        assert!(rendered.contains("line-one"), "folded detail missing");
    }
}
