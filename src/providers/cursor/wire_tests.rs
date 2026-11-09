//! Regression coverage for atomic, bounded Cursor protobuf decoding.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use futures::{FutureExt, Stream, StreamExt, stream};

use crate::cost::CostBudget;
use crate::provider::{Context, ModelCost, Provider, StreamOptions};

use super::{
    AssistantMessage, CONNECT_COMPRESSED_FLAG, CONNECT_END_STREAM_FLAG, CursorProvider, Error,
    MAX_PROTOBUF_EVENTS, MAX_PROTOBUF_FIELDS, Result, ServerEvent, StreamEvent, StreamState,
    connect, decode_server_message, encode_frame, pb,
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

struct UsageWireProbe {
    chunks: Mutex<VecDeque<std::io::Result<Vec<u8>>>>,
    pending: bool,
    dropped: AtomicBool,
}

struct UsageWireBody(Arc<UsageWireProbe>);

impl Stream for UsageWireBody {
    type Item = std::io::Result<Vec<u8>>;

    fn poll_next(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        if let Some(chunk) = self.0.chunks.lock().unwrap().pop_front() {
            Poll::Ready(Some(chunk))
        } else if self.0.pending {
            Poll::Pending
        } else {
            Poll::Ready(None)
        }
    }
}

impl Drop for UsageWireBody {
    fn drop(&mut self) {
        self.0.dropped.store(true, Ordering::SeqCst);
    }
}

struct UsageWireProvider(Mutex<Option<UsageWireBody>>);

fn usage_prices() -> ModelCost {
    ModelCost {
        input: 1_000_000.0,
        output: 2_000_000.0,
        ..ModelCost::default()
    }
}

#[async_trait::async_trait]
impl Provider for UsageWireProvider {
    fn name(&self) -> &str {
        "cursor"
    }

    fn api(&self) -> &str {
        "cursor-agent"
    }

    fn model_id(&self) -> &str {
        "cursor-usage-fixture"
    }

    fn model_cost(&self) -> Option<ModelCost> {
        Some(usage_prices())
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let source = self.0.lock().unwrap().take().expect("one wire request");
        let state = StreamState::new(Box::pin(source), AssistantMessage::default())
            .with_usage_observer(options.usage_observer.clone());
        Ok(state.into_stream())
    }
}

fn guarded_usage_wire(
    chunks: Vec<std::io::Result<Vec<u8>>>,
    pending: bool,
    cap: f64,
) -> (Arc<dyn Provider>, CostBudget, Arc<UsageWireProbe>) {
    let probe = Arc::new(UsageWireProbe {
        chunks: Mutex::new(chunks.into()),
        pending,
        dropped: AtomicBool::new(false),
    });
    let provider = Arc::new(UsageWireProvider(Mutex::new(Some(UsageWireBody(
        Arc::clone(&probe),
    )))));
    let budget = CostBudget::new(None, Some(cap), 0.0).unwrap();
    (
        crate::provider::guard_with_cost_budget(provider, budget.clone()),
        budget,
        probe,
    )
}

fn usage_frames() -> Vec<u8> {
    [
        encode_frame(0, &interaction(8, &integer_field(1, 2))),
        encode_frame(0, &checkpoint(10)),
        encode_frame(0, &interaction(8, &integer_field(1, 3))),
        encode_frame(0, &checkpoint(12)),
        encode_frame(0, &checkpoint(12)),
        encode_frame(0, &interaction(8, &integer_field(1, u64::MAX))),
    ]
    .concat()
}

#[test]
fn usage_frames_reserve_spend_before_pending_and_settle_on_drop() {
    futures::executor::block_on(async {
        for (wire, expected) in [
            (usage_frames(), 17.0),
            (encode_frame(0, &interaction(8, &integer_field(1, 5))), 10.0),
        ] {
            let (provider, budget, probe) =
                guarded_usage_wire(vec![Ok(wire)], true, expected - 1.0);
            let context = Context::default();
            let options = StreamOptions::default();
            let mut output = provider.stream(&context, &options).await.unwrap();
            assert!(output.next().now_or_never().is_none());
            assert_eq!(budget.snapshot().pending, expected);
            assert_eq!(budget.spent(), 0.0);
            assert!(!probe.dropped.load(Ordering::SeqCst));
            assert!(matches!(
                provider.stream(&context, &options).await,
                Err(Error::CostCap { .. })
            ));
            drop(output);
            assert!(probe.dropped.load(Ordering::SeqCst));
            assert_eq!(budget.snapshot().pending, 0.0);
            assert_eq!(budget.spent(), expected);
        }
    });
}

#[test]
fn accepted_usage_survives_transport_and_protocol_failures_once() {
    futures::executor::block_on(async {
        for outcome in [
            "done",
            "refused",
            "eof",
            "io",
            "truncated",
            "protobuf",
            "terminal",
        ] {
            let mut chunks = vec![Ok(usage_frames())];
            match outcome {
                "done" => chunks.push(Ok(encode_frame(CONNECT_END_STREAM_FLAG, b"{}"))),
                "refused" => chunks.push(Ok(encode_frame(
                    CONNECT_END_STREAM_FLAG,
                    br#"{"error":{"code":"unavailable","message":"capacity"}}"#,
                ))),
                "io" => chunks.push(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "fixture transport failure",
                ))),
                "truncated" => chunks.push(Ok(vec![0, 0])),
                "protobuf" => {
                    let mut invalid = checkpoint(1_000);
                    invalid.extend_from_slice(&interaction(8, &integer_field(1, 100)));
                    invalid.push(0x80);
                    chunks.push(Ok(encode_frame(0, &invalid)));
                }
                "terminal" => chunks.push(Ok(encode_frame(CONNECT_END_STREAM_FLAG, b"[]"))),
                _ => {}
            }
            let (provider, budget, probe) = guarded_usage_wire(chunks, false, 16.0);
            let context = Context::default();
            let options = StreamOptions::default();
            let mut output = provider.stream(&context, &options).await.unwrap();
            let mut terminal = None;
            while let Some(event) = output.next().await {
                match event {
                    Ok(StreamEvent::Start { .. }) => {}
                    Ok(StreamEvent::Done { message, .. }) => {
                        assert_eq!(outcome, "done");
                        assert!(terminal.replace("done").is_none());
                        assert_eq!(message.usage.cost.total, 17.0);
                        assert_eq!(message.usage.input, 7);
                        assert_eq!(message.usage.output, 5);
                        assert_eq!(message.usage.total_tokens, 12);
                    }
                    Ok(StreamEvent::Error { error, .. }) => {
                        assert_eq!(outcome, "refused");
                        assert!(terminal.replace("refused").is_none());
                        assert_eq!(error.usage.cost.total, 17.0);
                    }
                    Err(_) => {
                        assert!(!matches!(outcome, "done" | "refused"));
                        assert!(terminal.replace("failed").is_none());
                    }
                    event => panic!("unexpected usage-only event: {event:?}"),
                }
            }
            assert!(terminal.is_some(), "{outcome}");
            assert!(probe.dropped.load(Ordering::SeqCst));
            assert_eq!(budget.snapshot().pending, 0.0);
            assert_eq!(budget.spent(), 17.0, "{outcome}");
            assert!(matches!(
                provider.stream(&context, &options).await,
                Err(Error::CostCap { .. })
            ));
            drop(output);
            assert_eq!(
                budget.spent(),
                17.0,
                "terminal settlement is not charged twice"
            );
        }
    });
}

#[test]
fn terminal_usage_replaces_estimate_before_buffered_terminal_is_consumed() {
    futures::executor::block_on(async {
        for with_text in [false, true] {
            for refused in [false, true] {
                let mut prefix = if with_text {
                    encode_frame(0, &text_message(b"preview"))
                } else {
                    Vec::new()
                };
                prefix.extend_from_slice(&encode_frame(0, &interaction(8, &integer_field(1, 5))));
                prefix.extend_from_slice(&encode_frame(0, &checkpoint(50)));
                let (provider, budget, probe) = guarded_usage_wire(vec![Ok(prefix)], true, 20.0);
                let context = Context::default();
                let options = StreamOptions::default();
                let mut output = provider.stream(&context, &options).await.unwrap();
                if with_text {
                    assert!(matches!(
                        output.next().await,
                        Some(Ok(StreamEvent::Start { .. }))
                    ));
                    assert!(matches!(
                        output.next().await,
                        Some(Ok(StreamEvent::TextStart { .. }))
                    ));
                    assert!(matches!(
                        output.next().await,
                        Some(Ok(StreamEvent::TextDelta { .. }))
                    ));
                }
                assert!(output.next().now_or_never().is_none());
                assert_eq!(budget.snapshot().pending, 55.0);
                assert!(matches!(
                    provider.stream(&context, &options).await,
                    Err(Error::CostCap { .. })
                ));
                let terminal: &[u8] = if refused {
                    br#"{"error":{"code":"unavailable","message":"capacity"}}"#
                } else {
                    b"{}"
                };
                probe.chunks.lock().unwrap().push_back(Ok([
                    encode_frame(0, &checkpoint(12)),
                    encode_frame(CONNECT_END_STREAM_FLAG, terminal),
                ]
                .concat()));
                let preview = output.next().await.unwrap().unwrap();
                if with_text {
                    assert!(matches!(preview, StreamEvent::TextEnd { .. }));
                } else {
                    assert!(matches!(preview, StreamEvent::Start { .. }));
                }
                assert!(probe.dropped.load(Ordering::SeqCst));
                assert_eq!(budget.snapshot().pending, 17.0);
                assert_eq!(budget.spent(), 0.0);
                budget
                    .admit()
                    .expect("the final counters replace the larger provisional bill");
                // Deliberately do not consume the buffered Done/Error.
                drop(output);
                assert_eq!(budget.snapshot().pending, 0.0);
                assert_eq!(budget.spent(), 17.0);
            }
        }
    });
}

fn cursor_http_server(
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{BufRead, Read, Write};
    use std::time::{Duration, Instant};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "Cursor usage fixture was not contacted"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("Cursor fixture accept failed: {error}"),
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut request = std::io::BufReader::new(socket);
        let mut line = String::new();
        let mut content_length = None;
        loop {
            line.clear();
            assert!(request.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let mut request_body = vec![0; content_length.expect("framed Cursor request body")];
        request.read_exact(&mut request_body).unwrap();
        let socket = request.get_mut();
        write!(
            socket,
            "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        socket.write_all(&body).unwrap();
    });
    (
        format!("http://{address}/agent.v1.AgentService/Run"),
        server,
    )
}

#[test]
fn cursor_provider_passes_observed_usage_through_http_transport_failure() {
    let (url, server) = cursor_http_server(200, "application/connect+proto", usage_frames());
    let priced = Arc::new(crate::providers::PricedProvider {
        inner: Arc::new(CursorProvider::new("cursor-usage-fixture").with_base_url(url)),
        cost: usage_prices(),
    });
    let budget = CostBudget::new(None, Some(16.0), 0.0).unwrap();
    let provider = crate::provider::guard_with_cost_budget(priced, budget.clone());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let context = Context {
            messages: vec![crate::model::Message::User(crate::model::UserMessage {
                content: crate::model::UserContent::Text("usage probe".to_string()),
                timestamp: 0,
            })]
            .into(),
            ..Context::default()
        };
        let options = StreamOptions {
            api_key: Some("cursor-test-key".to_string()),
            ..StreamOptions::default()
        };
        let mut output = provider.stream(&context, &options).await.unwrap();
        let error = output.next().await.unwrap().unwrap_err();
        assert!(error.to_string().contains("PI_CURSOR_STREAM_INCOMPLETE"));
        assert_eq!(budget.spent(), 17.0);
        assert_eq!(budget.snapshot().pending, 0.0);
        assert!(matches!(
            provider.stream(&context, &options).await,
            Err(Error::CostCap { .. })
        ));
        assert!(output.next().await.is_none());
    });
    server.join().expect("Cursor usage fixture completed");
}

fn diagnostic_context() -> Context<'static> {
    Context {
        messages: vec![crate::model::Message::User(crate::model::UserMessage {
            content: crate::model::UserContent::Text("diagnostic probe".to_string()),
            timestamp: 0,
        })]
        .into(),
        ..Context::default()
    }
}

#[test]
fn cursor_http_errors_are_bounded_redacted_and_keep_the_actual_status() {
    let credential = "cursor+credential/%42";
    let encoded = "cursor%2Bcredential%2F%2542";
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let context = diagnostic_context();
    let options = StreamOptions {
        api_key: Some(credential.to_string()),
        ..StreamOptions::default()
    };
    for (status, body, oversized) in [
        (401, format!("denied: {credential}; {encoded}; \u{1b}]52;c;control\u{7}\u{202e}").into_bytes(), false),
        (503, br#"{"message":"capacity","access_token":"server-only-access","refresh_token":"server-only-refresh"}"#.to_vec(), false),
        (429, vec![b'x'; super::MAX_ERROR_BODY_BYTES + 1], true),
    ] {
        let (url, server) = cursor_http_server(status, "application/json", body);
        let provider = CursorProvider::new("cursor-diagnostic-fixture").with_base_url(url);
        runtime.block_on(async {
            let error = match provider.stream(&context, &options).await {
                Err(error) => error,
                Ok(_) => panic!("an HTTP error must not admit a response stream"),
            };
            let diagnostic = error.to_string();
            assert!(diagnostic.contains(&format!("Cursor API error (HTTP {status}):")));
            for secret in [credential, encoded, "server-only-access", "server-only-refresh"] {
                assert!(!diagnostic.contains(secret), "credential escaped in {diagnostic}");
            }
            assert!(!diagnostic.chars().any(char::is_control));
            assert!(!diagnostic.contains('\u{202e}'));
            assert!(diagnostic.len() < connect::MAX_DIAGNOSTIC_BYTES + 128);
            if oversized {
                assert!(diagnostic.contains("65536-byte diagnostic limit"));
            } else {
                assert!(diagnostic.contains("[REDACTED]"));
            }
            assert_eq!(
                crate::failover::call_error_is_retryable(&error),
                status != 401,
                "diagnostic screening must retain HTTP recovery semantics",
            );
        });
        server.join().expect("Cursor diagnostic fixture completed");
    }
}

#[test]
fn cursor_terminal_errors_screen_the_request_credential_and_preserve_preview() {
    let credential = "cursor+credential/%42";
    let terminal = serde_json::to_vec(&serde_json::json!({
        "error": {
            "code": "resource_exhausted",
            "message": format!("capacity: {credential}; cursor%2Bcredential%2F%2542; \u{1b}]52;c;control\u{7}"),
        }
    }))
    .unwrap();
    let mut wire = encode_frame(0, &text_message(b"accepted preview"));
    wire.extend_from_slice(&encode_frame(CONNECT_END_STREAM_FLAG, &terminal));
    let (url, server) = cursor_http_server(200, "application/connect+proto", wire);
    let provider = CursorProvider::new("cursor-diagnostic-fixture").with_base_url(url);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let options = StreamOptions {
            api_key: Some(credential.to_string()),
            ..StreamOptions::default()
        };
        let mut output = provider.stream(&diagnostic_context(), &options).await.unwrap();
        let mut failures = 0;
        let mut preview = String::new();
        while let Some(event) = output.next().await {
            match event.unwrap() {
                StreamEvent::TextDelta { delta, .. } => preview.push_str(&delta),
                StreamEvent::Error { error, reason } => {
                    failures += 1;
                    assert_eq!(reason, crate::model::StopReason::Error);
                    let diagnostic = error.error_message.as_deref().unwrap();
                    assert!(diagnostic.starts_with("PI_CONNECT_ERROR[resource_exhausted]:"));
                    assert_eq!(diagnostic.matches("[REDACTED]").count(), 2);
                    assert!(!diagnostic.contains(credential));
                    assert!(!diagnostic.chars().any(char::is_control));
                    assert_eq!(
                        crate::failover::classify_failover(diagnostic),
                        Some(crate::failover::FailoverClass::Quota),
                    );
                    assert!(matches!(&error.content[0], crate::model::ContentBlock::Text(text) if text.text == "accepted preview"));
                }
                StreamEvent::Done { .. } => panic!("a rejected Connect call cannot succeed"),
                _ => {}
            }
        }
        assert_eq!(preview, "accepted preview");
        assert_eq!(failures, 1);
        assert!(output.next().await.is_none());
        assert!(output.next().await.is_none());
    });
    server.join().expect("Cursor terminal fixture completed");
}

#[test]
fn cursor_response_stream_is_fused_after_success_and_protocol_failure() {
    futures::executor::block_on(async {
        for terminal in [b"{}".as_slice(), b"invalid-json".as_slice()] {
            let wire = encode_frame(CONNECT_END_STREAM_FLAG, terminal);
            let state = StreamState::new(
                Box::pin(stream::iter([Ok(wire)])),
                AssistantMessage::default(),
            )
            .with_diagnostic_token("private-token".into());
            let mut output = state.into_stream();
            let mut terminal_count = 0;
            while let Some(event) = output.next().await {
                if matches!(event, Err(_) | Ok(StreamEvent::Done { .. })) {
                    terminal_count += 1;
                }
            }
            assert_eq!(terminal_count, 1);
            for _ in 0..3 {
                assert!(output.next().await.is_none());
            }
        }
    });
}
