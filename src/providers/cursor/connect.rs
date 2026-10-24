//! Connect response admission and terminal validation for the Cursor provider.
//!
//! A closed HTTP body is not a successful RPC. Connect requires a final JSON
//! EndStreamResponse, even for protobuf streams. Keep transport diagnostics
//! independent of the untrusted response body.

use std::borrow::Cow;
use std::io::Read;

use flate2::bufread::MultiGzDecoder;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::failover::provider_code::ConnectCode;

use super::MAX_CONNECT_FRAME_LEN;

pub(super) const ACCEPT_ENCODING: &str = "gzip, identity";

pub(super) fn protocol(message: &str) -> Error {
    Error::api(format!("PI_CURSOR_PROTOCOL: {message}"))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Encoding {
    #[default]
    Identity,
    Gzip,
}

impl Encoding {
    pub(super) fn from_headers(headers: &[(String, String)]) -> Result<Self> {
        let content_type = single_header(headers, "content-type")?
            .ok_or_else(|| protocol("missing Connect response content type"))?;
        if !content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case("application/connect+proto")
        {
            return Err(protocol("unexpected Connect response content type"));
        }
        match single_header(headers, "connect-content-encoding")? {
            None => Ok(Self::Identity),
            Some(value) if value.eq_ignore_ascii_case("identity") => Ok(Self::Identity),
            Some(value) if value.eq_ignore_ascii_case("gzip") => Ok(Self::Gzip),
            Some(_) => Err(protocol("unsupported Connect response encoding")),
        }
    }

    pub(super) fn decode(self, payload: &[u8], compressed: bool) -> Result<Cow<'_, [u8]>> {
        if payload.len() > MAX_CONNECT_FRAME_LEN {
            return Err(protocol("frame exceeds the encoded byte limit"));
        }
        // Compression is per envelope, not per HTTP body. Even when gzip was
        // negotiated, a clear compression bit means the payload is plain.
        if !compressed {
            return Ok(Cow::Borrowed(payload));
        }
        match self {
            Self::Identity => Err(protocol("compressed frame without a negotiated encoding")),
            Self::Gzip => decode_gzip(payload, MAX_CONNECT_FRAME_LEN).map(Cow::Owned),
        }
    }
}

/// Read at most one byte beyond the decoded budget. Reading through EOF at
/// the exact limit is essential: it verifies the gzip footer, rather than
/// accepting a valid-looking prefix with a truncated or corrupt checksum.
/// Each call owns a fresh decoder. Multiple gzip members inside one envelope
/// are legal, but share this budget and must all be valid; trailing garbage
/// must not be silently ignored after a successful first member.
fn decode_gzip(payload: &[u8], max_decoded: usize) -> Result<Vec<u8>> {
    let limit = u64::try_from(max_decoded)
        .ok()
        .and_then(|limit| limit.checked_add(1))
        .ok_or_else(|| protocol("invalid decoded byte limit"))?;
    let mut decoder = MultiGzDecoder::new(payload);
    let mut decoded = Vec::new();
    decoder
        .by_ref()
        .take(limit)
        .read_to_end(&mut decoded)
        .map_err(|_| protocol("invalid or truncated gzip frame"))?;
    if decoded.len() > max_decoded {
        return Err(protocol("frame exceeds the decoded byte limit"));
    }
    if !decoder.get_ref().is_empty() {
        return Err(protocol("unconsumed bytes after gzip frame"));
    }
    Ok(decoded)
}

fn single_header<'a>(headers: &'a [(String, String)], name: &str) -> Result<Option<&'a str>> {
    let mut values = headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim());
    let first = values.next();
    if values.next().is_some() {
        return Err(protocol("duplicate Connect response header"));
    }
    Ok(first)
}

/// Success omits `error` entirely. In particular, `error: null` is invalid,
/// not a successful RPC. Unknown top-level fields remain forward-compatible.
pub(super) fn terminal_error(payload: &[u8]) -> Result<Option<String>> {
    let value: Value =
        serde_json::from_slice(payload).map_err(|_| protocol("malformed Connect terminal JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| protocol("Connect terminal must be a JSON object"))?;
    if let Some(metadata) = object.get("metadata") {
        let metadata = metadata
            .as_object()
            .ok_or_else(|| protocol("invalid Connect terminal metadata"))?;
        if !metadata.values().all(|values| {
            values
                .as_array()
                .is_some_and(|values| values.iter().all(Value::is_string))
        }) {
            return Err(protocol("invalid Connect terminal metadata values"));
        }
    }
    let Some(error) = object.get("error") else {
        return Ok(None);
    };
    let error = error
        .as_object()
        .ok_or_else(|| protocol("invalid Connect terminal error"))?;
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .and_then(ConnectCode::parse)
        .ok_or_else(|| protocol("invalid Connect terminal error code"))?;
    let message = match error.get("message") {
        None => "",
        Some(value) => value
            .as_str()
            .ok_or_else(|| protocol("invalid Connect terminal error message"))?,
    };
    // The agent flattens stream errors into message text. Carry the validated
    // code across that boundary rather than asking recovery to infer it from
    // optional, untrusted prose. No synthetic HTTP status is introduced.
    Ok(Some(code.diagnostic(message)))
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io::Write;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use futures::stream::{self, Stream, StreamExt};

    use super::super::{
        CONNECT_COMPRESSED_FLAG, CONNECT_END_STREAM_FLAG, StreamState, encode_frame, pb,
    };
    use super::*;
    use crate::model::{AssistantMessage, ContentBlock, StopReason, StreamEvent};

    fn next_ready(state: &mut StreamState) -> Option<Result<StreamEvent>> {
        let mut future = std::pin::pin!(state.next_event());
        let waker = futures::task::noop_waker();
        match future.as_mut().poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(event) => event,
            Poll::Pending => panic!("finite ready test source unexpectedly blocked"),
        }
    }

    fn drain(chunks: Vec<Vec<u8>>) -> Vec<Result<StreamEvent>> {
        drain_with_encoding(chunks, Encoding::Identity)
    }

    fn drain_with_encoding(chunks: Vec<Vec<u8>>, encoding: Encoding) -> Vec<Result<StreamEvent>> {
        let source: Pin<Box<dyn Stream<Item = std::io::Result<Vec<u8>>> + Send>> =
            Box::pin(stream::iter(chunks.into_iter().map(Ok)));
        let mut state = StreamState::new(source, AssistantMessage::default());
        state.encoding = encoding;
        let mut events = Vec::new();
        for _ in 0..64 {
            match next_ready(&mut state) {
                Some(event) => events.push(event),
                None => return events,
            }
        }
        panic!("stream failed to terminate");
    }

    fn text_frame(text: &str) -> Vec<u8> {
        let mut delta = Vec::new();
        pb::write_string(&mut delta, 1, text);
        let mut update = Vec::new();
        pb::write_len_delim(&mut update, 1, &delta);
        let mut message = Vec::new();
        pb::write_len_delim(&mut message, 1, &update);
        encode_frame(0, &message)
    }

    fn assert_failed(events: &[Result<StreamEvent>], marker: &str) {
        assert!(events.iter().any(|event| {
            event
                .as_ref()
                .err()
                .is_some_and(|error| error.to_string().contains(marker))
        }));
        assert_eq!(events.iter().filter(|event| event.is_err()).count(), 1);
        assert!(!events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::Done { .. } | StreamEvent::Error { .. })
        )));
    }

    #[test]
    fn eof_never_promotes_empty_or_partial_output_to_success() {
        assert_failed(&drain(Vec::new()), "PI_CURSOR_STREAM_INCOMPLETE");
        let events = drain(vec![text_frame("partial answer")]);
        assert!(events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::TextDelta { delta, .. }) if delta == "partial answer"
        )));
        assert_failed(&events, "PI_CURSOR_STREAM_INCOMPLETE");
    }

    #[test]
    fn every_truncated_terminal_prefix_fails_after_real_text() {
        let terminal = encode_frame(CONNECT_END_STREAM_FLAG, b"{}");
        for length in 0..terminal.len() {
            let mut wire = text_frame("partial");
            wire.extend_from_slice(&terminal[..length]);
            assert_failed(&drain(vec![wire]), "PI_CURSOR_STREAM_INCOMPLETE");
        }
    }

    #[test]
    fn eof_in_a_data_frame_is_not_success() {
        let frame = text_frame("not complete");
        for length in 1..frame.len() {
            assert_failed(
                &drain(vec![frame[..length].to_vec()]),
                "response ended inside a Connect frame",
            );
        }
    }

    #[test]
    fn application_turn_end_does_not_replace_connect_terminal() {
        let mut update = Vec::new();
        pb::write_len_delim(&mut update, 14, &[]);
        let mut message = Vec::new();
        pb::write_len_delim(&mut message, 1, &update);
        assert_failed(
            &drain(vec![encode_frame(0, &message)]),
            "PI_CURSOR_STREAM_INCOMPLETE",
        );
    }

    #[test]
    fn fragmented_valid_terminal_completes_exactly_once() {
        let mut wire = text_frame("complete");
        wire.extend_from_slice(&encode_frame(CONNECT_END_STREAM_FLAG, b"{}"));
        let events = drain(wire.into_iter().map(|byte| vec![byte]).collect());
        assert!(events.iter().all(Result::is_ok));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(StreamEvent::Done { .. })))
                .count(),
            1
        );
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Done {
                reason: StopReason::Stop,
                ..
            }))
        ));
    }

    #[test]
    fn terminal_does_not_wait_for_body_close_or_poll_the_source_again() {
        let terminal = encode_frame(CONNECT_END_STREAM_FLAG, b"{}");
        let source = stream::iter(vec![Ok(terminal)]).chain(stream::poll_fn(|_| {
            panic!("source polled after Connect terminal")
        }));
        let mut state = StreamState::new(Box::pin(source), AssistantMessage::default());
        assert!(matches!(
            next_ready(&mut state),
            Some(Ok(StreamEvent::Start { .. }))
        ));
        assert!(matches!(
            next_ready(&mut state),
            Some(Ok(StreamEvent::Done { .. }))
        ));
        assert!(next_ready(&mut state).is_none());
        assert!(next_ready(&mut state).is_none());
    }

    #[test]
    fn malformed_terminal_never_completes_partial_text() {
        for payload in [
            "",
            "{",
            "null",
            "[]",
            "true",
            "{} trailing",
            r#"{"error":null}"#,
            r#"{"error":{}}"#,
            r#"{"error":{"code":null}}"#,
            r#"{"error":{"code":"not-a-code"}}"#,
            r#"{"error":{"code":"unknown","message":3}}"#,
            r#"{"metadata":null}"#,
            r#"{"metadata":{"key":"not-an-array"}}"#,
            r#"{"metadata":{"key":[3]}}"#,
        ] {
            let events = drain(vec![
                text_frame("preview"),
                encode_frame(CONNECT_END_STREAM_FLAG, payload.as_bytes()),
            ]);
            assert_failed(&events, "PI_CURSOR_PROTOCOL");
        }
    }

    #[test]
    fn terminal_error_keeps_partial_output_and_never_emits_done() {
        let events = drain(vec![
            text_frame("preview"),
            encode_frame(
                CONNECT_END_STREAM_FLAG,
                br#"{"error":{"code":"unavailable","message":"try later"}}"#,
            ),
        ]);
        assert!(events.iter().all(Result::is_ok));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::Done { .. })))
        );
        match events.last() {
            Some(Ok(StreamEvent::Error { error, reason })) => {
                assert_eq!(*reason, StopReason::Error);
                assert_eq!(
                    error.error_message.as_deref(),
                    Some("PI_CONNECT_ERROR[unavailable]: try later")
                );
                assert!(!error.content.is_empty());
            }
            event => panic!("expected provider error, got {event:?}"),
        }
    }

    #[test]
    fn code_only_error_is_not_lost_and_metadata_is_supported() {
        assert_eq!(
            terminal_error(br#"{"error":{"code":"resource_exhausted"}}"#).unwrap(),
            Some(
                "PI_CONNECT_ERROR[resource_exhausted]: Cursor stream returned resource_exhausted"
                    .to_string()
            )
        );
        assert_eq!(
            terminal_error(br#"{"metadata":{"cost":["1","2"]},"future":true}"#).unwrap(),
            None
        );
    }

    #[test]
    fn malformed_terminal_diagnostics_do_not_echo_the_body() {
        let error = terminal_error(b"credential-marker-not-json").unwrap_err();
        assert!(!error.to_string().contains("credential-marker"));
    }

    #[test]
    fn reserved_envelope_flags_fail_before_interpretation() {
        for flags in [4, 128, 255] {
            assert_failed(
                &drain(vec![encode_frame(flags, b"{}")]),
                "unsupported envelope flags",
            );
        }
    }

    #[test]
    fn compressed_frame_requires_negotiation() {
        assert_failed(
            &drain(vec![encode_frame(
                CONNECT_COMPRESSED_FLAG | CONNECT_END_STREAM_FLAG,
                b"{}",
            )]),
            "compressed frame without a negotiated encoding",
        );
    }

    fn headers(extra: &[(&str, &str)]) -> Vec<(String, String)> {
        std::iter::once(("Content-Type", "application/connect+proto"))
            .chain(extra.iter().copied())
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn response_admission_requires_the_expected_content_type() {
        assert_eq!(
            Encoding::from_headers(&headers(&[])).unwrap(),
            Encoding::Identity
        );
        assert_eq!(
            Encoding::from_headers(&[(
                "CONTENT-TYPE".to_string(),
                "Application/Connect+Proto; charset=utf-8".to_string(),
            )])
            .unwrap(),
            Encoding::Identity
        );
        assert!(Encoding::from_headers(&[]).is_err());
        assert!(
            Encoding::from_headers(&[("content-type".to_string(), "text/html".to_string())])
                .is_err()
        );
        assert!(Encoding::from_headers(&headers(&[("content-type", "text/html")])).is_err());
    }

    #[test]
    fn response_admission_rejects_ambiguous_or_unadvertised_encodings() {
        assert_eq!(
            Encoding::from_headers(&headers(&[("Connect-Content-Encoding", "identity")])).unwrap(),
            Encoding::Identity
        );
        for encoding in ["", "br", "identity, gzip", "credential-marker"] {
            let error = Encoding::from_headers(&headers(&[("connect-content-encoding", encoding)]))
                .unwrap_err();
            assert!(!error.to_string().contains("credential-marker"));
        }
        assert!(
            Encoding::from_headers(&headers(&[
                ("connect-content-encoding", "identity"),
                ("Connect-Content-Encoding", "identity"),
            ]))
            .is_err()
        );
    }

    fn gzip(payload: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap()
    }

    fn compressed_frame(flags: u8, payload: &[u8]) -> Vec<u8> {
        encode_frame(flags | CONNECT_COMPRESSED_FLAG, &gzip(payload))
    }

    #[test]
    fn gzip_is_advertised_and_negotiated_case_insensitively() {
        assert_eq!(ACCEPT_ENCODING, "gzip, identity");
        assert_eq!(
            Encoding::from_headers(&headers(&[("Connect-Content-Encoding", " GZip ")])).unwrap(),
            Encoding::Gzip
        );
    }

    #[test]
    fn negotiated_gzip_still_accepts_uncompressed_frames_without_copying() {
        assert!(matches!(
            Encoding::Gzip.decode(b"plain protobuf", false).unwrap(),
            Cow::Borrowed(b"plain protobuf")
        ));
    }

    #[test]
    fn gzip_round_trips_empty_and_binary_payloads() {
        for payload in [b"".as_slice(), b"\0\xff\x01protobuf\0".as_slice()] {
            assert_eq!(
                Encoding::Gzip
                    .decode(&gzip(payload), true)
                    .unwrap()
                    .as_ref(),
                payload
            );
        }
    }

    #[test]
    fn decoded_limit_checks_the_boundary_and_bounds_compression_bombs() {
        let exact = [b'x'; 32];
        assert_eq!(decode_gzip(&gzip(&exact), exact.len()).unwrap(), exact);
        assert!(decode_gzip(&gzip(&exact), exact.len() - 1).is_err());
        assert!(decode_gzip(&gzip(&[]), 0).unwrap().is_empty());
        let bomb = gzip(&vec![b'x'; 64 * 1024]);
        assert!(bomb.len() < 4096);
        let error = decode_gzip(&bomb, 1024).unwrap_err();
        assert!(error.to_string().contains("decoded byte limit"));
    }

    #[test]
    fn every_truncated_gzip_prefix_and_a_corrupt_footer_fail() {
        let payload = b"must verify the checksum too";
        let compressed = gzip(payload);
        for length in 0..compressed.len() {
            assert!(decode_gzip(&compressed[..length], payload.len()).is_err());
        }
        let mut corrupt = compressed;
        let checksum = corrupt.len() - 8;
        corrupt[checksum] ^= 1;
        assert!(decode_gzip(&corrupt, payload.len()).is_err());
    }

    #[test]
    fn gzip_members_share_one_output_budget_and_reject_trailing_garbage() {
        let mut members = gzip(b"first");
        members.extend_from_slice(&gzip(b"second"));
        assert_eq!(decode_gzip(&members, 11).unwrap(), b"firstsecond");
        assert!(decode_gzip(&members, 10).is_err());
        for suffix in [b"trailing garbage".as_slice(), b"\x1f\x8b".as_slice()] {
            let mut corrupt = members.clone();
            corrupt.extend_from_slice(suffix);
            assert!(decode_gzip(&corrupt, 11).is_err());
        }
    }

    #[test]
    fn mixed_fragmented_gzip_and_plain_frames_complete_in_order() {
        let first = text_frame("Hello");
        let mut wire = compressed_frame(0, &first[5..]);
        wire.extend_from_slice(&text_frame(", world"));
        wire.extend_from_slice(&compressed_frame(CONNECT_END_STREAM_FLAG, b"{}"));
        let events = drain_with_encoding(
            wire.into_iter().map(|byte| vec![byte]).collect(),
            Encoding::Gzip,
        );
        assert!(events.iter().all(Result::is_ok));
        match events.last() {
            Some(Ok(StreamEvent::Done { message, .. })) => {
                assert!(matches!(
                    &message.content[0],
                    ContentBlock::Text(text) if text.text == "Hello, world"
                ));
            }
            event => panic!("expected complete gzip stream, got {event:?}"),
        }
    }

    #[test]
    fn gzip_data_can_finish_with_an_uncompressed_terminal() {
        let text = text_frame("answer");
        let events = drain_with_encoding(
            vec![
                compressed_frame(0, &text[5..]),
                encode_frame(CONNECT_END_STREAM_FLAG, b"{}"),
            ],
            Encoding::Gzip,
        );
        assert!(events.iter().all(Result::is_ok));
        assert!(matches!(events.last(), Some(Ok(StreamEvent::Done { .. }))));
    }

    #[test]
    fn corrupt_compressed_terminal_never_completes_partial_output() {
        let mut compressed = gzip(b"{}");
        let checksum = compressed.len() - 8;
        compressed[checksum] ^= 1;
        let events = drain_with_encoding(
            vec![
                text_frame("partial"),
                encode_frame(
                    CONNECT_END_STREAM_FLAG | CONNECT_COMPRESSED_FLAG,
                    &compressed,
                ),
            ],
            Encoding::Gzip,
        );
        assert_failed(&events, "invalid or truncated gzip frame");
    }

    #[test]
    fn compressed_provider_error_stays_an_error() {
        let events = drain_with_encoding(
            vec![
                text_frame("preview"),
                compressed_frame(
                    CONNECT_END_STREAM_FLAG,
                    br#"{"error":{"code":"unavailable"}}"#,
                ),
            ],
            Encoding::Gzip,
        );
        assert!(events.iter().all(Result::is_ok));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::Done { .. })))
        );
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Error {
                reason: StopReason::Error,
                ..
            }))
        ));
    }

    fn recovery_policy() -> crate::failover::RetryPolicy {
        crate::failover::RetryPolicy {
            max_retries: 2,
            max_failovers_per_turn: 1,
            model_fallback: true,
            fallback_on: crate::failover::FallbackOn::Any,
            base_delay_ms: 250,
            max_delay_ms: 1_000,
        }
    }

    fn fresh_progress() -> crate::failover::TurnProgress {
        crate::failover::TurnProgress {
            retry_count: 0,
            failovers_this_turn: 0,
            stream_can_retry: true,
        }
    }

    #[test]
    fn decoded_error_codes_control_recovery_even_with_opaque_or_misleading_prose() {
        use crate::failover::{TurnDecision, TurnOutcome, decide};

        for (code, recoverable) in [
            ("canceled", false),
            ("unknown", false),
            ("invalid_argument", false),
            ("deadline_exceeded", true),
            ("not_found", false),
            ("already_exists", false),
            ("permission_denied", false),
            ("resource_exhausted", true),
            ("failed_precondition", false),
            ("aborted", false),
            ("out_of_range", false),
            ("unimplemented", false),
            ("internal", false),
            ("unavailable", true),
            ("data_loss", false),
            ("unauthenticated", false),
        ] {
            for detail in ["", "opaque", "HTTP 503: timeout; quota exceeded"] {
                let payload = serde_json::to_vec(&serde_json::json!({
                    "error": { "code": code, "message": detail }
                }))
                .unwrap();
                for encoding in [Encoding::Identity, Encoding::Gzip] {
                    let terminal = if encoding == Encoding::Gzip {
                        compressed_frame(CONNECT_END_STREAM_FLAG, &payload)
                    } else {
                        encode_frame(CONNECT_END_STREAM_FLAG, &payload)
                    };
                    let events =
                        drain_with_encoding(vec![text_frame("preview"), terminal], encoding);
                    assert!(events.iter().all(Result::is_ok));
                    let Some(Ok(StreamEvent::Error { error, reason })) = events.last() else {
                        panic!("expected {code} error terminal, got {events:?}");
                    };
                    assert_eq!(*reason, StopReason::Error);
                    assert!(matches!(
                        &error.content[0],
                        ContentBlock::Text(text) if text.text == "preview"
                    ));
                    let diagnostic = error.error_message.as_deref().unwrap();
                    assert!(diagnostic.starts_with(&format!("PI_CONNECT_ERROR[{code}]: ")));
                    assert!(detail.is_empty() || diagnostic.ends_with(detail));
                    assert_eq!(
                        decide(
                            TurnOutcome::Completed(error),
                            &fresh_progress(),
                            &recovery_policy(),
                            None,
                        ),
                        if recoverable {
                            TurnDecision::Retry {
                                attempt: 1,
                                delay_ms: 250,
                            }
                        } else {
                            TurnDecision::Finish { success: false }
                        },
                        "{code}: {detail}"
                    );
                }
            }
        }
    }

    #[test]
    fn decoded_auth_and_context_refusals_do_not_become_capacity_retries() {
        use crate::failover::{TurnDecision, TurnOutcome, decide};

        for detail in [
            "invalid_api_key",
            "prompt is too long",
            "HTTP 401: retry delay",
        ] {
            let payload = serde_json::to_vec(&serde_json::json!({
                "error": { "code": "unavailable", "message": detail }
            }))
            .unwrap();
            let events = drain(vec![encode_frame(CONNECT_END_STREAM_FLAG, &payload)]);
            let Some(Ok(StreamEvent::Error { error, .. })) = events.last() else {
                panic!("missing provider error");
            };
            assert_eq!(
                decide(
                    TurnOutcome::Completed(error),
                    &fresh_progress(),
                    &recovery_policy(),
                    None,
                ),
                TurnDecision::Finish { success: false }
            );
        }
    }

    #[test]
    fn premature_eof_is_retryable_without_becoming_a_successful_message() {
        use crate::failover::{TurnDecision, TurnOutcome, decide};

        let terminal = encode_frame(CONNECT_END_STREAM_FLAG, b"{}");
        for length in 0..terminal.len() {
            let events = drain(vec![text_frame("preview"), terminal[..length].to_vec()]);
            assert_failed(&events, "PI_CURSOR_STREAM_INCOMPLETE");
            let error = events.last().unwrap().as_ref().unwrap_err();
            let policy = recovery_policy();
            assert_eq!(
                decide(TurnOutcome::Failed(error), &fresh_progress(), &policy, None),
                TurnDecision::Retry {
                    attempt: 1,
                    delay_ms: 250,
                }
            );
            // Print and any other surface that cannot retract already-visible
            // output must still refuse both same-provider and chain reentry.
            let mut visible = fresh_progress();
            visible.stream_can_retry = false;
            assert_eq!(
                decide(TurnOutcome::Failed(error), &visible, &policy, None),
                TurnDecision::Finish { success: false }
            );
            let flattened = AssistantMessage {
                stop_reason: StopReason::Error,
                error_message: Some(error.to_string()),
                ..AssistantMessage::default()
            };
            assert_eq!(
                decide(
                    TurnOutcome::Completed(&flattened),
                    &fresh_progress(),
                    &policy,
                    None,
                ),
                TurnDecision::Retry {
                    attempt: 1,
                    delay_ms: 250,
                }
            );
        }
    }

    #[test]
    fn malformed_protocol_and_host_limits_remain_non_retryable() {
        use crate::failover::{TurnDecision, TurnOutcome, decide};

        for wire in [
            encode_frame(CONNECT_END_STREAM_FLAG, b"not valid JSON: quota exceeded"),
            encode_frame(CONNECT_END_STREAM_FLAG, br#"{"error":{"code":"INVALID"}}"#),
            encode_frame(0, &[0]),
            encode_frame(4, b"{}"),
        ] {
            let events = drain(vec![wire]);
            let error = events.last().unwrap().as_ref().unwrap_err();
            assert_eq!(
                decide(
                    TurnOutcome::Failed(error),
                    &fresh_progress(),
                    &recovery_policy(),
                    None,
                ),
                TurnDecision::Finish { success: false }
            );
        }
        let limit = super::super::stream_limits::exceeded();
        assert_eq!(
            decide(
                TurnOutcome::Failed(&limit),
                &fresh_progress(),
                &recovery_policy(),
                None,
            ),
            TurnDecision::Finish { success: false }
        );
    }
}
