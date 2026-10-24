//! Regression coverage for atomic, bounded Cursor protobuf decoding.

use std::future::Future;
use std::task::{Context as TaskContext, Poll};

use futures::stream;

use super::{
    AssistantMessage, CONNECT_COMPRESSED_FLAG, CONNECT_END_STREAM_FLAG, MAX_PROTOBUF_EVENTS,
    MAX_PROTOBUF_FIELDS, Result, ServerEvent, StreamEvent, StreamState, connect,
    decode_server_message, encode_frame, pb,
};

fn length_field(field: u32, payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::new();
    pb::write_len_delim(&mut message, field, payload);
    message
}

fn integer_field(field: u32, value: u64) -> Vec<u8> {
    let mut message = Vec::new();
    pb::write_varint(&mut message, u64::from(field) << 3);
    pb::write_varint(&mut message, value);
    message
}

fn interaction(field: u32, payload: &[u8]) -> Vec<u8> {
    length_field(1, &length_field(field, payload))
}

fn text_message(text: &[u8]) -> Vec<u8> {
    interaction(1, &length_field(1, text))
}

fn checkpoint(value: u64) -> Vec<u8> {
    length_field(3, &length_field(5, &integer_field(1, value)))
}

fn next_ready(state: &mut StreamState) -> Option<Result<StreamEvent>> {
    let mut future = std::pin::pin!(state.next_event());
    let waker = futures::task::noop_waker();
    match future.as_mut().poll(&mut TaskContext::from_waker(&waker)) {
        Poll::Ready(event) => event,
        Poll::Pending => panic!("finite ready source unexpectedly blocked"),
    }
}

fn assert_frame_failure(mut wire: Vec<u8>, encoding: connect::Encoding) {
    // Even a valid terminal after the corrupt frame must not turn its prefix
    // into a completed answer. No events from that corrupt frame may escape.
    wire.extend_from_slice(&encode_frame(CONNECT_END_STREAM_FLAG, b"{}"));
    let mut state = StreamState::new(
        Box::pin(stream::iter(vec![Ok(wire)])),
        AssistantMessage::default(),
    );
    state.encoding = encoding;
    match next_ready(&mut state) {
        Some(Err(error)) => assert!(error.to_string().contains("protobuf")),
        event => panic!("expected atomic protobuf rejection, got {event:?}"),
    }
    assert!(state.partial.content.is_empty());
    assert!(state.pending.is_empty());
    assert!(next_ready(&mut state).is_none());
    assert!(next_ready(&mut state).is_none());
}

fn assert_atomic_failure(payload: &[u8]) {
    assert!(decode_server_message(payload).is_err());
    assert_frame_failure(encode_frame(0, payload), connect::Encoding::Identity);
}

#[test]
fn reader_distinguishes_eof_from_malformed_input() {
    let mut empty = pb::Reader::new(&[]);
    assert!(empty.next_field().is_none());
    assert!(empty.finish().is_ok());

    let mut bytes = integer_field(99, 7);
    bytes.push(0x80);
    let mut reader = pb::Reader::new(&bytes);
    assert!(reader.finish().is_err()); // an unread message is not validated
    assert!(matches!(
        reader.next_field(),
        Some((99, pb::Field::Varint(7)))
    ));
    assert!(reader.next_field().is_none());
    assert!(reader.finish().is_err());
    assert!(reader.next_field().is_none()); // failure is sticky
    assert!(reader.finish().is_err());
}

#[test]
fn reader_rejects_overflowing_varints_and_invalid_field_numbers() {
    let mut overflow = integer_field(99, u64::MAX);
    *overflow.last_mut().unwrap() = 2;
    assert_atomic_failure(&overflow);

    let mut too_large_tag = Vec::new();
    pb::write_varint(&mut too_large_tag, 0x2000_0000_u64 << 3);
    too_large_tag.push(0);
    assert_atomic_failure(&too_large_tag);
    assert_atomic_failure(&integer_field(0, 0));
    assert_atomic_failure(&[0x0e]); // unsupported wire type 6

    let mut unterminated = vec![0x08];
    unterminated.extend_from_slice(&[0x80; 10]);
    assert_atomic_failure(&unterminated);
    assert!(
        decode_server_message(&integer_field(0x1fff_ffff, u64::MAX))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn unknown_fixed_width_fields_are_validated_not_discarded() {
    for (wire_type, width) in [(1, 8), (5, 4)] {
        let mut tag = Vec::new();
        pb::write_varint(&mut tag, (99_u64 << 3) | wire_type);
        let mut complete = tag.clone();
        complete.extend_from_slice(&[0; 8][..width]);
        assert!(decode_server_message(&complete).unwrap().is_empty());
        for length in 0..width {
            let mut truncated = tag.clone();
            truncated.extend_from_slice(&[0; 8][..length]);
            assert_atomic_failure(&truncated);
        }
    }
}

#[test]
fn malformed_top_level_tail_never_publishes_valid_prefix() {
    let mut message = text_message(b"must not be published");
    message.push(0x80);
    assert_atomic_failure(&message);
    assert_atomic_failure(&[0x12, 2, 0]); // truncated unknown length-delimited field
}

#[test]
fn malformed_nested_tail_never_publishes_valid_text() {
    let mut text = length_field(1, b"valid prefix");
    text.push(0x80);
    for field in [1, 4] {
        assert_atomic_failure(&interaction(field, &text));
    }
    let mut update = length_field(1, &length_field(1, b"valid prefix"));
    update.push(0x80);
    assert_atomic_failure(&length_field(1, &update));
    assert_atomic_failure(&interaction(14, &[0x80]));
}

#[test]
fn invalid_utf8_is_rejected_in_text_and_thinking() {
    for invalid in [vec![0xff], vec![0xc0, 0xaf], vec![0xed, 0xa0, 0x80]] {
        for field in [1, 4] {
            assert_atomic_failure(&interaction(field, &length_field(1, &invalid)));
            // A later valid duplicate must not hide invalid UTF-8 in the first.
            let mut duplicate = length_field(1, &invalid);
            duplicate.extend_from_slice(&length_field(1, b"later valid value"));
            assert_atomic_failure(&interaction(field, &duplicate));
        }
    }
}

#[test]
fn known_fields_with_wrong_wire_types_fail_at_every_level() {
    for message in [
        integer_field(1, 1),
        integer_field(3, 1),
        length_field(1, &integer_field(1, 1)),
        interaction(1, &integer_field(1, 1)),
        interaction(4, &integer_field(1, 1)),
        interaction(8, &length_field(1, b"not an integer")),
        length_field(1, &integer_field(14, 1)),
        length_field(3, &integer_field(5, 1)),
        length_field(3, &length_field(5, &length_field(1, b"not an integer"))),
    ] {
        assert_atomic_failure(&message);
    }
}

#[test]
fn scalar_decoders_validate_trailing_bytes() {
    let mut scalar = integer_field(1, 7);
    scalar.push(0x80);
    assert_atomic_failure(&interaction(8, &scalar));
    assert_atomic_failure(&length_field(3, &length_field(5, &scalar)));
    let mut checkpoint_state = length_field(5, &integer_field(1, 7));
    checkpoint_state.push(0x80);
    assert_atomic_failure(&length_field(3, &checkpoint_state));
}

#[test]
fn valid_unknown_fields_remain_forward_compatible() {
    let mut message = integer_field(99, u64::MAX);
    message.extend_from_slice(&text_message(b"known text"));
    // Unknown length-delimited fields may contain arbitrary binary data, not
    // necessarily another protobuf message or a UTF-8 string.
    message.extend_from_slice(&length_field(100, &[0xff, 0x80, 0x00]));
    assert_eq!(
        decode_server_message(&message).unwrap(),
        vec![ServerEvent::TextDelta("known text".to_string())]
    );
    let mut text = length_field(99, &[0xff, 0x80]);
    text.extend_from_slice(&length_field(1, b"known thinking"));
    text.extend_from_slice(&integer_field(100, u64::MAX));
    assert_eq!(
        decode_server_message(&interaction(4, &text)).unwrap(),
        vec![ServerEvent::ThinkingDelta("known thinking".to_string())]
    );
}

#[test]
fn duplicate_singular_scalars_use_the_last_value() {
    let mut text = length_field(1, b"old");
    text.extend_from_slice(&length_field(1, b"new"));
    assert_eq!(
        decode_server_message(&interaction(1, &text)).unwrap(),
        vec![ServerEvent::TextDelta("new".to_string())]
    );
    let mut tokens = integer_field(1, 3);
    tokens.extend_from_slice(&integer_field(1, 7));
    assert_eq!(
        decode_server_message(&interaction(8, &tokens)).unwrap(),
        vec![ServerEvent::TokenDelta(7)]
    );
    assert_eq!(
        decode_server_message(&length_field(3, &length_field(5, &tokens))).unwrap(),
        vec![ServerEvent::UsedTokens(7)]
    );
}

#[test]
fn checkpoint_submessages_merge_without_erasing_previous_usage() {
    let mut state = length_field(5, &integer_field(1, 42));
    state.extend_from_slice(&length_field(5, &[]));
    state.extend_from_slice(&length_field(5, &integer_field(99, 1)));
    assert_eq!(
        decode_server_message(&length_field(3, &state)).unwrap(),
        vec![ServerEvent::UsedTokens(42)]
    );
    state.extend_from_slice(&length_field(5, &integer_field(1, 64)));
    assert_eq!(
        decode_server_message(&length_field(3, &state)).unwrap(),
        vec![ServerEvent::UsedTokens(64)]
    );
}

#[test]
fn negative_token_deltas_and_full_width_usage_are_preserved() {
    assert_eq!(
        decode_server_message(&interaction(8, &integer_field(1, u64::MAX))).unwrap(),
        vec![ServerEvent::TokenDelta(-1)]
    );
    assert_eq!(
        decode_server_message(&checkpoint(u64::MAX)).unwrap(),
        vec![ServerEvent::UsedTokens(u64::MAX)]
    );
}

#[test]
fn field_budget_applies_across_nested_messages() {
    let unknown = integer_field(99, 0);
    let mut message = unknown.repeat(MAX_PROTOBUF_FIELDS);
    assert!(decode_server_message(&message).unwrap().is_empty());
    message.extend_from_slice(&unknown);
    assert!(
        decode_server_message(&message)
            .unwrap_err()
            .to_string()
            .contains("field limit")
    );
    // Two wrapping message fields also count against the same global budget.
    let mut nested = unknown.repeat(MAX_PROTOBUF_FIELDS - 2);
    assert_eq!(
        decode_server_message(&interaction(14, &nested)).unwrap(),
        vec![ServerEvent::TurnEnded]
    );
    nested.extend_from_slice(&unknown);
    assert_atomic_failure(&interaction(14, &nested));
}

#[test]
fn event_budget_bounds_dense_frames_at_exact_boundary() {
    let event = checkpoint(1);
    let mut message = event.repeat(MAX_PROTOBUF_EVENTS);
    assert_eq!(
        decode_server_message(&message).unwrap().len(),
        MAX_PROTOBUF_EVENTS
    );
    message.extend_from_slice(&event);
    assert!(
        decode_server_message(&message)
            .unwrap_err()
            .to_string()
            .contains("event limit")
    );
    assert_atomic_failure(&message);
}

#[test]
fn valid_empty_message_is_allowed_but_requires_a_terminal() {
    assert!(decode_server_message(&[]).unwrap().is_empty());
    let empty_frame = encode_frame(0, &[]);
    let mut missing = StreamState::new(
        Box::pin(stream::iter(vec![Ok(empty_frame.clone())])),
        AssistantMessage::default(),
    );
    assert!(matches!(next_ready(&mut missing), Some(Err(_))));
    assert!(next_ready(&mut missing).is_none());
    let mut complete = StreamState::new(
        Box::pin(stream::iter(vec![
            Ok(empty_frame),
            Ok(encode_frame(CONNECT_END_STREAM_FLAG, b"{}")),
        ])),
        AssistantMessage::default(),
    );
    assert!(matches!(
        next_ready(&mut complete),
        Some(Ok(StreamEvent::Start { .. }))
    ));
    assert!(matches!(
        next_ready(&mut complete),
        Some(Ok(StreamEvent::Done { .. }))
    ));
    assert!(next_ready(&mut complete).is_none());
}

#[test]
fn corrupt_protobuf_inside_valid_gzip_is_rejected_atomically() {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    let mut invalid = text_message(b"valid-looking prefix");
    invalid.push(0x80);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&invalid).unwrap();
    let compressed = encoder.finish().unwrap();
    assert_frame_failure(
        encode_frame(CONNECT_COMPRESSED_FLAG, &compressed),
        connect::Encoding::Gzip,
    );
}
