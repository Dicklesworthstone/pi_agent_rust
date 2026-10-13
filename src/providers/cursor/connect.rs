//! Connect response admission and terminal validation for the Cursor provider.
//!
//! A closed HTTP body is not a successful RPC. Connect requires a final JSON
//! EndStreamResponse, even for protobuf streams. Keep transport diagnostics
//! independent of the untrusted response body.

use std::borrow::Cow;

use serde_json::Value;

use crate::error::{Error, Result};

use super::MAX_CONNECT_FRAME_LEN;

pub(super) const ACCEPT_ENCODING: &str = "identity";

pub(super) fn protocol(message: &str) -> Error {
    Error::api(format!("PI_CURSOR_PROTOCOL: {message}"))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Encoding {
    #[default]
    Identity,
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
            Some(_) => Err(protocol("unsupported Connect response encoding")),
        }
    }

    pub(super) fn decode(self, payload: &[u8], compressed: bool) -> Result<Cow<'_, [u8]>> {
        if payload.len() > MAX_CONNECT_FRAME_LEN {
            return Err(protocol("frame exceeds the decoded byte limit"));
        }
        if compressed {
            return Err(protocol("compressed frame without a negotiated encoding"));
        }
        Ok(Cow::Borrowed(payload))
    }
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
    let value: Value = serde_json::from_slice(payload)
        .map_err(|_| protocol("malformed Connect terminal JSON"))?;
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
        .filter(|code| {
            matches!(
                *code,
                "canceled"
                    | "unknown"
                    | "invalid_argument"
                    | "deadline_exceeded"
                    | "not_found"
                    | "already_exists"
                    | "permission_denied"
                    | "resource_exhausted"
                    | "failed_precondition"
                    | "aborted"
                    | "out_of_range"
                    | "unimplemented"
                    | "internal"
                    | "unavailable"
                    | "data_loss"
                    | "unauthenticated"
            )
        })
        .ok_or_else(|| protocol("invalid Connect terminal error code"))?;
    let message = match error.get("message") {
        None => "",
        Some(value) => value
            .as_str()
            .ok_or_else(|| protocol("invalid Connect terminal error message"))?,
    };
    Ok(Some(if message.is_empty() {
        format!("Cursor stream returned {code}")
    } else {
        message.to_string()
    }))
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use futures::stream::{self, Stream, StreamExt};

    use super::super::{
        CONNECT_COMPRESSED_FLAG, CONNECT_END_STREAM_FLAG, StreamState, encode_frame, pb,
    };
    use super::*;
    use crate::model::{AssistantMessage, StopReason, StreamEvent};

    fn next_ready(state: &mut StreamState) -> Option<Result<StreamEvent>> {
        let mut future = std::pin::pin!(state.next_event());
        let waker = futures::task::noop_waker();
        match future.as_mut().poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(event) => event,
            Poll::Pending => panic!("finite ready test source unexpectedly blocked"),
        }
    }

    fn drain(chunks: Vec<Vec<u8>>) -> Vec<Result<StreamEvent>> {
        let source: Pin<Box<dyn Stream<Item = std::io::Result<Vec<u8>>> + Send>> =
            Box::pin(stream::iter(chunks.into_iter().map(Ok)));
        let mut state = StreamState::new(source, AssistantMessage::default());
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
            Ok(StreamEvent::Done { .. }) | Ok(StreamEvent::Error { .. })
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
            Some(Ok(StreamEvent::Done { reason: StopReason::Stop, .. }))
        ));
    }

    #[test]
    fn terminal_does_not_wait_for_body_close_or_poll_the_source_again() {
        let terminal = encode_frame(CONNECT_END_STREAM_FLAG, b"{}");
        let source = stream::iter(vec![Ok(terminal)]).chain(stream::poll_fn(|_| {
            panic!("source polled after Connect terminal")
        }));
        let mut state = StreamState::new(Box::pin(source), AssistantMessage::default());
        assert!(matches!(next_ready(&mut state), Some(Ok(StreamEvent::Start { .. }))));
        assert!(matches!(next_ready(&mut state), Some(Ok(StreamEvent::Done { .. }))));
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
        assert!(!events.iter().any(|event| matches!(event, Ok(StreamEvent::Done { .. }))));
        match events.last() {
            Some(Ok(StreamEvent::Error { error, reason })) => {
                assert_eq!(*reason, StopReason::Error);
                assert_eq!(error.error_message.as_deref(), Some("try later"));
                assert!(!error.content.is_empty());
            }
            event => panic!("expected provider error, got {event:?}"),
        }
    }

    #[test]
    fn code_only_error_is_not_lost_and_metadata_is_supported() {
        assert_eq!(
            terminal_error(br#"{"error":{"code":"resource_exhausted"}}"#).unwrap(),
            Some("Cursor stream returned resource_exhausted".to_string())
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
        assert_eq!(Encoding::from_headers(&headers(&[])).unwrap(), Encoding::Identity);
        assert_eq!(
            Encoding::from_headers(&[(
                "CONTENT-TYPE".to_string(),
                "Application/Connect+Proto; charset=utf-8".to_string(),
            )]).unwrap(),
            Encoding::Identity
        );
        assert!(Encoding::from_headers(&[]).is_err());
        assert!(Encoding::from_headers(&[(
            "content-type".to_string(),
            "text/html".to_string(),
        )]).is_err());
        assert!(Encoding::from_headers(&headers(&[("content-type", "text/html")])).is_err());
    }

    #[test]
    fn response_admission_rejects_ambiguous_or_unadvertised_encodings() {
        assert_eq!(
            Encoding::from_headers(&headers(&[("Connect-Content-Encoding", "identity")])).unwrap(),
            Encoding::Identity
        );
        for encoding in ["", "gzip", "br", "identity, gzip", "credential-marker"] {
            let error = Encoding::from_headers(&headers(&[("connect-content-encoding", encoding)]))
                .unwrap_err();
            assert!(!error.to_string().contains("credential-marker"));
        }
        assert!(Encoding::from_headers(&headers(&[
            ("connect-content-encoding", "identity"),
            ("Connect-Content-Encoding", "identity"),
        ])).is_err());
    }
}
