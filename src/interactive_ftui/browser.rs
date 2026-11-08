//! Local approval boundary for the loopback browser companion.
//!
//! Pairing links live only in this terminal panel. Browser proposals are
//! plain text, carry their original session identity, and cannot reach the
//! SDK until a local key press and the driver's final validity check agree.

use super::{
    AgentUiState, Cmd, Event, Frame, KeyCode, Modifiers, Paragraph, PiFtuiModel, PiFtuiMsg, PiMsg,
    Rect, Sender, StopSignal, SubId, Subscription, SubscriptionSender, Text, TurnAbortSlot,
    TurnControlSlot, UiCommand, Widget, WrapMode, sanitize, strip_command,
};
use crate::agent::AbortHandle;
use crate::web_remote::{CompanionHandle, PromptProposal, TokenKind};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};

const SUB_ID: SubId = 0x5049_5742; // "PIWB"
const MAX_LOCAL_PROPOSALS: usize = 16;
const MAX_PROMPT_BYTES: usize = 8 * 1024;

/// Only server proposals and local driver completion cross this channel.
#[derive(Debug)]
pub enum BrowserEvent {
    Proposal(PromptProposal),
    Finished {
        request_id: String,
        owner_session_id: String,
        error: Option<String>,
    },
}

/// The approval and its cancellation authority survive driver queueing.
#[derive(Debug, Clone)]
pub struct PromptRequest {
    proposal: PromptProposal,
    abort: AbortHandle,
}

impl PartialEq for PromptRequest {
    fn eq(&self, other: &Self) -> bool {
        self.proposal == other.proposal
    }
}

impl Eq for PromptRequest {}

enum Panel {
    Pairing(String),
    Proposal,
    Notice(String),
}

type EventReceiver = Arc<Mutex<Option<mpsc::Receiver<BrowserEvent>>>>;

pub(super) struct CompanionUi {
    handle: CompanionHandle,
    view_only: bool,
    pending: VecDeque<PromptProposal>,
    request: Option<PromptRequest>,
    panel: Option<Panel>,
    scroll: usize,
    events: EventReceiver,
}

impl CompanionUi {
    pub(super) fn new(
        handle: CompanionHandle,
        view_only: bool,
    ) -> std::io::Result<(Self, Sender<BrowserEvent>)> {
        let (sender, receiver) = mpsc::channel();
        let mut ui = Self {
            handle,
            view_only,
            pending: VecDeque::new(),
            request: None,
            panel: None,
            scroll: 0,
            events: Arc::new(Mutex::new(Some(receiver))),
        };
        ui.show_pairing()?;
        Ok((ui, sender))
    }

    fn show_pairing(&mut self) -> std::io::Result<()> {
        let view = self.handle.pairing_url(TokenKind::View)?;
        let mut text = format!(
            "Open a one-time link in a browser on this computer.\n\
             Links expire after ten minutes. Keep them private.\n\n\
             VIEW\n{view}\n"
        );
        if !self.view_only {
            let steer = self.handle.pairing_url(TokenKind::Steer)?;
            text.push_str(&format!(
                "\nPROPOSE A PROMPT\n{steer}\n\n\
                 Every browser prompt needs F8 approval here.\n\
                 Tool approvals and login input stay on this terminal.\n"
            ));
        }
        text.push_str(
            "\nThe browser shows completed messages and activity status.\n\
             Drafts, thinking, tools, and approval cards stay local.\n\
             F8 reviews a waiting prompt without changing your draft.\n\
             /web reviews the inbox or creates new links.\n\
             /web pair creates new links; /web revoke closes browser access.",
        );
        self.panel = Some(Panel::Pairing(text));
        self.scroll = 0;
        Ok(())
    }

    fn reject_front(&mut self, reason: &str) {
        if let Some(proposal) = self.pending.pop_front() {
            let _ = self.handle.resolve_proposal(&proposal.id, false, reason);
        }
        self.panel = None;
        self.scroll = 0;
    }

    fn revoke(&mut self) {
        self.handle.revoke_all();
        self.pending.clear();
        self.panel = Some(Panel::Notice(String::from(
            "Browser clients, unused links, and pending proposals were revoked.\n\
             A prompt already admitted locally remains under normal terminal control.\n\
             Use /web to create new pairing links.",
        )));
        self.scroll = 0;
    }
}

impl Drop for CompanionUi {
    fn drop(&mut self) {
        self.handle.revoke_all();
        if let Some(request) = &self.request {
            request.abort.abort();
        }
    }
}

/// Owned by the same runtime as the agent event bridge. The 50ms wakeup
/// observes shutdown even when both the server and the driver are idle.
struct BrowserSubscription {
    handle: CompanionHandle,
    events: EventReceiver,
}

impl Subscription<PiFtuiMsg> for BrowserSubscription {
    fn id(&self) -> SubId {
        SUB_ID
    }

    #[allow(clippy::result_large_err)]
    fn run(&self, sender: SubscriptionSender<PiFtuiMsg>, stop: StopSignal) {
        let Some(receiver) = self.events.lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        while !stop.is_stopped() {
            if let Some(proposal) = self.handle.take_proposal()
                && sender
                    .send(PiFtuiMsg::Browser(BrowserEvent::Proposal(proposal)))
                    .is_err()
            {
                return;
            }
            match receiver.recv_timeout(super::AGENT_EVENT_POLL) {
                Ok(event) => {
                    if sender.send(PiFtuiMsg::Browser(event)).is_err() {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

fn plain_prompt(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty() && text.len() <= MAX_PROMPT_BYTES && !trimmed.starts_with(['/', '!'])
}

impl PiFtuiModel {
    pub(super) fn with_browser(mut self, browser: Option<CompanionUi>) -> Self {
        self.browser = browser;
        self
    }

    pub(super) fn browser_subscription(&self) -> Option<Box<dyn Subscription<PiFtuiMsg>>> {
        self.browser.as_ref().map(|browser| {
            Box::new(BrowserSubscription {
                handle: browser.handle.clone(),
                events: Arc::clone(&browser.events),
            }) as Box<dyn Subscription<PiFtuiMsg>>
        })
    }

    pub(super) fn browser_request_active(&self) -> bool {
        self.browser
            .as_ref()
            .is_some_and(|browser| browser.request.is_some())
    }

    pub(super) fn browser_abort(&self) -> Option<AbortHandle> {
        self.browser
            .as_ref()?
            .request
            .as_ref()
            .map(|request| request.abort.clone())
    }

    fn browser_ui_available(&self) -> bool {
        self.displayed_session_id.is_some()
            && self.state == AgentUiState::Ready
            && self.busy.is_none()
            && self.active_ask.is_none()
            && self.active_ext.is_none()
            && self.ext_queue.is_empty()
            && self.login_pending.is_none()
            && self.picker.is_none()
            && !self.custom_overlay_open()
            && !self.settings_pending
            && !self.suspending
            && !self.has_pending_cards()
            && self.live_goal().is_none()
            && !self.browser_request_active()
    }

    pub(super) fn browser_panel_open(&self) -> bool {
        self.browser_ui_available()
            && self
                .browser
                .as_ref()
                .is_some_and(|browser| browser.panel.is_some())
    }

    pub(super) fn browser_inbox_len(&self) -> usize {
        self.browser
            .as_ref()
            .map_or(0, |browser| browser.pending.len())
    }

    /// Intercept before ordinary slash-command history or transcript echo.
    /// Neither a pairing secret nor the resulting local panel is an agent
    /// message. An active login/ask retains its own input routing.
    pub(super) fn submit_browser_command(&mut self) -> bool {
        if self.login_pending.is_some() {
            return false;
        }
        let text = self.input.text();
        let clean = sanitize(text.trim());
        let Some(action) = strip_command(&clean, "/web") else {
            return false;
        };
        self.input.set_text("");
        self.autocomplete.close();
        if self.submit_terminal_web_command(action.trim()) {
            return true;
        }
        let Some(browser) = &mut self.browser else {
            self.set_error_banner(String::from(
                "Start with pi web to use the browser companion.",
            ));
            return true;
        };
        match action.trim() {
            "revoke" => browser.revoke(),
            "" | "inbox" if !browser.pending.is_empty() => {
                browser.panel = Some(Panel::Proposal);
                browser.scroll = 0;
            }
            "" | "pair" => {
                if let Err(error) = browser.show_pairing() {
                    browser.panel = Some(Panel::Notice(format!(
                        "Could not create a pairing link: {error}"
                    )));
                }
            }
            "inbox" => {
                browser.panel = Some(Panel::Notice(String::from(
                    "No browser prompts are waiting.",
                )));
                browser.scroll = 0;
            }
            _ => {
                browser.panel = Some(Panel::Notice(String::from(
                    "Usage: /web [pair|inbox|revoke]",
                )));
                browser.scroll = 0;
            }
        }
        true
    }

    pub(super) fn handle_browser_event(&mut self, event: BrowserEvent) -> Cmd<PiFtuiMsg> {
        let Some(browser) = &mut self.browser else {
            return Cmd::none();
        };
        match event {
            BrowserEvent::Proposal(proposal) => {
                if !plain_prompt(&proposal.text)
                    || self.displayed_session_id.as_deref()
                        != Some(proposal.owner_session_id.as_str())
                    || browser.pending.len() >= MAX_LOCAL_PROPOSALS
                {
                    let _ = browser.handle.resolve_proposal(
                        &proposal.id,
                        false,
                        "The local session cannot review this prompt",
                    );
                } else if !browser.pending.iter().any(|item| item.id == proposal.id) {
                    browser.pending.push_back(proposal);
                }
            }
            BrowserEvent::Finished {
                request_id,
                owner_session_id,
                error,
            } => {
                if browser
                    .request
                    .as_ref()
                    .map(|request| request.proposal.id.as_str())
                    != Some(request_id.as_str())
                {
                    return Cmd::none();
                }
                browser.request = None;
                self.busy = None;
                if self.displayed_session_id.as_deref() == Some(owner_session_id.as_str())
                    && let Some(error) = error
                {
                    self.set_error_banner(sanitize(&error).into_owned());
                }
            }
        }
        Cmd::none()
    }

    pub(super) fn reconcile_browser_session(&mut self) {
        let available = self.browser_ui_available();
        let Some(browser) = &mut self.browser else {
            return;
        };
        if let Some(owner) = self.displayed_session_id.as_deref() {
            let handle = browser.handle.clone();
            browser.pending.retain(|proposal| {
                if proposal.owner_session_id == owner {
                    true
                } else {
                    let _ = handle.resolve_proposal(
                        &proposal.id,
                        false,
                        "The local terminal changed sessions",
                    );
                    false
                }
            });
            if browser
                .request
                .as_ref()
                .is_some_and(|request| request.proposal.owner_session_id != owner)
                && let Some(request) = browser.request.take()
            {
                request.abort.abort();
                let _ = browser.handle.resolve_proposal(
                    &request.proposal.id,
                    false,
                    "The local terminal changed sessions",
                );
                self.busy = None;
            }
        }
        if matches!(browser.panel, Some(Panel::Proposal)) && browser.pending.is_empty() {
            browser.panel = None;
        }
        // Never steal a partially authored draft. Its header advertises the
        // inbox; /web opens it when the user decides to review the proposal.
        if available
            && browser.panel.is_none()
            && !browser.pending.is_empty()
            && self.input.is_empty()
        {
            browser.panel = Some(Panel::Proposal);
            browser.scroll = 0;
        }
    }

    pub(super) fn handle_browser_term(&mut self, event: &Event) -> bool {
        if !self.browser_panel_open() {
            if self.browser_ui_available()
                && let Event::Key(key) = event
                && key.code == KeyCode::F(8)
                && key.modifiers.is_empty()
                && key.kind == ftui::KeyEventKind::Press
                && let Some(browser) = &mut self.browser
                && !browser.pending.is_empty()
            {
                browser.panel = Some(Panel::Proposal);
                browser.scroll = 0;
                return true;
            }
            return false;
        }
        match event {
            Event::Key(key) => {
                if !super::key_event_is_input(key) {
                    return true;
                }
                let ctrl_c =
                    key.code == KeyCode::Char('c') && key.modifiers.contains(Modifiers::CTRL);
                if key.code == KeyCode::Escape || ctrl_c {
                    if let Some(browser) = &mut self.browser {
                        if matches!(browser.panel, Some(Panel::Proposal)) {
                            browser.reject_front("Rejected on the local terminal");
                        } else {
                            browser.panel = None;
                            browser.scroll = 0;
                        }
                    }
                    if ctrl_c {
                        self.last_ctrl_c = Some(std::time::Instant::now());
                    }
                    return true;
                }
                if key.code == KeyCode::F(8)
                    && key.modifiers.is_empty()
                    && key.kind == ftui::KeyEventKind::Press
                {
                    self.accept_browser_proposal();
                    return true;
                }
                if let Some(browser) = &mut self.browser {
                    match key.code {
                        KeyCode::Up => browser.scroll = browser.scroll.saturating_sub(1),
                        KeyCode::Down => browser.scroll = browser.scroll.saturating_add(1),
                        KeyCode::PageUp => browser.scroll = browser.scroll.saturating_sub(10),
                        KeyCode::PageDown => browser.scroll = browser.scroll.saturating_add(10),
                        KeyCode::Home => browser.scroll = 0,
                        _ => {}
                    }
                }
                true
            }
            Event::Paste(_) | Event::Mouse(_) => true,
            _ => false,
        }
    }

    fn accept_browser_proposal(&mut self) {
        let Some(proposal) = self.browser.as_ref().and_then(|browser| {
            matches!(browser.panel, Some(Panel::Proposal))
                .then(|| browser.pending.front().cloned())
                .flatten()
        }) else {
            return;
        };
        if self.displayed_session_id.as_deref() != Some(proposal.owner_session_id.as_str())
            || !plain_prompt(&proposal.text)
        {
            if let Some(browser) = &mut self.browser {
                browser.reject_front("The prompt no longer belongs to the displayed session");
            }
            return;
        }
        let request = PromptRequest {
            proposal,
            abort: AbortHandle::new().0,
        };
        if self.try_send_command(UiCommand::BrowserPrompt(request.clone())) {
            if let Some(browser) = &mut self.browser {
                browser.pending.pop_front();
                browser.panel = None;
                browser.request = Some(request);
            }
            self.begin_busy("Submitting locally approved browser prompt (Escape to cancel) ...");
        } else {
            if let Some(browser) = &mut self.browser {
                browser.reject_front("The local driver is no longer available");
            }
            self.set_error_banner(String::from("The local driver is no longer available."));
        }
    }

    pub(super) fn render_browser(&self, frame: &mut Frame) -> bool {
        if !self.browser_panel_open() {
            return false;
        }
        let Some(browser) = &self.browser else {
            return false;
        };
        let (title, content, footer) = match &browser.panel {
            Some(Panel::Pairing(text)) => (
                "Browser companion · private pairing links",
                text.clone(),
                "Esc close · ↑/↓ scroll",
            ),
            Some(Panel::Notice(text)) => {
                ("Browser companion", text.clone(), "Esc close · ↑/↓ scroll")
            }
            Some(Panel::Proposal) => {
                let Some(proposal) = browser.pending.front() else {
                    return false;
                };
                (
                    "Browser prompt · local approval required",
                    format!(
                        "This plain-text prompt will run in the displayed session.\n\
                         Your draft stays in the editor. Tool approvals remain local.\n\n{}",
                        sanitize(&proposal.text),
                    ),
                    "F8 accept prompt · Esc reject · ↑/↓ scroll",
                )
            }
            None => return false,
        };
        let width = frame.width().max(1);
        let height = frame.height();
        if height == 0 {
            return true;
        }
        Paragraph::new(Text::from_lines([ftui::text::Line::styled(
            title,
            ftui::Style::new().bold().fg(self.palette.accent),
        )]))
        .render(Rect::new(0, 0, width, 1), frame);
        let rows: Vec<_> = content
            .lines()
            .flat_map(|line| {
                ftui::text::Line::from(line.to_string())
                    .wrap(usize::from(width), WrapMode::WordChar)
            })
            .collect();
        let visible = usize::from(height.saturating_sub(2));
        let offset = browser.scroll.min(rows.len().saturating_sub(visible));
        Paragraph::new(Text::from_lines(
            rows.into_iter().skip(offset).take(visible),
        ))
        .render(Rect::new(0, 1, width, height.saturating_sub(2)), frame);
        if height > 1 {
            Paragraph::new(footer).render(Rect::new(0, height - 1, width, 1), frame);
        }
        true
    }
}

/// A local F8 press only queues an approval. Re-check both its original
/// session and its live server record here, immediately before SDK work.
/// Accepted text is literal: no shell, slash, template or @file expansion.
pub(super) async fn run_prompt(
    session: &mut crate::sdk::AgentSessionHandle,
    request: PromptRequest,
    companion: &CompanionHandle,
    agent_tx: &Sender<PiMsg>,
    browser_tx: &Sender<BrowserEvent>,
    turn_control: &TurnControlSlot,
    turn_abort: &TurnAbortSlot,
) {
    let _operation =
        super::queued_compaction::OperationAbort::publish(turn_abort, turn_control, &request.abort);
    let signal = request.abort.signal();
    let proposal = &request.proposal;
    // Drop the immutable session future before beginning its mutable turn.
    let owner = {
        let read = session.with_session(|store| store.header.id.clone());
        let cancelled = signal.wait();
        futures::pin_mut!(read, cancelled);
        match futures::future::select(cancelled, read).await {
            futures::future::Either::Left(_) => Err(crate::error::Error::Aborted),
            futures::future::Either::Right((result, _)) => result,
        }
    };
    let error = match owner {
        Ok(owner) if owner != proposal.owner_session_id => Some(String::from(
            "Browser prompt cancelled: the local session changed.",
        )),
        Ok(_) if signal.is_aborted() => Some(String::from("Browser prompt cancelled.")),
        Ok(_) if !plain_prompt(&proposal.text) => Some(String::from(
            "Browser prompt refused: only bounded plain-text prompts are supported.",
        )),
        Ok(owner) => {
            if !companion.resolve_proposal(&proposal.id, true, "Accepted on the local terminal") {
                Some(String::from(
                    "Browser prompt expired or browser access was revoked.",
                ))
            } else if agent_tx
                .send(PiMsg::UserInputSubmitted {
                    owner_session_id: owner,
                    text: proposal.text.clone(),
                    history_text: String::new(),
                })
                .is_err()
            {
                Some(String::from(
                    "Browser prompt cancelled: the local terminal closed.",
                ))
            } else {
                let result = super::run_controlled_turn_with_pending(
                    session,
                    crate::model::UserContent::Text(proposal.text.clone()),
                    None,
                    Vec::new(),
                    Some(signal.clone()),
                    agent_tx,
                    turn_control,
                )
                .await;
                super::finish_prompt_sequence(
                    session,
                    result,
                    agent_tx,
                    turn_control,
                    Some(signal),
                )
                .await;
                None
            }
        }
        Err(crate::error::Error::Aborted) => Some(String::from("Browser prompt cancelled.")),
        Err(_) => Some(String::from(
            "Browser prompt cancelled: the local session is unavailable.",
        )),
    };
    if error.is_some() {
        let _ = companion.resolve_proposal(
            &proposal.id,
            false,
            "The local session did not admit the prompt",
        );
    }
    let _ = browser_tx.send(BrowserEvent::Finished {
        request_id: proposal.id.clone(),
        owner_session_id: proposal.owner_session_id.clone(),
        error,
    });
}
