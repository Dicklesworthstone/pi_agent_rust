//! Server-Sent Events (SSE) parser for asupersync HTTP client.
//!
//! Implements the SSE protocol (text/event-stream) on top of asupersync's
//! HTTP client for streaming LLM responses.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// Event type (from "event:" field, defaults to "message").
    pub event: String,
    /// Event data (from "data:" field(s), joined with newlines).
    pub data: String,
    /// Last event ID (from "id:" field).
    pub id: Option<String>,
    /// Retry interval hint in milliseconds (from "retry:" field).
    pub retry: Option<u64>,
}

impl Default for SseEvent {
    fn default() -> Self {
        Self {
            event: "message".to_string(),
            data: String::new(),
            id: None,
            retry: None,
        }
    }
}

/// Parser state for SSE stream.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: String,
    current: SseEvent,
    has_data: bool,
}

impl SseParser {
    /// Create a new SSE parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Process a single line of SSE data.
    fn process_line(line: &str, current: &mut SseEvent, has_data: &mut bool) {
        if let Some(rest) = line.strip_prefix(':') {
            // Comment line - ignore (but could be used for keep-alive)
            let _ = rest;
        } else if let Some((field, value)) = line.split_once(':') {
            // Field: value
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => current.event = value.to_string(),
                "data" => {
                    current.data.push_str(value);
                    current.data.push('\n');
                    *has_data = true;
                }
                "id" => current.id = Some(value.to_string()),
                "retry" => current.retry = value.parse().ok(),
                _ => {} // Unknown field - ignore
            }
        } else {
            // Field with no value
            match line {
                "event" => current.event = String::new(),
                "data" => {
                    current.data.push('\n');
                    *has_data = true;
                }
                "id" => current.id = Some(String::new()),
                _ => {}
            }
        }
    }

    /// Feed data to the parser and extract any complete events.
    ///
    /// Returns a vector of parsed events. Events are delimited by blank lines.
    pub fn feed(&mut self, data: &str) -> Vec<SseEvent> {
        self.buffer.push_str(data);
        let mut events = Vec::new();

        let mut buffer = std::mem::take(&mut self.buffer);
        let mut start = 0usize;
        // Use memchr for ~4x faster newline scanning vs str::find.
        while let Some(rel_newline) = memchr::memchr(b'\n', &buffer.as_bytes()[start..]) {
            let newline_pos = start + rel_newline;
            let mut line = &buffer[start..newline_pos];
            if let Some(stripped) = line.strip_suffix('\r') {
                line = stripped;
            }
            start = newline_pos + 1;

            if line.is_empty() {
                // Blank line = event boundary
                if self.has_data {
                    // Trim trailing newline from data
                    if self.current.data.ends_with('\n') {
                        self.current.data.pop();
                    }
                    events.push(std::mem::take(&mut self.current));
                    self.current = SseEvent::default();
                    self.has_data = false;
                }
            } else {
                Self::process_line(line, &mut self.current, &mut self.has_data);
            }
        }

        if start > 0 {
            buffer.drain(..start);
        }
        self.buffer = buffer;
        events
    }

    /// Check if the parser has any pending data.
    pub fn has_pending(&self) -> bool {
        !self.buffer.is_empty() || self.has_data
    }

    /// Flush any pending event (called when stream ends).
    pub fn flush(&mut self) -> Option<SseEvent> {
        // First, process any remaining buffer content that doesn't end with newline
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            let line = line.trim_end_matches('\r');
            Self::process_line(line, &mut self.current, &mut self.has_data);
        }

        if self.has_data {
            if self.current.data.ends_with('\n') {
                self.current.data.pop();
            }
            let event = std::mem::take(&mut self.current);
            self.current = SseEvent::default();
            self.has_data = false;
            Some(event)
        } else {
            None
        }
    }
}

/// Stream wrapper for SSE events.
///
/// Converts a byte stream into an SSE event stream.
pub struct SseStream<S> {
    inner: S,
    parser: SseParser,
    pending_events: VecDeque<SseEvent>,
    utf8_buffer: Vec<u8>,
}

impl<S> SseStream<S> {
    /// Create a new SSE stream from a byte stream.
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            parser: SseParser::new(),
            pending_events: VecDeque::new(),
            utf8_buffer: Vec::new(),
        }
    }
}

impl<S> SseStream<S>
where
    S: futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Unpin,
{
    /// Poll for the next SSE event.
    pub fn poll_next_event(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<SseEvent, std::io::Error>>> {
        // Return any pending events first
        if let Some(event) = self.pending_events.pop_front() {
            return Poll::Ready(Some(Ok(event)));
        }

        // Poll the inner stream for more data
        loop {
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    self.utf8_buffer.extend_from_slice(&bytes);

                    // Determine how much of the buffer is valid UTF-8.
                    // Common path: the entire buffer is valid (no intermediate String copy).
                    let valid_len = match std::str::from_utf8(&self.utf8_buffer) {
                        Ok(_) => self.utf8_buffer.len(),
                        Err(e) => {
                            if e.error_len().is_some() {
                                return Poll::Ready(Some(Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    e,
                                ))));
                            }
                            e.valid_up_to()
                        }
                    };

                    // Feed valid portion to the parser.
                    if valid_len > 0 {
                        // Convert to owned String to avoid borrow conflict with self.parser.
                        let valid_str = std::str::from_utf8(&self.utf8_buffer[..valid_len])
                            .unwrap()
                            .to_string();
                        let events = self.parser.feed(&valid_str);
                        if !events.is_empty() {
                            self.pending_events = events.into_iter().collect();
                        }

                        // Remove the consumed bytes efficiently.
                        if valid_len == self.utf8_buffer.len() {
                            self.utf8_buffer.clear();
                        } else {
                            // Keep only the trailing incomplete UTF-8 bytes.
                            let remaining = self.utf8_buffer[valid_len..].to_vec();
                            self.utf8_buffer = remaining;
                        }
                    }

                    // If we have pending events, return the first one
                    if let Some(event) = self.pending_events.pop_front() {
                        return Poll::Ready(Some(Ok(event)));
                    }
                    // Otherwise continue polling
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => {
                    // Stream ended
                    // If we have incomplete bytes in buffer, that's an error
                    if !self.utf8_buffer.is_empty() {
                        return Poll::Ready(Some(Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Stream ended with incomplete UTF-8 sequence",
                        ))));
                    }

                    // Flush any pending event
                    if let Some(event) = self.parser.flush() {
                        return Poll::Ready(Some(Ok(event)));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}

impl<S> futures::Stream for SseStream<S>
where
    S: futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Unpin,
{
    type Item = Result<SseEvent, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll_next_event(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use futures::stream;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    struct GeneratedEvent {
        event: Option<String>,
        id: Option<String>,
        retry: Option<u32>,
        data: Vec<String>,
        comment: Option<String>,
    }

    impl GeneratedEvent {
        fn render(&self) -> String {
            let mut out = String::new();
            if let Some(comment) = &self.comment {
                out.push(':');
                out.push_str(comment);
                out.push('\n');
            }
            if let Some(event) = &self.event {
                out.push_str("event: ");
                out.push_str(event);
                out.push('\n');
            }
            if let Some(id) = &self.id {
                out.push_str("id: ");
                out.push_str(id);
                out.push('\n');
            }
            if let Some(retry) = &self.retry {
                out.push_str("retry: ");
                out.push_str(&retry.to_string());
                out.push('\n');
            }
            for line in &self.data {
                out.push_str("data: ");
                out.push_str(line);
                out.push('\n');
            }
            out.push('\n');
            out
        }
    }

    fn ascii_line() -> impl Strategy<Value = String> {
        // ASCII printable range (no CR/LF), keeps chunking safe with byte splits.
        "[ -~]{0,24}".prop_map(|s| s)
    }

    fn event_strategy() -> impl Strategy<Value = GeneratedEvent> {
        (
            prop::option::of("[a-z_]{1,12}"),
            prop::option::of("[0-9]{1,8}"),
            prop::option::of(0u32..5000),
            prop::collection::vec(ascii_line(), 1..4),
            prop::option::of(ascii_line()),
        )
            .prop_map(|(event, id, retry, data, comment)| GeneratedEvent {
                event,
                id,
                retry,
                data,
                comment,
            })
    }

    fn render_stream(events: &[GeneratedEvent], terminal_delimiter: bool) -> String {
        let mut out = String::new();
        for event in events {
            out.push_str(&event.render());
        }
        if !terminal_delimiter && out.ends_with('\n') {
            out.pop();
        }
        out
    }

    fn parse_all(input: &str) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        let mut events = parser.feed(input);
        if let Some(event) = parser.flush() {
            events.push(event);
        }
        events
    }

    fn parse_chunked(input: &str, chunk_sizes: &[usize]) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        let bytes = input.as_bytes();
        let mut start = 0usize;

        for &size in chunk_sizes {
            if start >= bytes.len() {
                break;
            }
            let end = (start + size).min(bytes.len());
            let chunk = std::str::from_utf8(&bytes[start..end]).expect("ascii chunks");
            events.extend(parser.feed(chunk));
            start = end;
        }

        if start < bytes.len() {
            let chunk = std::str::from_utf8(&bytes[start..]).expect("ascii remainder");
            events.extend(parser.feed(chunk));
        }

        if let Some(event) = parser.flush() {
            events.push(event);
        }

        events
    }

    #[test]
    fn test_simple_event() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "message");
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn test_multiline_data() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: line1\ndata: line2\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn test_named_event() {
        let mut parser = SseParser::new();
        let events = parser.feed("event: ping\ndata: {}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "ping");
        assert_eq!(events[0].data, "{}");
    }

    #[test]
    fn test_event_with_id() {
        let mut parser = SseParser::new();
        let events = parser.feed("id: 123\ndata: test\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, Some("123".to_string()));
        assert_eq!(events[0].data, "test");
    }

    #[test]
    fn test_multiple_events() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: first\n\ndata: second\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "first");
        assert_eq!(events[1].data, "second");
    }

    #[test]
    fn test_incremental_feed() {
        let mut parser = SseParser::new();

        // Feed partial data
        let events = parser.feed("data: hel");
        assert!(events.is_empty());

        // Feed more
        let events = parser.feed("lo\n");
        assert!(events.is_empty());

        // Feed blank line to complete event
        let events = parser.feed("\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn test_comment_ignored() {
        let mut parser = SseParser::new();
        let events = parser.feed(":this is a comment\ndata: actual\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "actual");
    }

    #[test]
    fn test_retry_field() {
        let mut parser = SseParser::new();
        let events = parser.feed("retry: 3000\ndata: test\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].retry, Some(3000));
    }

    #[test]
    fn test_crlf_handling() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: hello\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn test_flush_pending() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: incomplete");
        assert!(events.is_empty());
        assert!(parser.has_pending());

        // Flush at stream end
        let event = parser.flush();
        assert!(event.is_some());
        assert_eq!(event.unwrap().data, "incomplete");
    }

    #[test]
    fn test_anthropic_style_events() {
        let mut parser = SseParser::new();

        // Simulate Anthropic API response
        let events = parser.feed(
            r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_123"}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_stop
data: {"type":"message_stop"}

"#,
        );

        assert_eq!(events.len(), 5);
        assert_eq!(events[0].event, "message_start");
        assert!(events[0].data.contains("message_start"));
        assert_eq!(events[1].event, "content_block_start");
        assert_eq!(events[2].event, "content_block_delta");
        assert!(events[2].data.contains("Hello"));
        assert_eq!(events[3].event, "content_block_stop");
        assert_eq!(events[4].event, "message_stop");
    }

    #[test]
    fn test_stream_yields_multiple_events_from_one_chunk() {
        let bytes = b"data: first\n\ndata: second\n\n".to_vec();
        let mut stream = SseStream::new(stream::iter(vec![Ok(bytes)]));

        futures::executor::block_on(async {
            let first = stream.next().await.expect("first event").expect("ok");
            assert_eq!(first.data, "first");

            let second = stream.next().await.expect("second event").expect("ok");
            assert_eq!(second.data, "second");

            assert!(stream.next().await.is_none());
        });
    }

    #[test]
    fn test_stream_handles_utf8_split_across_chunks() {
        // Snowman is a 3-byte UTF-8 sequence: E2 98 83. Split it across chunks.
        let chunks = vec![Ok(b"data: \xE2".to_vec()), Ok(b"\x98\x83\n\n".to_vec())];
        let mut stream = SseStream::new(stream::iter(chunks));

        futures::executor::block_on(async {
            let event = stream.next().await.expect("event").expect("ok");
            assert_eq!(event.data, "☃");
            assert!(stream.next().await.is_none());
        });
    }

    #[test]
    fn test_stream_flushes_pending_event_at_end() {
        let mut stream = SseStream::new(stream::iter(vec![Ok(b"data: last".to_vec())]));

        futures::executor::block_on(async {
            let event = stream.next().await.expect("event").expect("ok");
            assert_eq!(event.data, "last");
            assert!(stream.next().await.is_none());
        });
    }

    #[test]
    fn test_stream_errors_on_incomplete_utf8_at_end() {
        let mut stream = SseStream::new(stream::iter(vec![Ok(b"data: \xE2".to_vec())]));

        futures::executor::block_on(async {
            let err = stream
                .next()
                .await
                .expect("expected a result")
                .expect_err("expected utf8 error");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        });
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 64,
            max_shrink_iters: 200,
            .. ProptestConfig::default()
        })]

        #[test]
        fn sse_chunking_invariant(
            events in prop::collection::vec(event_strategy(), 1..10),
            chunk_sizes in prop::collection::vec(1usize..32, 0..20),
            terminal_delimiter in any::<bool>(),
        ) {
            let input = render_stream(&events, terminal_delimiter);
            let expected = parse_all(&input);
            let actual = parse_chunked(&input, &chunk_sizes);
            prop_assert_eq!(actual, expected);
        }
    }
}
