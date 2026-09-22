//! Explicit, all-or-nothing handoff of unclaimed input after a turn finishes.
//!
//! No transcript entry or already-delivered input is replayed. Payloads move,
//! including native attachments and authored keyword text, under both mailbox
//! locks. Fresh identities prevent an old turn's receipt retracting new input.

use std::sync::Arc;

use super::{
    InputId, InputKind, MAX_PENDING_BYTES, MAX_PENDING_INPUTS, RunData, SessionControlHandle,
    control_error, lock,
};
use crate::error::Result;

/// Identity mapping for one input moved to a new turn's mailbox.
///
/// Neither identity acknowledges provider execution or session persistence.
/// The new identity can be passed to the destination handle's `retract` method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferredInput {
    pub previous_id: InputId,
    pub new_id: InputId,
    pub kind: InputKind,
}

impl SessionControlHandle {
    /// Move all unclaimed inputs from this FINISHED turn into an accepting one.
    ///
    /// Build the next turn and obtain its control handle before awaiting it.
    /// This call appends after any inputs already queued there, preserving the
    /// source's admission order and each input's steering/follow-up lane. Text,
    /// images and media are moved without flattening or re-expansion.
    ///
    /// Capacity, same-turn and lifecycle failures leave BOTH queues unchanged.
    /// A successful call empties this mailbox; repeating it while the target
    /// still accepts input transfers nothing.
    /// Concurrent transfers use one lock order and cannot deliver input twice.
    /// Already handed-off inputs are never transferred: inspect the completed
    /// turn and history before choosing prompt versus continue_turn.
    ///
    /// ```no_run
    /// use pi::session_control::{ControllableSession, SessionControlHandle};
    ///
    /// # async fn recover(session: &mut ControllableSession, old: &SessionControlHandle)
    /// #     -> Result<(), Box<dyn std::error::Error>> {
    /// let next = session.continue_turn(|_| {})?;
    /// let receipts = old.transfer_pending_to(&next.control())?;
    /// let completion = next.await?;
    /// # let _ = (receipts, completion);
    /// # Ok(())
    /// # }
    /// ```
    pub fn transfer_pending_to(&self, destination: &Self) -> Result<Vec<TransferredInput>> {
        if Arc::ptr_eq(&self.run, &destination.run) {
            return Err(control_error(
                "SESSION_CONTROL_SAME_TURN",
                "pending input cannot be transferred to the same turn",
            ));
        }
        // Arc allocations stay alive throughout this operation. Address order
        // is used only to order locks, never as a persisted session identity.
        if Arc::as_ptr(&self.run) < Arc::as_ptr(&destination.run) {
            let mut source = lock(&self.run.data);
            let mut target = lock(&destination.run.data);
            transfer(&mut source, &mut target)
        } else {
            let mut target = lock(&destination.run.data);
            let mut source = lock(&self.run.data);
            transfer(&mut source, &mut target)
        }
    }
}

fn transfer(source: &mut RunData, target: &mut RunData) -> Result<Vec<TransferredInput>> {
    if !source.finished || source.accepting_input {
        return Err(control_error(
            "SESSION_CONTROL_NOT_FINISHED",
            "await or drop the source turn before transferring unclaimed input",
        ));
    }
    if !target.accepting_input || target.finished {
        return Err(control_error(
            "SESSION_CONTROL_CLOSED",
            "the destination turn is no longer accepting input",
        ));
    }
    let count = source.pending.len();
    let bytes = target
        .pending_bytes
        .checked_add(source.pending_bytes)
        .filter(|bytes| *bytes <= MAX_PENDING_BYTES);
    let total = target
        .pending
        .len()
        .checked_add(count)
        .filter(|count| *count <= MAX_PENDING_INPUTS);
    let (Some(bytes), Some(_)) = (bytes, total) else {
        return Err(control_error(
            "SESSION_CONTROL_FULL",
            "the destination cannot hold the entire recovered input batch; neither queue changed",
        ));
    };
    let mut transfers = Vec::new();
    transfers.try_reserve(count).map_err(|_| {
        control_error(
            "SESSION_CONTROL_CAPACITY",
            "cannot allocate input transfer receipts",
        )
    })?;
    target.pending.try_reserve(count).map_err(|_| {
        control_error(
            "SESSION_CONTROL_CAPACITY",
            "cannot allocate destination input queue",
        )
    })?;
    // Finish allocation and identity generation BEFORE taking any payload.
    // Even a failure generating an identity cannot leave a partially moved batch.
    for input in &source.pending {
        transfers.push(TransferredInput {
            previous_id: input.id,
            new_id: InputId(uuid::Uuid::new_v4()),
            kind: input.kind,
        });
    }
    for (mut input, receipt) in source.pending.drain(..).zip(&transfers) {
        input.id = receipt.new_id;
        target.pending.push_back(input);
    }
    source.pending_bytes = 0;
    target.pending_bytes = bytes;
    Ok(transfers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ContentBlock, ImageContent, TextContent, UserContent};
    use crate::session_control::tests::live;
    use crate::session_control::{InputKind, MAX_INPUT_BYTES};
    use std::sync::Barrier;

    #[test]
    fn recovery_preserves_order_lanes_payloads_and_authored_text() {
        let (source, guard, _) = live();
        let first = source.follow_up("first follow-up").unwrap();
        let content = UserContent::Blocks(vec![
            ContentBlock::Text(TextContent::new("expanded context")),
            ContentBlock::Image(ImageContent {
                data: "aGVsbG8=".to_string(),
                mime_type: "image/png".to_string(),
            }),
        ]);
        let second = source
            .steer_with_content(&content, "  authored text  ")
            .unwrap();
        let before = source.snapshot().pending_bytes;
        drop(guard);
        let (target, _target_guard, _) = live();
        let existing = target.follow_up("already there").unwrap();
        let existing_bytes = target.snapshot().pending_bytes;
        let receipts = source.transfer_pending_to(&target).unwrap();
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0].previous_id, first);
        assert_eq!(receipts[1].previous_id, second);
        assert_eq!(receipts[0].kind, InputKind::FollowUp);
        assert_eq!(receipts[1].kind, InputKind::Steering);
        assert!(
            receipts
                .iter()
                .all(|receipt| receipt.new_id != receipt.previous_id)
        );
        assert_eq!(source.snapshot().pending_bytes, 0);
        assert_eq!(target.snapshot().pending_bytes, before + existing_bytes);
        assert!(source.transfer_pending_to(&target).unwrap().is_empty());
        assert!(
            target.retract(second).is_none(),
            "old identities are retired"
        );
        let recovered = target.take_pending();
        assert_eq!(recovered[0].id, existing);
        assert_eq!(recovered[1].id, receipts[0].new_id);
        assert_eq!(recovered[1].text, "first follow-up");
        assert_eq!(recovered[2].id, receipts[1].new_id);
        assert_eq!(recovered[2].text, "  authored text  ");
        assert_eq!(
            serde_json::to_value(&recovered[2].content).unwrap(),
            serde_json::to_value(&content).unwrap()
        );
        assert_eq!(target.snapshot().handed_to_agent, 0);
    }

    #[test]
    fn handed_off_input_is_never_replayed_by_recovery() {
        let (source, guard, _) = live();
        source.steer("already consumed").unwrap();
        let pending = source.follow_up("not consumed").unwrap();
        assert_eq!(source.run.fetch(InputKind::Steering).len(), 1);
        drop(guard);
        let (target, _target_guard, _) = live();
        let moved = source.transfer_pending_to(&target).unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].previous_id, pending);
        assert_eq!(source.snapshot().handed_to_agent, 1);
        assert_eq!(target.snapshot().handed_to_agent, 0);
        assert_eq!(
            target.run.fetch(InputKind::FollowUp)[0].text_for_display(),
            Some("not consumed")
        );
        assert!(target.run.fetch(InputKind::Steering).is_empty());
        assert_eq!(target.snapshot().pending_bytes, 0);
    }

    #[test]
    fn running_source_and_aborted_but_undrained_source_are_refused() {
        let (source, guard, _) = live();
        source.steer("retain me").unwrap();
        let (target, _target_guard, _) = live();
        for aborted in [false, true] {
            if aborted {
                assert!(source.abort());
            }
            let before = source.snapshot();
            let target_before = target.snapshot();
            let error = source.transfer_pending_to(&target).unwrap_err();
            assert!(error.to_string().contains("SESSION_CONTROL_NOT_FINISHED"));
            assert_eq!(source.snapshot(), before);
            assert_eq!(target.snapshot(), target_before);
        }
        drop(guard);
        assert_eq!(source.transfer_pending_to(&target).unwrap().len(), 1);
    }

    #[test]
    fn same_turn_and_closed_destination_leave_source_recoverable() {
        let (source, guard, _) = live();
        let id = source.steer("retain me").unwrap();
        drop(guard);
        assert!(source.transfer_pending_to(&source).is_err());
        let (target, target_guard, _) = live();
        assert!(target.abort());
        assert!(source.transfer_pending_to(&target).is_err());
        drop(target_guard);
        assert!(source.transfer_pending_to(&target).is_err());
        assert_eq!(source.take_pending()[0].id, id);
    }

    #[test]
    fn count_capacity_failure_is_atomic_and_can_be_retried() {
        let (source, guard, _) = live();
        let id = source.steer("batch A").unwrap();
        source.follow_up("batch B").unwrap();
        drop(guard);
        let (target, _target_guard, _) = live();
        for _ in 0..MAX_PENDING_INPUTS - 1 {
            target.steer("occupied").unwrap();
        }
        let source_before = source.snapshot();
        let target_before = target.snapshot();
        let error = source.transfer_pending_to(&target).unwrap_err();
        assert!(error.to_string().contains("SESSION_CONTROL_FULL"));
        assert_eq!(source.snapshot(), source_before);
        assert_eq!(target.snapshot(), target_before);
        assert_eq!(target.run.fetch(InputKind::Steering).len(), 1);
        let receipts = source.transfer_pending_to(&target).unwrap();
        assert_eq!(receipts[0].previous_id, id);
        assert_eq!(
            target.snapshot().pending_steering + target.snapshot().pending_follow_up,
            MAX_PENDING_INPUTS
        );
    }

    #[test]
    fn byte_capacity_failure_does_not_partially_move_a_batch() {
        let (source, guard, _) = live();
        let text = "x".repeat(MAX_INPUT_BYTES);
        source.steer(&text).unwrap();
        source.steer(&text).unwrap();
        drop(guard);
        let (target, _target_guard, _) = live();
        while target.follow_up(&text).is_ok() {}
        let before = target.snapshot();
        assert!(source.transfer_pending_to(&target).is_err());
        assert_eq!(target.snapshot(), before);
        assert_eq!(source.snapshot().pending_steering, 2);
        drop(target.take_pending());
        assert_eq!(source.transfer_pending_to(&target).unwrap().len(), 2);
    }

    #[test]
    fn concurrent_recovery_moves_each_payload_exactly_once() {
        let (source, guard, _) = live();
        for index in 0..16 {
            source.steer(&format!("input {index}")).unwrap();
        }
        drop(guard);
        let (target, _target_guard, _) = live();
        let start = Arc::new(Barrier::new(3));
        std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                start.wait();
                source.transfer_pending_to(&target).unwrap().len()
            });
            let b = scope.spawn(|| {
                start.wait();
                source.transfer_pending_to(&target).unwrap().len()
            });
            start.wait();
            assert_eq!(a.join().unwrap() + b.join().unwrap(), 16);
        });
        let queued = target.take_pending();
        assert_eq!(queued.len(), 16);
        for (index, input) in queued.iter().enumerate() {
            assert_eq!(input.text, format!("input {index}"));
        }
        assert!(source.take_pending().is_empty());
    }
}
