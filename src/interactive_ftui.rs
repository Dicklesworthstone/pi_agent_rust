//! FrankenTUI interactive stack (bd-cv653.9.1) — the default TUI since the
//! 2026-08-25 cutover (bd-ti0tq).
//!
//! This module hosts the ftui-runtime port of the interactive front-end. The
//! `ftui` feature is on by default, so plain `pi` launches this stack; the
//! charmed_rust/bubbletea stack in [`crate::interactive`] remains selectable
//! with `pi --classic` (aliases `--classic-tui`, `--charmed`, `--bubbletea`)
//! until it is deleted. Add `--inline` to keep shell scrollback instead of
//! the alternate screen, or try the fake-agent demo:
//! `cargo run --example ftui_preview --features ftui`.
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
//!   picker overlay (`/theme`), the slash-command completion popup above the
//!   editor (issue #208; shares [`crate::autocomplete`] with the charmed
//!   stack), and input routing for `/model`, `/help`, and
//!   display-only `!`/`!!` bash. All agent/tool-originated text passes
//!   through `ftui::render::sanitize` before it can reach a frame.
//! - [`run`]: the `pi --ftui` launch path — a driver thread owns an
//!   asupersync runtime plus an SDK session; prompts become real agent turns
//!   ([`agent_event_to_pi_msgs`] pins the translation), asks pair through
//!   `respond_ui`, sessions persist per the usual CLI flags.
//!
//! Fully ported surfaces:
//! - Interactive tree/fork selector overlays and toast queue ([`crate::overlay_system`])
//! - Command-palette autocomplete and composer ([`crate::autocomplete`])
//! - Rich Powerline status line with responsive dropping ([`crate::status_line`])
//! - Rich markdown with LaTeX symbols, mermaid diagrams, and hex swatches ([`crate::markdown_rich`])
//! - Delight animations, sparklines, and terminal titles ([`crate::delight`])
//! - Visual regression test matrix ([`crate::gallery`])
//! - Core session slash commands (/new, /clear, /session, /tree summary, /thinking, /name), bash context-inclusion, extension UIs, and the PTY/e2e acceptance lanes.

use std::cell::Cell;
use std::fmt::Write as _;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
use crate::autocomplete::{AutocompleteCatalog, AutocompleteItem, AutocompleteItemKind};
use crate::extensions::{ExtensionUiRequest, ExtensionUiResponse};
use crate::interactive::{AutocompleteState, PiMsg, extension_commands_for_catalog};
use crate::interactive::{format_extension_ui_prompt, parse_extension_ui_response};
use crate::keybindings::{AppAction, KeyBinding, KeyBindings};
use std::collections::VecDeque;

/// Typed message for the ftui model: terminal events plus bridged agent events.
///
/// `Model::Message` must be `From<Event>`, so terminal input arrives through
/// [`PiFtuiMsg::Term`]; everything async arrives through [`PiFtuiMsg::Agent`]
/// via [`AgentEventSubscription`]. [`PiFtuiMsg::Resumed`] is produced by the
/// suspend task after the process returns from a SIGTSTP stop (ctrl+z).
#[derive(Debug)]
pub enum PiFtuiMsg {
    /// A raw terminal event (key, mouse, resize, paste, focus, ...).
    Term(Event),
    /// An agent/system event bridged from the async side.
    Agent(PiMsg),
    /// The process came back from a SIGTSTP suspension: the terminal has
    /// been re-acquired and the next frame must repaint everything.
    Resumed,
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

/// Loop-lag budget. A single `update()` or `view()` call that holds the event
/// loop this long has stalled it: input sits unread, agent deltas queue, and
/// the next present is late by at least this much.
const LOOP_STALL_BUDGET: Duration = Duration::from_millis(250);

/// Event-loop phase a stall is attributed to. Mirrors the render / input /
/// agent-event split that omp's `loop-watchdog.ts` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopPhase {
    /// Time inside `view()` — layout, styling, and widget rendering.
    Render,
    /// Time inside `update()` for anything the terminal produced: keys, mouse,
    /// paste, resize, and the spinner tick. Named for the dominant case rather
    /// than split further, since omp's three-way attribution is what this
    /// mirrors and a stalled tick is diagnosed the same way as a stalled key.
    Input,
    /// Time inside `update()` for anything the async side produced: bridged
    /// agent events (deltas, tool cards) and the post-SIGTSTP resume, which
    /// arrives on the same non-terminal path.
    AgentEvent,
}

impl LoopPhase {
    /// Every phase, in report order.
    const ALL: [Self; 3] = [Self::Render, Self::Input, Self::AgentEvent];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Render => "render",
            Self::Input => "input",
            Self::AgentEvent => "agent_event",
        }
    }
}

/// Per-phase counters. `Cell` because `view()` only has `&self`; the ftui
/// event loop is single-threaded so no synchronization is needed.
#[derive(Debug, Default)]
struct PhaseCounters {
    /// Calls observed.
    samples: Cell<u64>,
    /// Cumulative busy time.
    busy_us: Cell<u64>,
    /// Worst single call.
    worst_us: Cell<u64>,
    /// Calls that blew the loop budget.
    stalls: Cell<u64>,
}

/// Loop watchdog: a lag probe over the model's own callbacks, with phase
/// attribution and one structured log per stall (omp `loop-watchdog.ts`
/// parity).
///
/// What it measures is deliberately narrow and honest: how long a single
/// `update()` or `view()` call holds the ftui event loop. That *is* the loop
/// lag the model can be responsible for. It does not time the gap *between*
/// callbacks — a parked idle session has an unbounded such gap by design, so
/// reporting it would be noise, not signal.
///
/// Stall reporting is latched: a sustained stall (many consecutive
/// over-budget frames during, say, one enormous paste) emits one warning, not
/// one per frame. The latch clears as soon as a phase completes inside budget,
/// so a later stall is reported again.
///
/// Gated behind `PI_PERF_TELEMETRY=1`, matching the bubbletea stack's
/// [`crate::interactive::perf`] frame telemetry. When disabled no
/// `Instant::now()` is called and the probe costs one bool test per callback.
///
/// `view()` takes `&self`, so the counters are `Cell`s. The ftui event loop is
/// single-threaded, so no synchronization is needed (same rationale as
/// `FrameTimingStats` on the bubbletea side).
#[derive(Debug)]
struct LoopWatchdog {
    enabled: bool,
    /// Time inside `view()`.
    render: PhaseCounters,
    /// Time inside `update()` for terminal events.
    input: PhaseCounters,
    /// Time inside `update()` for bridged agent events.
    agent_event: PhaseCounters,
    /// Latch suppressing repeat logs for one continuous stall.
    stalled: Cell<bool>,
}

impl Default for LoopWatchdog {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopWatchdog {
    fn new() -> Self {
        Self::with_enabled(
            std::env::var_os("PI_PERF_TELEMETRY").is_some_and(|v| v == "1" || v == "true"),
        )
    }

    /// Construct with an explicit gate. `new()` reads the environment; tests
    /// pin the gate so they never depend on the ambient `PI_PERF_TELEMETRY`.
    fn with_enabled(enabled: bool) -> Self {
        Self {
            enabled,
            render: PhaseCounters::default(),
            input: PhaseCounters::default(),
            agent_event: PhaseCounters::default(),
            stalled: Cell::new(false),
        }
    }

    /// Counters owned by one phase. Named-field dispatch, so a phase can never
    /// read another phase's numbers and there is no index to get wrong.
    const fn counters(&self, phase: LoopPhase) -> &PhaseCounters {
        match phase {
            LoopPhase::Render => &self.render,
            LoopPhase::Input => &self.input,
            LoopPhase::AgentEvent => &self.agent_event,
        }
    }

    /// Start timing a phase. Returns `None` when telemetry is off, which is
    /// what keeps the disabled path free of clock reads.
    fn start(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    /// Close out a phase started by [`Self::start`] and report a stall if this
    /// call blew the loop budget.
    fn finish(&self, phase: LoopPhase, started: Option<Instant>) {
        let Some(started) = started else {
            return;
        };
        let elapsed = started.elapsed();
        let elapsed_us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        let counters = self.counters(phase);
        counters
            .samples
            .set(counters.samples.get().saturating_add(1));
        counters
            .busy_us
            .set(counters.busy_us.get().saturating_add(elapsed_us));
        if elapsed_us > counters.worst_us.get() {
            counters.worst_us.set(elapsed_us);
        }

        if elapsed < LOOP_STALL_BUDGET {
            // Back inside budget: re-arm reporting for the next stall.
            self.stalled.set(false);
            return;
        }
        counters.stalls.set(counters.stalls.get().saturating_add(1));
        if self.stalled.replace(true) {
            // Already inside a reported stall — stay quiet.
            return;
        }
        tracing::warn!(
            schema = "pi.tui.loop_watchdog.v1",
            surface = "ftui",
            phase = phase.as_str(),
            lag_us = elapsed_us,
            budget_us = u64::try_from(LOOP_STALL_BUDGET.as_micros()).unwrap_or(u64::MAX),
            "TUI event loop stalled past the lag budget"
        );
    }

    /// Structured counters for tests and evidence artifacts. Shares the
    /// redaction posture of `pi.tui.frame_budget.v1`: timings only, never
    /// prompt, tool, or model content.
    fn snapshot(&self) -> serde_json::Value {
        let phases: serde_json::Map<String, serde_json::Value> = LoopPhase::ALL
            .into_iter()
            .map(|phase| {
                let counters = self.counters(phase);
                let samples = counters.samples.get();
                let busy_us = counters.busy_us.get();
                (
                    phase.as_str().to_string(),
                    serde_json::json!({
                        "samples": samples,
                        "busy_us": busy_us,
                        "mean_us": busy_us.checked_div(samples).unwrap_or(0),
                        "worst_us": counters.worst_us.get(),
                        "stalls": counters.stalls.get(),
                    }),
                )
            })
            .collect();
        let stalls_total: u64 = LoopPhase::ALL
            .into_iter()
            .map(|phase| self.counters(phase).stalls.get())
            .sum();
        serde_json::json!({
            "schema": "pi.tui.loop_watchdog.v1",
            "surface": "ftui",
            "enabled": self.enabled,
            "budget_us": u64::try_from(LOOP_STALL_BUDGET.as_micros()).unwrap_or(u64::MAX),
            "phases": phases,
            "totals": { "stalls": stalls_total },
            "verdict": if !self.enabled {
                "disabled"
            } else if stalls_total == 0 {
                "pass"
            } else {
                "warn"
            },
            "redaction": {
                "prompt_content": "omitted",
                "tool_payload_content": "omitted",
                "model_response_content": "omitted",
            },
        })
    }
}

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

/// Restore the terminal to shell-safe cooked state and stop the process
/// until the shell foregrounds it again (ctrl+z → `fg`, bd-cv653.9.1
/// round-4). Returns the post-resume terminal size so the caller can feed a
/// repaint.
///
/// Mirrors exactly the features `ProgramConfig` enables by default —
/// bracketed paste, SGR mouse, alternate screen (fullscreen only); kitty
/// keyboard and focus reporting stay off, so no sequences are needed for
/// them. Disabling a feature that was never enabled is ignored by the
/// terminal, which keeps this robust against capability probing.
///
/// The stop itself is `raise(SIGTSTP)` with the signal at its default
/// disposition: the whole process freezes inside this call and execution
/// continues on SIGCONT (`fg`). Between the restore writes and the raise
/// there is a sub-tick window in which a render could theoretically fire;
/// the model freezes spinner ticks while suspending so pending frames stay
/// byte-identical and the diff engine emits nothing.
#[cfg(unix)]
fn perform_terminal_suspend(alt_screen: bool) -> std::io::Result<(u16, u16)> {
    use std::io::{Write, stdout};

    use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};

    // Cooked mode first: the shell must own echo/signal handling while we
    // are stopped. Raw mode is process-global termios state, safe to toggle
    // from a task thread.
    disable_raw_mode()?;
    {
        let mut out = stdout();
        out.write_all(b"\x1b[?2004l")?; // bracketed paste off
        out.write_all(b"\x1b[?1006l\x1b[?1002l\x1b[?1000l")?; // SGR mouse off
        if alt_screen {
            out.write_all(b"\x1b[?1049l")?; // leave alternate screen
        }
        out.write_all(b"\x1b[?25h")?; // show cursor
        out.flush()?;
    };
    // Stops the process here; resumes after `fg`.
    signal_hook::low_level::raise(signal_hook::consts::signal::SIGTSTP)?;

    // --- continued ---
    enable_raw_mode()?;
    {
        let mut out = stdout();
        if alt_screen {
            out.write_all(b"\x1b[?1049h")?; // re-enter alternate screen
        }
        out.write_all(b"\x1b[?2004h")?; // bracketed paste on
        out.write_all(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h")?; // mouse on (SGR)
        out.write_all(b"\x1b[?25l")?; // hide cursor
        out.flush()?;
    }
    size()
}

/// Build the blocking task behind [`AppAction::Suspend`]: park the terminal,
/// stop until continued, then hand back a message that clears the suspend
/// state and triggers a full repaint.
#[cfg(unix)]
fn suspend_task(alt_screen: bool) -> impl FnOnce() -> PiFtuiMsg + Send + 'static {
    move || match perform_terminal_suspend(alt_screen) {
        Ok((width, height)) => PiFtuiMsg::Term(Event::Resize { width, height }),
        Err(err) => PiFtuiMsg::Agent(PiMsg::AgentError(format!("suspend/resume: {err}"))),
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

/// Word-level pairing for one removed/added line couplet: returns the
/// shared framing and the changed middles as `(prefix, removed_middle,
/// added_middle, suffix)` with whitespace normalized to single spaces.
/// `None` when the lines are word-identical or share no framing on either
/// side (a bare middle would not read as a focused change).
fn word_diff_parts(removed: &str, added: &str) -> Option<(String, String, String, String)> {
    let rem_words: Vec<&str> = removed.split_whitespace().collect();
    let add_words: Vec<&str> = added.split_whitespace().collect();
    if rem_words.is_empty() || add_words.is_empty() {
        return None;
    }
    let mut pre = 0;
    while pre < rem_words.len() && pre < add_words.len() && rem_words[pre] == add_words[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < rem_words.len() - pre
        && suf < add_words.len() - pre
        && rem_words[rem_words.len() - 1 - suf] == add_words[add_words.len() - 1 - suf]
    {
        suf += 1;
    }
    let rem_mid = &rem_words[pre..rem_words.len() - suf];
    let add_mid = &add_words[pre..add_words.len() - suf];
    if rem_mid.is_empty() && add_mid.is_empty() {
        return None;
    }
    if pre == 0 && suf == 0 {
        return None;
    }
    let join = |words: &[&str]| words.join(" ");
    let prefix = if pre > 0 {
        format!("{} ", join(&rem_words[..pre]))
    } else {
        String::new()
    };
    let suffix = if suf > 0 {
        format!(" {}", join(&rem_words[rem_words.len() - suf..]))
    } else {
        String::new()
    };
    Some((prefix, join(rem_mid), join(add_mid), suffix))
}

/// Render one tool-card block: state glyph + name on the head line (the
/// glyph is the SHARED spinner frame while pending), then the folded
/// result detail. Diff cards pair consecutive -/+ lines and emphasize the
/// changed words; everything else renders dim indented lines.
#[allow(clippy::too_many_arguments)]
fn push_card_block(
    lines: &mut Vec<ftui::text::Line<'static>>,
    state: CardState,
    text: &str,
    detail: Option<&String>,
    diff_styled: bool,
    group_count: u32,
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
    let head = if group_count > 1 {
        format!("{glyph} {text} ×{group_count}")
    } else {
        format!("{glyph} {text}")
    };
    lines.push(ftui::text::Line::styled(head, style));
    let Some(detail) = detail else {
        return;
    };
    let dim = |s: String| ftui::text::Span::styled(s, ftui::Style::new().dim().fg(palette.muted));
    let added_span = |s: String| ftui::text::Span::styled(s, ftui::Style::new().fg(palette.accent));
    let removed_span =
        |s: String| ftui::text::Span::styled(s, ftui::Style::new().fg(palette.error));
    let body = detail.lines().collect::<Vec<_>>();
    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        // Pair a removed line immediately followed by an added line and
        // emphasize only the changed middle words (markers kept).
        if diff_styled
            && line.starts_with('-')
            && i + 1 < body.len()
            && body[i + 1].starts_with('+')
            && let Some((prefix, rem_mid, add_mid, suffix)) =
                word_diff_parts(&line[1..], &body[i + 1][1..])
        {
            lines.push(ftui::text::Line::from_spans(vec![
                removed_span(format!("- {prefix}")),
                removed_span(rem_mid),
                dim(suffix.clone()),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                added_span(format!("+ {prefix}")),
                added_span(add_mid),
                dim(suffix),
            ]));
            i += 2;
            continue;
        }
        let span = match diff_styled.then(|| line.as_bytes().first().copied()) {
            Some(Some(b'+')) => added_span(format!("  {line}")),
            Some(Some(b'-')) => removed_span(format!("  {line}")),
            _ => dim(format!("  {line}")),
        };
        lines.push(ftui::text::Line::from_spans(vec![span]));
        i += 1;
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

/// Whether a rendered markdown line is a spacing boundary for compact mode
/// (issue #202): headings and code/math block content keep one line of air
/// around them so they never visually merge with body text. Detection works
/// off the theme styles the renderer stamps on those lines: code-family
/// lines carry the block style on their first span (the indent span for
/// highlighted code, the whole line otherwise, including whitespace-only
/// interior code lines — which therefore never count as blanks); heading
/// lines carry an `h1`–`h6` style on some span.
fn compact_line_is_boundary(
    line: &ftui::text::Line<'_>,
    theme: &ftui_extras::markdown::MarkdownTheme,
) -> bool {
    let code_styles = [
        theme.code_block,
        theme.math_block,
        // The "─── lang ───" fence header emitted for common languages.
        theme.code_inline.dim(),
    ];
    let heading_styles = [theme.h1, theme.h2, theme.h3, theme.h4, theme.h5, theme.h6];
    let Some(first_style) = line.spans().first().and_then(|span| span.style) else {
        return false;
    };
    code_styles.contains(&first_style)
        || line.spans().iter().any(|span| {
            span.style
                .is_some_and(|style| heading_styles.contains(&style))
        })
}

/// Compact spacing policy (issue #202): the markdown renderer emits one
/// blank line after every block, which reads ~2x the content height in a
/// transcript. Compact keeps a single blank only where one of the
/// neighboring lines is a boundary (heading or fence — collapsing those
/// gaps makes them merge with body text) and where the run trails the
/// message (preserving today's separation from the next transcript entry);
/// paragraph/list gaps collapse to nothing and multi-blank runs to at most
/// one.
fn apply_compact_spacing(
    lines: &[ftui::text::Line<'static>],
    theme: &ftui_extras::markdown::MarkdownTheme,
) -> Vec<ftui::text::Line<'static>> {
    let mut out: Vec<ftui::text::Line<'static>> = Vec::with_capacity(lines.len());
    let mut prev_boundary = false;
    let mut i = 0;
    while i < lines.len() {
        let boundary = compact_line_is_boundary(&lines[i], theme);
        let blank = !boundary && lines[i].to_plain_text().trim().is_empty();
        if !blank {
            out.push(lines[i].clone());
            prev_boundary = boundary;
            i += 1;
            continue;
        }
        // Measure the whole blank run, then decide once.
        let mut j = i + 1;
        while j < lines.len()
            && !compact_line_is_boundary(&lines[j], theme)
            && lines[j].to_plain_text().trim().is_empty()
        {
            j += 1;
        }
        let keep = !out.is_empty()
            && lines
                .get(j)
                .is_none_or(|next| prev_boundary || compact_line_is_boundary(next, theme));
        if keep {
            out.push(ftui::text::Line::new());
        }
        i = j;
    }
    out
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
    /// Render-cache key (issue #201): globally unique, monotonically
    /// assigned at push and re-assigned on EVERY in-place mutation (card
    /// state flips, head replacement, detail folds, read grouping). A
    /// cached block is reusable iff its recorded revision still matches —
    /// uniqueness makes index shifts from entry removal self-invalidating.
    revision: u64,
    /// Set for tool-execution cards; `None` renders as a plain role block.
    /// Pairing key: the sanitized tool_id that ties ToolStart/ToolEnd/
    /// ToolInvocation events to this card (stable even when the head text
    /// is later replaced by an invocation summary).
    card: Option<CardState>,
    pair_key: Option<String>,
    /// Folded result preview for tool cards (sanitized, size-capped).
    detail: Option<String>,
    /// Detail lines are diff content (edit/hashline_edit): style added and
    /// removed markers.
    diff_styled: bool,
    /// Sanitized tool name (semantic identity: fold-by-tool, diff-styling
    /// decisions); independent of the displayed head text.
    tool_name: Option<String>,
    /// Grouped consecutive successful runs (read-tool-group parity):
    /// 1 = standalone.
    group_count: u32,
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
    /// Inspect or change this session's MCP server state.
    Mcp {
        subcommand: String,
        name: Option<String>,
    },
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
    /// Grant access to an additional workspace root
    /// (`/add-dir <dir>`, bd-cv653.3.12).
    AddDir { dir: String },
    /// Revoke an additional workspace root (`/remove-dir <dir>`).
    RemoveDir { dir: String },
    /// Crash bundle management (`/crash list|show|delete`, bd-cv653.7.12).
    Crash { action: String },
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
    /// Session identity represented by the transcript. Owner-tagged async
    /// notes are accepted only when they match this reset-installed value.
    displayed_session_id: Option<String>,
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
    /// User draft captured when the first response-bearing card takes over the
    /// editor. Successor cards share the snapshot; the last terminal path
    /// restores it only after clearing card-owned input.
    card_draft_snapshot: Option<String>,
    /// Where completed extension UI replies go (driver pairs them back to the
    /// pending request via `FtuiExtensionUiHandler::resolve`).
    ext_reply_tx: Option<Sender<ExtensionUiResponse>>,
    /// Where completed ask interactions go (launch path calls respond_ui).
    ask_reply_tx: Option<Sender<AskUiReply>>,
    /// Abort handle for the driver's in-flight prompt turn (issue #205).
    /// `run_prompt_turn` installs a fresh handle per turn; Ctrl-C fires it so
    /// exit doesn't block on the provider stream and remaining tool calls.
    turn_abort: Option<TurnAbortSlot>,
    /// Terminal size, tracked from `Event::Resize` (cols, rows).
    term: (u16, u16),
    /// Conversation scroll, measured in lines UP from the tail. 0 means
    /// follow-the-stream (stick to bottom as new content arrives) — the same
    /// semantics as `follow_stream_tail` in the bubbletea stack, but derived
    /// instead of stored so update() never needs the rendered line count.
    scroll_from_tail: usize,
    /// Total rendered conversation lines from the last frame. Markdown
    /// rendering expands the raw text (blank lines after blocks, fence
    /// chrome), so the raw-line approximation in `conversation_line_count()`
    /// badly undercounts the real scroll range — clamping against it made
    /// PageUp stall partway up and made any resize collapse the scroll
    /// position (issue #206). The view records the authoritative total here
    /// each frame (a `Cell` because `view()` takes `&self`) and
    /// `max_scroll_from_tail()` prefers it. Visible rows are still derived
    /// from the live terminal size so a resize re-clamps against fresh
    /// geometry rather than the pre-resize frame.
    rendered_total_lines: std::cell::Cell<usize>,
    /// The input editor (ftui-widgets TextArea replaces bubbles TextArea).
    input: TextArea,
    /// Slash-command completion popup (issue #208). Shares the dropdown
    /// state machine and the [`crate::autocomplete`] provider with the
    /// charmed stack, so both surfaces complete from the same command list.
    autocomplete: AutocompleteState,
    /// Where submitted user input goes. The launch path hands the sending
    /// half of the channel its agent loop consumes; tests read the receiver
    /// directly. `None` falls back to echoing into the transcript only.
    submit_tx: Option<Sender<UiCommand>>,
    /// Shared slot for the agent-event receiver: `subscriptions()` re-declares
    /// the bridge each cycle, and the one instance the runtime actually starts
    /// takes the receiver out of this slot (see [`AgentEventSubscription`]).
    agent_rx: Arc<Mutex<Option<Receiver<PiMsg>>>>,
    /// Whether the program owns the alternate screen (fullscreen launch).
    /// The suspend path mirrors only the features actually enabled.
    alt_screen: bool,
    /// Set while a ctrl+z suspension is in flight: freezes spinner ticks so
    /// the pre-stop frames stay byte-identical (the diff engine then emits
    /// nothing into the restored cooked terminal). Cleared by
    /// [`PiFtuiMsg::Resumed`].
    suspending: bool,
    /// Test seam replacing the real SIGTSTP task (which would stop or fail
    /// on a headless test host). `None` in production.
    #[cfg(test)]
    suspend_task_override: Option<Box<dyn FnOnce() -> PiFtuiMsg + Send>>,
    /// Loop-lag probe with render/input/agent-event attribution. Inert unless
    /// `PI_PERF_TELEMETRY=1`.
    watchdog: LoopWatchdog,
    /// Monotonic source for [`TranscriptEntry::revision`] values (issue
    /// #201). Every push and every in-place entry mutation takes the next
    /// value, so a revision seen once in the render cache can never label
    /// different content.
    transcript_revision: u64,
    /// Per-entry rendered-line cache, index-aligned with `transcript`
    /// (issue #201): reuses a block's styled lines while its revision
    /// matches, so a frame re-renders only changed entries plus the
    /// in-flight streaming tail instead of the whole transcript. Interior
    /// mutability because `view()` builds frames through `&self`. Cleared
    /// whenever the styling inputs change (theme picker) — the markdown
    /// renderer itself is width-independent, so resizes need no flush.
    render_cache: std::cell::RefCell<Vec<Option<CachedBlock>>>,
    /// `(rendered, reused)` block counts from the most recent
    /// `conversation_text()` pass — the observable that keeps the cache
    /// honest in tests (O(changed) per frame, not O(transcript)).
    render_stats: std::cell::Cell<(usize, usize)>,
    /// Busy state for a long out-of-turn driver operation (issue #203):
    /// session load/new, model switch, compaction, extension commands.
    /// While set, the status region animates the shared spinner with the
    /// operation's label even though no agent turn is running; any driver
    /// reply clears it (the driver is sequential, so the next non-tick
    /// message belongs to the in-flight operation).
    busy: Option<BusyOp>,
    /// Transcript markdown spacing policy (issue #202), resolved from
    /// `markdown.spacing` in settings at launch.
    markdown_spacing: crate::config::MarkdownSpacing,
}

/// One cached transcript block (issue #201): the styled lines produced for
/// the entry whose revision is recorded here.
#[derive(Debug)]
struct CachedBlock {
    revision: u64,
    lines: Vec<ftui::text::Line<'static>>,
}

/// A long out-of-turn driver operation the status region is animating
/// (issue #203). `tick_pending` is set by [`PiFtuiModel::begin_busy`] and
/// consumed by the key handler that routed the input — `update()` owns Cmd
/// returns, the routing helpers don't.
#[derive(Debug)]
struct BusyOp {
    label: String,
    tick_pending: bool,
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
    /// Slash-command completion popup, directly above the editor (issue
    /// #208); zero rows while no suggestions are showing.
    completion: Rect,
    input: Rect,
    footer: Rect,
}

/// Launch-time inputs for the completion popup (issue #208).
#[derive(Debug, Clone, Default)]
pub struct AutocompleteLaunch {
    /// Prompt templates, skills, and the skill-command toggle from the
    /// resource loader; extension commands are filled in by the driver.
    pub catalog: AutocompleteCatalog,
    /// Working directory for the provider (path/@-file resolution).
    pub cwd: std::path::PathBuf,
    /// Maximum suggestion rows shown at once (`autocompleteMaxVisible`).
    pub max_visible: usize,
}

/// Default popup height when settings don't override it (matches the
/// charmed stack's `autocompleteMaxVisible` default).
const DEFAULT_COMPLETION_ROWS: usize = 5;

/// Keyboard hint rendered under the suggestion rows.
const COMPLETION_HINT: &str = "↑↓ move · Tab/Enter accept · Esc dismiss";

/// Rows of single-line chrome around the conversation body: header, status,
/// footer. The input region's height is dynamic (see
/// [`PiFtuiModel::input_rows`]), so total chrome = this + input rows.
const FIXED_CHROME_ROWS: u16 = 3;

/// The input editor grows with its content up to this many rows.
const MAX_INPUT_ROWS: u16 = 5;

fn layout_regions(area: Rect, input_rows: u16, banner_rows: u16, completion_rows: u16) -> Regions {
    use ftui::layout::{Constraint, Flex};
    let rects = Flex::vertical()
        .constraints([
            Constraint::Fixed(1),               // header
            Constraint::Fill,                   // conversation body
            Constraint::Fixed(banner_rows),     // pinned error banner (0 = none)
            Constraint::Fixed(1),               // status line (tool/todo/messages)
            Constraint::Fixed(completion_rows), // completion popup (0 = closed)
            Constraint::Fixed(input_rows),      // input editor
            Constraint::Fixed(1),               // footer (usage)
        ])
        .split(area);
    Regions {
        header: rects[0],
        body: rects[1],
        banner: rects[2],
        status: rects[3],
        completion: rects[4],
        input: rects[5],
        footer: rects[6],
    }
}

impl PiFtuiModel {
    pub fn new(agent_rx: Receiver<PiMsg>) -> Self {
        Self {
            state: AgentUiState::Ready,
            transcript: Vec::new(),
            displayed_session_id: None,
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
            card_draft_snapshot: None,
            ext_reply_tx: None,
            ask_reply_tx: None,
            turn_abort: None,
            term: (80, 24),
            scroll_from_tail: 0,
            rendered_total_lines: std::cell::Cell::new(0),
            agent_rx: Arc::new(Mutex::new(Some(agent_rx))),

            alt_screen: false,
            suspending: false,
            watchdog: LoopWatchdog::new(),
            transcript_revision: 0,
            render_cache: std::cell::RefCell::new(Vec::new()),
            render_stats: std::cell::Cell::new((0, 0)),
            busy: None,
            markdown_spacing: crate::config::MarkdownSpacing::Comfortable,
            #[cfg(test)]
            suspend_task_override: None,
            input: TextArea::new()
                .with_placeholder("Type a message (Enter to send, Alt+Enter for newline)")
                .with_focus(true)
                .with_soft_wrap(true),
            autocomplete: {
                let mut state = AutocompleteState::new(
                    std::path::PathBuf::from("."),
                    AutocompleteCatalog::default(),
                );
                state.max_visible = DEFAULT_COMPLETION_ROWS;
                state
            },
            submit_tx: None,
        }
    }

    /// Install the launch-time completion catalog (prompt templates, skills)
    /// plus the working directory and popup height (issue #208). Extension
    /// commands arrive later via [`PiMsg::AutocompleteCatalog`] once the
    /// driver's session exists.
    #[must_use]
    pub fn with_autocomplete(mut self, launch: AutocompleteLaunch) -> Self {
        self.autocomplete.provider.set_cwd(launch.cwd);
        self.autocomplete.provider.set_catalog(launch.catalog);
        self.autocomplete.max_visible = launch.max_visible.clamp(1, 20);
        self.autocomplete.close();
        self
    }

    /// Route submitted input to the agent loop via this channel. The launch
    /// path calls this before starting the program.
    #[must_use]
    pub fn with_submit_channel(mut self, tx: Sender<UiCommand>) -> Self {
        self.submit_tx = Some(tx);
        self
    }

    /// Set the transcript markdown spacing policy (issue #202).
    #[must_use]
    pub const fn with_markdown_spacing(mut self, spacing: crate::config::MarkdownSpacing) -> Self {
        self.markdown_spacing = spacing;
        self
    }

    /// Share the driver's in-flight-turn abort slot so Ctrl-C can cancel the
    /// running prompt instead of waiting out the full turn (issue #205).
    #[must_use]
    pub fn with_turn_abort(mut self, slot: TurnAbortSlot) -> Self {
        self.turn_abort = Some(slot);
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

    /// Record whether the program runs fullscreen (alternate screen). The
    /// suspend path mirrors only the features actually enabled; the launch
    /// path calls this with `!inline`.
    #[must_use]
    pub const fn with_alt_screen(mut self, alt_screen: bool) -> Self {
        self.alt_screen = alt_screen;
        self
    }

    /// Swap in a fake suspend task (tests only): the simulator executes
    /// `Cmd::Task` closures synchronously, so the real SIGTSTP closure would
    /// touch termios (and stop the process) inside a unit test.
    #[cfg(test)]
    #[must_use]
    pub fn with_suspend_task(mut self, task: impl FnOnce() -> PiFtuiMsg + Send + 'static) -> Self {
        self.suspend_task_override = Some(Box::new(task));
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
        usize::from(self.term.1.saturating_sub(
            FIXED_CHROME_ROWS + banner + self.input_rows() + self.completion_rows(),
        ))
        .max(1)
    }

    /// Whether the editor is in plain prompt-composition mode: idle agent,
    /// no card or picker owning the keys. Only then may completion open.
    fn completion_allowed(&self) -> bool {
        self.state == AgentUiState::Ready
            && self.active_ask.is_none()
            && self.active_ext.is_none()
            && self.picker.is_none()
    }

    /// Whether the completion popup should capture keys and take rows.
    fn completion_visible(&self) -> bool {
        self.autocomplete.open && !self.autocomplete.items.is_empty() && self.completion_allowed()
    }

    /// Rows the popup reserves above the editor: one per visible suggestion
    /// (capped at `max_visible`) plus the keyboard hint line. The popup
    /// never takes more than a third of the terminal so a short or inline
    /// viewport keeps its conversation rows.
    fn completion_rows(&self) -> u16 {
        if !self.completion_visible() {
            return 0;
        }
        let budget = usize::from(self.term.1 / 3).max(2);
        let items = self
            .autocomplete
            .items
            .len()
            .min(self.autocomplete.max_visible)
            .min(budget - 1);
        u16::try_from(items + 1).unwrap_or(u16::MAX)
    }

    /// Recompute the completion popup from the editor contents (issue #208).
    /// Runs after every editor mutation; the popup only ever opens for a
    /// slash-command draft so ordinary prose never grows a dropdown.
    fn maybe_trigger_autocomplete(&mut self) {
        if !self.completion_allowed() {
            self.autocomplete.close();
            return;
        }
        let text = self.input.text();
        if !text.trim_start().starts_with('/') {
            self.autocomplete.close();
            return;
        }
        let editor = self.input.editor();
        let cursor = ftui::text::CursorNavigator::new(editor.rope()).to_byte_index(editor.cursor());
        let response = self.autocomplete.provider.suggest(&text, cursor);
        // Bare filesystem-path matches are Tab-triggered in the charmed
        // stack; the popup here is for commands and their arguments.
        if response
            .items
            .iter()
            .all(|item| item.kind == AutocompleteItemKind::Path)
        {
            self.autocomplete.close();
            return;
        }
        self.autocomplete.open_with(response);
    }

    /// Splice the accepted suggestion over the token it completes and park
    /// the cursor at the end of the draft.
    fn accept_autocomplete(&mut self, item: &AutocompleteItem) {
        let text = self.input.text();
        let range = &self.autocomplete.replace_range;
        // The range was computed against the text at trigger time; clamp to
        // char boundaries in case the editor moved on since.
        let mut start = range.start.min(text.len());
        while start > 0 && !text.is_char_boundary(start) {
            start -= 1;
        }
        let mut end = range.end.min(text.len()).max(start);
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        let mut next = String::with_capacity(text.len() + item.insert.len());
        next.push_str(&text[..start]);
        next.push_str(&item.insert);
        next.push_str(&text[end..]);
        self.input.set_text(&next);
        self.input.move_to_document_end();
    }

    /// Keys the open popup owns (issue #208). Returns `true` when the key was
    /// consumed; Enter without a highlighted row closes the popup and falls
    /// through so the draft submits exactly as typed, matching the charmed
    /// stack (Tab always accepts, defaulting to the first row).
    fn handle_completion_key(&mut self, key: &ftui::KeyEvent, submit: bool) -> bool {
        match key.code {
            KeyCode::Up => {
                self.autocomplete.select_prev();
                true
            }
            KeyCode::Down => {
                self.autocomplete.select_next();
                true
            }
            KeyCode::Tab => {
                if self.autocomplete.selected.is_none() {
                    self.autocomplete.select_next();
                }
                if let Some(item) = self.autocomplete.selected_item().cloned() {
                    self.accept_autocomplete(&item);
                }
                self.autocomplete.close();
                true
            }
            KeyCode::Escape => {
                self.autocomplete.close();
                true
            }
            _ if submit => {
                if let Some(item) = self.autocomplete.selected_item().cloned() {
                    self.accept_autocomplete(&item);
                    self.autocomplete.close();
                    return true;
                }
                self.autocomplete.close();
                false
            }
            _ => false,
        }
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

    /// Next globally-unique transcript revision (issue #201). Taken at push
    /// and at every in-place entry mutation so the render cache can trust a
    /// matching revision completely.
    const fn next_revision(&mut self) -> u64 {
        self.transcript_revision += 1;
        self.transcript_revision
    }

    fn push_entry(&mut self, role: EntryRole, text: String) {
        let revision = self.next_revision();
        self.transcript.push(TranscriptEntry {
            role,
            text,
            revision,
            card: None,
            pair_key: None,
            detail: None,
            diff_styled: false,
            tool_name: None,
            group_count: 1,
        });
    }

    /// Push a pending tool-execution card keyed by the sanitized tool_id
    /// (stable across head-text replacement by invocation summaries);
    /// `display` is the sanitized initial head (the tool name).
    fn push_tool_card(&mut self, pair_id: &str, display: &str, sanitized_name: &str) {
        let revision = self.next_revision();
        self.transcript.push(TranscriptEntry {
            role: EntryRole::System,
            text: display.to_string(),
            revision,
            card: Some(CardState::Pending),
            pair_key: Some(pair_id.to_string()),
            detail: None,
            diff_styled: false,
            tool_name: Some(sanitized_name.to_string()),
            group_count: 1,
        });
    }
    /// Close the last pending tool card named `sanitized_name`, falling
    /// back to a plain trace line when no matching open card exists.
    /// A turn that ends (or dies) between ToolStart and ToolEnd leaves a
    /// pending card whose spinner freezes once ticks stop. Settle any
    /// leftover pending cards as errors so the transcript never shows a
    /// tool that is "still running" after the turn is over.
    fn settle_pending_cards(&mut self) {
        let mut revision = self.transcript_revision;
        for entry in &mut self.transcript {
            if entry.card == Some(CardState::Pending) {
                entry.card = Some(CardState::Err);
                revision += 1;
                entry.revision = revision;
            }
        }
        self.transcript_revision = revision;
    }

    fn finish_tool_card(
        &mut self,
        sanitized_pair_id: &str,
        display_name: &str,
        ok: bool,
        sanitized_output: Option<String>,
        diff_styled: bool,
    ) {
        let pending_idx = self.transcript.iter().rposition(|e| {
            e.card == Some(CardState::Pending) && e.pair_key.as_deref() == Some(sanitized_pair_id)
        });
        let Some(idx) = pending_idx else {
            let mark = if ok { "✓" } else { "✗" };
            self.push_entry(EntryRole::System, format!("{mark} {display_name}"));
            return;
        };
        let revision = self.next_revision();
        self.transcript[idx].card = Some(if ok { CardState::Ok } else { CardState::Err });
        self.transcript[idx].revision = revision;
        if let Some(output) = sanitized_output {
            self.transcript[idx].detail = Some(output);
            self.transcript[idx].diff_styled = diff_styled;
        }
        // Read-call grouping (bd-cv653.9.2, read-tool-group parity): a
        // successful read DIRECTLY following another successful read card
        // collapses into it with a ×N counter — agent turns batch many
        // reads, and one line per file drowns the transcript.
        // Group on tool_name, not the displayed head: ToolInvocation
        // replaces the head with the per-file summary (every read has a
        // path), so text-based matching never fired in production.
        if ok && display_name == "read" && idx > 0 {
            let prev = &self.transcript[idx - 1];
            if prev.card == Some(CardState::Ok) && prev.tool_name.as_deref() == Some("read") {
                let revision = self.next_revision();
                self.transcript[idx - 1].group_count += 1;
                // A grouped card can't show one file's summary as its head:
                // render the generic name ("read ×N") once merging starts.
                self.transcript[idx - 1].text = "read".to_string();
                self.transcript[idx - 1].revision = revision;
                self.transcript.remove(idx);
            }
        }
    }

    /// Fold a bash result preview into the still-pending bash card
    /// (driver emits BashResult between ToolStart and ToolEnd). Caps the
    /// preview at 8 lines with an elision counter. Returns false when no
    /// open bash card exists (caller falls back to a plain block).
    fn fold_bash_detail(&mut self, sanitized_display: &str) -> bool {
        const MAX_DETAIL_LINES: usize = 8;
        let revision = self.next_revision();
        let Some(entry) =
            self.transcript.iter_mut().rev().find(|e| {
                e.card == Some(CardState::Pending) && e.tool_name.as_deref() == Some("bash")
            })
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
            let _ = write!(collected, "\n… +{} more lines", total - MAX_DETAIL_LINES);
        }
        entry.detail = Some(collected);
        entry.revision = revision;
        true
    }

    /// Cap for `scroll_from_tail`: can't scroll further up than the content.
    ///
    /// Uses the rendered total recorded by the last `view()` frame — the
    /// raw-line approximation only fills in before the first frame renders.
    /// The recorded total is at most one frame stale (every scroll/resize
    /// event triggers a redraw), and the view re-clamps against the exact
    /// total when drawing.
    fn max_scroll_from_tail(&self) -> usize {
        let total = match self.rendered_total_lines.get() {
            0 => self.conversation_line_count(),
            rendered => rendered,
        };
        total.saturating_sub(self.body_height())
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

    #[allow(clippy::too_many_lines)]
    fn handle_agent(&mut self, msg: PiMsg) -> Cmd<PiFtuiMsg> {
        // A busy out-of-turn operation (issue #203) is over once the
        // sequential driver replies with anything of substance. Background
        // ticks don't count, and neither does the in-progress chatter of an
        // extension command's own tool card (its ToolEnd/System reply is
        // what settles it).
        if self.busy.is_some()
            && !matches!(
                msg,
                PiMsg::AutocompleteRefresh
                    | PiMsg::AutocompleteCatalog(_)
                    | PiMsg::ToolStart { .. }
                    | PiMsg::ToolInvocation { .. }
                    | PiMsg::ToolUpdate { .. }
            )
        {
            self.busy = None;
        }
        match msg {
            PiMsg::AgentStart => {
                self.state = AgentUiState::Working;
                self.autocomplete.close();
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
            PiMsg::ToolStart { name, tool_id, .. } => {
                let name = sanitize(&name).into_owned();
                // The card pairs on the sanitized tool_id; the head starts
                // as the tool name and is later replaced by the invocation
                // summary when one arrives.
                let pair = sanitize(&tool_id).into_owned();
                self.current_tool = Some(name.clone());
                self.push_tool_card(&pair, &name, &name);
            }
            PiMsg::ToolInvocation { tool_id, summary } => {
                // The invocation summary REPLACES the card head (omp
                // renderCall description): pairing by tool_id is immune to
                // the text change.
                let pair = sanitize(&tool_id).into_owned();
                let summary = sanitize(&summary).into_owned();
                let revision = self.next_revision();
                if let Some(entry) = self.transcript.iter_mut().rev().find(|e| {
                    e.card == Some(CardState::Pending)
                        && e.pair_key.as_deref() == Some(pair.as_str())
                }) {
                    entry.text = summary;
                    entry.revision = revision;
                }
            }
            PiMsg::ToolEnd {
                name,
                tool_id,
                is_error,
                output,
                ..
            } => {
                // The tool card flips to its terminal state in place
                // (bd-cv653.9.2 card framework). Sanitize ONCE per field,
                // matching ToolStart, so start/end always pair.
                let name = sanitize(&name).into_owned();
                let pair = sanitize(&tool_id).into_owned();
                let output = output.map(|o| sanitize(&o).into_owned());
                let diff_styled = matches!(name.as_str(), "edit" | "hashline_edit");
                self.finish_tool_card(&pair, &name, !is_error, output, diff_styled);
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
                self.dismiss_pending_interactions();
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
                self.settle_pending_cards();
            }
            PiMsg::AgentError(err) => {
                self.dismiss_pending_interactions();
                // Pinned above the editor (bd-cv653.9.2), dismiss-on-send —
                // not duplicated into the transcript. Partial streamed text
                // is still flushed so it isn't merged into the next turn.
                if !self.streaming.is_empty() {
                    let text = std::mem::take(&mut self.streaming);
                    self.push_entry(EntryRole::Assistant, text);
                }
                self.error_banner = Some(sanitize(&err).into_owned());
                self.state = AgentUiState::Ready;
                self.current_tool = None;
                self.thinking.clear();
                self.settle_pending_cards();
            }
            PiMsg::System(text) | PiMsg::SystemNote(text) => {
                let text = sanitize(&text).into_owned();
                self.push_entry(EntryRole::System, text);
            }
            PiMsg::SessionSystemNote {
                owner_session_id,
                message,
            } => {
                if self.displayed_session_id.as_deref() == Some(owner_session_id.as_str()) {
                    let text = sanitize(&message).into_owned();
                    self.push_entry(EntryRole::System, text);
                }
            }
            PiMsg::ConversationReset {
                session_id,
                messages,
                status,
                ..
            }
            | PiMsg::RetryCommitted {
                session_id,
                messages,
                status,
                ..
            } => {
                self.dismiss_pending_interactions();
                self.displayed_session_id = Some(session_id);
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
                } else if self.active_ask.is_some() || self.active_ext.is_some() {
                    // The model-side scheduling barrier should serialize Ask,
                    // but reject overlap defensively rather than overwriting an
                    // already reachable modal and stranding its waiter.
                    self.send_ask_reply(request.id, Vec::new(), true);
                } else {
                    self.autocomplete.close();
                    self.capture_preexisting_card_draft();
                    self.push_ask_card(&request, 0);
                    self.active_ask = Some(ActiveAsk {
                        request,
                        question_index: 0,
                        answers: Vec::new(),
                    });
                }
            }
            PiMsg::ExtensionUiRequest(request) => {
                if !request.expects_response() {
                    let text =
                        sanitize(format_extension_ui_prompt(&request).trim_end()).into_owned();
                    self.push_entry(EntryRole::System, text);
                } else if self.active_ext.is_none() && self.active_ask.is_none() {
                    self.activate_ext_request(request);
                } else {
                    self.ext_queue.push_back(request);
                }
            }
            PiMsg::UiShutdown => return Cmd::quit(),
            PiMsg::AutocompleteCatalog(catalog) => {
                // Issue #208: extension commands join the popup's command
                // list once the driver's session (and its extension
                // runtime) exists. Any open popup was computed against the
                // old list; drop it, the next keystroke recomputes.
                self.autocomplete.provider.set_catalog(catalog);
                self.autocomplete.close();
            }
            PiMsg::TerminalTitle(title) => {
                // Issue #200: the cell-grid renderer can't carry OSC escapes
                // in frame content, so write the title directly. This runs on
                // the UI thread — the same thread that owns renderer writes —
                // so the sequence cannot interleave with a frame.
                use std::io::Write as _;

                let sequence = crate::delight::format_terminal_title(&title);
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(sequence.as_bytes());
                let _ = out.flush();
            }
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
        self.autocomplete.close();
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
                self.begin_busy(format!("switching model to {spec} ..."));
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
    #[allow(clippy::too_many_lines)]
    fn route_slash_command_tail(&mut self, clean: &str) -> bool {
        // Case-insensitive tokens (SlashCommand::parse parity): compare on
        // an ASCII-lowercased copy; args keep their original case.
        let canon = clean.to_ascii_lowercase();
        if canon == "/exit" || canon == "/quit" || canon == "/q" {
            self.pending_quit = true;
            return true;
        }
        if let Some(rest) = strip_command(clean, "/add-dir") {
            self.push_entry(
                EntryRole::System,
                format!("adding workspace root {} ...", rest.trim()),
            );
            self.send_command(UiCommand::AddDir {
                dir: rest.trim().to_string(),
            });
            return true;
        }
        if let Some(rest) = strip_command(clean, "/remove-dir") {
            self.push_entry(
                EntryRole::System,
                format!("removing workspace root {} ...", rest.trim()),
            );
            self.send_command(UiCommand::RemoveDir {
                dir: rest.trim().to_string(),
            });
            return true;
        }
        if let Some(rest) = strip_command(clean, "/crash") {
            self.send_command(UiCommand::Crash {
                action: rest.trim().to_ascii_lowercase(),
            });
            return true;
        }
        let canon = clean.to_ascii_lowercase();
        if canon == "/compact" {
            self.push_entry(
                EntryRole::System,
                String::from("compacting conversation ..."),
            );
            self.begin_busy("compacting conversation ...");
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
            self.begin_busy("fetching provider usage ...");
            self.send_command(UiCommand::Usage { refresh });
            return true;
        }
        if let Some(rest) = strip_command(clean, "/mcp") {
            let mut parts = rest.split_whitespace();
            let subcommand = parts.next().unwrap_or("list").to_ascii_lowercase();
            let name = parts.next().map(str::to_string);
            let valid = match subcommand.as_str() {
                "list" => name.is_none() && parts.next().is_none(),
                "trust" | "deny" | "test" => name.is_some() && parts.next().is_none(),
                _ => false,
            };
            if valid {
                self.send_command(UiCommand::Mcp { subcommand, name });
            } else {
                self.push_entry(
                    EntryRole::Error,
                    String::from("usage: /mcp [list|trust <name>|deny <name>|test <name>]"),
                );
            }
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
                     /theme, /new, /clear, /session, /tree, /mcp, /thinking [level], \
                     /name <name>, /exit, /help, !<cmd> (runs + sends output to the \
                     agent), !!<cmd> (display-only)",
                ),
            );
            return true;
        }
        let (cmd_name, cmd_args) = clean.split_once(char::is_whitespace).unwrap_or((clean, ""));
        match cmd_name.to_ascii_lowercase().as_str() {
            "/new" => {
                self.begin_busy("starting new session ...");
                self.send_command(UiCommand::NewSession);
                return true;
            }
            "/clear" | "/cls" => {
                // Display-only clear (SlashCommand::Clear parity): the
                // session file and its history stay untouched. Unreachable
                // mid-turn — the editor gate (`input_active`) already blocks
                // input while the agent works.
                self.transcript.clear();
                self.render_cache.borrow_mut().clear();
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
                self.begin_busy(format!("running /{name} ..."));
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
                // Role styling is palette-derived: drop every cached block
                // so the whole transcript re-renders under the new theme.
                self.render_cache.borrow_mut().clear();
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
                    self.begin_busy(format!("switching model to {choice} ..."));
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
                self.begin_busy("loading session ...");
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

    /// Arm the busy indicator for a long out-of-turn driver operation
    /// (issue #203): the status region animates the shared spinner with
    /// `label` until the driver replies. Call right before/after sending
    /// the matching [`UiCommand`]; the key handler that routed the input
    /// turns the pending flag into the spinner tick chain.
    fn begin_busy(&mut self, label: impl Into<String>) {
        self.busy = Some(BusyOp {
            label: label.into(),
            tick_pending: true,
        });
    }

    /// The active busy operation's status label, if any.
    fn busy_label(&self) -> Option<&str> {
        self.busy.as_ref().map(|op| op.label.as_str())
    }

    /// Convert a freshly armed busy operation into the tick command that
    /// starts the spinner chain (`Cmd::none()` when nothing was armed).
    /// Split from [`Self::begin_busy`] because the routing helpers return
    /// `bool`/`()` — only `update()`'s key paths own Cmd returns.
    fn take_busy_tick(&mut self) -> Cmd<PiFtuiMsg> {
        if let Some(op) = &mut self.busy
            && std::mem::take(&mut op.tick_pending)
        {
            Cmd::tick(SPINNER_INTERVAL)
        } else {
            Cmd::none()
        }
    }

    /// Whether any tool card is still pending — those animate with the
    /// shared spinner frame, so ticks must keep flowing for them even when
    /// no turn is running (extension commands render a card out-of-turn).
    fn has_pending_cards(&self) -> bool {
        self.transcript
            .iter()
            .rev()
            .any(|entry| entry.card == Some(CardState::Pending))
    }

    #[allow(clippy::too_many_lines)]
    fn handle_term(&mut self, event: &Event) -> Cmd<PiFtuiMsg> {
        match event {
            Event::Tick => {
                // While a ctrl+z stop is in flight the model must not
                // change: byte-identical frames keep the diff engine silent
                // in the window between terminal restore and SIGTSTP.
                if self.suspending {
                    return Cmd::none();
                }
                // Spinner heartbeat: advance and reschedule only while
                // something animated needs it — a working turn, an
                // out-of-turn busy operation (issue #203), or a pending
                // tool card — so idle sessions stay fully parked.
                if self.state == AgentUiState::Working
                    || self.busy.is_some()
                    || self.has_pending_cards()
                {
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
                    // Issue #205: cancel the in-flight turn before quitting;
                    // otherwise teardown blocks on the driver joining a full
                    // provider stream plus remaining tool calls.
                    if let Some(slot) = &self.turn_abort
                        && let Some(handle) = slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take()
                    {
                        handle.abort();
                    }
                    return Cmd::quit();
                }

                // Modal picker captures all input while open (same precedence
                // as the bubbletea modal-capture chain).
                if self.picker.is_some() {
                    self.handle_picker_key(key);
                    // A picker selection may have armed a busy operation
                    // (model switch, session load); start its spinner chain.
                    return self.take_busy_tick();
                }

                // Resolve through the shared keybinding catalog so user
                // config behaves identically on both stacks. Chords can bind
                // several context-dependent actions (ctrl+d = delete-forward
                // in a non-empty editor, exit otherwise), so resolve the set
                // against UI state.
                let actions = KeyBinding::from_ftui_key(key)
                    .map(|binding| self.keybindings.matching_actions(&binding))
                    .unwrap_or_default();
                // The completion popup owns navigation/accept/dismiss keys
                // while it shows (issue #208); anything it doesn't consume
                // continues through the catalog and the editor as usual.
                if self.completion_visible()
                    && self.handle_completion_key(key, actions.contains(&AppAction::Submit))
                {
                    return Cmd::none();
                }
                let pick = |wanted: AppAction| actions.contains(&wanted).then_some(wanted);
                // Suspend wins over everything (vim semantics): ctrl+z is
                // unambiguous, and backgrounding must work mid-edit too.
                let action = pick(AppAction::Suspend)
                    .or_else(|| pick(AppAction::PageUp))
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
                    Some(AppAction::Suspend) => {
                        // Freeze model mutations synchronously (the flag is
                        // what keeps pre-stop frames byte-identical), then
                        // hand the terminal dance to a task: it restores
                        // cooked mode, stops on SIGTSTP, re-acquires the
                        // terminal after SIGCONT, and reports back.
                        self.suspending = true;
                        #[cfg(unix)]
                        {
                            #[cfg(test)]
                            let task = self
                                .suspend_task_override
                                .take()
                                .unwrap_or_else(|| Box::new(suspend_task(self.alt_screen)));
                            #[cfg(not(test))]
                            let task = suspend_task(self.alt_screen);
                            return Cmd::task(task);
                        }
                        #[cfg(not(unix))]
                        {
                            return Cmd::none();
                        }
                    }
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
                            self.input.set_text("");
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
                        // A routed slash command may have armed a busy
                        // operation (issue #203); start its spinner chain.
                        return self.take_busy_tick();
                    }
                    Some(AppAction::NewLine) if self.input_active() => {
                        self.input.insert_newline();
                        self.maybe_trigger_autocomplete();
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
                if self.input_active() && self.input.handle_event(event) {
                    // Unrouted keys reach the editor (its own emacs-style
                    // bindings cover cursor/delete/kill-ring behavior); every
                    // edit or cursor move re-derives the completion popup.
                    self.maybe_trigger_autocomplete();
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_up(3),
                MouseEventKind::ScrollDown => self.scroll_down(3),
                _ => {}
            },
            Event::Resize { width, height } => {
                // Rendered markdown depends on the width only through the
                // table budget; drop cached blocks when it changes so tables
                // re-fit (gh #195). Height-only resizes keep the cache.
                if *width != self.term.0 {
                    self.render_cache.borrow_mut().clear();
                }
                self.term = (*width, *height);
                // The suspend task reports back through a Resize; the flag
                // unfreezes tick-driven model changes from here on.
                self.suspending = false;
                // Re-clamp: a taller window may make the old offset overshoot.
                self.scroll_from_tail = self.scroll_from_tail.min(self.max_scroll_from_tail());
            }
            _ => {
                if self.input_active() && self.input.handle_event(event) {
                    // Paste and other editor-relevant events flow through.
                    self.maybe_trigger_autocomplete();
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

    /// Fail closed every modal owned by the completed/replaced turn. Replies
    /// are emitted before state is cleared so no Ask or extension waiter can
    /// survive invisibly and consume ordinary editor input later.
    // The flag accumulates across three sequential dismissal steps with side
    // effects; collapsing them into one expression would obscure that order.
    #[allow(clippy::useless_let_if_seq)]
    fn dismiss_pending_interactions(&mut self) -> bool {
        let mut dismissed = if let Some(ask) = self.active_ask.take() {
            self.send_ask_reply(ask.request.id, Vec::new(), true);
            true
        } else {
            false
        };
        if let Some(request) = self.active_ext.take() {
            self.send_ext_reply(ExtensionUiResponse {
                id: request.id,
                value: None,
                cancelled: true,
            });
            dismissed = true;
        }
        let queued = std::mem::take(&mut self.ext_queue);
        if !queued.is_empty() {
            dismissed = true;
        }
        for request in queued {
            self.send_ext_reply(ExtensionUiResponse {
                id: request.id,
                value: None,
                cancelled: true,
            });
        }
        if dismissed {
            self.input.set_text("");
        }
        self.restore_card_draft_after_cards_settle();
        dismissed
    }

    /// Snapshot the pre-card editor exactly once per contiguous card burst.
    /// Card answers own the editor until the burst settles, so the draft is
    /// cleared before the first card becomes reachable.
    fn capture_preexisting_card_draft(&mut self) {
        let draft = self.input.text();
        if self.card_draft_snapshot.is_none() && !draft.is_empty() {
            self.card_draft_snapshot = Some(draft);
            self.input.set_text("");
        }
    }

    /// Explicit merge policy: after the last response-bearing card settles,
    /// restore the original draft only into an empty editor.
    fn restore_card_draft_after_cards_settle(&mut self) {
        if self.active_ask.is_some() || self.active_ext.is_some() || !self.ext_queue.is_empty() {
            return;
        }
        if self.input.text().is_empty()
            && let Some(draft) = self.card_draft_snapshot.take()
        {
            self.input.set_text(&draft);
        }
    }

    /// Rebuild the transcript from a resumed/forked/compacted session.
    fn apply_conversation_reset(
        &mut self,
        messages: Vec<crate::interactive::ConversationMessage>,
        status: Option<String>,
    ) {
        self.transcript.clear();
        self.render_cache.borrow_mut().clear();
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
        self.autocomplete.close();
        self.capture_preexisting_card_draft();
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
                self.maybe_activate_queued_ext();
            }
        }
    }

    /// Activate a queued extension prompt once no ask card or prompt is
    /// holding the input line.
    fn maybe_activate_queued_ext(&mut self) {
        if self.active_ask.is_some() || self.active_ext.is_some() {
            return;
        }
        if let Some(next) = self.ext_queue.pop_front() {
            self.activate_ext_request(next);
        } else {
            self.restore_card_draft_after_cards_settle();
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
            self.input.set_text("");
            self.maybe_activate_queued_ext();
        }
    }

    fn consume_scroll(&mut self, scroll: impl FnOnce(&mut Self)) -> Cmd<PiFtuiMsg> {
        scroll(self);
        Cmd::none()
    }

    /// Markdown table budget for a terminal `cols` wide: the conversation
    /// region minus a one-cell gutter on each side, never below the width a
    /// two-column table needs to stay legible.
    const fn table_width_for(cols: u16) -> u16 {
        let usable = cols.saturating_sub(2);
        if usable < 20 { 20 } else { usable }
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
        // or mostly-plain replies unstyled). Tables are fitted to the
        // terminal width (gh #195: at natural width a wide table overflowed
        // the frame and its cells were clipped mid-column); the resize handler
        // drops the render cache when the width changes so cached table
        // blocks re-fit.
        let theme = ftui_extras::markdown::MarkdownTheme::default();
        let md = ftui_extras::markdown::MarkdownRenderer::new(theme.clone())
            .table_max_width(Self::table_width_for(self.term.0));
        let palette = self.palette;
        let compact = self.markdown_spacing == crate::config::MarkdownSpacing::Compact;
        // Per-entry render cache (issue #201): reuse each block's styled
        // lines while its revision matches, so a frame costs O(changed
        // entries + streaming tail) markdown renders, not O(transcript).
        let mut cache = self.render_cache.borrow_mut();
        cache.resize_with(self.transcript.len(), || None);
        let mut rendered_blocks = 0_usize;
        let mut reused_blocks = 0_usize;
        let mut lines: Vec<ftui::text::Line<'static>> =
            Vec::with_capacity(self.conversation_line_count());
        for (idx, entry) in self.transcript.iter().enumerate() {
            if entry.card == Some(CardState::Pending) {
                // Pending cards animate with the shared spinner frame, so
                // their lines are frame-dependent: always render fresh and
                // leave no stale cache slot behind.
                cache[idx] = None;
                rendered_blocks += 1;
                push_card_block(
                    &mut lines,
                    CardState::Pending,
                    &entry.text,
                    entry.detail.as_ref(),
                    entry.diff_styled,
                    entry.group_count,
                    &palette,
                    self.spinner.current_frame,
                );
                continue;
            }
            if let Some(block) = cache[idx].as_ref()
                && block.revision == entry.revision
            {
                reused_blocks += 1;
                lines.extend(block.lines.iter().cloned());
                continue;
            }
            rendered_blocks += 1;
            let mut block_lines: Vec<ftui::text::Line<'static>> = Vec::new();
            if let Some(state) = entry.card {
                push_card_block(
                    &mut block_lines,
                    state,
                    &entry.text,
                    entry.detail.as_ref(),
                    entry.diff_styled,
                    entry.group_count,
                    &palette,
                    self.spinner.current_frame,
                );
            } else {
                push_role_block(&mut block_lines, entry.role, &entry.text, &palette, &md);
                if compact && entry.role == EntryRole::Assistant {
                    block_lines = apply_compact_spacing(&block_lines, &theme);
                }
            }
            lines.extend(block_lines.iter().cloned());
            cache[idx] = Some(CachedBlock {
                revision: entry.revision,
                lines: block_lines,
            });
        }
        self.render_stats.set((rendered_blocks, reused_blocks));
        if !self.streaming.is_empty() {
            // Streaming fragments may end mid-construct; the streaming
            // renderer is tolerant of unterminated markdown. The in-flight
            // tail is deliberately uncached — it changes every delta.
            let rendered = md.render_streaming(&self.streaming);
            if compact {
                lines.extend(apply_compact_spacing(rendered.lines(), &theme));
            } else {
                lines.extend(rendered.lines().iter().cloned());
            }
        }
        Text::from_lines(lines)
    }
}

impl Model for PiFtuiModel {
    type Message = PiFtuiMsg;

    fn update(&mut self, msg: PiFtuiMsg) -> Cmd<PiFtuiMsg> {
        let probe = self.watchdog.start();
        let phase = match &msg {
            PiFtuiMsg::Term(_) => LoopPhase::Input,
            PiFtuiMsg::Agent(_) | PiFtuiMsg::Resumed => LoopPhase::AgentEvent,
        };
        let cmd = match msg {
            PiFtuiMsg::Term(event) => self.handle_term(&event),
            PiFtuiMsg::Agent(agent) => self.handle_agent(agent),
            // Back from a SIGTSTP stop: the suspend task already re-acquired
            // raw mode / alt screen / mouse; the next frame repaints the
            // freshly-cleared alternate buffer in full.
            PiFtuiMsg::Resumed => {
                self.suspending = false;
                Cmd::none()
            }
        };
        self.watchdog.finish(phase, probe);
        cmd
    }

    fn view(&self, frame: &mut Frame) {
        let probe = self.watchdog.start();
        self.render_frame(frame);
        self.watchdog.finish(LoopPhase::Render, probe);
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

impl PiFtuiModel {
    /// Counters for the loop watchdog (`pi.tui.loop_watchdog.v1`). Timings
    /// only — never prompt, tool, or model content.
    #[must_use]
    pub fn loop_watchdog_snapshot(&self) -> serde_json::Value {
        self.watchdog.snapshot()
    }

    /// The real render pass. Split out of [`Model::view`] so the watchdog can
    /// time it without an extra guard type.
    #[allow(clippy::too_many_lines)]
    /// Suggestion rows plus a keyboard hint (issue #208). The highlighted
    /// row is kept inside the window via the shared `scroll_offset`, so a
    /// short terminal that clamps the region below `max_visible` still
    /// shows what Up/Down selected.
    fn render_completion(&self, area: Rect, frame: &mut Frame) {
        use ftui::text::{Line, Span};

        let items = &self.autocomplete.items;
        let height = usize::from(area.height);
        // The hint takes the last row whenever at least one suggestion fits
        // above it; a single-row region shows one suggestion instead.
        let show_hint = height >= 2;
        let visible = height
            .saturating_sub(usize::from(show_hint))
            .min(items.len());
        let offset = self.autocomplete.scroll_offset(visible);
        let end = (offset + visible).min(items.len());
        let window = &items[offset..end];
        let label_width = window
            .iter()
            .map(|item| item.label.chars().count())
            .max()
            .unwrap_or(0)
            .min(32);
        let selected_style = ftui::Style::new().bold().fg(self.palette.accent);
        let plain_style = ftui::Style::new();
        let muted_style = ftui::Style::new().dim().fg(self.palette.muted);
        let mut lines = Vec::with_capacity(window.len() + 1);
        for (row, item) in window.iter().enumerate() {
            let selected = self.autocomplete.selected == Some(offset + row);
            let (marker, style) = if selected {
                ("▸ ", selected_style)
            } else {
                ("  ", plain_style)
            };
            let mut spans = vec![
                Span::styled(marker, style),
                Span::styled(format!("{:<label_width$}", item.label), style),
            ];
            // Descriptions come from templates, skills, and extensions —
            // untrusted text, so they go through sanitize like everything
            // else that reaches a frame.
            if let Some(desc) = item
                .description
                .as_deref()
                .map(str::trim)
                .filter(|desc| !desc.is_empty())
            {
                spans.push(Span::styled(format!("  {}", sanitize(desc)), muted_style));
            }
            lines.push(Line::from_spans(spans));
        }
        if show_hint {
            let hint = if items.len() > visible {
                format!("{COMPLETION_HINT} · {end}/{} shown", items.len())
            } else {
                String::from(COMPLETION_HINT)
            };
            lines.push(Line::styled(hint, muted_style));
        }
        Paragraph::new(Text::from_lines(lines)).render(area, frame);
    }

    /// Modal picker body + footer hint. Lines borrow the picker's strings —
    /// no per-frame allocation.
    fn render_picker(&self, picker: &PickerOverlay, regions: &Regions, frame: &mut Frame) {
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
    }

    fn render_frame(&self, frame: &mut Frame) {
        let area = Rect::new(0, 0, frame.width(), frame.height());
        let regions = layout_regions(
            area,
            self.input_rows(),
            u16::from(self.error_banner.is_some()),
            self.completion_rows(),
        );

        // Header: identity + agent state.
        let header = format!("pi · {}", self.state.label());
        let header_style = ftui::Style::new().bold().fg(self.palette.accent);
        Paragraph::new(Text::from_lines([ftui::text::Line::styled(
            header,
            header_style,
        )]))
        .render(regions.header, frame);

        // Modal picker takes over the conversation body while open.
        if let Some(picker) = &self.picker {
            self.render_picker(picker, &regions, frame);
            return;
        }

        // Conversation body with tail-follow scroll. `scroll_from_tail == 0`
        // sticks to the bottom; scrolling up pins an offset measured from the
        // tail so streaming appends don't yank the view.
        let body_text = self.conversation_text();
        let total_lines = body_text.lines().len();
        let visible = usize::from(regions.body.height).max(1);
        // Record the authoritative total for update()'s scroll clamping
        // (issue #206); see `rendered_total_lines`.
        self.rendered_total_lines.set(total_lines);
        let from_tail = self
            .scroll_from_tail
            .min(total_lines.saturating_sub(visible));
        let offset = total_lines.saturating_sub(visible + from_tail);
        let offset_u16 = u16::try_from(offset).unwrap_or(u16::MAX);
        Paragraph::new(body_text)
            .scroll((offset_u16, 0))
            .render(regions.body, frame);

        // Pinned error banner (bd-cv653.9.2): sits between the conversation
        // and the status line until the next sent input dismisses it.
        if let Some(banner) = &self.error_banner {
            let banner_style = ftui::Style::new().bold().fg(self.palette.error);
            Paragraph::new(Text::from_lines([ftui::text::Line::styled(
                format!("✗ {banner}"),
                banner_style,
            )]))
            .render(regions.banner, frame);
        }
        // Status region. While working: spinner + activity (tool > thinking >
        // responding). While a long out-of-turn driver operation runs
        // (issue #203): spinner + its label. While idle: the todo summary.
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
        } else if let Some(busy) = self.busy_label() {
            let spin = DOTS[self.spinner.current_frame % DOTS.len()];
            format!("{spin} {busy}")
        } else {
            self.todo_summary
                .as_ref()
                .map_or_else(String::new, |todo| format!("todo {todo}"))
        };
        if !status_line.is_empty() {
            let status_style = if self.state == AgentUiState::Working || self.busy.is_some() {
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

        // Slash-command completion popup (issue #208), pinned to the editor.
        if regions.completion.height > 0 {
            self.render_completion(regions.completion, frame);
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
}

// ── Launch path ─────────────────────────────────────────────────────────────

/// Cap a tool result's text content into an 8-line preview for the card
/// detail, with an elision counter. `None` when the result has no text.
fn tool_output_preview(result: &crate::tools::ToolOutput) -> Option<String> {
    const MAX_DETAIL_LINES: usize = 8;
    const MAX_LINE_CHARS: usize = 300;

    let mut text = String::new();
    for block in &result.content {
        if let crate::model::ContentBlock::Text(t) = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&t.text);
        }
    }
    if text.trim().is_empty() {
        return None;
    }
    let total = text.lines().count();
    // Byte-cap each kept line too: a multi-megabyte single-line result
    // (minified JSON, long grep hit) must not land whole in the transcript
    // and be re-laid-out every frame.
    let mut preview = text
        .lines()
        .take(MAX_DETAIL_LINES)
        .map(|line| match line.char_indices().nth(MAX_LINE_CHARS) {
            Some((cut, _)) => format!("{}…", &line[..cut]),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    if total > MAX_DETAIL_LINES {
        let _ = write!(preview, "\n… +{} more lines", total - MAX_DETAIL_LINES);
    }
    Some(preview)
}

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
            let stop_reason =
                last_assistant.map_or(crate::model::StopReason::Stop, |a| a.stop_reason);
            // #209: a provider failure ends the turn as a structured card
            // (provider · HTTP status · retry status · bounded detail), not a
            // raw payload dump. Aborts keep their plain "Aborted" line.
            let error_message = error.as_ref().map(|raw| {
                if stop_reason == crate::model::StopReason::Error {
                    crate::error::ProviderErrorSummary::from_error_text(
                        last_assistant.map(|a| a.provider.as_str()),
                        raw,
                    )
                    .turn_end_card(raw, None)
                } else {
                    raw.clone()
                }
            });
            vec![PiMsg::AgentDone {
                usage: last_assistant.map(|a| a.usage.clone()),
                stop_reason,
                error_message,
            }]
        }
        // `ProviderError` is deliberately silent here: the structured card is
        // built from `AgentEnd` above so it lands exactly once, at turn end;
        // the event itself serves JSON/RPC consumers.
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
            args,
            ..
        } => {
            let mut msgs = vec![PiMsg::ToolStart {
                name: tool_name.clone(),
                tool_id: tool_call_id.clone(),
            }];
            // The per-tool registry (tool_invocation_summary) derives the
            // human head ("Bash: cargo test", "Read src/main.rs") from the
            // args; absent a derivable summary the card keeps the name.
            if let Some(summary) = crate::interactive::tool_invocation_summary(tool_name, args) {
                msgs.push(PiMsg::ToolInvocation {
                    tool_id: tool_call_id.clone(),
                    summary,
                });
            }
            msgs
        }
        E::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            is_error,
            result,
            ..
        } => vec![PiMsg::ToolEnd {
            name: tool_name.clone(),
            tool_id: tool_call_id.clone(),
            is_error: *is_error,
            output: tool_output_preview(result),
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
    reply_channel_open: std::sync::atomic::AtomicBool,
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
            reply_channel_open: std::sync::atomic::AtomicBool::new(true),
            pending: Mutex::new(std::collections::HashMap::new()),
        }
    }

    const fn cancelled_response(id: String) -> ExtensionUiResponse {
        ExtensionUiResponse {
            id,
            value: None,
            cancelled: true,
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

    fn cancel_all_pending(&self) -> usize {
        let pending = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.reply_channel_open
                .store(false, std::sync::atomic::Ordering::Release);
            std::mem::take(&mut *pending)
        };
        let count = pending.len();
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        for (id, sender) in pending {
            let _ = sender.send(cx.cx(), Self::cancelled_response(id));
        }
        count
    }
}

struct FtuiPendingUiLease<'a> {
    handler: &'a FtuiExtensionUiHandler,
    id: String,
}

impl Drop for FtuiPendingUiLease<'_> {
    fn drop(&mut self) {
        self.handler.drop_pending(&self.id);
    }
}

#[async_trait::async_trait]
impl crate::sdk::ExtensionUiHandler for FtuiExtensionUiHandler {
    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    async fn request_ui(
        &self,
        request: ExtensionUiRequest,
    ) -> crate::error::Result<Option<ExtensionUiResponse>> {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let id = request.id.clone();
        if !self
            .reply_channel_open
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(Some(Self::cancelled_response(id)));
        }
        if !request.expects_response() {
            let _ = self.agent_tx.send(PiMsg::ExtensionUiRequest(request));
            return Ok(None);
        }
        let timeout_ms = request.timeout_ms.unwrap_or(EXT_UI_TIMEOUT_MS);
        let (reply_tx, mut reply_rx) = asupersync::channel::oneshot::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !self
                .reply_channel_open
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Ok(Some(Self::cancelled_response(id)));
            }
            pending.insert(id.clone(), reply_tx);
            if self
                .agent_tx
                .send(PiMsg::ExtensionUiRequest(request))
                .is_err()
            {
                pending.remove(&id);
                return Ok(None);
            }
        }
        let _pending_reply = FtuiPendingUiLease {
            handler: self,
            id: id.clone(),
        };
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
            Ok(Some(Self::cancelled_response(id)))
        }
    }
}

enum ExtReplyPoll {
    Resolved,
    Empty,
    Disconnected,
}

fn poll_ext_reply(
    handler: &FtuiExtensionUiHandler,
    ext_reply_rx: &Receiver<ExtensionUiResponse>,
) -> ExtReplyPoll {
    match ext_reply_rx.try_recv() {
        Ok(response) => {
            handler.resolve(response);
            ExtReplyPoll::Resolved
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => ExtReplyPoll::Empty,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            handler.cancel_all_pending();
            ExtReplyPoll::Disconnected
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
            match poll_ext_reply(&handler, &ext_reply_rx) {
                ExtReplyPoll::Resolved => {}
                ExtReplyPoll::Empty => {
                    asupersync::time::sleep(asupersync::time::wall_now(), SUBMIT_POLL).await;
                }
                ExtReplyPoll::Disconnected => break,
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
        drop(install_ask_forwarder(&ask, agent_tx, runtime_handle));
    }
    spawn_ask_reply_pump(Arc::clone(&current_ask), ask_reply_rx, runtime_handle);
    current_ask
}

/// Shared slot for the CURRENT ask tool: `/resume` swaps the session handle
/// (and with it the ask tool), so the long-lived reply pump resolves against
/// whatever tool is current when the reply arrives.
type CurrentAsk = Arc<Mutex<Option<crate::ask::AskTool>>>;

/// Shared slot holding the abort handle for the driver's in-flight prompt
/// turn (issue #205): the driver installs a handle per turn, the UI thread
/// fires it on Ctrl-C.
type TurnAbortSlot = Arc<Mutex<Option<crate::agent::AbortHandle>>>;

/// Install the per-handle half of the ask bridge: a channel picker surface on
/// the tool plus a forwarder task that turns cards into `PiMsg::AskUiRequest`.
/// The forwarder dies naturally when the handle (and its ask tool clones)
/// drop. Spawned, not inline: asks arrive MID-TURN while the driver loop is
/// blocked inside `prompt().await`.
fn install_ask_forwarder(
    ask: &crate::ask::AskTool,
    agent_tx: &Sender<PiMsg>,
    runtime_handle: &asupersync::runtime::RuntimeHandle,
) -> asupersync::runtime::JoinHandle<()> {
    let (ask_ui_tx, mut ask_ui_rx) = asupersync::channel::mpsc::channel::<AskUiRequest>(4);
    ask.install_channel_ui(ask_ui_tx);
    let ask_forwarder = ask.clone();
    let ask_fwd_tx = agent_tx.clone();
    runtime_handle.spawn(async move {
        let cx = crate::agent_cx::AgentCx::for_request();
        while let Ok(request) = ask_ui_rx.recv(&cx).await {
            forward_ask_ui_request(&ask_forwarder, &ask_fwd_tx, request);
        }
    })
}

fn forward_ask_ui_request(
    ask: &crate::ask::AskTool,
    agent_tx: &Sender<PiMsg>,
    request: AskUiRequest,
) -> bool {
    let request_id = request.id.clone();
    ask.try_forward_channel_ui_request(&request_id, || {
        agent_tx.send(PiMsg::AskUiRequest(request)).is_ok()
    })
}

/// Spawn the long-lived reply pump: answered cards pair back through the
/// CURRENT ask tool's `respond_ui` (see [`CurrentAsk`]).
enum AskReplyPoll {
    Resolved,
    Empty,
    Disconnected,
}

// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::significant_drop_tightening)]
fn poll_ask_reply(current_ask: &CurrentAsk, ask_reply_rx: &Receiver<AskUiReply>) -> AskReplyPoll {
    match ask_reply_rx.try_recv() {
        Ok(reply) => {
            let guard = current_ask
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(ask) = guard.as_ref() {
                let _ = ask.respond_ui(&reply.request_id, reply.response);
            }
            AskReplyPoll::Resolved
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => AskReplyPoll::Empty,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            let guard = current_ask
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(ask) = guard.as_ref() {
                ask.close_channel_ui();
            }
            AskReplyPoll::Disconnected
        }
    }
}

fn spawn_ask_reply_pump(
    current_ask: CurrentAsk,
    ask_reply_rx: Receiver<AskUiReply>,
    runtime_handle: &asupersync::runtime::RuntimeHandle,
) {
    runtime_handle.spawn(async move {
        loop {
            match poll_ask_reply(&current_ask, &ask_reply_rx) {
                AskReplyPoll::Resolved => {}
                AskReplyPoll::Empty => {
                    asupersync::time::sleep(asupersync::time::wall_now(), SUBMIT_POLL).await;
                }
                AskReplyPoll::Disconnected => break,
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
    turn_abort: &TurnAbortSlot,
) {
    // ubs:ignore Sender clone per turn — the event callback must own its sender
    let tx = agent_tx.clone();
    // Issue #205: install a per-turn abort handle the UI thread can fire on
    // Ctrl-C, so exit doesn't wait out the provider stream and tool calls.
    let (abort_handle, abort_signal) = crate::sdk::AgentSessionHandle::new_abort_handle();
    *turn_abort
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(abort_handle);
    let result = handle
        .prompt_with_abort(prompt, abort_signal, move |event| {
            for msg in agent_event_to_pi_msgs(&event) {
                let _ = tx.send(msg);
            }
        })
        .await;
    *turn_abort
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    match result {
        // #209: the transcript already holds the structured turn-end card
        // (built from `AgentEnd`); the banner pinned above the editor carries
        // the one-line headline so the failure is visible even when the
        // transcript is scrolled away.
        Err(err @ crate::error::Error::Provider { .. }) => {
            let raw = err.to_string();
            let headline =
                crate::error::ProviderErrorSummary::from_error_text(None, &raw).headline();
            let _ = agent_tx.send(PiMsg::AgentError(headline));
        }
        Err(err) => {
            let _ = agent_tx.send(PiMsg::AgentError(err.to_string()));
        }
        Ok(message) if message.stop_reason == crate::model::StopReason::Error => {
            let raw = message.error_message.as_deref().unwrap_or("Request failed");
            let headline =
                crate::error::ProviderErrorSummary::from_error_text(Some(&message.provider), raw)
                    .headline();
            let _ = agent_tx.send(PiMsg::AgentError(headline));
        }
        Ok(_) => {}
    }
}

/// Template for `/resume`: a resumed session keeps the launch selection
/// (provider/model/key/cwd) but swaps the session file.
fn resume_template_from(options: &crate::sdk::SessionOptions) -> crate::sdk::SessionOptions {
    let mut template = options.clone();
    template.no_session = false;
    template.session_path = None;
    // The replacement session must receive the live handler created by this
    // driver's reply channel, never retain an earlier handler instance.
    template.extension_ui_handler = None;
    template
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
    let is_error = result.is_err();
    let msg = match result {
        Ok(value) if value.is_null() => PiMsg::SystemNote(format!("/{name} done")),
        Ok(value) => PiMsg::SystemNote(format!("/{name} → {value}")),
        Err(err) => PiMsg::AgentError(format!("/{name}: {err}")),
    };
    // ToolEnd before the error: the AgentError sweep settles pending
    // cards, which would turn this ToolEnd into a duplicate trace line.
    let _ = agent_tx.send(PiMsg::ToolEnd {
        name: format!("/{name}"),
        tool_id: String::from("ftui-ext-command"),
        is_error,
        output: None,
    });
    let _ = agent_tx.send(msg);
}

/// Handle the default FTUI's MCP control surface against the manager owned by
/// this exact SDK session (bd-vjfol).
async fn run_mcp_command(
    handle: &mut crate::sdk::AgentSessionHandle,
    subcommand: &str,
    name: Option<&str>,
    agent_tx: &Sender<PiMsg>,
) {
    let Some(manager) = handle.mcp_manager() else {
        let _ = agent_tx.send(PiMsg::AgentError(String::from(
            "MCP discovery is disabled for this session",
        )));
        return;
    };

    if subcommand == "list" {
        let rows = manager.list();
        let mut content = String::from("MCP servers (Model Context Protocol)\n");
        if rows.is_empty() {
            content.push_str("\n  No MCP servers configured.\n");
        } else {
            let _ = writeln!(content, "\n  {} configured:", rows.len());
            for row in rows {
                let _ = writeln!(
                    content,
                    "    • {} — {} [{}; trust: {}; {}]",
                    row.name, row.target, row.provenance, row.trust, row.health
                );
            }
        }
        for warning in manager.warnings() {
            let _ = writeln!(
                content,
                "  ⚠ {}: {} ({})",
                warning.source_file.display(),
                warning.entry,
                warning.reason
            );
        }
        let _ = agent_tx.send(PiMsg::System(content));
        return;
    }

    let Some(name) = name else {
        let _ = agent_tx.send(PiMsg::AgentError(format!(
            "usage: /mcp {subcommand} <name>"
        )));
        return;
    };
    let outcome = match subcommand {
        "deny" => manager.deny(name).await.map(|()| Vec::new()),
        "test" => manager.test(name).await,
        "trust" => manager.trust(name).await,
        _ => {
            let _ = agent_tx.send(PiMsg::AgentError(format!(
                "unknown /mcp subcommand {subcommand:?}"
            )));
            return;
        }
    };
    let message = match outcome {
        Ok(_) if subcommand == "deny" => format!("MCP server {name:?} denied and stopped."),
        Ok(tools) => {
            let mounted = handle.mount_mcp_server_tools_if_absent(name);
            let verb = if subcommand == "test" {
                "tested"
            } else {
                "trusted"
            };
            let mut line = format!(
                "MCP server {name:?} {verb}: {} tool(s) available.",
                tools.len()
            );
            for tool in tools.iter().take(12) {
                let _ = writeln!(line, "  • {} — {}", tool.name, tool.description);
            }
            if tools.len() > 12 {
                let _ = writeln!(line, "  … and {} more", tools.len() - 12);
            }
            if mounted > 0 {
                let _ = writeln!(
                    line,
                    "Mounted {mounted} new mcp__* tool(s) into the live session."
                );
            }
            line
        }
        Err(err) => format!("MCP {name:?}: {err}"),
    };
    let _ = agent_tx.send(PiMsg::System(message));
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

fn report_replacement_shutdown_failure(
    shutdown: &crate::sdk::SessionResourceShutdown,
    agent_tx: &Sender<PiMsg>,
) {
    let summary = shutdown.failures().collect::<Vec<_>>().join("; ");
    let _ = agent_tx.send(PiMsg::AgentError(format!(
        "session replacement cancelled because previous-session shutdown preflight failed: {summary}"
    )));
}

async fn complete_replacement_after_shutdown(
    handle: &mut crate::sdk::AgentSessionHandle,
    shutdown: &crate::sdk::SessionResourceShutdown,
) -> std::result::Result<(), String> {
    if !shutdown.completed_cleanly() || !shutdown.permits_replacement_mcp_activation() {
        handle.disable_mcp();
        let issues = shutdown.messages().collect::<Vec<_>>().join("; ");
        return Err(if issues.is_empty() {
            String::from("previous-session cleanup did not prove complete MCP shutdown")
        } else {
            issues
        });
    }
    handle.activate_mcp().await;
    Ok(())
}

/// Handle `/new` in the driver: build a fresh session from the launch
/// template with the CURRENT provider/model selection preserved and thinking
/// restored to the launch/configured default (issue #197: it used to be
/// force-reset to off, so a `defaultThinkingLevel: max` setup showed
/// "[thinking: off]" on every new session). MCP activation is deferred until
/// after the old handle completes awaited shutdown so singleton servers never
/// overlap and the previous session flushes before replacement.
/// Construction failures surface as UI errors and keep the current session.
async fn new_session_command(
    template: &crate::sdk::SessionOptions,
    handle: &mut crate::sdk::AgentSessionHandle,
    current_ask: &CurrentAsk,
    ext_handler: &Arc<FtuiExtensionUiHandler>,
    agent_tx: &Sender<PiMsg>,
    runtime_handle: &asupersync::runtime::RuntimeHandle,
) -> std::result::Result<(), String> {
    let (provider, model_id) = handle.model();
    let mut options = template.clone();
    options.provider = Some(provider.clone());
    options.model = Some(model_id.clone());
    options.session_path = None;
    options.extension_ui_handler =
        Some(Arc::clone(ext_handler) as Arc<dyn crate::sdk::ExtensionUiHandler>);
    // `template.thinking` carries the launch selection; `None` lets session
    // creation re-resolve the configured default (issue #197).
    options.no_session = false;
    match crate::sdk::create_agent_session_deferred_mcp(options).await {
        Ok(new_handle) => {
            let prepared = match handle.preflight_replacement().await {
                Ok(prepared) => prepared,
                Err(shutdown) => {
                    report_replacement_shutdown_failure(&shutdown, agent_tx);
                    let cleanup = new_handle.discard_uncommitted_resources().await;
                    for issue in cleanup.messages() {
                        let _ = agent_tx.send(PiMsg::System(format!(
                            "Uncommitted new session cleanup issue: {issue}"
                        )));
                    }
                    return Ok(());
                }
            };
            // Commit the candidate before teardown begins. Cancellation during
            // any later await can no longer leave a partially decommissioned
            // old handle installed as the current session.
            let old_handle = std::mem::replace(handle, new_handle);
            if let Some(ask) = old_handle.ask_tool() {
                ask.close_channel_ui();
            }
            let shutdown = old_handle.commit_resource_shutdown(prepared).await;
            if let Err(issues) = complete_replacement_after_shutdown(handle, &shutdown).await {
                let _ = agent_tx.send(PiMsg::AgentError(format!(
                    "Previous-session cleanup was incomplete; ending the FTUI session without activating replacement MCP: {issues}"
                )));
                return Err(issues);
            }
            *current_ask
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = handle.ask_tool();
            if let Some(ask) = handle.ask_tool() {
                drop(install_ask_forwarder(&ask, agent_tx, runtime_handle));
            }
            let thinking_label = handle.state().await.ok().map_or_else(
                || String::from("off"),
                |state| {
                    state
                        .thinking_level
                        .map_or_else(|| String::from("off"), |level| level.to_string())
                },
            );
            send_conversation_reset(
                handle,
                agent_tx,
                &format!(
                    "Started new session\nModel set to {provider}/{model_id}\nThinking level: {thinking_label}"
                ),
            )
            .await;
            // Issue #200: the fresh session has no name; drop any previous
            // session's tab title back to the model label.
            let _ = agent_tx.send(PiMsg::TerminalTitle(format!("Pi · {provider}/{model_id}")));
        }
        Err(err) => {
            let _ = agent_tx.send(PiMsg::AgentError(format!("new session: {err}")));
        }
    }
    Ok(())
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
        Ok(()) => {
            // Issue #200: a named session titles the terminal tab after
            // itself.
            let _ = agent_tx.send(PiMsg::TerminalTitle(format!("Pi · {name}")));
            PiMsg::System(format!("Session name: {name}"))
        }
        Err(err) => PiMsg::AgentError(format!("name: {err}")),
    };
    let _ = agent_tx.send(msg);
}

/// `/add-dir <dir>` driver (bd-cv653.3.12): validate + add on the shared
/// workspace handle and persist the canonical set into the session header.
async fn run_add_dir_command(
    handle: &mut crate::sdk::AgentSessionHandle,
    dir: &str,
    agent_tx: &Sender<PiMsg>,
) {
    if dir.trim().is_empty() {
        let _ = agent_tx.send(PiMsg::AgentError(String::from(
            "usage: /add-dir <directory>",
        )));
        return;
    }
    let msg = match handle.add_workspace_root(dir).await {
        Ok(status) => PiMsg::System(status),
        Err(err) => PiMsg::AgentError(format!("add-dir: {err}")),
    };
    let _ = agent_tx.send(msg);
}

/// `/remove-dir <dir>` driver (bd-cv653.3.12): revoke on the shared
/// workspace handle — every tool holding a clone sees the removal on its
/// next confinement check.
async fn run_remove_dir_command(
    handle: &mut crate::sdk::AgentSessionHandle,
    dir: &str,
    agent_tx: &Sender<PiMsg>,
) {
    if dir.trim().is_empty() {
        let _ = agent_tx.send(PiMsg::AgentError(String::from(
            "usage: /remove-dir <directory>",
        )));
        return;
    }
    let msg = match handle.remove_workspace_root(dir).await {
        Ok(status) => PiMsg::System(status),
        Err(err) => PiMsg::AgentError(format!("remove-dir: {err}")),
    };
    let _ = agent_tx.send(msg);
}

/// `/crash [list|show|delete]` driver (bd-cv653.7.12): inspect or clear
/// redacted crash bundles under the agent dir. Nothing is transmitted.
fn run_crash_command(action: &str, agent_tx: &Sender<PiMsg>) {
    let agent_dir = crate::config::Config::global_dir();
    let msg = match action {
        "" | "list" => {
            let bundles = pi::crash::list_bundles(&agent_dir);
            if bundles.is_empty() {
                PiMsg::System(String::from("No crash bundles recorded."))
            } else {
                PiMsg::System(
                    bundles
                        .iter()
                        .map(|b| {
                            format!(
                                "{} {} {}{}",
                                b.created_at,
                                b.kind,
                                b.dir.display(),
                                if b.noticed { "" } else { " (new)" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            }
        }
        "show" => pi::crash::show_latest(&agent_dir).map_or_else(
            || PiMsg::System(String::from("No crash bundles recorded.")),
            PiMsg::System,
        ),
        "delete" => {
            let removed = pi::crash::delete_all(&agent_dir);
            PiMsg::System(format!("Deleted {removed} crash bundle(s)"))
        }
        other => PiMsg::AgentError(format!("usage: /crash [list|show|delete] (got: {other})")),
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
/// launch selection preserved, await the previous handle's shutdown before
/// starting the replacement's MCP servers, rewire the ask bridge, and replay
/// the conversation into the UI. Construction failures keep the current
/// session.
async fn resume_session_command(
    path: &str,
    template: &crate::sdk::SessionOptions,
    handle: &mut crate::sdk::AgentSessionHandle,
    current_ask: &CurrentAsk,
    ext_handler: &Arc<FtuiExtensionUiHandler>,
    agent_tx: &Sender<PiMsg>,
    runtime_handle: &asupersync::runtime::RuntimeHandle,
) -> std::result::Result<(), String> {
    let mut options = template.clone();
    options.session_path = Some(std::path::PathBuf::from(path));
    options.extension_ui_handler =
        Some(Arc::clone(ext_handler) as Arc<dyn crate::sdk::ExtensionUiHandler>);
    options.no_session = false;
    match crate::sdk::create_agent_session_deferred_mcp(options).await {
        Ok(new_handle) => {
            let prepared = match handle.preflight_replacement().await {
                Ok(prepared) => prepared,
                Err(shutdown) => {
                    report_replacement_shutdown_failure(&shutdown, agent_tx);
                    let cleanup = new_handle.discard_uncommitted_resources().await;
                    for issue in cleanup.messages() {
                        let _ = agent_tx.send(PiMsg::System(format!(
                            "Uncommitted resumed session cleanup issue: {issue}"
                        )));
                    }
                    return Ok(());
                }
            };
            let old_handle = std::mem::replace(handle, new_handle);
            if let Some(ask) = old_handle.ask_tool() {
                ask.close_channel_ui();
            }
            let shutdown = old_handle.commit_resource_shutdown(prepared).await;
            if let Err(issues) = complete_replacement_after_shutdown(handle, &shutdown).await {
                let _ = agent_tx.send(PiMsg::AgentError(format!(
                    "Previous-session cleanup was incomplete; ending the FTUI session without activating replacement MCP: {issues}"
                )));
                return Err(issues);
            }
            *current_ask
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = handle.ask_tool();
            if let Some(ask) = handle.ask_tool() {
                drop(install_ask_forwarder(&ask, agent_tx, runtime_handle));
            }
            send_conversation_reset(handle, agent_tx, "session resumed").await;
            // Issue #200: a resumed named session restores its tab title.
            if let Ok(Some(name)) = handle.with_session(crate::session::Session::get_name).await {
                let _ = agent_tx.send(PiMsg::TerminalTitle(format!("Pi · {name}")));
            }
        }
        Err(err) => {
            let _ = agent_tx.send(PiMsg::AgentError(format!("resume: {err}")));
        }
    }
    Ok(())
}

/// Snapshot the handle's conversation and reset the UI transcript from it.
async fn send_conversation_reset(
    handle: &crate::sdk::AgentSessionHandle,
    agent_tx: &Sender<PiMsg>,
    status: &str,
) {
    match handle
        .with_session(|session| {
            let session_id = session.header.id.clone();
            let (messages, usage) = crate::interactive::conversation_from_session(session);
            (session_id, messages, usage)
        })
        .await
    {
        Ok((session_id, messages, usage)) => {
            let _ = agent_tx.send(PiMsg::ConversationReset {
                session_id,
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
            // ToolEnd must precede AgentError: the error sweep settles
            // pending cards, and a card already settled there makes this
            // ToolEnd fall back to a duplicate trace line.
            let _ = agent_tx.send(PiMsg::ToolEnd {
                name: String::from("bash"),
                tool_id: String::from("ftui-bash"),
                is_error: true,
                output: None,
            });
            let _ = agent_tx.send(PiMsg::AgentError(format!("bash: {err}")));
            None
        }
    };
    if output.is_some() {
        let _ = agent_tx.send(PiMsg::ToolEnd {
            name: String::from("bash"),
            tool_id: String::from("ftui-bash"),
            is_error: false,
            // BashResult already folded the display into the card; a second
            // detail here would duplicate it.
            output: None,
        });
    }
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

fn finish_ftui_run(
    app_result: std::io::Result<()>,
    driver_result: std::thread::Result<std::io::Result<()>>,
) -> std::io::Result<()> {
    app_result?;
    driver_result
        .map_err(|_| std::io::Error::other("FTUI agent driver panicked during shutdown"))?
}

fn terminal_replacement_error(
    agent_tx: &Sender<PiMsg>,
    replacement_failure: String,
    shutdown: &crate::sdk::SessionResourceShutdown,
) -> std::io::Error {
    // The diagnostic was enqueued before the driver broke its command loop.
    // Follow it with an ordered quit event so the app stops waiting, joins the
    // failed driver, and returns this terminal error without manual input.
    let _ = agent_tx.send(PiMsg::UiShutdown);
    let cleanup_issues = shutdown.failures().collect::<Vec<_>>().join("; ");
    let detail = if cleanup_issues.is_empty() {
        replacement_failure
    } else {
        format!("{replacement_failure}; replacement cleanup was also incomplete: {cleanup_issues}")
    };
    std::io::Error::other(format!(
        "FTUI session replacement failed terminally: {detail}"
    ))
}

#[allow(clippy::too_many_lines)]
pub fn run(
    session_options: crate::sdk::SessionOptions,
    theme: &crate::theme::Theme,
    inline: bool,
    available_models: Vec<String>,
    available_sessions: Vec<(String, String)>,
    markdown_spacing: crate::config::MarkdownSpacing,
    autocomplete: AutocompleteLaunch,
) -> std::io::Result<()> {
    const DRIVER_STACK_BYTES: usize = 16 * 1024 * 1024;
    // Issue #208: the driver re-sends the catalog with extension commands
    // once its session exists; the model starts from the resource catalog.
    let driver_catalog = autocomplete.catalog.clone();

    let (submit_tx, submit_rx) = std::sync::mpsc::channel::<UiCommand>();
    let (agent_tx, agent_rx) = std::sync::mpsc::channel::<PiMsg>();
    let (ask_reply_tx, ask_reply_rx) = std::sync::mpsc::channel::<AskUiReply>();
    let (ext_reply_tx, ext_reply_rx) = std::sync::mpsc::channel::<ExtensionUiResponse>();
    let bash_cwd = driver_bash_cwd(&session_options);
    let resume_template = resume_template_from(&session_options);
    // Issue #205: shared slot so Ctrl-C on the UI thread can abort the
    // driver's in-flight prompt turn instead of waiting it out.
    let turn_abort: TurnAbortSlot = Arc::new(Mutex::new(None));
    let driver_turn_abort = Arc::clone(&turn_abort);

    let driver = std::thread::Builder::new()
        .name("pi-ftui-agent-driver".into())
        .stack_size(DRIVER_STACK_BYTES)
        .spawn(move || -> std::io::Result<()> {
            let runtime = match asupersync::runtime::RuntimeBuilder::new().build() {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = agent_tx.send(PiMsg::AgentError(format!("runtime build: {err}")));
                    return Err(std::io::Error::other(format!(
                        "FTUI runtime build failed: {err}"
                    )));
                }
            };
            let runtime_handle = runtime.handle();
            let terminal_agent_tx = agent_tx.clone();
            let shutdown = runtime.block_on(async move {
                let (mut handle, ext_handler) = create_driver_session(
                    session_options,
                    &agent_tx,
                    ext_reply_rx,
                    &runtime_handle,
                )
                .await?;
                let current_ask =
                    install_ask_bridges(&handle, &agent_tx, ask_reply_rx, &runtime_handle);
                send_conversation_reset(
                    &handle,
                    &agent_tx,
                    "ftui preview stack — experimental (bd-cv653.9.1)",
                )
                .await;
                // Issue #208: extension-contributed slash commands become
                // completable now that the extension runtime is up.
                if let Some(manager) = handle.extension_manager() {
                    let mut catalog = driver_catalog;
                    catalog.extension_commands = extension_commands_for_catalog(manager);
                    let _ = agent_tx.send(PiMsg::AutocompleteCatalog(catalog));
                }
                // Issue #200: a session opened named at launch (--session)
                // titles the terminal tab after itself immediately.
                if let Ok(Some(name)) = handle.with_session(crate::session::Session::get_name).await
                {
                    let _ = agent_tx.send(PiMsg::TerminalTitle(format!("Pi · {name}")));
                }
                let mut replacement_failure = None;
                loop {
                    match submit_rx.try_recv() {
                        Ok(UiCommand::Prompt(prompt)) => {
                            run_prompt_turn(&mut handle, prompt, &agent_tx, &driver_turn_abort)
                                .await;
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
                                run_prompt_turn(&mut handle, output, &agent_tx, &driver_turn_abort)
                                    .await;
                            }
                        }
                        Ok(UiCommand::Compact) => {
                            run_compact_command(&mut handle, &agent_tx).await;
                        }
                        Ok(UiCommand::AddDir { dir }) => {
                            run_add_dir_command(&mut handle, &dir, &agent_tx).await;
                        }
                        Ok(UiCommand::RemoveDir { dir }) => {
                            run_remove_dir_command(&mut handle, &dir, &agent_tx).await;
                        }
                        Ok(UiCommand::Crash { action }) => {
                            run_crash_command(&action, &agent_tx);
                        }
                        Ok(UiCommand::Undo { count, force, redo }) => {
                            run_undo_command(&handle, count, force, redo, &agent_tx);
                        }
                        Ok(UiCommand::Usage { refresh }) => {
                            run_usage_command(refresh, &agent_tx).await;
                        }
                        Ok(UiCommand::Mcp { subcommand, name }) => {
                            run_mcp_command(&mut handle, &subcommand, name.as_deref(), &agent_tx)
                                .await;
                        }
                        Ok(UiCommand::ExtensionCommand { name, args }) => {
                            run_extension_command(&handle, &bash_cwd, &name, &args, &agent_tx)
                                .await;
                        }
                        Ok(UiCommand::ResumeSession { path }) => {
                            // Boxed: clippy::large_futures.
                            if let Err(err) = Box::pin(resume_session_command(
                                &path,
                                &resume_template,
                                &mut handle,
                                &current_ask,
                                &ext_handler,
                                &agent_tx,
                                &runtime_handle,
                            ))
                            .await
                            {
                                replacement_failure = Some(err);
                                break;
                            }
                        }
                        Ok(UiCommand::NewSession) => {
                            // Boxed: clippy::large_futures.
                            if let Err(err) = Box::pin(new_session_command(
                                &resume_template,
                                &mut handle,
                                &current_ask,
                                &ext_handler,
                                &agent_tx,
                                &runtime_handle,
                            ))
                            .await
                            {
                                replacement_failure = Some(err);
                                break;
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
                let shutdown = if replacement_failure.is_some() {
                    handle.discard_uncommitted_resources().await
                } else {
                    handle.shutdown_owned_resources().await
                };
                Some((shutdown, replacement_failure))
            });
            let Some((shutdown, replacement_failure)) = shutdown else {
                return Err(std::io::Error::other(
                    "FTUI agent session failed to initialize",
                ));
            };
            if let Some(replacement_failure) = replacement_failure {
                return Err(terminal_replacement_error(
                    &terminal_agent_tx,
                    replacement_failure,
                    &shutdown,
                ));
            }
            if shutdown.completed_cleanly() {
                return Ok(());
            }
            let issues = shutdown.failures().collect::<Vec<_>>().join("; ");
            tracing::warn!(
                event = "ftui.session.shutdown.incomplete",
                issues,
                "session-owned resources remained after exhaustive shutdown"
            );
            Err(std::io::Error::other(format!(
                "FTUI session shutdown incomplete: {issues}"
            )))
        })?;

    // Issue #194: Windows Terminal has supported synchronized output
    // (DECSET 2026) since 1.18, but it identifies via WT_SESSION rather than
    // TERM_PROGRAM, so ftui's allowlist misses it and every multi-cell frame
    // update tears — the reported flicker while typing, streaming, and
    // wheel-scrolling. Force the capability on for WT sessions; the guard
    // must outlive the app run.
    let _wt_sync_guard = std::env::var_os("WT_SESSION").map(|_| {
        let mut over = ftui::core::capability_override::CapabilityOverride::new();
        over.sync_output = Some(true);
        ftui::core::capability_override::push_override(over)
    });

    let model = PiFtuiModel::new(agent_rx)
        .with_submit_channel(submit_tx)
        .with_turn_abort(turn_abort)
        .with_ask_reply_channel(ask_reply_tx)
        .with_palette(FtuiPalette::from_theme(theme))
        .with_available_models(available_models)
        .with_available_sessions(available_sessions)
        .with_alt_screen(!inline)
        .with_markdown_spacing(markdown_spacing)
        .with_autocomplete(autocomplete)
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
    // sees Disconnected and unwinds. Await the teardown result so final save
    // or resource-shutdown failures cannot be reported as a successful exit.
    finish_ftui_run(result, driver.join())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StopReason;
    use ftui::runtime::simulator::{CmdRecord, ProgramSimulator};
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
    fn replacement_session_template_preserves_launch_capabilities() {
        let options = crate::sdk::SessionOptions {
            no_session: true,
            session_path: Some(std::path::PathBuf::from("original.jsonl")),
            enabled_tools: Some(vec!["read".to_string()]),
            repair_policy: Some("auto-safe".to_string()),
            extension_flags: vec![crate::cli::ExtensionCliFlag {
                name: "verbose".to_string(),
                value: Some("true".to_string()),
            }],
            max_tool_iterations: 17,
            mcp: Some(crate::sdk::McpSessionOptions {
                config_paths: vec![std::path::PathBuf::from("extra-mcp.json")],
                global_dir: Some(std::path::PathBuf::from("isolated-global")),
            }),
            ..crate::sdk::SessionOptions::default()
        };

        let template = resume_template_from(&options);
        assert!(!template.no_session);
        assert!(template.session_path.is_none());
        assert_eq!(template.enabled_tools, options.enabled_tools);
        assert_eq!(template.repair_policy, options.repair_policy);
        assert_eq!(template.extension_flags, options.extension_flags);
        assert_eq!(template.max_tool_iterations, 17);
        let mcp = template.mcp.expect("MCP launch options preserved");
        assert_eq!(
            mcp.config_paths,
            vec![std::path::PathBuf::from("extra-mcp.json")]
        );
        assert_eq!(
            mcp.global_dir,
            Some(std::path::PathBuf::from("isolated-global"))
        );
    }

    #[test]
    fn ftui_exit_surfaces_driver_shutdown_failures_and_panics() {
        let shutdown_error = finish_ftui_run(
            Ok(()),
            Ok(Err(std::io::Error::other("autosave was not flushed"))),
        )
        .expect_err("driver shutdown failure must make FTUI exit fail");
        assert!(
            shutdown_error
                .to_string()
                .contains("autosave was not flushed")
        );

        let panic_error = finish_ftui_run(Ok(()), Err(Box::new("driver panic")))
            .expect_err("driver panic must make FTUI exit fail");
        assert!(panic_error.to_string().contains("driver panicked"));

        let app_error = finish_ftui_run(
            Err(std::io::Error::other("terminal restore failed")),
            Ok(Err(std::io::Error::other("driver shutdown failed"))),
        )
        .expect_err("primary app failure must be preserved");
        assert!(app_error.to_string().contains("terminal restore failed"));

        let app_error_before_panic = finish_ftui_run(
            Err(std::io::Error::other("terminal restore failed first")),
            Err(Box::new("driver panic")),
        )
        .expect_err("app failure must remain primary even when the driver also panics");
        assert!(
            app_error_before_panic
                .to_string()
                .contains("terminal restore failed first")
        );
    }

    #[test]
    fn terminal_replacement_failure_requests_ui_shutdown_and_aggregates_cleanup() {
        let (agent_tx, agent_rx) = mpsc::channel();
        let mut shutdown = crate::sdk::SessionResourceShutdown::default();
        shutdown.fail(String::from("candidate extension shutdown timed out"));

        let error = terminal_replacement_error(
            &agent_tx,
            String::from("old MCP shutdown timed out"),
            &shutdown,
        );

        assert!(matches!(
            agent_rx.recv_timeout(std::time::Duration::from_secs(1)),
            Ok(PiMsg::UiShutdown)
        ));
        let message = error.to_string();
        assert!(message.contains("old MCP shutdown timed out"), "{message}");
        assert!(
            message.contains("candidate extension shutdown timed out"),
            "{message}"
        );
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
    fn ctrl_z_dispatches_suspend_task_and_fake_resumes() {
        let (_tx, model) = new_model();
        let model = model
            .with_alt_screen(true)
            .with_suspend_task(|| PiFtuiMsg::Resumed);
        let mut sim = ProgramSimulator::new(model);
        sim.init();

        // The simulator executes Cmd::Task closures synchronously: our fake
        // stands in for the real SIGTSTP closure (which would touch termios
        // and stop the process on this host). End state: freeze requested,
        // fake ran, Resumed cleared it.
        sim.inject_event(key(KeyCode::Char('z'), Modifiers::CTRL));
        assert!(!sim.model().suspending);
        assert!(
            sim.command_log()
                .iter()
                .any(|record| matches!(record, CmdRecord::Task))
        );
    }

    #[test]
    fn suspend_freeze_gates_ticks_until_resize_clears_it() {
        let (_tx, mut model) = new_model();
        model.state = AgentUiState::Working;
        model.suspending = true;
        let mut sim = ProgramSimulator::new(model);
        sim.init();

        // Frozen: ticks must not mutate the model — byte-identical pre-stop
        // frames keep the diff engine silent between terminal restore and
        // SIGTSTP delivery.
        let before = sim.model().spinner.current_frame;
        sim.send(PiFtuiMsg::Term(Event::Tick));
        assert_eq!(sim.model().spinner.current_frame, before);

        // The suspend task reports back through a Resize after SIGCONT:
        // unfreezes ticks and adopts the post-resume size.
        sim.send(PiFtuiMsg::Term(Event::Resize {
            width: 100,
            height: 30,
        }));
        assert!(!sim.model().suspending);
        assert_eq!(sim.model().term, (100, 30));

        let before = sim.model().spinner.current_frame;
        sim.send(PiFtuiMsg::Term(Event::Tick));
        assert_eq!(sim.model().spinner.current_frame, before + 1);
    }

    #[test]
    fn resumed_message_clears_suspend_freeze() {
        let (_tx, mut model) = new_model();
        model.suspending = true;
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Resumed);
        assert!(!sim.model().suspending);
    }

    #[test]
    fn with_alt_screen_records_launch_mode_for_suspend() {
        let (_tx, model) = new_model();
        assert!(!model.alt_screen);
        let model = model.with_alt_screen(true);
        assert!(model.alt_screen);
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
            output: None,
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
            output: None,
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

    /// Issue #205: Ctrl-C must fire the in-flight turn's abort handle before
    /// quitting, so process teardown doesn't wait for the provider stream
    /// and remaining tool calls to finish naturally.
    #[test]
    fn ctrl_c_aborts_in_flight_turn_before_quit() {
        let slot: TurnAbortSlot = Arc::new(Mutex::new(None));
        let (abort_handle, abort_signal) = crate::agent::AbortHandle::new();
        *slot.lock().expect("slot lock") = Some(abort_handle);

        let (_tx, model) = new_model();
        let model = model.with_turn_abort(Arc::clone(&slot));
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.inject_event(key(KeyCode::Char('c'), Modifiers::CTRL));

        assert!(
            abort_signal.is_aborted(),
            "Ctrl-C must abort the in-flight turn"
        );
        assert!(
            slot.lock().expect("slot lock").is_none(),
            "the fired handle must be consumed"
        );
    }

    /// Issue #206: markdown rendering expands raw text, so clamping the
    /// scroll range against the raw-line approximation made PageUp stall
    /// partway up long sessions (the reported fixed-percentage snap-back)
    /// and made any resize collapse the scroll position. The clamp must
    /// honor the rendered total recorded by the last frame, and a resize
    /// must preserve — not reset — an in-range offset.
    #[test]
    fn scroll_clamp_honors_rendered_total_and_survives_resize() {
        let (_tx, mut model) = new_model();
        // Markdown-heavy assistant entries: headings, paragraphs, and lists
        // render with inserted blank lines, so the rendered line total far
        // exceeds the raw `\n` count.
        for i in 0..12 {
            model.push_entry(EntryRole::Assistant, format!("# Head {i}\ntext {i}"));
        }
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.inject_event(Event::Resize {
            width: 40,
            height: 12,
        });
        let _ = sim.capture_frame(40, 12);

        let rendered = sim.model().rendered_total_lines.get();
        let approx = sim.model().conversation_line_count();
        assert!(
            rendered > approx,
            "markdown must expand raw text (rendered={rendered}, approx={approx})"
        );

        // Page all the way up: the offset must reach the rendered maximum,
        // beyond where the raw approximation would have stalled.
        for _ in 0..64 {
            sim.inject_event(key(KeyCode::PageUp, Modifiers::empty()));
        }
        let pinned = sim.model().scroll_from_tail;
        assert_eq!(pinned, sim.model().max_scroll_from_tail());
        assert!(
            pinned > approx.saturating_sub(sim.model().body_height()),
            "scroll range still limited by the raw approximation: pinned={pinned}"
        );

        // A one-row resize must keep the reading position (clamped only by
        // the fresh geometry), not snap to the bottom.
        sim.inject_event(Event::Resize {
            width: 40,
            height: 11,
        });
        let after_resize = sim.model().scroll_from_tail;
        assert!(
            after_resize >= pinned.saturating_sub(2),
            "resize collapsed the scroll position: before={pinned}, after={after_resize}"
        );
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
    fn overlapping_ask_is_dismissed_without_replacing_active_card() {
        let (agent_tx, agent_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel::<AskUiReply>();
        let model = PiFtuiModel::new(agent_rx).with_ask_reply_channel(reply_tx);
        drop(agent_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::AskUiRequest(ask_request(
            "ask-active",
            vec![question("First?", &["a", "b"], false)],
        ))));
        sim.send(PiFtuiMsg::Agent(PiMsg::AskUiRequest(ask_request(
            "ask-overlap",
            vec![question("Second?", &["c", "d"], false)],
        ))));

        assert_eq!(
            sim.model()
                .active_ask
                .as_ref()
                .map(|ask| ask.request.id.as_str()),
            Some("ask-active"),
            "overlap must not replace the reachable card"
        );
        let dismissed = reply_rx.try_recv().expect("overlap receives dismissal");
        assert_eq!(dismissed.request_id, "ask-overlap");
        assert!(dismissed.response.dismissed);
        assert!(dismissed.response.answers.is_empty());
    }

    #[test]
    fn ask_overlapping_active_extension_is_dismissed_without_replacement() {
        let (_agent_tx, rx) = mpsc::channel();
        let (ask_tx, ask_rx) = mpsc::channel::<AskUiReply>();
        let (ext_tx, _ext_rx) = mpsc::channel::<ExtensionUiResponse>();
        let model = PiFtuiModel::new(rx)
            .with_ask_reply_channel(ask_tx)
            .with_ext_reply_channel(ext_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::ExtensionUiRequest(ext_request(
            "ext-active",
            "confirm",
            serde_json::json!({"title": "First?"}),
        ))));
        sim.send(PiFtuiMsg::Agent(PiMsg::AskUiRequest(ask_request(
            "ask-overlap",
            vec![question("Second?", &["a", "b"], false)],
        ))));

        assert_eq!(
            sim.model()
                .active_ext
                .as_ref()
                .map(|request| request.id.as_str()),
            Some("ext-active")
        );
        assert!(sim.model().active_ask.is_none());
        let dismissed = ask_rx.try_recv().expect("overlap receives dismissal");
        assert_eq!(dismissed.request_id, "ask-overlap");
        assert!(dismissed.response.dismissed);
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
        ExtensionUiRequest::new(id, method, payload)
            .with_extension_id(Some(String::from("demo-ext")))
    }

    #[test]
    fn extension_reply_disconnect_cancels_pending_and_rejects_new_prompts() {
        let (agent_tx, agent_rx) = mpsc::channel();
        let handler = FtuiExtensionUiHandler::new(agent_tx);
        let (pending_tx, mut pending_rx) = asupersync::channel::oneshot::channel();
        handler
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(String::from("startup-prompt"), pending_tx);
        let (ext_reply_tx, ext_reply_rx) = mpsc::channel::<ExtensionUiResponse>();
        drop(ext_reply_tx);

        assert!(matches!(
            poll_ext_reply(&handler, &ext_reply_rx),
            ExtReplyPoll::Disconnected
        ));
        assert!(
            handler
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "disconnect must drain every outstanding prompt"
        );

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let cancelled = runtime
            .block_on(async {
                let cx = crate::agent_cx::AgentCx::for_current_or_request();
                pending_rx.recv(cx.cx()).await
            })
            .expect("pending prompt receives cancellation");
        assert_eq!(cancelled.id, "startup-prompt");
        assert!(cancelled.cancelled);

        let mut late_request = ext_request(
            "late-prompt",
            "confirm",
            serde_json::json!({"title": "Too late?"}),
        );
        late_request.timeout_ms = Some(10);
        let rejected = runtime
            .block_on(crate::sdk::ExtensionUiHandler::request_ui(
                &handler,
                late_request,
            ))
            .expect("closed UI returns a typed response")
            .expect("closed UI returns cancellation rather than absence");
        assert_eq!(rejected.id, "late-prompt");
        assert!(rejected.cancelled);
        assert!(
            agent_rx.try_recv().is_err(),
            "closed reply channel must reject new prompts before UI dispatch"
        );
    }

    #[test]
    fn ftui_extension_handler_notification_never_waits_for_reply() {
        asupersync::test_utils::run_test(|| async {
            let (agent_tx, agent_rx) = mpsc::channel();
            let handler = FtuiExtensionUiHandler::new(agent_tx);
            let notification = ext_request(
                "notify-1",
                "notify",
                serde_json::json!({"title": "Complete", "message": "Build finished"}),
            );

            let outcome = asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_millis(20),
                crate::sdk::ExtensionUiHandler::request_ui(&handler, notification),
            )
            .await
            .expect("notification must not wait on the response timeout")
            .expect("notification dispatch succeeds");

            assert!(outcome.is_none(), "notification has no response contract");
            assert!(
                handler
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_empty(),
                "notification must not allocate a pending waiter"
            );
            assert!(matches!(
                agent_rx.try_recv(),
                Ok(PiMsg::ExtensionUiRequest(request)) if request.id == "notify-1"
            ));
        });
    }

    #[test]
    fn dropping_ftui_extension_request_releases_pending_entry() {
        asupersync::test_utils::run_test(|| async {
            let (agent_tx, agent_rx) = mpsc::channel();
            let handler = FtuiExtensionUiHandler::new(agent_tx);
            let request = ext_request(
                "cancelled-ftui-request",
                "confirm",
                serde_json::json!({"title": "Cancel me"}),
            );
            let mut attempt = Box::pin(crate::sdk::ExtensionUiHandler::request_ui(
                &handler, request,
            ));
            assert!(futures::poll!(attempt.as_mut()).is_pending());
            assert!(matches!(
                agent_rx.try_recv(),
                Ok(PiMsg::ExtensionUiRequest(request))
                    if request.id == "cancelled-ftui-request"
            ));
            assert_eq!(
                handler
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .len(),
                1
            );

            drop(attempt);

            assert!(
                handler
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_empty(),
                "cancelling the outer future must release its pending lease"
            );
        });
    }

    #[test]
    fn ask_reply_disconnect_closes_picker_surface() {
        let tool = crate::ask::AskTool::new(crate::ask::AskPolicy::Error);
        let (ui_tx, _ui_rx) = asupersync::channel::mpsc::channel::<AskUiRequest>(4);
        tool.install_channel_ui(ui_tx);
        let current_ask = Arc::new(Mutex::new(Some(tool.clone())));
        let (reply_tx, reply_rx) = mpsc::channel::<AskUiReply>();
        drop(reply_tx);

        assert!(matches!(
            poll_ask_reply(&current_ask, &reply_rx),
            AskReplyPoll::Disconnected
        ));

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let result = runtime.block_on(async {
            asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_millis(20),
                crate::tools::Tool::execute(
                    &tool,
                    "ask-after-disconnect",
                    serde_json::json!({
                        "questions": [{
                            "question": "Too late?",
                            "options": [{"label": "A"}, {"label": "B"}]
                        }]
                    }),
                    None,
                ),
            )
            .await
        });
        let error = result
            .expect("closed picker must reject before the short mutation guard expires")
            .expect_err("closed picker must reject later cards");
        assert!(
            error.to_string().contains("picker surface closed"),
            "{error}"
        );
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
    fn extension_prompt_escape_discards_partial_answer_and_restores_draft() {
        let (_agent_tx, rx) = mpsc::channel();
        let (ext_tx, ext_rx) = mpsc::channel::<ExtensionUiResponse>();
        let model = PiFtuiModel::new(rx).with_ext_reply_channel(ext_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "saved extension draft");
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::ExtensionUiRequest(ext_request(
            "ext-escape",
            "input",
            serde_json::json!({"title": "Value?"}),
        ))));
        assert!(sim.model().input.text().is_empty());
        type_str(&mut sim, "partial card answer");

        sim.inject_event(key(KeyCode::Escape, Modifiers::empty()));

        let reply = ext_rx.try_recv().expect("extension cancellation routed");
        assert_eq!(reply.id, "ext-escape");
        assert!(reply.cancelled);
        assert!(sim.model().active_ext.is_none());
        assert_eq!(sim.model().input.text(), "saved extension draft");
        assert!(sim.model().card_draft_snapshot.is_none());
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
    fn mixed_card_burst_restores_only_the_preexisting_draft() {
        let (_agent_tx, rx) = mpsc::channel();
        let (ask_tx, _ask_rx) = mpsc::channel::<AskUiReply>();
        let (ext_tx, _ext_rx) = mpsc::channel::<ExtensionUiResponse>();
        let model = PiFtuiModel::new(rx)
            .with_ask_reply_channel(ask_tx)
            .with_ext_reply_channel(ext_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "keep this draft");
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::AskUiRequest(ask_request(
            "ask-first",
            vec![question("Pick?", &["a", "b"], false)],
        ))));
        assert!(sim.model().input.text().is_empty());
        sim.send(PiFtuiMsg::Agent(PiMsg::ExtensionUiRequest(ext_request(
            "ext-second",
            "confirm",
            serde_json::json!({"title": "Continue?"}),
        ))));

        type_str(&mut sim, "1");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            sim.model()
                .active_ext
                .as_ref()
                .map(|request| request.id.as_str()),
            Some("ext-second")
        );
        assert!(
            sim.model().input.text().is_empty(),
            "the Ask answer must not become the extension draft"
        );
        type_str(&mut sim, "yes");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));

        assert_eq!(sim.model().input.text(), "keep this draft");
        assert!(sim.model().card_draft_snapshot.is_none());
    }

    #[test]
    fn whitespace_only_draft_is_preserved_byte_for_byte_around_card() {
        let (_agent_tx, rx) = mpsc::channel();
        let (ask_tx, _ask_rx) = mpsc::channel::<AskUiReply>();
        let model = PiFtuiModel::new(rx).with_ask_reply_channel(ask_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.model_mut().input.set_text(" \n\t");
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::AskUiRequest(ask_request(
            "ask-whitespace-draft",
            vec![question("Pick?", &["a", "b"], false)],
        ))));
        assert!(
            sim.model().input.text().is_empty(),
            "card activation must clear even a whitespace-only draft"
        );

        type_str(&mut sim, "1");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));

        assert_eq!(sim.model().input.text(), " \n\t");
        assert!(sim.model().card_draft_snapshot.is_none());
    }

    #[test]
    fn extension_notification_is_nonmodal_during_active_ask() {
        let (_agent_tx, rx) = mpsc::channel();
        let (ask_tx, _ask_rx) = mpsc::channel::<AskUiReply>();
        let (ext_tx, ext_rx) = mpsc::channel::<ExtensionUiResponse>();
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
            "notify-during-ask",
            "notify",
            serde_json::json!({"title": "Heads up", "message": "Build finished"}),
        ))));

        assert_eq!(
            sim.model()
                .active_ask
                .as_ref()
                .map(|ask| ask.request.id.as_str()),
            Some("ask-hold")
        );
        assert!(sim.model().active_ext.is_none());
        assert!(sim.model().ext_queue.is_empty());
        assert!(ext_rx.try_recv().is_err(), "notification has no reply");
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|entry| entry.text.contains("Build finished")),
            "notification must remain visible in the transcript"
        );
    }

    fn assert_terminal_event_dismisses_ask_and_queued_extension(event: PiMsg) {
        let (_agent_tx, rx) = mpsc::channel();
        let (ask_tx, ask_rx) = mpsc::channel::<AskUiReply>();
        let (ext_tx, ext_rx) = mpsc::channel::<ExtensionUiResponse>();
        let model = PiFtuiModel::new(rx)
            .with_ask_reply_channel(ask_tx)
            .with_ext_reply_channel(ext_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "saved draft");
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::AskUiRequest(ask_request(
            "ask-stale",
            vec![question("Pick?", &["a", "b"], false)],
        ))));
        sim.send(PiFtuiMsg::Agent(PiMsg::ExtensionUiRequest(ext_request(
            "ext-stale",
            "confirm",
            serde_json::json!({"title": "Later?"}),
        ))));
        type_str(&mut sim, "draft");

        sim.send(PiFtuiMsg::Agent(event));

        assert!(sim.model().active_ask.is_none());
        assert!(sim.model().active_ext.is_none());
        assert!(sim.model().ext_queue.is_empty());
        assert_eq!(sim.model().input.text(), "saved draft");
        assert!(sim.model().card_draft_snapshot.is_none());
        let ask_reply = ask_rx.try_recv().expect("active Ask receives dismissal");
        assert_eq!(ask_reply.request_id, "ask-stale");
        assert!(ask_reply.response.dismissed);
        let ext_reply = ext_rx
            .try_recv()
            .expect("queued extension prompt receives cancellation");
        assert_eq!(ext_reply.id, "ext-stale");
        assert!(ext_reply.cancelled);
    }

    #[test]
    fn terminal_agent_events_invalidate_turn_owned_interactions() {
        assert_terminal_event_dismisses_ask_and_queued_extension(PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
        });
        assert_terminal_event_dismisses_ask_and_queued_extension(PiMsg::AgentError(String::from(
            "turn failed",
        )));
        assert_terminal_event_dismisses_ask_and_queued_extension(PiMsg::ConversationReset {
            session_id: String::from("replacement"),
            messages: Vec::new(),
            usage: crate::model::Usage::default(),
            status: None,
        });
    }

    #[test]
    fn agent_done_cancels_active_and_queued_extension_prompts() {
        let (_agent_tx, rx) = mpsc::channel();
        let (ext_tx, ext_rx) = mpsc::channel::<ExtensionUiResponse>();
        let model = PiFtuiModel::new(rx).with_ext_reply_channel(ext_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::ExtensionUiRequest(ext_request(
            "ext-active",
            "confirm",
            serde_json::json!({"title": "Now?"}),
        ))));
        sim.send(PiFtuiMsg::Agent(PiMsg::ExtensionUiRequest(ext_request(
            "ext-queued",
            "input",
            serde_json::json!({"title": "Later?"}),
        ))));

        sim.send(PiFtuiMsg::Agent(PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
        }));

        assert!(sim.model().active_ext.is_none());
        assert!(sim.model().ext_queue.is_empty());
        let first = ext_rx.try_recv().expect("active prompt cancelled");
        let second = ext_rx.try_recv().expect("queued prompt cancelled");
        assert_eq!(first.id, "ext-active");
        assert_eq!(second.id, "ext-queued");
        assert!(first.cancelled && second.cancelled);
    }

    #[test]
    fn ask_forwarder_guard_drops_requests_closed_before_dispatch() {
        // `RuntimeBuilder::current_thread()` still drives spawned tasks on a
        // worker thread, so a forwarder installed with `install_ask_forwarder`
        // may legitimately deliver a request before this thread closes the
        // surface (the FTUI model re-checks `channel_ui_request_is_pending`
        // for exactly that case). What must hold deterministically is the
        // forwarder guard itself: a request whose surface closed before
        // dispatch never reaches the model. Drive that step by hand.
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let tool = crate::ask::AskTool::new(crate::ask::AskPolicy::Error);
        let (agent_tx, agent_rx) = mpsc::channel();
        let (ask_ui_tx, mut ask_ui_rx) = asupersync::channel::mpsc::channel::<AskUiRequest>(4);
        tool.install_channel_ui(ask_ui_tx);

        runtime.block_on(async {
            let mut execution = Box::pin(crate::tools::Tool::execute(
                &tool,
                "ask-close-before-ftui-forward",
                serde_json::json!({
                    "questions": [{
                        "question": "Pick?",
                        "options": [{"label": "A"}, {"label": "B"}]
                    }]
                }),
                None,
            ));
            assert!(futures::poll!(execution.as_mut()).is_pending());
            assert_eq!(tool.close_channel_ui(), 1);
            drop(execution);
            let cx = crate::agent_cx::AgentCx::for_request();
            let request = ask_ui_rx
                .recv(&cx)
                .await
                .expect("the queued ask request survives the close");
            assert!(
                !forward_ask_ui_request(&tool, &agent_tx, request),
                "the forwarder guard must reject a request whose surface closed"
            );
            assert!(
                agent_rx.try_recv().is_err(),
                "closed Ask must not reach the FTUI model"
            );
        });
    }

    #[test]
    fn escape_dismisses_active_ask() {
        let (agent_tx, agent_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel::<AskUiReply>();
        let model = PiFtuiModel::new(agent_rx).with_ask_reply_channel(reply_tx);
        drop(agent_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "saved draft");
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::AskUiRequest(ask_request(
            "ask-esc",
            vec![question("Continue?", &["yes", "no"], false)],
        ))));
        assert!(sim.model().input.text().is_empty());
        type_str(&mut sim, "partial answer");
        sim.inject_event(key(KeyCode::Escape, Modifiers::empty()));
        let reply = reply_rx.try_recv().expect("dismissal sent");
        assert!(reply.response.dismissed);
        assert!(sim.model().active_ask.is_none());
        assert_eq!(sim.model().input.text(), "saved draft");
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
            session_id: "resumed-session".into(),
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

        sim.send(PiFtuiMsg::Agent(PiMsg::SessionSystemNote {
            owner_session_id: "replaced-session".into(),
            message: "stale note".into(),
        }));
        assert!(
            !sim.model()
                .transcript
                .iter()
                .any(|entry| entry.text == "stale note"),
            "an old session's note must not enter the replacement transcript"
        );

        sim.send(PiFtuiMsg::Agent(PiMsg::SessionSystemNote {
            owner_session_id: "resumed-session".into(),
            message: "current note".into(),
        }));
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|entry| entry.text == "current note"),
            "the displayed session's note must remain visible"
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
        type_str(&mut sim, "/mcp");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::Mcp {
                subcommand: String::from("list"),
                name: None,
            }
        );
        type_str(&mut sim, "/mcp trust docs");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::Mcp {
                subcommand: String::from("trust"),
                name: Some(String::from("docs")),
            }
        );
        type_str(&mut sim, "/mcp trust");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(submit_rx.try_recv().is_err());
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|entry| entry.role == EntryRole::Error && entry.text.contains("usage: /mcp"))
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

    // ── Slash-command completion popup (issue #208) ─────────────────────

    #[test]
    fn slash_prefix_opens_completion_popup_with_descriptions() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/he");
        let model = sim.model();
        assert!(
            model.completion_visible(),
            "a slash prefix must open the popup"
        );
        assert_eq!(model.autocomplete.items[0].label, "/help");
        assert_eq!(
            model.autocomplete.selected, None,
            "nothing is highlighted until the user navigates"
        );
        let rendered = buffer_text(sim.capture_frame(80, 12), 80, 12);
        assert!(
            rendered.contains("/help"),
            "popup row missing: {rendered:?}"
        );
        assert!(
            rendered.contains("Show help for interactive commands"),
            "description missing: {rendered:?}"
        );
        assert!(
            rendered.contains("Tab/Enter accept"),
            "keyboard hint missing: {rendered:?}"
        );
    }

    #[test]
    fn plain_text_never_opens_completion_popup() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "hello");
        assert!(
            !sim.model().autocomplete.open,
            "prose must not open the popup"
        );
        assert_eq!(sim.model().completion_rows(), 0);
        // A slash mid-message is not a command either.
        type_str(&mut sim, " /he");
        assert!(!sim.model().autocomplete.open, "mid-message slash is prose");
        let rendered = buffer_text(sim.capture_frame(80, 12), 80, 12);
        assert!(
            !rendered.contains("Tab/Enter accept"),
            "no popup chrome for prose: {rendered:?}"
        );
    }

    #[test]
    fn tab_accepts_first_completion_and_enter_then_submits_the_command() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/he");
        sim.inject_event(key(KeyCode::Tab, Modifiers::empty()));
        assert_eq!(sim.model().input.text(), "/help", "Tab completes the token");
        assert!(!sim.model().autocomplete.open, "accepting closes the popup");
        assert!(
            submit_rx.try_recv().is_err(),
            "Tab must not submit anything"
        );
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(
            sim.model().input.is_empty(),
            "Enter submits the completed draft"
        );
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.text.contains("ftui preview commands")),
            "the completed /help must route like a typed one"
        );
    }

    #[test]
    fn arrow_keys_navigate_and_enter_accepts_the_highlighted_row() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/");
        assert!(
            sim.model().autocomplete.items.len() > 2,
            "bare slash lists commands"
        );
        sim.inject_event(key(KeyCode::Down, Modifiers::empty()));
        assert_eq!(sim.model().autocomplete.selected, Some(0));
        sim.inject_event(key(KeyCode::Down, Modifiers::empty()));
        assert_eq!(sim.model().autocomplete.selected, Some(1));
        sim.inject_event(key(KeyCode::Up, Modifiers::empty()));
        assert_eq!(sim.model().autocomplete.selected, Some(0));
        // Up from the top wraps to the last row.
        sim.inject_event(key(KeyCode::Up, Modifiers::empty()));
        let last = sim.model().autocomplete.items.len() - 1;
        assert_eq!(sim.model().autocomplete.selected, Some(last));
        let expected = sim.model().autocomplete.items[last].insert.clone();
        let rendered = buffer_text(sim.capture_frame(80, 14), 80, 14);
        assert!(
            rendered.contains(&format!("▸ {expected}")),
            "highlighted row must stay in the rendered window: {rendered:?}"
        );
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            sim.model().input.text(),
            expected,
            "Enter accepts the highlight"
        );
        assert!(!sim.model().autocomplete.open);
        assert!(
            submit_rx.try_recv().is_err(),
            "accepting a row must not submit the draft"
        );
        assert!(sim.model().transcript.is_empty());
    }

    #[test]
    fn enter_without_highlight_submits_the_draft_verbatim() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/help");
        assert!(
            sim.model().completion_visible(),
            "exact command still lists matches"
        );
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(sim.model().input.is_empty());
        assert!(!sim.model().autocomplete.open);
        assert!(
            sim.model()
                .transcript
                .iter()
                .any(|e| e.text.contains("ftui preview commands")),
            "Enter with no highlight submits what was typed"
        );
    }

    #[test]
    fn escape_dismisses_popup_and_typing_reopens_it() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/he");
        sim.inject_event(key(KeyCode::Escape, Modifiers::empty()));
        assert!(!sim.model().autocomplete.open, "Esc closes the popup");
        assert_eq!(sim.model().input.text(), "/he", "Esc keeps the draft");
        assert_eq!(sim.model().completion_rows(), 0);
        type_str(&mut sim, "l");
        assert!(sim.model().completion_visible(), "the next edit recomputes");
        assert_eq!(sim.model().autocomplete.items[0].label, "/help");
    }

    #[test]
    fn fuzzy_query_still_offers_the_command() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/hlp");
        assert!(
            sim.model()
                .autocomplete
                .items
                .iter()
                .any(|item| item.label == "/help"),
            "subsequence matches ride the shared fuzzy matcher: {:?}",
            sim.model().autocomplete.items
        );
    }

    #[test]
    fn popup_closes_when_the_agent_starts_working() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/he");
        assert!(sim.model().completion_visible());
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        assert!(!sim.model().autocomplete.open, "a turn owns the editor");
        assert_eq!(sim.model().completion_rows(), 0);
        let rendered = buffer_text(sim.capture_frame(80, 12), 80, 12);
        assert!(!rendered.contains("Tab/Enter accept"), "{rendered:?}");
    }

    #[test]
    fn catalog_message_makes_extension_commands_completable() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/dep");
        assert!(
            !sim.model()
                .autocomplete
                .items
                .iter()
                .any(|i| i.label == "/deploy"),
            "unknown command before the catalog arrives"
        );
        let catalog = AutocompleteCatalog {
            extension_commands: vec![crate::autocomplete::NamedEntry {
                name: String::from("deploy"),
                description: Some(String::from("Ship the current branch")),
            }],
            ..AutocompleteCatalog::default()
        };
        sim.send(PiFtuiMsg::Agent(PiMsg::AutocompleteCatalog(catalog)));
        assert!(!sim.model().autocomplete.open, "a stale popup is dropped");
        type_str(&mut sim, "l");
        let model = sim.model();
        assert!(model.completion_visible());
        assert_eq!(model.autocomplete.items[0].label, "/deploy");
        assert_eq!(
            model.autocomplete.items[0].kind,
            AutocompleteItemKind::ExtensionCommand
        );
        let rendered = buffer_text(sim.capture_frame(80, 12), 80, 12);
        assert!(rendered.contains("Ship the current branch"), "{rendered:?}");
    }

    #[test]
    fn popup_height_caps_at_max_visible_and_scrolls_to_the_highlight() {
        let (_agent_tx, rx) = mpsc::channel();
        let model = PiFtuiModel::new(rx).with_autocomplete(AutocompleteLaunch {
            catalog: AutocompleteCatalog::default(),
            cwd: std::path::PathBuf::from("."),
            max_visible: 3,
        });
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.inject_event(Event::Resize {
            width: 80,
            height: 20,
        });
        let body_before = sim.model().body_height();
        type_str(&mut sim, "/");
        let total = sim.model().autocomplete.items.len();
        assert!(total > 3, "need more commands than rows for this test");
        assert_eq!(sim.model().completion_rows(), 4, "3 rows + hint");
        assert_eq!(
            sim.model().body_height(),
            body_before - 4,
            "the popup takes its rows from the conversation body"
        );
        let rendered = buffer_text(sim.capture_frame(80, 20), 80, 20);
        assert!(
            rendered.contains(&format!("3/{total} shown")),
            "overflow counter missing: {rendered:?}"
        );
        // Highlight the fourth row: the window scrolls so it stays visible.
        for _ in 0..4 {
            sim.inject_event(key(KeyCode::Down, Modifiers::empty()));
        }
        assert_eq!(sim.model().autocomplete.selected, Some(3));
        let fourth = sim.model().autocomplete.items[3].label.clone();
        let first = sim.model().autocomplete.items[0].label.clone();
        let rendered = buffer_text(sim.capture_frame(80, 20), 80, 20);
        assert!(
            rendered.contains(&format!("▸ {fourth}")),
            "scrolled window must show the highlight: {rendered:?}"
        );
        assert!(
            !rendered.contains(&format!("  {first} ")),
            "first row scrolled out of the window: {rendered:?}"
        );
        assert!(
            rendered.contains(&format!("4/{total} shown")),
            "counter follows the window: {rendered:?}"
        );
    }

    #[test]
    fn accepting_a_completion_replaces_only_the_command_token() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        // The provider's token range covers `/he`; the accepted insert must
        // replace exactly that span even with the cursor mid-token.
        type_str(&mut sim, "/he");
        sim.inject_event(key(KeyCode::Left, Modifiers::empty()));
        assert!(
            sim.model().completion_visible(),
            "cursor moves recompute the popup"
        );
        sim.inject_event(key(KeyCode::Tab, Modifiers::empty()));
        assert_eq!(sim.model().input.text(), "/help");
        assert_eq!(
            sim.model().input.cursor().grapheme,
            "/help".len(),
            "cursor parks at the end of the accepted draft"
        );
    }

    #[test]
    fn layout_reserves_the_completion_rows_above_the_editor() {
        let area = Rect::new(0, 0, 80, 20);
        let regions = layout_regions(area, 1, 0, 4);
        assert_eq!(regions.completion.height, 4);
        assert_eq!(
            regions.completion.y + regions.completion.height,
            regions.input.y
        );
        assert_eq!(regions.status.y + 1, regions.completion.y);
        let closed = layout_regions(area, 1, 0, 0);
        assert_eq!(closed.completion.height, 0);
        assert_eq!(closed.body.height, regions.body.height + 4);
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
            output: None,
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
            output: None,
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
    /// #209: a provider failure at turn end translates into ONE structured
    /// card (provider · HTTP status · retry status · bounded detail) carried
    /// by `AgentDone`; the `ProviderError` event itself is silent for the UI
    /// and an abort keeps its plain message.
    #[test]
    fn agent_end_provider_error_translates_to_structured_card() {
        use crate::agent::AgentEvent as E;
        use crate::model::{AssistantMessage, Message};
        use std::sync::Arc;

        let raw = "Provider error: deepseek: OpenAI API error (HTTP 503): \
{\"error\":{\"code\":\"service_unavailable_error\",\"message\":\"Server Overloaded\"}}";
        let assistant = Arc::new(AssistantMessage {
            provider: "deepseek".into(),
            stop_reason: StopReason::Error,
            error_message: Some(raw.into()),
            ..Default::default()
        });
        let msgs = agent_event_to_pi_msgs(&E::AgentEnd {
            session_id: Arc::from("s1"),
            messages: vec![Message::Assistant(Arc::clone(&assistant))],
            error: Some(raw.into()),
        });
        let [
            PiMsg::AgentDone {
                stop_reason: StopReason::Error,
                error_message: Some(card),
                ..
            },
        ] = msgs.as_slice()
        else {
            // ubs:ignore panic in #[cfg(test)] let-else is an assertion failure, not library code
            panic!("unexpected translation: {msgs:?}");
        };
        let mut lines = card.lines();
        assert_eq!(
            lines.next(),
            Some("Provider error: deepseek: HTTP 503 (service unavailable / overloaded)")
        );
        assert!(
            lines
                .next()
                .is_some_and(|line| line.contains("not auto-retried")),
            "retry status line missing: {card}"
        );
        assert!(
            lines
                .next()
                .is_some_and(|line| line.starts_with("Detail: OpenAI API error (HTTP 503)")),
            "detail line missing: {card}"
        );

        let summary = crate::error::ProviderErrorSummary::from_error_text(Some("deepseek"), raw);
        assert!(
            agent_event_to_pi_msgs(&E::ProviderError {
                session_id: Arc::from("s1"),
                provider: "deepseek".into(),
                model: "deepseek-v4-pro".into(),
                summary,
                message: raw.into(),
            })
            .is_empty(),
            "ProviderError must not double-post the card"
        );

        let aborted = Arc::new(AssistantMessage {
            stop_reason: StopReason::Aborted,
            error_message: Some("Aborted".into()),
            ..Default::default()
        });
        let msgs = agent_event_to_pi_msgs(&E::AgentEnd {
            session_id: Arc::from("s1"),
            messages: vec![Message::Assistant(aborted)],
            error: Some("Aborted".into()),
        });
        assert!(
            matches!(
                msgs.as_slice(),
                [PiMsg::AgentDone { error_message: Some(text), .. }] if text == "Aborted"
            ),
            "abort must keep its plain message: {msgs:?}"
        );

        // The card lands in the transcript as an error entry at turn end,
        // even when partial text streamed first.
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, _submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::TextDelta(String::from("partial "))));
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Error,
            error_message: Some(card.clone()),
        }));
        let transcript = &sim.model().transcript;
        assert!(
            transcript
                .iter()
                .any(|e| e.role == EntryRole::Assistant && e.text.starts_with("partial")),
            "partial text must be kept"
        );
        assert!(
            transcript.iter().any(|e| e.role == EntryRole::Error
                && e.text.starts_with("Provider error: deepseek: HTTP 503")),
            "turn-end error card missing: {transcript:?}"
        );
        assert_eq!(sim.model().state, AgentUiState::Ready);
    }

    #[test]
    fn agent_error_pins_banner_and_send_dismisses() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentError(String::from("boom"))));
        assert_eq!(
            sim.model().error_banner.as_deref(),
            Some("boom"),
            "AgentError pins the banner"
        );
        // Not duplicated into the transcript.
        assert!(
            !sim.model()
                .transcript
                .iter()
                .any(|e| e.text.contains("boom"))
        );
        let rendered = buffer_text(sim.capture_frame(60, 12), 60, 12);
        assert!(rendered.contains("✗ boom"), "banner missing: {rendered:?}");
        // The next sent input dismisses it and still routes the prompt.
        type_str(&mut sim, "hi");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(sim.model().error_banner, None, "send must dismiss");
        assert_eq!(
            submit_rx.try_recv().expect("routed"),
            UiCommand::Prompt(String::from("hi"))
        );
    }
    #[test]
    fn consecutive_reads_group_with_counter() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        for _ in 0..3 {
            sim.send(PiFtuiMsg::Agent(PiMsg::ToolStart {
                name: "read".into(),
                tool_id: "t".into(),
            }));
            sim.send(PiFtuiMsg::Agent(PiMsg::ToolEnd {
                name: "read".into(),
                tool_id: "t".into(),
                is_error: false,
                output: None,
            }));
        }
        let read_cards: Vec<_> = sim
            .model()
            .transcript
            .iter()
            .filter(|e| e.text == "read")
            .collect();
        assert_eq!(read_cards.len(), 1, "reads must collapse into one card");
        assert_eq!(read_cards[0].group_count, 3);
        // A non-read entry between runs splits the group.
        sim.send(PiFtuiMsg::Agent(PiMsg::System(String::from("note"))));
        sim.send(PiFtuiMsg::Agent(PiMsg::ToolStart {
            name: "read".into(),
            tool_id: "t2".into(),
        }));
        sim.send(PiFtuiMsg::Agent(PiMsg::ToolEnd {
            name: "read".into(),
            tool_id: "t2".into(),
            is_error: false,
            output: None,
        }));
        assert_eq!(
            sim.model()
                .transcript
                .iter()
                .filter(|e| e.text == "read")
                .count(),
            2,
            "intervening entry must split the group"
        );
        let rendered = buffer_text(sim.capture_frame(60, 16), 60, 16);
        assert!(
            rendered.contains("×3"),
            "group counter missing: {rendered:?}"
        );
    }
    #[test]
    fn word_diff_parts_pairs_shared_framing() {
        let (prefix, removed_mid, added_mid, suffix) =
            word_diff_parts("foo bar baz", "foo qux baz").expect("paired");
        assert_eq!(prefix, "foo ");
        assert_eq!(removed_mid, "bar");
        assert_eq!(added_mid, "qux");
        assert_eq!(suffix, " baz");
    }

    #[test]
    fn word_diff_parts_rejects_unframed_and_identical() {
        // Word-identical lines are a no-change pair.
        assert!(word_diff_parts("same line", "same line").is_none());
        // Nothing shared: a bare middle would not read as a focused change.
        assert!(word_diff_parts("alpha beta", "gamma delta").is_none());
        // Prefix-only framing still pairs, with an empty suffix.
        let (prefix, removed_mid, added_mid, suffix) =
            word_diff_parts("keep a", "keep b").expect("paired");
        assert_eq!(prefix, "keep ");
        assert_eq!(
            (removed_mid.as_str(), added_mid.as_str(), suffix.as_str()),
            ("a", "b", "")
        );
    }

    // ── Issue #201: per-message render cache ─────────────────────────────

    /// Complete one streamed assistant turn, leaving `text` as a transcript
    /// entry.
    fn finish_turn(sim: &mut ProgramSimulator<PiFtuiModel>, text: &str) {
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::TextDelta(text.into())));
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
        }));
    }

    #[test]
    fn render_cache_makes_warm_frames_o_changed_not_o_transcript() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        for i in 0..4 {
            finish_turn(&mut sim, &format!("message **{i}** body"));
        }
        assert_eq!(sim.model().transcript.len(), 4);
        let _ = sim.model().conversation_text();
        assert_eq!(
            sim.model().render_stats.get(),
            (4, 0),
            "cold frame renders every block"
        );
        let _ = sim.model().conversation_text();
        assert_eq!(
            sim.model().render_stats.get(),
            (0, 4),
            "warm frame must reuse every unchanged block"
        );
        finish_turn(&mut sim, "one more");
        let _ = sim.model().conversation_text();
        assert_eq!(
            sim.model().render_stats.get(),
            (1, 4),
            "only the new entry may render"
        );
        let _ = sim.model().conversation_text();
        assert_eq!(sim.model().render_stats.get(), (0, 5));
    }

    #[test]
    fn render_cache_skips_pending_cards_and_settles_finished_ones() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        finish_turn(&mut sim, "before the tool");
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::ToolStart {
            name: "bash".into(),
            tool_id: "t1".into(),
        }));
        let _ = sim.model().conversation_text();
        let _ = sim.model().conversation_text();
        assert_eq!(
            sim.model().render_stats.get(),
            (1, 1),
            "a pending card renders fresh every frame (spinner-dependent)"
        );
        sim.send(PiFtuiMsg::Agent(PiMsg::ToolEnd {
            name: "bash".into(),
            tool_id: "t1".into(),
            is_error: false,
            output: Some("ok".into()),
        }));
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
        }));
        let _ = sim.model().conversation_text();
        let (rendered, _) = sim.model().render_stats.get();
        assert!(
            rendered >= 1,
            "the settled card must re-render once after mutation"
        );
        let _ = sim.model().conversation_text();
        let (rendered, reused) = sim.model().render_stats.get();
        assert_eq!(rendered, 0, "settled card caches like any other block");
        assert_eq!(reused, sim.model().transcript.len());
    }

    #[test]
    fn render_cache_leaves_streaming_tail_uncached() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentStart));
        sim.send(PiFtuiMsg::Agent(PiMsg::TextDelta("streaming tail".into())));
        let text = sim.model().conversation_text();
        assert!(
            text.lines()
                .iter()
                .any(|line| line.to_plain_text().contains("streaming tail")),
            "streaming tail must render"
        );
        assert_eq!(
            sim.model().render_stats.get(),
            (0, 0),
            "the in-flight tail is not a cached block"
        );
    }

    #[test]
    fn render_cache_flushes_on_theme_change() {
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        finish_turn(&mut sim, "themed message");
        let _ = sim.model().conversation_text();
        let _ = sim.model().conversation_text();
        assert_eq!(sim.model().render_stats.get(), (0, 1));
        type_str(&mut sim, "/theme");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty())); // apply "dark"
        let _ = sim.model().conversation_text();
        let (rendered, reused) = sim.model().render_stats.get();
        assert_eq!(reused, 0, "theme change must drop every cached block");
        assert_eq!(rendered, sim.model().transcript.len());
    }

    // ── gh #195: tables fit the terminal width ───────────────────────────

    #[test]
    fn markdown_tables_fit_the_terminal_width_and_refit_on_resize() {
        let source = "| Column one with a long header | Column two with a longer header | Column three |\n\
                      |---|---|---|\n\
                      | first cell has quite a lot of text in it | second cell also has a lot of text | third |\n\
                      | short | short | short |";
        let (_tx, model) = new_model();
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        sim.inject_event(Event::Resize {
            width: 48,
            height: 24,
        });
        finish_turn(&mut sim, source);
        let widest = |model: &PiFtuiModel| {
            model
                .conversation_text()
                .lines()
                .iter()
                .map(|line| line.to_plain_text().chars().count())
                .max()
                .unwrap_or(0)
        };
        let narrow = widest(sim.model());
        assert!(
            narrow <= 48,
            "table must be fitted to a 48-column terminal, widest rendered line is {narrow}"
        );

        // A width change must drop the cache so cached table blocks re-fit.
        sim.inject_event(Event::Resize {
            width: 120,
            height: 24,
        });
        let _ = sim.model().conversation_text();
        let (rendered, reused) = sim.model().render_stats.get();
        assert_eq!(reused, 0, "width change must drop every cached block");
        assert_eq!(rendered, sim.model().transcript.len());
        let wide = widest(sim.model());
        assert!(
            wide > narrow,
            "a wider terminal must let the table use more width (narrow={narrow}, wide={wide})"
        );

        // A height-only resize keeps the cache warm.
        let _ = sim.model().conversation_text();
        sim.inject_event(Event::Resize {
            width: 120,
            height: 40,
        });
        let _ = sim.model().conversation_text();
        let (_, reused) = sim.model().render_stats.get();
        assert!(
            reused > 0,
            "height-only resize must not flush the render cache"
        );
    }

    // ── Issue #202: compact markdown spacing ─────────────────────────────

    #[test]
    fn compact_spacing_collapses_paragraph_gaps_keeps_heading_and_fence_air() {
        let source = "para one\n\npara two\n\n# Section\n\nbody text\n\n```rust\nlet x = 1;\n```\n\ntail line";
        let render = |spacing: crate::config::MarkdownSpacing| {
            let (_tx, rx) = mpsc::channel();
            let model = PiFtuiModel::new(rx).with_markdown_spacing(spacing);
            let mut sim = ProgramSimulator::new(model);
            sim.init();
            finish_turn(&mut sim, source);
            sim.model()
                .conversation_text()
                .lines()
                .iter()
                .map(ftui::text::Line::to_plain_text)
                .collect::<Vec<_>>()
        };
        let comfortable = render(crate::config::MarkdownSpacing::Comfortable);
        let compact = render(crate::config::MarkdownSpacing::Compact);
        assert!(
            compact.len() < comfortable.len(),
            "compact must be denser: comfortable={comfortable:?} compact={compact:?}"
        );
        let para = compact
            .iter()
            .position(|l| l.contains("para one"))
            .expect("para one rendered");
        assert!(
            compact[para + 1].contains("para two"),
            "paragraph gap must collapse: {compact:?}"
        );
        let head = compact
            .iter()
            .position(|l| l.contains("Section"))
            .expect("heading rendered");
        assert!(
            compact[head - 1].trim().is_empty(),
            "heading keeps a blank above: {compact:?}"
        );
        assert!(
            compact[head + 1].trim().is_empty(),
            "heading keeps a blank below: {compact:?}"
        );
        let code = compact
            .iter()
            .position(|l| l.contains("let x = 1;"))
            .expect("code line rendered");
        assert!(
            compact[code].starts_with("  "),
            "code body keeps its indent: {compact:?}"
        );
        let tail = compact
            .iter()
            .position(|l| l.contains("tail line"))
            .expect("tail rendered");
        assert!(
            compact[tail - 1].trim().is_empty(),
            "fence keeps a blank below: {compact:?}"
        );
    }

    #[test]
    fn comfortable_spacing_is_the_unchanged_default() {
        let (_tx, model) = new_model();
        assert_eq!(
            model.markdown_spacing,
            crate::config::MarkdownSpacing::Comfortable
        );
    }

    // ── Issue #203: busy indicator for out-of-turn driver operations ─────

    #[test]
    fn model_switch_arms_busy_spinner_until_driver_replies() {
        use ftui::runtime::simulator::CmdRecord;
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, _submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/model openai/gpt-5");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            sim.model().busy_label(),
            Some("switching model to openai/gpt-5 ..."),
            "routing /model must arm the busy indicator"
        );
        assert!(
            matches!(sim.command_log().last(), Some(CmdRecord::Tick(_))),
            "arming busy must start the spinner tick chain"
        );
        // Ticks animate and re-arm while busy even though no turn runs.
        let before = sim.model().spinner.current_frame;
        sim.inject_event(Event::Tick);
        assert_eq!(sim.model().spinner.current_frame, before + 1);
        assert!(matches!(sim.command_log().last(), Some(CmdRecord::Tick(_))));
        let spin = DOTS[sim.model().spinner.current_frame % DOTS.len()];
        let rendered = buffer_text(sim.capture_frame(50, 10), 50, 10);
        assert!(
            rendered.contains(&format!("{spin} switching model to")),
            "status region missing busy spinner: {rendered:?}"
        );
        // The driver's reply clears busy and parks the ticks.
        sim.send(PiFtuiMsg::Agent(PiMsg::System(
            "model set to openai/gpt-5".into(),
        )));
        assert!(sim.model().busy.is_none(), "driver reply must clear busy");
        let frame = sim.model().spinner.current_frame;
        sim.inject_event(Event::Tick);
        assert_eq!(sim.model().spinner.current_frame, frame);
        assert!(matches!(sim.command_log().last(), Some(CmdRecord::None)));
    }

    #[test]
    fn resume_picker_selection_arms_busy_indicator() {
        use ftui::runtime::simulator::CmdRecord;
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx)
            .with_submit_channel(submit_tx)
            .with_available_sessions(vec![("old session · 3 msgs".into(), "/tmp/s.jsonl".into())]);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/resume");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert!(sim.model().picker.is_some(), "picker must open");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(
            submit_rx.try_recv().expect("resume routed"),
            UiCommand::ResumeSession {
                path: "/tmp/s.jsonl".into()
            }
        );
        assert_eq!(sim.model().busy_label(), Some("loading session ..."));
        assert!(
            matches!(sim.command_log().last(), Some(CmdRecord::Tick(_))),
            "picker selection must start the spinner tick chain"
        );
    }

    #[test]
    fn extension_command_busy_survives_its_own_tool_card_until_it_ends() {
        let (_agent_tx, rx) = mpsc::channel();
        let (submit_tx, _submit_rx) = mpsc::channel::<UiCommand>();
        let model = PiFtuiModel::new(rx).with_submit_channel(submit_tx);
        let mut sim = ProgramSimulator::new(model);
        sim.init();
        type_str(&mut sim, "/mycmd arg");
        sim.inject_event(key(KeyCode::Enter, Modifiers::empty()));
        assert_eq!(sim.model().busy_label(), Some("running /mycmd ..."));
        // The driver renders the command as a tool card; its start/progress
        // must not clear the busy state — only the settled result does.
        sim.send(PiFtuiMsg::Agent(PiMsg::ToolStart {
            name: "/mycmd".into(),
            tool_id: "ftui-ext-command".into(),
        }));
        assert!(
            sim.model().busy.is_some(),
            "ToolStart must not clear the busy label"
        );
        sim.send(PiFtuiMsg::Agent(PiMsg::ToolEnd {
            name: "/mycmd".into(),
            tool_id: "ftui-ext-command".into(),
            is_error: false,
            output: None,
        }));
        assert!(sim.model().busy.is_none(), "ToolEnd settles the busy op");
    }
}

#[cfg(test)]
mod loop_watchdog_tests {
    use super::{LOOP_STALL_BUDGET, LoopPhase, LoopWatchdog};
    use std::time::{Duration, Instant};

    /// A probe start far enough in the past that `finish` sees `over` as the
    /// measured phase duration.
    fn started_ago(over: Duration) -> Option<Instant> {
        Instant::now().checked_sub(over)
    }

    #[test]
    fn disabled_watchdog_reads_no_clock_and_records_nothing() {
        let wd = LoopWatchdog::with_enabled(false);
        // The zero-overhead contract: with telemetry off, `start` must not
        // hand back an Instant, so no clock is read on the hot path.
        assert!(wd.start().is_none());
        // Even a deliberately over-budget probe stays unrecorded.
        wd.finish(LoopPhase::Render, None);
        let snap = wd.snapshot();
        assert_eq!(snap["verdict"], "disabled");
        assert_eq!(snap["enabled"], false);
        assert_eq!(snap["totals"]["stalls"], 0);
        assert_eq!(snap["phases"]["render"]["samples"], 0);
    }

    #[test]
    fn enabled_watchdog_attributes_samples_per_phase() {
        let wd = LoopWatchdog::with_enabled(true);
        assert!(wd.start().is_some(), "enabled watchdog reads the clock");
        for phase in [LoopPhase::Render, LoopPhase::Input, LoopPhase::AgentEvent] {
            wd.finish(phase, wd.start());
        }
        wd.finish(LoopPhase::Render, wd.start());

        let snap = wd.snapshot();
        assert_eq!(snap["schema"], "pi.tui.loop_watchdog.v1");
        assert_eq!(snap["surface"], "ftui");
        assert_eq!(snap["phases"]["render"]["samples"], 2);
        assert_eq!(snap["phases"]["input"]["samples"], 1);
        assert_eq!(snap["phases"]["agent_event"]["samples"], 1);
        // Sub-budget work is not a stall.
        assert_eq!(snap["totals"]["stalls"], 0);
        assert_eq!(snap["verdict"], "pass");
    }

    #[test]
    fn over_budget_phase_is_counted_and_attributed() {
        let wd = LoopWatchdog::with_enabled(true);
        let Some(started) = started_ago(LOOP_STALL_BUDGET + Duration::from_millis(50)) else {
            return; // Monotonic clock too young to subtract from; nothing to assert.
        };
        wd.finish(LoopPhase::AgentEvent, Some(started));

        let snap = wd.snapshot();
        assert_eq!(snap["totals"]["stalls"], 1);
        assert_eq!(snap["phases"]["agent_event"]["stalls"], 1);
        // Attribution must be exact: a stalled agent-event phase never shows
        // up against render or input.
        assert_eq!(snap["phases"]["render"]["stalls"], 0);
        assert_eq!(snap["phases"]["input"]["stalls"], 0);
        assert_eq!(snap["verdict"], "warn");
        let worst = snap["phases"]["agent_event"]["worst_us"]
            .as_u64()
            .expect("worst_us is a number");
        assert!(
            worst >= u64::try_from(LOOP_STALL_BUDGET.as_micros()).unwrap_or(u64::MAX),
            "worst_us {worst} should be at least the budget"
        );
    }

    #[test]
    fn sustained_stall_latches_until_a_phase_comes_back_in_budget() {
        let wd = LoopWatchdog::with_enabled(true);
        let over = LOOP_STALL_BUDGET + Duration::from_millis(10);
        let Some(first) = started_ago(over) else {
            return;
        };

        // Three consecutive over-budget renders: all three count, but the
        // latch means only the first would have logged.
        wd.finish(LoopPhase::Render, Some(first));
        assert!(wd.stalled.get(), "first over-budget call arms the latch");
        for _ in 0..2 {
            let Some(again) = started_ago(over) else {
                return;
            };
            wd.finish(LoopPhase::Render, Some(again));
            assert!(wd.stalled.get(), "latch stays armed through the stall");
        }
        assert_eq!(wd.snapshot()["totals"]["stalls"], 3);

        // Recovery re-arms reporting so a later stall is not swallowed.
        wd.finish(LoopPhase::Render, wd.start());
        assert!(
            !wd.stalled.get(),
            "an in-budget phase clears the latch for the next stall"
        );
    }
}
