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
        self.0 = self.0.checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("queued content exceeds its byte limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn admitted_bytes(content: &UserContent, source: &str) -> Result<usize> {
    if source.len() > MAX_INPUT_BYTES {
        return Err(control_error("SESSION_CONTROL_INPUT", "authored source exceeds 256 KiB"));
    }
    let valid = match content {
        UserContent::Text(text) => !text.trim().is_empty(),
        UserContent::Blocks(blocks) => {
            !blocks.is_empty() && blocks.len() <= MAX_CONTENT_BLOCKS
                && blocks.iter().all(|block| matches!(
                    block,
                    ContentBlock::Text(_) | ContentBlock::Image(_) | ContentBlock::Media(_)
                ))
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
    serde_json::to_writer(&mut budget, content).map_err(|_| control_error(
        "SESSION_CONTROL_INPUT", "queued content exceeds 4 MiB when serialized",
    ))?;
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
                "SESSION_CONTROL_CLOSED", "this turn no longer accepts input",
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
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ImageContent, Message, TextContent};
    use super::super::tests::live;

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
        control.steer_with_content(&content, "inspect this attachment").unwrap();
        let claimed = control.run.fetch(InputKind::Steering);
        assert_eq!(claimed[0].keyword_scan_source(), Some("inspect this attachment"));
        let Message::User(user) = claimed[0].message() else { panic!("user content"); };
        assert_eq!(serde_json::to_value(&user.content).unwrap(), serde_json::to_value(content).unwrap());
    }

    #[test]
    fn reclaim_preserves_expanded_content_and_raw_editor_text() {
        let (control, guard, _) = live();
        let content = attachment();
        control.follow_up_with_content(&content, "@diagram.png inspect").unwrap();
        drop(guard);
        let pending = control.take_pending();
        assert_eq!(pending[0].text, "@diagram.png inspect");
        assert_eq!(serde_json::to_value(&pending[0].content).unwrap(), serde_json::to_value(content).unwrap());
        assert_eq!(control.snapshot().pending_bytes, 0);
    }

    #[test]
    fn retract_removes_only_selected_pending_input_and_refunds_bytes() {
        let (control, _guard, _) = live();
        let first = control.steer("keep first").unwrap();
        let second = control.follow_up_with_content(&attachment(), "remove this").unwrap();
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
            ContentBlock::Text(TextContent::new("x")); MAX_CONTENT_BLOCKS + 1
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
}
