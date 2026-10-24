//! Whole-response admission for Cursor streams, not just individual frames.
//!
//! Count bytes even in unknown protobuf fields and frames that emit no text.
//! Preview publication is transactional per frame: a rejected batch cannot
//! publish its valid prefix, mutate usage, or open a new content block.

use super::{BlockKind, Error, Result, ServerEvent};

const MAX_ENCODED_BYTES: usize = 256 * 1024 * 1024;
const MAX_DECODED_BYTES: usize = 256 * 1024 * 1024;
const MAX_FRAMES: usize = 262_144;
const MAX_EVENTS: usize = 1_048_576;
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_BLOCKS: usize = 4_096;
// Count framing, body-read and event-delivery steps, including steps that
// emit no events. This is a cooperative scheduling bound, not a wall clock.
const READY_STEPS: usize = 256;

pub(super) fn exceeded() -> Error {
    Error::api("PI_CURSOR_STREAM_LIMIT: response exceeds its byte, frame, event or content budget")
}

fn charge(remaining: &mut usize, amount: usize) -> Result<()> {
    *remaining = remaining.checked_sub(amount).ok_or_else(exceeded)?;
    Ok(())
}

/// Each invocation owns an independent budget. Counters never reset when the
/// provider switches from text to thinking, sends a checkpoint, or compresses
/// a frame. Limits are host admission policy, not provider token estimates.
#[derive(Clone, Debug)]
pub(super) struct Budget {
    encoded: usize,
    decoded: usize,
    frames: usize,
    events: usize,
    output: usize,
    blocks: usize,
    ready_steps: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            encoded: MAX_ENCODED_BYTES,
            decoded: MAX_DECODED_BYTES,
            frames: MAX_FRAMES,
            events: MAX_EVENTS,
            output: MAX_OUTPUT_BYTES,
            blocks: MAX_BLOCKS,
            ready_steps: 0,
        }
    }
}

impl Budget {
    /// Preserve progress across next_event calls, so an always-ready consumer
    /// draining previews yields too. A genuinely pending body read owns its
    /// own wakeup; we only self-wake once after bounded real progress.
    pub(super) async fn cooperate(&mut self) {
        if self.ready_steps < READY_STEPS {
            self.ready_steps += 1;
            return;
        }
        self.ready_steps = 0;
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                std::task::Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        })
        .await;
    }

    /// Called before copying an HTTP chunk into the framing buffer. Include
    /// envelope headers, empty/unknown frames and bytes after a terminator in
    /// the received chunk; none are free input to the host.
    pub(super) fn receive(&mut self, bytes: usize) -> Result<()> {
        charge(&mut self.encoded, bytes)
    }

    pub(super) fn frame(&mut self) -> Result<()> {
        charge(&mut self.frames, 1)
    }

    pub(super) fn decoded(&mut self, bytes: usize) -> Result<()> {
        charge(&mut self.decoded, bytes)
    }

    /// Preview the whole batch before committing its consumption. Empty
    /// deltas consume event work but never allocate a content block. The last
    /// block kind comes from the same StreamState that will apply the batch.
    pub(super) fn admit(
        &mut self,
        events: &[ServerEvent],
        mut open_kind: Option<BlockKind>,
    ) -> Result<()> {
        let mut candidate = self.clone();
        charge(&mut candidate.events, events.len())?;
        for event in events {
            let (kind, text) = match event {
                ServerEvent::TextDelta(text) => (BlockKind::Text, text),
                ServerEvent::ThinkingDelta(text) => (BlockKind::Thinking, text),
                _ => continue,
            };
            if text.is_empty() {
                continue;
            }
            charge(&mut candidate.output, text.len())?;
            if open_kind != Some(kind) {
                charge(&mut candidate.blocks, 1)?;
                open_kind = Some(kind);
            }
        }
        *self = candidate;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        AssistantMessage, ConnectFrame, ContentBlock, StreamEvent, StreamState, pb,
    };
    use super::*;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    struct Body {
        chunks: VecDeque<std::io::Result<Vec<u8>>>,
        repeat: Option<Vec<u8>>,
        stalled: bool,
        polls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl futures::Stream for Body {
        type Item = std::io::Result<Vec<u8>>;

        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            // A missing cooperative yield fails deterministically rather
            // than hanging the regression test on an infinite ready source.
            assert!(self.polls.fetch_add(1, Ordering::SeqCst) < READY_STEPS * 8);
            if let Some(chunk) = self.chunks.pop_front() {
                return Poll::Ready(Some(chunk));
            }
            if let Some(chunk) = &self.repeat {
                Poll::Ready(Some(Ok(chunk.clone())))
            } else if self.stalled {
                Poll::Pending
            } else {
                Poll::Ready(None)
            }
        }
    }

    impl Drop for Body {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl futures::task::ArcWake for WakeCount {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn body(chunks: Vec<std::io::Result<Vec<u8>>>) -> Body {
        Body {
            chunks: chunks.into(),
            repeat: None,
            stalled: false,
            polls: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn next_ready(state: &mut StreamState) -> Option<Result<StreamEvent>> {
        let mut next = std::pin::pin!(state.next_event());
        let waker = futures::task::noop_waker();
        for _ in 0..32 {
            if let Poll::Ready(event) = next.as_mut().poll(&mut Context::from_waker(&waker)) {
                return event;
            }
        }
        panic!("finite ready source did not complete within its poll bound");
    }

    fn state(budget: Budget) -> StreamState {
        let mut state = StreamState::new(
            Box::pin(futures::stream::empty()),
            AssistantMessage::default(),
        );
        state.budget = budget;
        state
    }

    fn frame(parts: &[(u32, &str)]) -> ConnectFrame {
        let mut update = Vec::new();
        for (kind, text) in parts {
            let mut delta = Vec::new();
            pb::write_string(&mut delta, 1, text);
            pb::write_len_delim(&mut update, *kind, &delta);
        }
        let mut payload = Vec::new();
        pb::write_len_delim(&mut payload, 1, &update);
        ConnectFrame {
            end_stream: false,
            compressed: false,
            payload,
        }
    }

    fn terminal() -> ConnectFrame {
        ConnectFrame {
            end_stream: true,
            compressed: false,
            payload: b"{}".to_vec(),
        }
    }

    fn text(value: &str) -> ServerEvent {
        ServerEvent::TextDelta(value.into())
    }

    fn thinking(value: &str) -> ServerEvent {
        ServerEvent::ThinkingDelta(value.into())
    }

    #[test]
    fn encoded_and_decoded_limits_are_independent_and_exact() {
        let mut budget = Budget::default();
        budget.receive(MAX_ENCODED_BYTES).unwrap();
        assert!(budget.receive(1).is_err());
        budget.decoded(MAX_DECODED_BYTES).unwrap();
        assert!(budget.decoded(1).is_err());
        // A new invocation is not affected by a previous invocation's limit.
        Budget::default().receive(1).unwrap();
    }

    #[test]
    fn oversized_charge_does_not_wrap_or_consume_remaining_budget() {
        let mut budget = Budget::default();
        assert!(budget.receive(usize::MAX).is_err());
        budget.receive(MAX_ENCODED_BYTES).unwrap();
        assert!(budget.decoded(usize::MAX).is_err());
        budget.decoded(MAX_DECODED_BYTES).unwrap();
    }

    #[test]
    fn empty_frames_and_nontext_events_are_not_free() {
        let mut budget = Budget {
            frames: 2,
            events: 2,
            ..Budget::default()
        };
        budget.frame().unwrap();
        budget.frame().unwrap();
        assert!(budget.frame().is_err());
        budget
            .admit(
                &[ServerEvent::TokenDelta(0), ServerEvent::UsedTokens(0)],
                None,
            )
            .unwrap();
        assert!(budget.admit(&[ServerEvent::TurnEnded], None).is_err());
    }

    #[test]
    fn text_and_thinking_share_a_utf8_byte_budget() {
        let mut budget = Budget {
            output: 6,
            ..Budget::default()
        };
        budget.admit(&[text("é"), thinking("🦀")], None).unwrap();
        assert!(
            budget
                .admit(&[thinking("x")], Some(BlockKind::Thinking))
                .is_err()
        );
    }

    #[test]
    fn block_changes_span_batches_but_empty_deltas_do_not_open_blocks() {
        let mut budget = Budget {
            blocks: 2,
            ..Budget::default()
        };
        budget.admit(&[text("a")], None).unwrap();
        budget
            .admit(&[thinking(""), text("b")], Some(BlockKind::Text))
            .unwrap();
        budget
            .admit(&[thinking("c"), thinking("d")], Some(BlockKind::Text))
            .unwrap();
        assert!(
            budget
                .admit(&[text("e")], Some(BlockKind::Thinking))
                .is_err()
        );
    }

    #[test]
    fn failed_batch_commits_neither_its_prefix_nor_its_counters() {
        let mut budget = Budget {
            output: 2,
            blocks: 1,
            events: 2,
            ..Budget::default()
        };
        assert!(budget.admit(&[text("a"), thinking("b")], None).is_err());
        budget.admit(&[text("a"), text("b")], None).unwrap();
        assert_eq!(budget.output, 0);
        assert_eq!(budget.blocks, 0);
        assert_eq!(budget.events, 0);
    }

    #[test]
    fn production_batch_rejection_publishes_no_prefix_and_cannot_later_succeed() {
        let mut state = state(Budget {
            output: 3,
            ..Budget::default()
        });
        let error = state
            .process_frame(&frame(&[(1, "ab"), (4, "cd")]))
            .unwrap_err();
        assert!(error.to_string().contains("PI_CURSOR_STREAM_LIMIT"));
        assert!(state.partial.content.is_empty());
        assert!(state.pending.is_empty());
        assert!(!state.started);
        assert!(!state.finalized);
        assert!(state.done);
        assert!(state.process_frame(&terminal()).is_err());
        assert!(state.pending.is_empty());
    }

    #[test]
    fn production_output_budget_survives_frame_and_content_kind_boundaries() {
        let mut state = state(Budget {
            output: 4,
            ..Budget::default()
        });
        state.process_frame(&frame(&[(1, "ab")])).unwrap();
        state.pending.clear();
        state.process_frame(&frame(&[(4, "cd")])).unwrap();
        state.pending.clear();
        assert!(state.process_frame(&frame(&[(1, "e")])).is_err());
        assert_eq!(state.partial.content.len(), 2);
        assert!(matches!(&state.partial.content[0], ContentBlock::Text(text) if text.text == "ab"));
        assert!(
            matches!(&state.partial.content[1], ContentBlock::Thinking(text) if text.thinking == "cd")
        );
        assert!(
            state.pending.is_empty(),
            "rejected switch must not close the previous block"
        );
    }

    #[test]
    fn production_block_budget_is_checked_before_closing_the_previous_block() {
        let mut state = state(Budget {
            blocks: 1,
            ..Budget::default()
        });
        state.process_frame(&frame(&[(1, "a")])).unwrap();
        state.pending.clear();
        state.process_frame(&frame(&[(1, "b")])).unwrap();
        state.pending.clear();
        assert!(state.process_frame(&frame(&[(4, "c")])).is_err());
        assert_eq!(state.partial.content.len(), 1);
        assert!(matches!(&state.partial.content[0], ContentBlock::Text(text) if text.text == "ab"));
        assert!(state.pending.is_empty());
    }

    #[test]
    fn production_empty_frames_and_terminal_frames_share_one_quota() {
        let mut state = state(Budget {
            frames: 1,
            ..Budget::default()
        });
        state.process_frame(&frame(&[])).unwrap();
        assert!(state.pending.is_empty());
        assert!(state.process_frame(&terminal()).is_err());
        assert!(state.pending.is_empty());
    }

    #[test]
    fn production_gzip_counts_decoded_bytes_not_only_compressed_bytes() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;

        let original = frame(&[(1, &"x".repeat(4096))]);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&original.payload).unwrap();
        let compressed = ConnectFrame {
            end_stream: false,
            compressed: true,
            payload: encoder.finish().unwrap(),
        };
        assert!(compressed.payload.len() < original.payload.len());
        let mut rejected = state(Budget {
            decoded: original.payload.len() - 1,
            ..Budget::default()
        });
        rejected.encoding = super::super::connect::Encoding::Gzip;
        assert!(rejected.process_frame(&compressed).is_err());
        assert!(rejected.pending.is_empty());
        let mut exact = state(Budget {
            decoded: original.payload.len() + 2,
            ..Budget::default()
        });
        exact.encoding = super::super::connect::Encoding::Gzip;
        exact.process_frame(&compressed).unwrap();
        exact.process_frame(&terminal()).unwrap();
        assert!(matches!(
            exact.pending.back(),
            Some(StreamEvent::Done { .. })
        ));
        assert_eq!(exact.budget.decoded, 0);
    }

    #[test]
    fn production_receive_budget_rejects_before_copying_the_chunk() {
        let mut state = state(Budget {
            encoded: 4,
            ..Budget::default()
        });
        state.source = Box::pin(futures::stream::iter([Ok(vec![0; 5])]));
        let waker = futures::task::noop_waker();
        {
            let mut next = std::pin::pin!(state.next_event());
            assert!(matches!(
                next.as_mut().poll(&mut Context::from_waker(&waker)),
                Poll::Ready(Some(Err(error))) if error.to_string().contains("PI_CURSOR_STREAM_LIMIT")
            ));
        }
        assert!(state.frames.buf.is_empty());
        assert!(state.pending.is_empty());
        assert!(state.done);
    }

    #[test]
    fn ready_empty_chunks_and_unknown_frames_yield_to_the_caller() {
        for chunk in [Vec::new(), super::super::encode_frame(0, &[])] {
            let mut source = body(Vec::new());
            source.repeat = Some(chunk);
            let polls = Arc::clone(&source.polls);
            let mut state = state(Budget::default());
            state.source = Box::pin(source);
            let wake = Arc::new(WakeCount::default());
            let waker = futures::task::waker(Arc::clone(&wake));
            let mut next = std::pin::pin!(state.next_event());
            assert!(
                next.as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            assert!(polls.load(Ordering::SeqCst) > 0);
            assert!(polls.load(Ordering::SeqCst) <= READY_STEPS);
            assert_eq!(wake.0.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn ready_preview_delivery_preserves_the_yield_budget_across_next_calls() {
        let mut source = body(Vec::new());
        source.repeat = Some(super::super::encode_frame(0, &frame(&[(1, "a")]).payload));
        let mut state = state(Budget::default());
        state.source = Box::pin(source);
        let waker = futures::task::noop_waker();
        let mut emitted = 0;
        for _ in 0..READY_STEPS * 2 {
            let mut next = std::pin::pin!(state.next_event());
            match next.as_mut().poll(&mut Context::from_waker(&waker)) {
                Poll::Ready(Some(Ok(_))) => emitted += 1,
                Poll::Pending => {
                    assert!(emitted > 0);
                    return;
                }
                other => panic!("unexpected ready stream result: {other:?}"),
            }
        }
        panic!("an always-ready consumer never yielded");
    }

    #[test]
    fn idle_body_does_not_self_wake_or_exhaust_the_budget() {
        let mut source = body(Vec::new());
        source.stalled = true;
        let mut state = state(Budget::default());
        state.source = Box::pin(source);
        let wake = Arc::new(WakeCount::default());
        let waker = futures::task::waker(Arc::clone(&wake));
        {
            let mut next = std::pin::pin!(state.next_event());
            for _ in 0..READY_STEPS * 2 {
                assert!(
                    next.as_mut()
                        .poll(&mut Context::from_waker(&waker))
                        .is_pending()
                );
            }
        }
        assert_eq!(wake.0.load(Ordering::SeqCst), 0);
        assert_eq!(state.budget.ready_steps, 1);
    }

    #[test]
    fn fragmented_stream_resumes_after_yields_without_losing_or_duplicating_text() {
        let text = "a".repeat(READY_STEPS * 2);
        let mut wire = super::super::encode_frame(0, &frame(&[(1, &text)]).payload);
        wire.extend_from_slice(&super::super::encode_frame(
            super::super::CONNECT_END_STREAM_FLAG,
            b"{}",
        ));
        let source = body(wire.into_iter().map(|byte| Ok(vec![byte])).collect());
        let drops = Arc::clone(&source.drops);
        let mut state = state(Budget::default());
        state.source = Box::pin(source);
        let mut deltas = 0;
        let mut terminals = 0;
        while let Some(event) = next_ready(&mut state) {
            match event.unwrap() {
                StreamEvent::TextDelta { delta, .. } => {
                    assert_eq!(delta, text);
                    deltas += 1;
                }
                StreamEvent::Done { message, .. } => {
                    assert!(
                        matches!(&message.content[0], ContentBlock::Text(value) if value.text == text)
                    );
                    terminals += 1;
                }
                _ => {}
            }
        }
        assert_eq!((deltas, terminals), (1, 1));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn terminal_releases_the_body_before_the_consumer_drains_queued_events() {
        for payload in [
            b"{}".as_slice(),
            br#"{"error":{"code":"unavailable","message":"busy"}}"#.as_slice(),
        ] {
            let mut wire =
                super::super::encode_frame(super::super::CONNECT_END_STREAM_FLAG, payload);
            wire.extend_from_slice(b"ignored after the valid terminator");
            let mut source = body(vec![Ok(wire)]);
            source.stalled = true;
            let drops = Arc::clone(&source.drops);
            let polls = Arc::clone(&source.polls);
            let mut state = state(Budget::default());
            state.source = Box::pin(source);
            assert!(matches!(
                next_ready(&mut state),
                Some(Ok(StreamEvent::Start { .. }))
            ));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(polls.load(Ordering::SeqCst), 1);
            assert!(state.frames.buf.is_empty());
            assert_eq!(state.pending.len(), 1);
            assert!(matches!(
                next_ready(&mut state),
                Some(Ok(StreamEvent::Done { .. } | StreamEvent::Error { .. }))
            ));
            assert!(next_ready(&mut state).is_none());
        }
    }

    #[test]
    fn every_fatal_stream_path_releases_its_body_and_cannot_publish_done() {
        let bad_flag = vec![4, 0, 0, 0, 0];
        let bad_proto = super::super::encode_frame(0, &[0]);
        let bad_terminal =
            super::super::encode_frame(super::super::CONNECT_END_STREAM_FLAG, b"null");
        for chunks in [
            vec![Ok(bad_flag)],
            vec![Ok(bad_proto)],
            vec![Ok(bad_terminal)],
            vec![Ok(vec![0])],
            vec![Err(std::io::Error::from(
                std::io::ErrorKind::ConnectionReset,
            ))],
        ] {
            let source = body(chunks);
            let drops = Arc::clone(&source.drops);
            let mut state = state(Budget::default());
            state.source = Box::pin(source);
            assert!(matches!(next_ready(&mut state), Some(Err(_))));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(state.frames.buf.is_empty());
            assert!(next_ready(&mut state).is_none());
        }
    }

    #[test]
    fn dropping_the_owned_provider_stream_after_a_yield_releases_the_body() {
        use futures::StreamExt;

        let mut source = body(Vec::new());
        source.repeat = Some(Vec::new());
        let drops = Arc::clone(&source.drops);
        let mut state = state(Budget::default());
        state.source = Box::pin(source);
        let mut stream = Box::pin(futures::stream::unfold(state, |mut state| async move {
            state.next_event().await.map(|event| (event, state))
        }));
        let waker = futures::task::noop_waker();
        {
            let mut next = std::pin::pin!(stream.next());
            assert!(
                next.as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
        }
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(stream);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
