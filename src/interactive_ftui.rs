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
    transcript: Vec<String>,
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
    submit_tx: Option<Sender<String>>,
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
    pub fn with_submit_channel(mut self, tx: Sender<String>) -> Self {
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
            .map(|e| e.lines().count().max(1))
            .sum();
        let streaming = if self.streaming.is_empty() {
            0
        } else {
            self.streaming.lines().count().max(1)
        };
        transcript + streaming
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
                    self.transcript.push(std::mem::take(&mut self.streaming));
                }
                if let Some(err) = error_message {
                    self.transcript.push(format!("error: {}", sanitize(&err)));
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
                self.transcript.push(format!("error: {}", sanitize(&err)));
                self.state = AgentUiState::Ready;
                self.current_tool = None;
                self.thinking.clear();
            }
            PiMsg::System(text) | PiMsg::SystemNote(text) => {
                self.transcript.push(sanitize(&text).into_owned());
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
        self.transcript.push(sanitize(card.trim_end()).into_owned());
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
                self.transcript.push(format!("  ! {}", sanitize(&err)));
                self.scroll_from_tail = 0;
                self.active_ask = Some(ask); // same question again
            }
            Ok(QuestionReply::Cancel) => {
                self.transcript.push(String::from("  (dismissed)"));
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
                self.transcript.push(sanitize(&echo).into_owned());
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
        self.transcript.push(format!("› {clean}"));
        if let Some(tx) = &self.submit_tx {
            // A dead agent loop is not a UI error; the transcript echo above
            // still shows what was typed.
            let _ = tx.send(clean);
        }
        self.input.set_text("");
        self.scroll_from_tail = 0;
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
                            self.transcript.push(String::from("  (dismissed)"));
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

    fn conversation_text(&self) -> String {
        let mut body = String::new();
        for line in &self.transcript {
            body.push_str(line);
            body.push('\n');
        }
        if !self.streaming.is_empty() {
            body.push_str(&self.streaming);
            body.push('\n');
        }
        body
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
        Paragraph::new(Text::raw(&header)).render(regions.header, frame);

        // Conversation body with tail-follow scroll. `scroll_from_tail == 0`
        // sticks to the bottom; scrolling up pins an offset measured from the
        // tail so streaming appends don't yank the view.
        let body_text = self.conversation_text();
        let total_lines = if body_text.is_empty() {
            0
        } else {
            body_text.lines().count()
        };
        let visible = usize::from(regions.body.height).max(1);
        let from_tail = self
            .scroll_from_tail
            .min(total_lines.saturating_sub(visible));
        let offset = total_lines.saturating_sub(visible + from_tail);
        let offset_u16 = u16::try_from(offset).unwrap_or(u16::MAX);
        Paragraph::new(Text::raw(&body_text))
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
            Paragraph::new(Text::raw(&status_line)).render(regions.status, frame);
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
        Paragraph::new(Text::raw(&footer)).render(regions.footer, frame);
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
        assert_eq!(sim.model().transcript, vec!["hello world".to_string()]);
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
        let (submit_tx, submit_rx) = mpsc::channel::<String>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        for ch in ['h', 'i'] {
            sim.inject_event(key(KeyCode::Char(ch), Modifiers::empty()));
        }
        assert_eq!(sim.model().input.text(), "hi");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(submit_rx.try_recv().expect("submitted"), "hi");
        assert!(sim.model().input.is_empty(), "editor not cleared");
        assert_eq!(sim.model().transcript, vec!["› hi".to_string()]);
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
        let (submit_tx, submit_rx) = mpsc::channel::<String>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        // Simulate a hostile paste carrying an OSC title change.
        sim.inject_event(Event::Paste(ftui::PasteEvent::new(
            "hello\x1b]0;pwned\x07world",
            true,
        )));
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        let submitted = submit_rx.try_recv().expect("submitted");
        assert!(!submitted.contains('\x1b'), "ESC survived: {submitted:?}");
        assert!(submitted.contains("hello"));
        assert!(submitted.contains("world"));
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
    fn subscription_id_is_stable() {
        let (_tx, rx) = mpsc::channel::<PiMsg>();
        let sub = AgentEventSubscription::new(rx);
        assert_eq!(sub.id(), AGENT_EVENTS_SUB_ID);
    }
}
