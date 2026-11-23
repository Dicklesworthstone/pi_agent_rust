//! Order manual compaction between the active turn's authored inputs.
//!
//! The mailbox barrier owns ordering; the normal sequential driver owns the
//! actual compaction. A command never enters provider history as a user prompt.

use super::{
    EntryRole, ManualCompactionMode, PiFtuiModel, PiMsg, RestoredInput, Sender, TurnAbortSlot,
    TurnControlSlot, UiCommand, parse_compaction_command, run_controlled_turn_with_pending,
    send_conversation_reset,
};
use crate::agent::AbortHandle;
use crate::session_control::{PendingInput, SessionControlHandle, YieldBarrierId};

/// A manual compaction tied to exactly one controlled turn and session.
#[derive(Clone)]
pub struct QueuedCompaction {
    owner_session_id: String,
    branch_anchor: Option<String>,
    control: SessionControlHandle,
    barrier: YieldBarrierId,
    command: String,
    instructions: Option<String>,
    mode: ManualCompactionMode,
    abort: AbortHandle,
}

impl std::fmt::Debug for QueuedCompaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueuedCompaction")
            .field("owner_session_id", &self.owner_session_id)
            .field("barrier", &self.barrier)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl PartialEq for QueuedCompaction {
    fn eq(&self, other: &Self) -> bool {
        self.barrier == other.barrier && self.owner_session_id == other.owner_session_id
    }
}

impl Eq for QueuedCompaction {}

impl QueuedCompaction {
    pub(super) fn is_pending(&self) -> bool {
        self.control.has_yield_barrier()
    }

    fn restoration(&self, before: Vec<PendingInput>, after: Vec<PendingInput>) -> RestoredInput {
        RestoredInput::for_compaction(
            self.owner_session_id.clone(),
            self.branch_anchor.clone(),
            before,
            self.command.clone(),
            self.instructions.clone(),
            self.mode,
            after,
        )
    }
}

impl PiFtuiModel {
    pub(super) fn queue_compaction_command(&mut self, command: &str) -> bool {
        let Some((mode, instructions)) = parse_compaction_command(command) else {
            return false;
        };
        if self.replaying_input.is_some() {
            self.push_entry(
                EntryRole::Error,
                "Finish or cancel recovered input before compacting; this command stays in the editor."
                    .to_string(),
            );
            return true;
        }
        if self.live_goal().is_some() {
            self.push_entry(
                EntryRole::Error,
                "Pause the goal with Escape before compacting; this command stays in the editor."
                    .to_string(),
            );
            return true;
        }
        if self
            .queued_compaction
            .as_ref()
            .is_some_and(|request| request.control.has_yield_barrier())
        {
            self.push_entry(
                EntryRole::Error,
                "A compaction is already queued; use Alt+Up to recall it before replacing it."
                    .to_string(),
            );
            return true;
        }
        let Some(control) = self.live_turn_control() else {
            self.push_entry(
                EntryRole::Error,
                "The turn is changing; keep this command in the editor and submit it again."
                    .to_string(),
            );
            return true;
        };
        let Some(owner_session_id) = self.displayed_session_id.clone() else {
            self.push_entry(
                EntryRole::Error,
                "Wait for the session to finish loading.".to_string(),
            );
            return true;
        };
        let barrier = match control.insert_yield_barrier() {
            Ok(barrier) => barrier,
            Err(error) => {
                self.push_entry(EntryRole::Error, error.to_string());
                return true;
            }
        };
        let request = QueuedCompaction {
            branch_anchor: self.live_input_branch_anchor(&owner_session_id),
            owner_session_id,
            control,
            barrier,
            command: command.to_string(),
            instructions: instructions.map(str::to_string),
            mode,
            abort: AbortHandle::new().0,
        };
        // Publish the driver command before consuming the draft. A closed
        // driver cannot leave a barrier that nobody will ever service.
        if self
            .submit_tx
            .as_ref()
            .is_none_or(|tx| tx.send(UiCommand::QueuedCompact(request.clone())).is_err())
        {
            request.control.remove_yield_barrier(barrier);
            self.push_entry(
                EntryRole::Error,
                "The agent driver is no longer available.".to_string(),
            );
            return true;
        }
        self.queued_compaction = Some(request);
        self.record_history(command);
        self.input.set_text("");
        self.autocomplete.close();
        self.scroll_from_tail = 0;
        self.push_entry(
            EntryRole::System,
            "Compaction queued after earlier input; later messages will wait. Alt+Up recalls the queue."
                .to_string(),
        );
        true
    }

    /// Taking a barrier is atomic with the driver's claim. If it already
    /// started, a stale UI copy cannot recall or duplicate its inputs.
    pub(super) fn take_queued_compaction(&mut self) -> Option<RestoredInput> {
        let request = self.queued_compaction.take()?;
        let (before, after) = request.control.take_pending_at_barrier(request.barrier)?;
        request.abort.abort();
        Some(request.restoration(before, after))
    }

    pub(super) fn restore_cancelled_compaction(&mut self) {
        let Some(request) = self.queued_compaction.as_ref() else {
            return;
        };
        request.abort.abort();
        let Some(restored) = self.take_queued_compaction() else {
            return;
        };
        self.restore_native_input(restored);
    }
}

fn restore_command(
    request: &QueuedCompaction,
    before: Vec<PendingInput>,
    after: Vec<PendingInput>,
    agent_tx: &Sender<PiMsg>,
) {
    let _ = agent_tx.send(PiMsg::RestorePreparedInput(
        request.restoration(before, after),
    ));
}

fn restore_inputs(
    request: &QueuedCompaction,
    pending: Vec<PendingInput>,
    agent_tx: &Sender<PiMsg>,
) {
    if !pending.is_empty() {
        let _ = agent_tx.send(PiMsg::RestorePreparedInput(RestoredInput::for_turn(
            request.owner_session_id.clone(),
            request.branch_anchor.clone(),
            pending,
        )));
    }
}

/// Hold cancellation authority through session admission, installation,
/// transcript refresh and the resumed native turn. Publication shares the
/// runtime's shutdown fence; no operation may start after that fence closes.
pub(super) struct OperationAbort<'a> {
    slot: &'a TurnAbortSlot,
}

impl<'a> OperationAbort<'a> {
    pub(super) fn publish(
        slot: &'a TurnAbortSlot,
        controls: &TurnControlSlot,
        abort: &AbortHandle,
    ) -> Self {
        let state = controls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            abort.abort();
        }
        *slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(abort.clone());
        drop(state);
        Self { slot }
    }
}

impl Drop for OperationAbort<'_> {
    fn drop(&mut self) {
        *self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

pub(super) async fn run(
    handle: &mut crate::sdk::AgentSessionHandle,
    request: QueuedCompaction,
    agent_tx: &Sender<PiMsg>,
    turn_control: &TurnControlSlot,
    turn_abort: &TurnAbortSlot,
) {
    let can_run = request.control.yield_barrier_can_run(request.barrier);
    if !can_run {
        // Obsolete/refused commands need no driver authority or async work.
        // In particular, a stale command must not mask a newer request's Esc.
        if let Some((before, after)) = request.control.take_pending_at_barrier(request.barrier) {
            restore_command(&request, before, after, agent_tx);
        }
        return;
    }
    let _operation_abort = OperationAbort::publish(turn_abort, turn_control, &request.abort);
    let Some((before, after)) = request.control.take_pending_at_barrier(request.barrier) else {
        return;
    };
    let signal = request.abort.signal();
    let same_session = {
        let read = handle.with_session(|session| session.header.id == request.owner_session_id);
        let cancelled = signal.wait();
        futures::pin_mut!(read, cancelled);
        match futures::future::select(cancelled, read).await {
            futures::future::Either::Left(_) => false,
            futures::future::Either::Right((result, _)) => result.unwrap_or(false),
        }
    };
    if !same_session || signal.is_aborted() {
        restore_command(&request, before, after, agent_tx);
        return;
    }

    // The normal turn is durably complete and unlocked. Own cancellation
    // separately until compaction's transaction has fully settled.
    let _ = agent_tx.send(PiMsg::AgentStart);
    let tx = agent_tx.clone();
    let progress =
        super::tool_progress::Bridge::new(Some(request.owner_session_id.clone()), move |message| {
            let _ = tx.send(message);
        });
    let on_event = progress.callback();
    let result = handle
        .compact_in_mode_with_abort(
            request.mode,
            request.instructions.as_deref(),
            request.abort.signal(),
            on_event,
        )
        .await;
    drop(progress);
    match result {
        Ok(true) => {
            send_conversation_reset(handle, agent_tx, "Queued compaction complete").await;
        }
        Ok(false) => {
            let _ = agent_tx.send(PiMsg::System(
                "Nothing to compact: context is already small enough".to_string(),
            ));
        }
        Err(error) => {
            let _ = agent_tx.send(PiMsg::AgentError(format!("Queued compaction: {error}")));
            restore_command(&request, before, after, agent_tx);
            return;
        }
    }
    let _ = agent_tx.send(PiMsg::AgentDone {
        usage: None,
        stop_reason: crate::model::StopReason::Stop,
        error_message: None,
    });
    if signal.is_aborted() {
        // A completed transaction is never replayed after a late cancellation.
        restore_inputs(&request, after, agent_tx);
        return;
    }

    // Earlier input has completed. Seed the entire held tail before the next
    // control is published, preserving its native content and authored source.
    // A new live input or compaction then sees every older accepted message.
    let mut after = after.into_iter();
    if let Some(input) = after.next() {
        let result = run_controlled_turn_with_pending(
            handle,
            input.content.clone(),
            Some(input),
            after.collect(),
            Some(signal),
            agent_tx,
            turn_control,
        )
        .await;
        // Stops/caps and arrivals at the final boundary are returned intact
        // instead of resetting the completed turn's timer or merging content.
        restore_inputs(&request, result.into_restored_inputs(), agent_tx);
    }
}
