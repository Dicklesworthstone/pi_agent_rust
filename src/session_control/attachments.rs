//! Attachment-aware queue admission. Count serialized bytes without building
//! another encoded copy; preserve native content and pre-expansion authorship.

use std::io::{self, Write};

use crate::error::Result;
use crate::model::{ContentBlock, UserContent};

use super::{
    InputId, InputKind, MAX_INPUT_BYTES, MAX_PENDING_BYTES, MAX_PENDING_INPUTS, PendingInput,
    SessionControlHandle, control_error, lock,
};

const MAX_CONTENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_CONTENT_BLOCKS: usize = 128;

struct ByteBudget(usize);

impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("queued content exceeds its byte limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn admitted_bytes(content: &UserContent, source: &str) -> Result<usize> {
    if source.len() > MAX_INPUT_BYTES {
        return Err(control_error(
            "SESSION_CONTROL_INPUT",
            "authored source exceeds 256 KiB",
        ));
    }
    let valid = match content {
        UserContent::Text(text) => !text.trim().is_empty(),
        UserContent::Blocks(blocks) => {
            !blocks.is_empty()
                && blocks.len() <= MAX_CONTENT_BLOCKS
                && blocks.iter().all(|block| {
                    matches!(
                        block,
                        ContentBlock::Text(_) | ContentBlock::Image(_) | ContentBlock::Media(_)
                    )
                })
                && blocks.iter().any(|block| match block {
                    ContentBlock::Text(text) => !text.text.trim().is_empty(),
                    ContentBlock::Image(image) => !image.data.is_empty(),
                    ContentBlock::Media(media) => !media.data.is_empty(),
                    _ => false,
                })
        }
    };
    if !valid {
        return Err(control_error(
            "SESSION_CONTROL_INPUT",
            "queued user content must be nonempty text or at most 128 text/image/media blocks",
        ));
    }
    let mut budget = ByteBudget(MAX_CONTENT_BYTES);
    serde_json::to_writer(&mut budget, content).map_err(|_| {
        control_error(
            "SESSION_CONTROL_INPUT",
            "queued content exceeds 4 MiB when serialized",
        )
    })?;
    // Both the expanded payload and raw authored source are retained. The
    // serialized count is conservative for escaped text and includes labels.
    Ok(MAX_CONTENT_BYTES - budget.0 + source.len())
}

impl SessionControlHandle {
    /// Queue user content without flattening images/media or scanning an
    /// attached document for magic keywords. `authored_source` is only the
    /// original user prose, before templates or file wrappers were expanded.
    /// This does not fetch paths/URLs or grant additional tool permissions.
    pub fn steer_with_content(
        &self,
        content: &UserContent,
        authored_source: &str,
    ) -> Result<InputId> {
        self.enqueue_content(InputKind::Steering, content, authored_source)
    }

    /// Attachment-aware counterpart of `follow_up`; provenance and block
    /// order survive queueing, recovery, and handoff to the agent unchanged.
    pub fn follow_up_with_content(
        &self,
        content: &UserContent,
        authored_source: &str,
    ) -> Result<InputId> {
        self.enqueue_content(InputKind::FollowUp, content, authored_source)
    }

    /// Admit several inputs together, or leave the mailbox completely unchanged.
    /// Each tuple is `(lane, native_content, authored_source)`. Identities are
    /// returned in that same order, and the batch appends after existing input.
    /// This avoids a loop of individual sends accepting an instruction before
    /// discovering that another required instruction is invalid or cannot fit.
    ///
    /// Every item uses the ordinary native-content and source limits; the
    /// complete batch must fit the shared count/byte budgets. Empty batches are
    /// rejected. Text, images and media retain their original block order and
    /// explicit authorship. Neither the caller's payloads nor existing queued
    /// input are changed on an error.
    ///
    /// Only ADMISSION is atomic. The agent still selects steering/follow-up
    /// inputs at its normal boundaries, and may stop after consuming part of
    /// the accepted batch. These identities acknowledge neither execution nor
    /// persistence; reclaim only unclaimed input rather than replaying a batch.
    pub fn enqueue_batch_with_content(
        &self,
        inputs: &[(InputKind, UserContent, String)],
    ) -> Result<Vec<InputId>> {
        if inputs.is_empty() || inputs.len() > MAX_PENDING_INPUTS {
            return Err(control_error(
                "SESSION_CONTROL_INPUT",
                "a batch must contain between 1 and 100 inputs",
            ));
        }
        let mut sizes = Vec::with_capacity(inputs.len());
        let mut bytes = 0_usize;
        for (_, content, source) in inputs {
            let size = admitted_bytes(content, source)?;
            bytes = bytes
                .checked_add(size)
                .filter(|total| *total <= MAX_PENDING_BYTES)
                .ok_or_else(|| {
                    control_error(
                        "SESSION_CONTROL_FULL",
                        "input batch exceeds the pending byte limit; no input was accepted",
                    )
                })?;
            sizes.push(size);
        }
        // Stage bounded payload copies and all identities before publication.
        // In particular, copying attachments must not hold up an abort/fetch.
        let capacity_error = |_| {
            control_error(
                "SESSION_CONTROL_CAPACITY",
                "cannot allocate the complete input batch; no input was accepted",
            )
        };
        let mut staged = Vec::new();
        staged
            .try_reserve_exact(inputs.len())
            .map_err(capacity_error)?;
        let mut ids = Vec::new();
        ids.try_reserve_exact(inputs.len())
            .map_err(capacity_error)?;
        for ((kind, content, source), size) in inputs.iter().zip(sizes) {
            let id = InputId(uuid::Uuid::new_v4());
            staged.push(PendingInput {
                id,
                kind: *kind,
                text: source.clone(),
                content: content.clone(),
                bytes: size,
            });
            ids.push(id);
        }
        let mut data = lock(&self.run.data);
        // Recheck only here: an abort, fetch or other producer could have run
        // during validation/copying. Nothing was visible in the mailbox yet.
        if !data.accepting_input {
            return Err(control_error(
                "SESSION_CONTROL_CLOSED",
                "this turn no longer accepts input",
            ));
        }
        if inputs.len() > MAX_PENDING_INPUTS.saturating_sub(data.pending.len())
            || bytes > MAX_PENDING_BYTES.saturating_sub(data.pending_bytes)
        {
            return Err(control_error(
                "SESSION_CONTROL_FULL",
                "the entire input batch does not fit; no input was accepted",
            ));
        }
        data.pending
            .try_reserve(inputs.len())
            .map_err(capacity_error)?;
        data.pending.extend(staged);
        data.pending_bytes += bytes;
        drop(data);
        Ok(ids)
    }

    /// Correct unclaimed input without retracting it or moving it to the tail.
    /// The replacement stays in the original steering/follow-up lane. Text is
    /// treated as user-authored, exactly as with `steer` and `follow_up`.
    ///
    /// `Some(new_id)` retires the old identity: a stale editor cannot overwrite
    /// or retract the correction. `None` means that identity is no longer in
    /// this queue (possibly already handed to the agent); it is NOT permission
    /// to resubmit the text. Validation, capacity and closed-turn errors leave
    /// the original input untouched. Admission is still not a durable receipt.
    pub fn replace_pending(&self, id: InputId, text: &str) -> Result<Option<InputId>> {
        if text.len() > MAX_INPUT_BYTES || text.trim().is_empty() {
            return Err(control_error(
                "SESSION_CONTROL_INPUT",
                "input must be nonblank and at most 256 KiB",
            ));
        }
        self.replace_pending_with_content(id, &UserContent::Text(text.to_string()), text)
    }

    /// Atomically replace a queued input's native payload and authored source.
    /// Block order and bytes are preserved; attachments are never flattened,
    /// re-read or implicitly granted keyword authority. This operation competes
    /// with fetch/retract under the same mailbox lock, so only the complete old
    /// or complete new input can be handed off, never both or a mixture.
    ///
    /// The original position and lane survive. Byte admission credits the old
    /// payload, allowing corrections in a count-full queue and smaller edits in
    /// a byte-full queue. A failed edit does not withdraw accepted work.
    pub fn replace_pending_with_content(
        &self,
        id: InputId,
        content: &UserContent,
        authored_source: &str,
    ) -> Result<Option<InputId>> {
        let bytes = admitted_bytes(content, authored_source)?;
        let mut data = lock(&self.run.data);
        if !data.accepting_input {
            return Err(control_error(
                "SESSION_CONTROL_CLOSED",
                "this turn no longer accepts input",
            ));
        }
        let Some(index) = data.pending.iter().position(|input| input.id == id) else {
            return Ok(None);
        };
        let retained_bytes = data.pending_bytes - data.pending[index].bytes;
        if bytes > MAX_PENDING_BYTES.saturating_sub(retained_bytes) {
            return Err(control_error(
                "SESSION_CONTROL_FULL",
                "replacement exceeds pending input capacity; original input was retained",
            ));
        }
        // Finish allocation and identity generation before changing the queue.
        let new_id = InputId(uuid::Uuid::new_v4());
        let replacement = PendingInput {
            id: new_id,
            kind: data.pending[index].kind,
            text: authored_source.to_string(),
            content: content.clone(),
            bytes,
        };
        let previous = std::mem::replace(&mut data.pending[index], replacement);
        data.pending_bytes = retained_bytes + bytes;
        drop(data);
        // Large replaced attachments are released outside the mailbox lock.
        drop(previous);
        Ok(Some(new_id))
    }

    pub(super) fn enqueue_content(
        &self,
        kind: InputKind,
        content: &UserContent,
        source: &str,
    ) -> Result<InputId> {
        let bytes = admitted_bytes(content, source)?;
        let mut data = lock(&self.run.data);
        if !data.accepting_input {
            return Err(control_error(
                "SESSION_CONTROL_CLOSED",
                "this turn no longer accepts input",
            ));
        }
        if data.pending.len() >= MAX_PENDING_INPUTS
            || bytes > MAX_PENDING_BYTES.saturating_sub(data.pending_bytes)
        {
            return Err(control_error(
                "SESSION_CONTROL_FULL",
                "pending input limit reached; no existing input was discarded",
            ));
        }
        let id = InputId(uuid::Uuid::new_v4());
        let input = PendingInput {
            id,
            kind,
            text: source.to_string(),
            content: content.clone(),
            bytes,
        };
        data.pending_bytes += bytes;
        data.pending.push_back(input);
        drop(data);
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::live;
    use super::*;
    use crate::model::{ImageContent, Message, TextContent};

    fn attachment() -> UserContent {
        UserContent::Blocks(vec![
            ContentBlock::Text(TextContent::new("<file>ultrathink in attached text</file>")),
            ContentBlock::Image(ImageContent {
                data: "aGVsbG8=".to_string(),
                mime_type: "image/png".to_string(),
            }),
            ContentBlock::Text(TextContent::new("after image")),
        ])
    }

    #[test]
    fn native_blocks_and_authorship_survive_handoff() {
        let (control, _guard, _) = live();
        let content = attachment();
        control
            .steer_with_content(&content, "inspect this attachment")
            .unwrap();
        let claimed = control.run.fetch(InputKind::Steering);
        assert_eq!(
            claimed[0].keyword_scan_source(),
            Some("inspect this attachment")
        );
        let Message::User(user) = claimed[0].message() else {
            panic!("user content");
        };
        assert_eq!(
            serde_json::to_value(&user.content).unwrap(),
            serde_json::to_value(content).unwrap()
        );
    }

    #[test]
    fn reclaim_preserves_expanded_content_and_raw_editor_text() {
        let (control, guard, _) = live();
        let content = attachment();
        control
            .follow_up_with_content(&content, "@diagram.png inspect")
            .unwrap();
        drop(guard);
        let pending = control.take_pending();
        assert_eq!(pending[0].text, "@diagram.png inspect");
        assert_eq!(
            serde_json::to_value(&pending[0].content).unwrap(),
            serde_json::to_value(content).unwrap()
        );
        assert_eq!(control.snapshot().pending_bytes, 0);
    }

    #[test]
    fn retract_removes_only_selected_pending_input_and_refunds_bytes() {
        let (control, _guard, _) = live();
        let first = control.steer("keep first").unwrap();
        let second = control
            .follow_up_with_content(&attachment(), "remove this")
            .unwrap();
        let before = control.snapshot().pending_bytes;
        let removed = control.retract(second).unwrap();
        assert_eq!(control.snapshot().pending_bytes, before - removed.bytes);
        assert!(control.retract(second).is_none());
        control.run.fetch(InputKind::Steering);
        assert!(control.retract(first).is_none());
    }

    #[test]
    fn content_shape_and_escaped_size_are_bounded_before_admission() {
        assert!(admitted_bytes(&UserContent::Blocks(vec![]), "").is_err());
        let wide = UserContent::Blocks(vec![
            ContentBlock::Text(TextContent::new("x"));
            MAX_CONTENT_BLOCKS + 1
        ]);
        assert!(admitted_bytes(&wide, "").is_err());
        let escaped = UserContent::Text("\u{0001}".repeat(MAX_CONTENT_BYTES / 2));
        assert!(admitted_bytes(&escaped, "").is_err());
        assert!(admitted_bytes(&attachment(), &"x".repeat(MAX_INPUT_BYTES + 1)).is_err());
    }

    #[test]
    fn attachment_text_is_never_the_implicit_keyword_scan_source() {
        let (control, _guard, _) = live();
        control.steer_with_content(&attachment(), "").unwrap();
        let claimed = control.run.fetch(InputKind::Steering);
        assert_eq!(claimed[0].keyword_scan_source(), Some(""));
    }

    #[test]
    fn stale_receipt_cannot_retract_input_from_a_later_turn() {
        let (old, guard, _) = live();
        let stale_id = old.steer("old input").unwrap();
        drop(guard);
        let (new, _guard, _) = live();
        let new_id = new.steer("new input").unwrap();
        assert_ne!(stale_id, new_id);
        assert!(new.retract(stale_id).is_none());
        assert_eq!(new.retract(new_id).unwrap().text, "new input");
    }

    #[test]
    fn replacement_preserves_position_lane_and_exact_authored_text() {
        let (control, _guard, _) = live();
        let first = control.follow_up("first").unwrap();
        let old = control.steer("draft").unwrap();
        let last = control.follow_up("last").unwrap();
        let replacement = "  corrected\nrequest  ";
        let new = control.replace_pending(old, replacement).unwrap().unwrap();
        assert_ne!(new, old);
        assert!(control.retract(old).is_none());
        assert_eq!(control.replace_pending(old, "stale edit").unwrap(), None);
        assert_eq!(control.snapshot().handed_to_agent, 0);
        let pending = control.take_pending();
        assert_eq!(
            pending
                .iter()
                .map(|input| (input.id, input.kind, input.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (first, InputKind::FollowUp, "first"),
                (new, InputKind::Steering, replacement),
                (last, InputKind::FollowUp, "last"),
            ]
        );
        assert!(matches!(
            &pending[1].content,
            UserContent::Text(text) if text == replacement
        ));
        assert_eq!(control.snapshot().pending_bytes, 0);
    }

    #[test]
    fn native_replacement_preserves_images_media_and_keyword_provenance() {
        let (control, _guard, _) = live();
        let old = control.follow_up("before attachments were ready").unwrap();
        let mut content = attachment();
        let UserContent::Blocks(blocks) = &mut content else {
            panic!("fixture is native content");
        };
        blocks.insert(
            1,
            ContentBlock::Media(crate::model::MediaContent {
                data: "AP+A/w==".to_string(),
                mime_type: "audio/wav".to_string(),
                name: Some("voice.wav".to_string()),
            }),
        );
        control
            .replace_pending_with_content(old, &content, "inspect the files")
            .unwrap()
            .unwrap();
        assert!(control.run.fetch(InputKind::Steering).is_empty());
        let claimed = control.run.fetch(InputKind::FollowUp);
        assert_eq!(claimed.len(), 1);
        assert_eq!(
            claimed[0].keyword_scan_source(),
            Some("inspect the files")
        );
        let Message::User(user) = claimed[0].message() else {
            panic!("native user input");
        };
        assert_eq!(
            serde_json::to_value(&user.content).unwrap(),
            serde_json::to_value(&content).unwrap()
        );
        assert_eq!(control.snapshot().pending_bytes, 0);
        assert_eq!(control.snapshot().handed_to_agent, 1);
        assert!(control.run.fetch(InputKind::FollowUp).is_empty());
    }

    #[test]
    fn replacement_is_allowed_at_the_input_count_limit() {
        let (control, _guard, _) = live();
        let old = control.steer("before").unwrap();
        for _ in 1..MAX_PENDING_INPUTS {
            control.follow_up("occupied").unwrap();
        }
        assert!(control.steer("does not fit").is_err());
        let corrected = control
            .replace_pending(old, "corrected without reordering")
            .unwrap()
            .unwrap();
        let snapshot = control.snapshot();
        assert_eq!(snapshot.pending_steering, 1);
        assert_eq!(snapshot.pending_follow_up, MAX_PENDING_INPUTS - 1);
        let pending = control.take_pending();
        assert_eq!(pending[0].id, corrected);
        assert_eq!(pending[0].text, "corrected without reordering");
    }

    #[test]
    fn replacement_credits_old_bytes_and_refuses_growth_without_losing_input() {
        let (control, _guard, _) = live();
        let old = control.steer("original").unwrap();
        // Two native text payloads occupy every remaining counted byte.
        let large = UserContent::Text("x".repeat(MAX_CONTENT_BYTES - 2));
        control.follow_up_with_content(&large, "").unwrap();
        let remaining = MAX_PENDING_BYTES - control.snapshot().pending_bytes;
        let rest = UserContent::Text("y".repeat(remaining - 2));
        control.follow_up_with_content(&rest, "").unwrap();
        let full = control.snapshot();
        assert_eq!(full.pending_bytes, MAX_PENDING_BYTES);
        let error = control
            .replace_pending(old, "larger than the original input")
            .unwrap_err();
        assert!(error.to_string().contains("SESSION_CONTROL_FULL"));
        assert_eq!(control.snapshot(), full);
        let corrected = control.replace_pending(old, "a").unwrap().unwrap();
        let refunded =
            admitted_bytes(&UserContent::Text("original".to_string()), "original").unwrap()
                - admitted_bytes(&UserContent::Text("a".to_string()), "a").unwrap();
        assert_eq!(
            control.snapshot().pending_bytes,
            MAX_PENDING_BYTES - refunded
        );
        control
            .steer("z")
            .expect("refunded bytes admit later input");
        let pending = control.take_pending();
        assert_eq!(pending[0].id, corrected);
        assert_eq!(pending[0].text, "a");
        assert_eq!(pending.len(), 4);
        assert_eq!(control.snapshot().pending_bytes, 0);
    }

    #[test]
    fn invalid_replacements_leave_the_original_identity_and_payload_intact() {
        let (control, _guard, _) = live();
        let content = attachment();
        let old = control
            .steer_with_content(&content, "keep original")
            .unwrap();
        let before = control.snapshot();
        for bad in [
            UserContent::Blocks(Vec::new()),
            UserContent::Text(" \n ".to_string()),
            UserContent::Text("\u{0001}".repeat(MAX_CONTENT_BYTES / 2)),
            UserContent::Blocks(vec![
                ContentBlock::Text(TextContent::new("x"));
                MAX_CONTENT_BLOCKS + 1
            ]),
        ] {
            assert!(
                control
                    .replace_pending_with_content(old, &bad, "valid source")
                    .is_err()
            );
            assert_eq!(control.snapshot(), before);
        }
        assert!(
            control
                .replace_pending_with_content(old, &content, &"x".repeat(MAX_INPUT_BYTES + 1))
                .is_err()
        );
        assert!(control.replace_pending(old, " ").is_err());
        assert!(
            control
                .replace_pending(old, &"x".repeat(MAX_INPUT_BYTES + 1))
                .is_err()
        );
        assert_eq!(control.snapshot(), before);
        let retained = control
            .retract(old)
            .expect("failed edits must not retire the original");
        assert_eq!(retained.text, "keep original");
        assert_eq!(
            serde_json::to_value(retained.content).unwrap(),
            serde_json::to_value(content).unwrap()
        );
    }

    #[test]
    fn absent_or_handed_off_id_is_not_requeued_by_an_edit() {
        let (control, _guard, _) = live();
        let consumed = control.steer("consumed").unwrap();
        control.run.fetch(InputKind::Steering);
        let retracted = control.follow_up("withdrawn").unwrap();
        control.retract(retracted).unwrap();
        let (other, _other_guard, _) = live();
        let foreign = other.steer("other turn").unwrap();
        let before = control.snapshot();
        for id in [consumed, retracted, foreign] {
            assert_eq!(
                control.replace_pending(id, "must not be enqueued").unwrap(),
                None
            );
            assert_eq!(control.snapshot(), before);
        }
        assert_eq!(other.retract(foreign).unwrap().text, "other turn");
        assert_eq!(control.snapshot().handed_to_agent, 1);
    }

    #[test]
    fn abort_and_completion_refuse_edits_without_discarding_recoverable_input() {
        for finished in [false, true] {
            let (control, guard, _) = live();
            let old = control.steer("accepted work").unwrap();
            if finished {
                drop(guard);
            } else {
                assert!(control.abort());
            }
            let before = control.snapshot();
            let error = control.replace_pending(old, "too late").unwrap_err();
            assert!(error.to_string().contains("SESSION_CONTROL_CLOSED"));
            assert_eq!(control.snapshot(), before);
            assert_eq!(control.retract(old).unwrap().text, "accepted work");
        }
    }

    #[test]
    fn competing_editors_cannot_overwrite_a_newer_revision() {
        let (control, _guard, _) = live();
        let old = control.steer("original").unwrap();
        let start = std::sync::Barrier::new(3);
        let (a, b) = std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                start.wait();
                control.replace_pending(old, "candidate A").unwrap()
            });
            let b = scope.spawn(|| {
                start.wait();
                control.replace_pending(old, "candidate B").unwrap()
            });
            start.wait();
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_ne!(
            a.is_some(),
            b.is_some(),
            "exactly one edit owns the old revision"
        );
        let pending = control.take_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(Some(pending[0].id), a.or(b));
        assert_eq!(
            pending[0].text,
            if a.is_some() { "candidate A" } else { "candidate B" }
        );
    }

    #[test]
    fn racing_fetch_observes_one_complete_input_not_both_revisions() {
        for _ in 0..32 {
            let (control, _guard, _) = live();
            let old = control.steer("original").unwrap();
            let start = std::sync::Barrier::new(3);
            let (edited, claimed) = std::thread::scope(|scope| {
                let edit = scope.spawn(|| {
                    start.wait();
                    control.replace_pending(old, "corrected").unwrap()
                });
                let fetch = scope.spawn(|| {
                    start.wait();
                    control.run.fetch(InputKind::Steering)
                });
                start.wait();
                (edit.join().unwrap(), fetch.join().unwrap())
            });
            let expected = if edited.is_some() {
                "corrected"
            } else {
                "original"
            };
            assert_eq!(claimed.len(), 1);
            assert_eq!(claimed[0].keyword_scan_source(), Some(expected));
            assert_eq!(claimed[0].text_for_display(), Some(expected));
            assert_eq!(control.snapshot().handed_to_agent, 1);
            assert_eq!(control.snapshot().pending_bytes, 0);
            assert!(control.take_pending().is_empty());
        }
    }

    #[test]
    fn corrected_native_input_remains_recoverable_after_the_turn_finishes() {
        let (source, guard, _) = live();
        let old = source.follow_up("initial draft").unwrap();
        let content = attachment();
        let corrected = source
            .replace_pending_with_content(old, &content, "corrected source")
            .unwrap()
            .unwrap();
        drop(guard);
        let (target, _target_guard, _) = live();
        let receipts = source.transfer_pending_to(&target).unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].previous_id, corrected);
        assert_eq!(receipts[0].kind, InputKind::FollowUp);
        assert!(target.retract(old).is_none());
        assert!(target.retract(corrected).is_none());
        let recovered = target.retract(receipts[0].new_id).unwrap();
        assert_eq!(recovered.text, "corrected source");
        assert_eq!(
            serde_json::to_value(recovered.content).unwrap(),
            serde_json::to_value(content).unwrap()
        );
        assert!(source.take_pending().is_empty());
    }

    fn batch_text(kind: InputKind, text: &str) -> (InputKind, UserContent, String) {
        (kind, UserContent::Text(text.to_string()), text.to_string())
    }

    #[test]
    fn batch_appends_in_admission_order_with_exact_payloads_and_accounting() {
        let (control, _guard, _) = live();
        let existing = control.steer("already accepted").unwrap();
        let existing_bytes = control.snapshot().pending_bytes;
        let inputs = vec![
            (
                InputKind::FollowUp,
                attachment(),
                "inspect @diagram.png".to_string(),
            ),
            batch_text(InputKind::Steering, "  preserve constraints  "),
            batch_text(InputKind::FollowUp, "then summarize"),
        ];
        let expected_bytes: usize = inputs
            .iter()
            .map(|(_, content, source)| serde_json::to_vec(content).unwrap().len() + source.len())
            .sum();
        let ids = control.enqueue_batch_with_content(&inputs).unwrap();
        assert_eq!(ids.len(), inputs.len());
        assert_eq!(
            control.snapshot().pending_bytes,
            existing_bytes + expected_bytes
        );
        assert_eq!(control.snapshot().handed_to_agent, 0);
        let pending = control.take_pending();
        assert_eq!(pending[0].id, existing);
        for ((input, id), (kind, content, source)) in
            pending[1..].iter().zip(ids).zip(&inputs)
        {
            assert_eq!(input.id, id);
            assert_eq!(input.kind, *kind);
            assert_eq!(&input.text, source);
            assert_eq!(
                serde_json::to_value(&input.content).unwrap(),
                serde_json::to_value(content).unwrap()
            );
        }
        assert_eq!(control.snapshot().pending_bytes, 0);
    }

    #[test]
    fn admitted_batch_uses_normal_lane_boundaries_and_explicit_authorship() {
        let (control, _guard, _) = live();
        let inputs = vec![
            batch_text(InputKind::FollowUp, "after the turn"),
            (InputKind::Steering, attachment(), String::new()),
            batch_text(InputKind::Steering, "second steer"),
        ];
        let ids = control.enqueue_batch_with_content(&inputs).unwrap();
        let first = control.run.fetch(InputKind::Steering);
        assert_eq!(first.len(), 1, "admission must not pre-drain the batch");
        assert_eq!(first[0].keyword_scan_source(), Some(""));
        let Message::User(user) = first[0].message() else {
            panic!("native user input");
        };
        assert_eq!(
            serde_json::to_value(&user.content).unwrap(),
            serde_json::to_value(&inputs[1].1).unwrap()
        );
        assert!(control.retract(ids[1]).is_none());
        let second = control.run.fetch(InputKind::Steering);
        assert_eq!(second[0].text_for_display(), Some("second steer"));
        assert!(control.run.fetch(InputKind::Steering).is_empty());
        let last = control.run.fetch(InputKind::FollowUp);
        assert_eq!(last[0].text_for_display(), Some("after the turn"));
        assert_eq!(control.snapshot().handed_to_agent, 3);
        assert_eq!(control.snapshot().pending_bytes, 0);
    }

    #[test]
    fn invalid_last_item_cannot_leave_an_accepted_batch_prefix() {
        let (control, _guard, _) = live();
        let existing = control.steer("keep existing").unwrap();
        let mut inputs = vec![
            batch_text(InputKind::Steering, "first batch instruction"),
            (
                InputKind::FollowUp,
                UserContent::Blocks(Vec::new()),
                "invalid last".to_string(),
            ),
        ];
        let before = control.snapshot();
        let error = control.enqueue_batch_with_content(&inputs).unwrap_err();
        assert!(error.to_string().contains("SESSION_CONTROL_INPUT"));
        assert_eq!(control.snapshot(), before);
        inputs[1] = batch_text(InputKind::FollowUp, "repaired last instruction");
        let ids = control.enqueue_batch_with_content(&inputs).unwrap();
        let pending = control.take_pending();
        assert_eq!(
            pending.len(),
            3,
            "retry must not duplicate a partially accepted prefix"
        );
        assert_eq!(pending[0].id, existing);
        assert_eq!(pending[1].id, ids[0]);
        assert_eq!(pending[2].id, ids[1]);
    }

    #[test]
    fn late_serialized_or_source_limit_failure_retains_the_entire_old_queue() {
        let (control, _guard, _) = live();
        let existing = control.steer("keep existing").unwrap();
        let before = control.snapshot();
        for bad in [
            (
                InputKind::FollowUp,
                UserContent::Text("\u{0001}".repeat(MAX_CONTENT_BYTES / 2)),
                String::new(),
            ),
            (
                InputKind::FollowUp,
                attachment(),
                "x".repeat(MAX_INPUT_BYTES + 1),
            ),
        ] {
            let inputs = vec![
                batch_text(InputKind::Steering, "must not be partially admitted"),
                bad,
            ];
            assert!(control.enqueue_batch_with_content(&inputs).is_err());
            assert_eq!(control.snapshot(), before);
        }
        let pending = control.take_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, existing);
        assert_eq!(pending[0].text, "keep existing");
    }

    #[test]
    fn batch_count_limit_is_all_or_nothing_and_admits_the_exact_boundary() {
        let (control, _guard, _) = live();
        let removable = control.steer("occupied").unwrap();
        for _ in 1..MAX_PENDING_INPUTS - 1 {
            control.follow_up("occupied").unwrap();
        }
        let inputs = [
            batch_text(InputKind::Steering, "batch A"),
            batch_text(InputKind::FollowUp, "batch B"),
        ];
        let before = control.snapshot();
        assert!(control.enqueue_batch_with_content(&inputs).is_err());
        assert_eq!(control.snapshot(), before);
        control.retract(removable).unwrap();
        let ids = control.enqueue_batch_with_content(&inputs).unwrap();
        let pending = control.take_pending();
        assert_eq!(pending.len(), MAX_PENDING_INPUTS);
        assert_eq!(pending[MAX_PENDING_INPUTS - 2].id, ids[0]);
        assert_eq!(pending[MAX_PENDING_INPUTS - 1].id, ids[1]);
    }

    #[test]
    fn aggregate_batch_bytes_are_bounded_even_when_every_item_fits_alone() {
        let (control, _guard, _) = live();
        let inputs = vec![
            (
                InputKind::Steering,
                UserContent::Text("x".repeat(MAX_CONTENT_BYTES - 2)),
                String::new(),
            ),
            (
                InputKind::FollowUp,
                UserContent::Text("y".repeat(MAX_CONTENT_BYTES - 2)),
                String::new(),
            ),
            batch_text(InputKind::FollowUp, "one more byte budget consumer"),
        ];
        let before = control.snapshot();
        let error = control.enqueue_batch_with_content(&inputs).unwrap_err();
        assert!(error.to_string().contains("SESSION_CONTROL_FULL"));
        assert_eq!(control.snapshot(), before);
        assert!(control.take_pending().is_empty());
    }

    #[test]
    fn batch_capacity_includes_existing_bytes_and_allows_retry_after_retraction() {
        let (control, _guard, _) = live();
        let existing = control.steer("existing bytes").unwrap();
        let inputs = vec![
            (
                InputKind::Steering,
                UserContent::Text("x".repeat(MAX_CONTENT_BYTES - 2)),
                String::new(),
            ),
            (
                InputKind::FollowUp,
                UserContent::Text("y".repeat(MAX_CONTENT_BYTES - 2)),
                String::new(),
            ),
        ];
        let before = control.snapshot();
        let error = control.enqueue_batch_with_content(&inputs).unwrap_err();
        assert!(error.to_string().contains("SESSION_CONTROL_FULL"));
        assert_eq!(control.snapshot(), before);
        assert_eq!(control.retract(existing).unwrap().text, "existing bytes");
        let ids = control.enqueue_batch_with_content(&inputs).unwrap();
        assert_eq!(control.snapshot().pending_bytes, MAX_PENDING_BYTES);
        let pending = control.take_pending();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].id, ids[0]);
        assert_eq!(pending[1].id, ids[1]);
        assert_eq!(control.snapshot().pending_bytes, 0);
    }

    #[test]
    fn empty_or_over_count_batches_do_not_change_the_mailbox() {
        let (control, _guard, _) = live();
        control.steer("original").unwrap();
        let before = control.snapshot();
        for inputs in [
            Vec::new(),
            vec![batch_text(InputKind::Steering, "x"); MAX_PENDING_INPUTS + 1],
        ] {
            let error = control.enqueue_batch_with_content(&inputs).unwrap_err();
            assert!(error.to_string().contains("SESSION_CONTROL_INPUT"));
            assert_eq!(control.snapshot(), before);
        }
        assert_eq!(control.take_pending()[0].text, "original");
    }

    #[test]
    fn closed_turn_refuses_a_whole_batch_without_discarding_accepted_work() {
        for finished in [false, true] {
            let (control, guard, _) = live();
            let existing = control.steer("accepted earlier").unwrap();
            if finished {
                drop(guard);
            } else {
                assert!(control.abort());
            }
            let before = control.snapshot();
            let error = control
                .enqueue_batch_with_content(&[
                    batch_text(InputKind::Steering, "A"),
                    batch_text(InputKind::FollowUp, "B"),
                ])
                .unwrap_err();
            assert!(error.to_string().contains("SESSION_CONTROL_CLOSED"));
            assert_eq!(control.snapshot(), before);
            assert_eq!(control.retract(existing).unwrap().text, "accepted earlier");
        }
    }

    #[test]
    fn competing_batches_cannot_interleave_or_split_the_last_queue_slots() {
        let (control, _guard, _) = live();
        for _ in 0..MAX_PENDING_INPUTS - 2 {
            control.steer("occupied").unwrap();
        }
        let start = std::sync::Barrier::new(3);
        let (a, b) = std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                start.wait();
                control.enqueue_batch_with_content(&[
                    batch_text(InputKind::Steering, "A0"),
                    batch_text(InputKind::FollowUp, "A1"),
                ])
            });
            let b = scope.spawn(|| {
                start.wait();
                control.enqueue_batch_with_content(&[
                    batch_text(InputKind::Steering, "B0"),
                    batch_text(InputKind::FollowUp, "B1"),
                ])
            });
            start.wait();
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_ne!(a.is_ok(), b.is_ok(), "exactly one complete batch fits");
        let expected = if a.is_ok() {
            ["A0", "A1"]
        } else {
            ["B0", "B1"]
        };
        let ids = a.or(b).unwrap();
        let pending = control.take_pending();
        assert_eq!(pending.len(), MAX_PENDING_INPUTS);
        for (index, item) in pending[MAX_PENDING_INPUTS - 2..].iter().enumerate() {
            assert_eq!(item.id, ids[index]);
            assert_eq!(item.text, expected[index]);
        }
    }

    #[test]
    fn abort_race_seals_the_mailbox_before_or_after_complete_batch_admission() {
        for _ in 0..16 {
            let (control, _guard, _) = live();
            let start = std::sync::Barrier::new(3);
            let accepted = std::thread::scope(|scope| {
                let send = scope.spawn(|| {
                    start.wait();
                    control.enqueue_batch_with_content(&[
                        batch_text(InputKind::Steering, "A"),
                        batch_text(InputKind::FollowUp, "B"),
                    ])
                });
                let abort = scope.spawn(|| {
                    start.wait();
                    control.abort()
                });
                start.wait();
                assert!(abort.join().unwrap());
                send.join().unwrap()
            });
            assert!(!control.snapshot().accepting_input);
            let pending = control.take_pending();
            match accepted {
                Ok(ids) => {
                    assert_eq!(pending.len(), 2);
                    assert_eq!(pending[0].id, ids[0]);
                    assert_eq!(pending[1].id, ids[1]);
                }
                Err(error) => {
                    assert!(error.to_string().contains("SESSION_CONTROL_CLOSED"));
                    assert!(pending.is_empty());
                }
            }
            assert_eq!(control.snapshot().handed_to_agent, 0);
        }
    }

    #[test]
    fn reclaim_race_observes_all_or_none_of_a_newly_admitted_batch() {
        for _ in 0..16 {
            let (control, _guard, _) = live();
            let inputs = (0..8)
                .map(|index| batch_text(InputKind::FollowUp, &format!("input {index}")))
                .collect::<Vec<_>>();
            let start = std::sync::Barrier::new(3);
            let (ids, reclaimed) = std::thread::scope(|scope| {
                let send = scope.spawn(|| {
                    start.wait();
                    control.enqueue_batch_with_content(&inputs).unwrap()
                });
                let reclaim = scope.spawn(|| {
                    start.wait();
                    control.take_pending()
                });
                start.wait();
                (send.join().unwrap(), reclaim.join().unwrap())
            });
            assert!(reclaimed.is_empty() || reclaimed.len() == inputs.len());
            let remaining = control.take_pending();
            assert_eq!(reclaimed.len() + remaining.len(), inputs.len());
            let recovered = if reclaimed.is_empty() {
                remaining
            } else {
                reclaimed
            };
            for (index, input) in recovered.iter().enumerate() {
                assert_eq!(input.id, ids[index]);
                assert_eq!(input.text, inputs[index].2);
            }
            assert_eq!(control.snapshot().pending_bytes, 0);
        }
    }
}
