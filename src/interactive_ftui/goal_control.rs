//! Invocation-owned goal control, scoped progress and native input recovery.
//!
//! The terminal never borrows the goal driver's inner turn or installs a
//! maintenance barrier in it. Pause belongs to the whole goal, including the
//! gaps between prompts. Recovered input requires an explicit user submission.

use super::{
    AgentUiState, Cmd, EntryRole, PiFtuiModel, PiFtuiMsg, PiMsg, Receiver, RecvTimeoutError,
    Sender, StopSignal, SubId, Subscription, SubscriptionSender, TurnControlSlot, UiCommand,
};
use crate::error::{Error, Result};
use crate::goal::GoalControl;
use crate::model::UserContent;
use crate::session_control::{PendingInput, SessionControlHandle};
use std::sync::{Arc, Mutex};

const SUB_ID: SubId = 0x5049_474f; // "PIGO"

/// A message belongs to one invocation even after its native prompt ends.
#[derive(Debug)]
pub enum Event {
    Message {
        invocation: uuid::Uuid,
        owner_session_id: String,
        message: PiMsg,
    },
    Pending(RestoredInput),
}

pub(super) type EventReceiver = Arc<Mutex<Option<Receiver<Event>>>>;

pub(super) struct EventSubscription(pub EventReceiver);

impl Subscription<PiFtuiMsg> for EventSubscription {
    fn id(&self) -> SubId {
        SUB_ID
    }

    #[allow(clippy::result_large_err)]
    fn run(&self, sender: SubscriptionSender<PiFtuiMsg>, stop: StopSignal) {
        let Some(receiver) = self.0.lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        while !stop.is_stopped() {
            match receiver.recv_timeout(super::AGENT_EVENT_POLL) {
                Ok(event) => {
                    if sender.send(PiFtuiMsg::Goal(event)).is_err() {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

#[derive(Default, Debug)]
struct ControlState {
    control: Option<GoalControl>,
    paused: bool,
    finished: bool,
}

#[derive(Clone, Debug)]
pub(super) struct Invocation {
    id: uuid::Uuid,
    owner_session_id: String,
    branch_anchor: Option<String>,
    state: Arc<Mutex<ControlState>>,
    sender: Sender<Event>,
}

impl Invocation {
    pub(super) fn pause(&self) -> bool {
        let control = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.finished || state.paused {
                return false;
            }
            state.paused = true;
            state.control.clone()
        };
        // Native abort can wake event callbacks. No terminal/display mutex
        // may be held across this call.
        if let Some(control) = control {
            control.pause();
        }
        true
    }

    fn publish(&self, control: GoalControl) {
        let pause = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let pause = state.paused || state.finished || state.control.is_some();
            if !state.finished && state.control.is_none() {
                state.control = Some(control.clone());
            }
            pause
        };
        if pause {
            control.pause();
        }
    }

    fn accepting_control(&self) -> Result<GoalControl> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.paused || state.finished {
            return Err(Error::validation(
                "[GOAL_INPUT_CLOSED] goal is paused or finished; input was not sent",
            ));
        }
        state.control.clone().ok_or_else(|| {
            Error::validation("[GOAL_INPUT_IDLE] goal is starting; input was not sent")
        })
    }

    pub(super) fn send(&self, message: PiMsg) -> std::result::Result<(), ()> {
        self.sender
            .send(Event::Message {
                invocation: self.id,
                owner_session_id: self.owner_session_id.clone(),
                message,
            })
            .map_err(|_| ())
    }

    fn return_pending(&self, pending: Vec<PendingInput>) {
        if !pending.is_empty() {
            let _ = self.sender.send(Event::Pending(RestoredInput::new(
                self.id,
                self.owner_session_id.clone(),
                self.branch_anchor.clone(),
                pending,
            )));
        }
    }

    fn take_pending(&self) -> Vec<PendingInput> {
        let control = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .control
            .clone();
        control.map_or_else(Vec::new, |control| control.take_pending())
    }
}

/// Publication and shutdown share the runtime fence. Retirement checks the
/// invocation identity so a late owner cannot clear a newer goal's control.
struct Binding {
    invocation: Invocation,
    slot: TurnControlSlot,
}

impl Binding {
    fn new(
        slot: &TurnControlSlot,
        sender: &Sender<Event>,
        owner_session_id: String,
        branch_anchor: Option<String>,
    ) -> Self {
        let invocation = Invocation {
            id: uuid::Uuid::new_v4(),
            owner_session_id,
            branch_anchor,
            state: Arc::default(),
            sender: sender.clone(),
        };
        let (closed, previous) = {
            let mut state = slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.last_goal = Some(invocation.id);
            (state.closed, state.goal.replace(invocation.clone()))
        };
        if let Some(previous) = previous {
            previous.pause();
        }
        if closed {
            invocation.pause();
        }
        Self {
            invocation,
            slot: Arc::clone(slot),
        }
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        self.invocation.pause();
        let pending = self.invocation.take_pending();
        self.invocation
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finished = true;
        {
            let mut slot = self
                .slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot
                .goal
                .as_ref()
                .is_some_and(|goal| goal.id == self.invocation.id)
            {
                slot.goal = None;
            }
        }
        self.invocation.return_pending(pending);
    }
}

pub(super) async fn run(
    handle: &mut crate::sdk::AgentSessionHandle,
    args: &str,
    slot: &TurnControlSlot,
    sender: &Sender<Event>,
    agent_tx: &Sender<PiMsg>,
) {
    let origin = handle
        .with_session(|session| {
            (
                session.header.id.clone(),
                session.leaf_id().map(str::to_string),
            )
        })
        .await;
    let (owner, branch) = match origin {
        Ok(origin) => origin,
        Err(error) => {
            let _ = agent_tx.send(PiMsg::AgentError(format!("goal: {error}")));
            return;
        }
    };
    let binding = Binding::new(slot, sender, owner, branch);
    let progress = super::workspace_commands::goal_progress::Observation::attach_to_goal(
        handle.listeners(),
        binding.invocation.clone(),
    );
    let publish_progress = progress.control_publisher();
    let publish = binding.invocation.clone();
    let recover = binding.invocation.clone();
    let result = Box::pin(crate::goal::terminal::run_with_control_and_pending(
        handle,
        args,
        move |control| {
            publish.publish(control.clone());
            publish_progress(control);
        },
        move |pending| recover.return_pending(pending),
    ))
    .await;
    let invocation = binding.invocation.clone();
    // Retire before the final display events; their receiver can immediately
    // restore pending input without waiting for another terminal tick.
    drop(binding);
    progress.finish();
    let message = match result {
        Ok(text) => PiMsg::System(text),
        Err(error) => PiMsg::AgentError(format!("goal: {error}")),
    };
    let _ = invocation.send(message);
}

/// One-use transfer. Clones share ownership instead of copying user payloads;
/// Debug/command equality never inspect private text or attachment contents.
#[derive(Clone)]
pub struct RestoredInput {
    id: uuid::Uuid,
    source_invocation: uuid::Uuid,
    owner_session_id: String,
    branch_anchor: Option<String>,
    pending: Arc<Mutex<Vec<PendingInput>>>,
}

impl std::fmt::Debug for RestoredInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestoredGoalInput")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl PartialEq for RestoredInput {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for RestoredInput {}

impl RestoredInput {
    fn new(
        source_invocation: uuid::Uuid,
        owner: String,
        branch: Option<String>,
        pending: Vec<PendingInput>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            source_invocation,
            owner_session_id: owner,
            branch_anchor: branch,
            pending: Arc::new(Mutex::new(pending)),
        }
    }

    fn text(&self) -> String {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|input| input.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

pub(super) struct Draft {
    request: RestoredInput,
    text: String,
}

impl PiFtuiModel {
    pub(super) fn park_goal_draft_for(&mut self, session_id: &str) {
        if self
            .goal_draft
            .as_ref()
            .is_some_and(|draft| draft.request.owner_session_id != session_id)
            && let Some(draft) = self.goal_draft.take()
        {
            if self.input.text() == draft.text {
                self.input.set_text("");
            }
            self.pending_goal_inputs.push_front(draft.request);
        }
    }

    pub(super) fn live_goal(&self) -> Option<Invocation> {
        let goal = self
            .turn_control
            .as_ref()?
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .goal
            .clone()?;
        (self.displayed_session_id.as_deref() == Some(goal.owner_session_id.as_str()))
            .then_some(goal)
    }

    pub(super) fn abort_goal(&mut self) -> bool {
        if let Some(goal) = self.live_goal() {
            if goal.pause() {
                self.push_entry(EntryRole::System, "Pausing goal...".to_string());
            }
            return true;
        }
        false
    }

    pub(super) fn handle_goal_event(&mut self, event: Event) -> Cmd<PiFtuiMsg> {
        match event {
            Event::Message {
                invocation,
                owner_session_id,
                message,
            } => {
                let current = self.turn_control.as_ref().and_then(|slot| {
                    slot.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .last_goal
                });
                if current == Some(invocation)
                    && self.displayed_session_id.as_deref() == Some(owner_session_id.as_str())
                {
                    return self.handle_agent(message);
                }
            }
            // Keep another session's payload parked; never place it in this
            // session's editor or redirect it into a newer goal invocation.
            Event::Pending(request) => self.pending_goal_inputs.push_back(request),
        }
        Cmd::none()
    }

    fn goal_composer_available(&self) -> bool {
        self.input_active()
            && self.active_ask.is_none()
            && self.active_ext.is_none()
            && self.ext_queue.is_empty()
            && self.login_pending.is_none()
            && self.picker.is_none()
            && !self.custom_overlay_open()
            && !self.settings_pending
            && !self.suspending
    }

    pub(super) fn apply_goal_input_restores(&mut self) {
        if self.goal_draft.is_some()
            || !self.input.is_empty()
            || !self.goal_composer_available()
            || self.state != AgentUiState::Ready
            || self.busy.is_some()
            || self.live_goal().is_some()
        {
            return;
        }
        let Some(index) = self.pending_goal_inputs.iter().position(|request| {
            self.displayed_session_id.as_deref() == Some(request.owner_session_id.as_str())
        }) else {
            return;
        };
        if let Some(request) = self.pending_goal_inputs.remove(index) {
            self.show_goal_draft(request);
        }
    }

    fn show_goal_draft(&mut self, request: RestoredInput) {
        let text = request.text();
        self.input.set_text(&text);
        self.goal_draft = Some(Draft { request, text });
        self.detach_edited_history();
        self.autocomplete.close();
        self.push_entry(
            EntryRole::System,
            "Restored unsent goal input with its original attachments. Review before sending; Ctrl+C clears it.".to_string(),
        );
    }

    /// Alt+Up restores exact payloads. Existing private/modal editors keep
    /// their keyboard ownership; an unrelated draft is never overwritten.
    pub(super) fn recall_goal_input(&mut self) -> bool {
        if !self.goal_composer_available() {
            return false;
        }
        if let Some(draft) = &self.goal_draft {
            self.input.set_text(&draft.text);
            return true;
        }
        let Some(goal) = self.live_goal() else {
            return false;
        };
        if !self.input.is_empty() {
            self.push_entry(
                EntryRole::System,
                "Clear or send the current draft before restoring queued goal input.".to_string(),
            );
            return true;
        }
        let pending = goal.take_pending();
        if pending.is_empty() {
            self.push_entry(
                EntryRole::System,
                "No queued messages to restore.".to_string(),
            );
        } else {
            self.show_goal_draft(RestoredInput::new(
                goal.id,
                goal.owner_session_id,
                goal.branch_anchor,
                pending,
            ));
        }
        true
    }

    /// `true` means this submission belongs to a recovered native draft.
    /// Never rebuild attachment bytes from files which may since have changed.
    pub(super) fn submit_goal_draft(&mut self) -> bool {
        let Some(draft) = &self.goal_draft else {
            return false;
        };
        if !self.goal_composer_available() {
            return true;
        }
        if self.displayed_session_id.as_deref() != Some(draft.request.owner_session_id.as_str()) {
            self.push_entry(
                EntryRole::Error,
                "This input belongs to another session; return there to send it, or Ctrl+C to clear it.".to_string(),
            );
            return true;
        }
        if self.input.text() != draft.text {
            self.push_entry(
                EntryRole::Error,
                "Recovered input retains prepared attachments. Alt+Up restores the original; Ctrl+C clears it so you can compose a new message.".to_string(),
            );
            return true;
        }
        let request = draft.request.clone();
        let result = if let Some(goal) = self.live_goal() {
            if goal.id != request.source_invocation {
                Err(Error::validation(
                    "Recovered input belongs to an earlier goal; finish or pause this goal before resubmitting it",
                ))
            } else {
                goal.accepting_control().and_then(|control| {
                    control
                        .restore_pending(
                            &mut request
                                .pending
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        )
                        .map(|_| ())
                })
            }
        } else if self.state == AgentUiState::Working {
            // Only the sequential driver can validate an older input's branch
            // anchor. Do not bypass that check via an unrelated live mailbox.
            Err(Error::validation(
                "Wait for the current turn to finish before resubmitting recovered goal input",
            ))
        } else {
            self.submit_tx
                .as_ref()
                .ok_or_else(|| Error::validation("The agent driver is unavailable"))
                .and_then(|tx| {
                    tx.send(UiCommand::RestoredGoalInput(request))
                        .map_err(|_| Error::validation("The agent driver is unavailable"))
                })
        };
        match result {
            Ok(()) => {
                let text = self
                    .goal_draft
                    .take()
                    .map(|draft| draft.text)
                    .unwrap_or_default();
                self.record_history(&text);
                self.push_entry(EntryRole::User, text);
                self.input.set_text("");
                self.autocomplete.close();
            }
            Err(error) => self.push_entry(EntryRole::Error, error.to_string()),
        }
        true
    }

    pub(super) fn goal_display_pending(&self) -> bool {
        self.turn_control.as_ref().is_some_and(|slot| {
            slot.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .last_goal
                .is_some()
        })
    }

    pub(super) fn submit_goal_content(
        &mut self,
        goal: &Invocation,
        content: UserContent,
        source: &str,
        follow_up: bool,
    ) {
        let result = goal.accepting_control().and_then(|control| {
            if follow_up {
                control.follow_up_with_content(&content, source)
            } else {
                control.steer_with_content(&content, source)
            }
        });
        match result {
            Ok(_) => {
                self.record_history(source);
                self.input.set_text("");
                self.autocomplete.close();
                self.scroll_from_tail = 0;
                let lane = if follow_up { "follow-up" } else { "steer" };
                self.push_entry(EntryRole::User, format!("({lane}) {source}"));
            }
            Err(error) => self.push_entry(EntryRole::Error, error.to_string()),
        }
    }
}

/// Declared before the native turn so cancellation drops that turn before
/// reclaiming its mailbox. A dropped driver cannot discard its unclaimed
/// tail, and a completed/claimed item never becomes an invented retry.
struct ReplayRecovery<'a> {
    request: RestoredInput,
    events: &'a Sender<Event>,
    control: Option<SessionControlHandle>,
}

impl Drop for ReplayRecovery<'_> {
    fn drop(&mut self) {
        let mut pending = std::mem::take(
            &mut *self
                .request
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if let Some(control) = &self.control {
            control.abort();
            pending.extend(control.take_pending());
        }
        if !pending.is_empty() {
            // Never refill a consumed command: a stale command clone must
            // not acquire new authority over its recovered tail.
            let _ = self.events.send(Event::Pending(RestoredInput::new(
                self.request.source_invocation,
                self.request.owner_session_id.clone(),
                self.request.branch_anchor.clone(),
                pending,
            )));
        }
    }
}

/// Explicit resubmission starts one ordinary native turn, using the original
/// prepared payload and author text. It cannot restart a goal or replay a
/// delivered input. Any unclaimed tail returns through the same typed channel.
pub(super) async fn run_restored(
    handle: &mut crate::sdk::AgentSessionHandle,
    request: RestoredInput,
    agent_tx: &Sender<PiMsg>,
    events: &Sender<Event>,
    controls: &TurnControlSlot,
) {
    let mut recovery = ReplayRecovery {
        request,
        events,
        control: None,
    };
    let owner = handle
        .with_session(|session| {
            session.header.id == recovery.request.owner_session_id
                && recovery
                    .request
                    .branch_anchor
                    .as_deref()
                    .is_none_or(|anchor| {
                        session
                            .entries_for_current_path()
                            .iter()
                            .any(|entry| entry.base().id.as_deref() == Some(anchor))
                    })
        })
        .await;
    if !matches!(owner, Ok(true)) {
        let _ = agent_tx.send(PiMsg::AgentError(
            "Recovered goal input belongs to a different session or branch; nothing was sent."
                .to_string(),
        ));
        return;
    }
    let mut pending = std::mem::take(
        &mut *recovery
            .request
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    if pending.is_empty() {
        return;
    }
    let first = pending.remove(0);
    handle
        .session_mut()
        .agent
        .set_magic_keyword_scan_override(Some(first.text.clone()));
    let tx = agent_tx.clone();
    let turn = handle.prompt_controlled_input(first.content.clone(), move |event| {
        for message in super::agent_event_to_pi_msgs(&event) {
            let _ = tx.send(message);
        }
    });
    let control = turn.control();
    if let Err(error) = control.restore_pending(&mut pending) {
        drop(turn);
        handle
            .session_mut()
            .agent
            .set_magic_keyword_scan_override(None);
        pending.insert(0, first);
        *recovery
            .request
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = pending;
        super::report_turn_result(Err(error), agent_tx);
        return;
    }
    recovery.control = Some(control.clone());
    controls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .publish(control.clone());
    let result = turn.await;
    controls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .current = None;
    if matches!(&result, Err(Error::Validation(message))
        if message.starts_with("[SESSION_CONTROL_CANCELLED] turn cancelled before execution;"))
    {
        handle
            .session_mut()
            .agent
            .set_magic_keyword_scan_override(None);
        recovery
            .request
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(first);
    }
    super::report_turn_result(result, agent_tx);
}

#[cfg(test)]
mod tests;
