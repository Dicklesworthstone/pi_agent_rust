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
use ftui::{Cmd, Event, Frame, KeyCode, Model, Modifiers};

use crate::interactive::PiMsg;

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

/// Seed ftui model: proves the Elm loop shape against real pi message types.
///
/// Covers init/update/view/subscriptions end to end but holds only what its
/// tests assert on; the real conversation state migrates here from
/// `interactive::state` as the view port proceeds.
pub struct PiFtuiModel {
    /// One-line status: what the agent is doing right now.
    status: String,
    /// Sanitized transcript lines (completed messages / system notes).
    transcript: Vec<String>,
    /// Sanitized in-flight assistant text (streaming deltas accumulate here).
    streaming: String,
    /// Shared slot for the agent-event receiver: `subscriptions()` re-declares
    /// the bridge each cycle, and the one instance the runtime actually starts
    /// takes the receiver out of this slot (see [`AgentEventSubscription`]).
    agent_rx: Arc<Mutex<Option<Receiver<PiMsg>>>>,
}

impl PiFtuiModel {
    pub fn new(agent_rx: Receiver<PiMsg>) -> Self {
        Self {
            status: String::from("ready"),
            transcript: Vec::new(),
            streaming: String::new(),
            agent_rx: Arc::new(Mutex::new(Some(agent_rx))),
        }
    }

    fn handle_agent(&mut self, msg: PiMsg) -> Cmd<PiFtuiMsg> {
        match msg {
            PiMsg::AgentStart => {
                self.status = String::from("working");
            }
            PiMsg::TextDelta(delta) => {
                // Adversarial-content safety: agent/tool text is sanitized
                // before it can ever reach a frame.
                self.streaming.push_str(&sanitize(&delta));
            }
            PiMsg::AgentDone { error_message, .. } => {
                if !self.streaming.is_empty() {
                    self.transcript.push(std::mem::take(&mut self.streaming));
                }
                if let Some(err) = error_message {
                    self.transcript.push(format!("error: {}", sanitize(&err)));
                }
                self.status = String::from("ready");
            }
            PiMsg::AgentError(err) => {
                self.transcript.push(format!("error: {}", sanitize(&err)));
                self.status = String::from("ready");
            }
            PiMsg::System(text) | PiMsg::SystemNote(text) => {
                self.transcript.push(sanitize(&text).into_owned());
            }
            PiMsg::UiShutdown => return Cmd::quit(),
            // Remaining variants are wired up as their owning surfaces are
            // ported (tools panel, todo footer, ask cards, OAuth flows, ...).
            _ => {}
        }
        Cmd::none()
    }

    fn handle_term(event: &Event) -> Cmd<PiFtuiMsg> {
        if let Event::Key(key) = event {
            let ctrl_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(Modifiers::CTRL);
            if ctrl_c {
                return Cmd::quit();
            }
        }
        Cmd::none()
    }
}

impl Model for PiFtuiModel {
    type Message = PiFtuiMsg;

    fn update(&mut self, msg: PiFtuiMsg) -> Cmd<PiFtuiMsg> {
        match msg {
            PiFtuiMsg::Term(event) => Self::handle_term(&event),
            PiFtuiMsg::Agent(agent) => self.handle_agent(agent),
        }
    }

    fn view(&self, frame: &mut Frame) {
        // Minimal placeholder view: transcript tail + streaming text + status.
        // The real port replaces this with the conversation/editor/footer
        // layout; this exists so simulator tests exercise a full frame pass.
        let mut body = String::new();
        for line in &self.transcript {
            body.push_str(line);
            body.push('\n');
        }
        if !self.streaming.is_empty() {
            body.push_str(&self.streaming);
            body.push('\n');
        }
        body.push_str(&self.status);
        let area = Rect::new(0, 0, frame.width(), frame.height());
        Paragraph::new(Text::raw(&body)).render(area, frame);
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
        assert_eq!(sim.model().status, "working");
        sim.send(PiFtuiMsg::Agent(PiMsg::TextDelta("hello ".into())));
        sim.send(PiFtuiMsg::Agent(PiMsg::TextDelta("world".into())));
        assert_eq!(sim.model().streaming, "hello world");
        sim.send(PiFtuiMsg::Agent(PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
        }));
        assert_eq!(sim.model().status, "ready");
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
        let rendered = buffer_text(sim.capture_frame(40, 6), 40, 6);
        assert!(
            rendered.contains("session restored"),
            "frame missing transcript line: {rendered:?}"
        );
        assert!(rendered.contains("ready"), "frame missing status");
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
    fn subscription_id_is_stable() {
        let (_tx, rx) = mpsc::channel::<PiMsg>();
        let sub = AgentEventSubscription::new(rx);
        assert_eq!(sub.id(), AGENT_EVENTS_SUB_ID);
    }
}
