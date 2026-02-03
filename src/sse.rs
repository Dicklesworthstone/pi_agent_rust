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

    /// Feed data to the parser and extract any complete events.
    ///
    /// Returns a vector of parsed events. Events are delimited by blank lines.
    pub fn feed(&mut self, data: &str) -> Vec<SseEvent> {
        self.buffer.push_str(data);
        let mut events = Vec::new();

        let mut start = 0usize;
        while let Some(rel_newline) = self.buffer[start..].find('\n') {
            let newline_pos = start + rel_newline;
            let mut line = &self.buffer[start..newline_pos];
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
            } else if let Some(rest) = line.strip_prefix(':') {
                // Comment line - ignore (but could be used for keep-alive)
                let _ = rest;
            } else if let Some((field, value)) = line.split_once(':') {
                // Field: value
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "event" => self.current.event = value.to_string(),
                    "data" => {
                        if self.has_data {
                            self.current.data.push('\n');
                        }
                        self.current.data.push_str(value);
                        self.has_data = true;
                    }
                    "id" => self.current.id = Some(value.to_string()),
                    "retry" => self.current.retry = value.parse().ok(),
                    _ => {} // Unknown field - ignore
                }
            } else {
                // Field with no value
                match line {
                    "event" => self.current.event = String::new(),
                    "data" => {
                        if self.has_data {
                            self.current.data.push('\n');
                        }
                        self.has_data = true;
                    }
                    "id" => self.current.id = Some(String::new()),
                    _ => {}
                }
            }
        }

        if start > 0 {
            self.buffer.drain(..start);
        }
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
            if let Some((field, value)) = line.split_once(':') {
                let value = value.strip_prefix(' ').unwrap_or(value);
                if field == "data" {
                    if self.has_data {
                        self.current.data.push('\n');
                    }
                    self.current.data.push_str(value);
                    self.has_data = true;
                }
            }
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
                    let mut buffer = std::mem::take(&mut self.utf8_buffer);
                    buffer.extend_from_slice(&bytes);

                    // Extract valid UTF-8 string and remaining bytes
                    let (valid_string, remaining_bytes) = match std::str::from_utf8(&buffer) {
                        Ok(s) => (s.to_string(), Vec::new()),
                        Err(e) => {
                            if e.error_len().is_some() {
                                // Invalid UTF-8 sequence encountered
                                return Poll::Ready(Some(Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    e,
                                ))));
                            }
                            // Incomplete UTF-8 sequence at the end
                            let valid_len = e.valid_up_to();
                            // valid_up_to() guarantees this range is valid UTF-8
                            let s = std::str::from_utf8(&buffer[..valid_len])
                                .expect("valid_up_to guarantees valid UTF-8")
                                .to_string();
                            let remaining = buffer[valid_len..].to_vec();
                            (s, remaining)
                        }
                    };

                    // Update buffer with remaining bytes first to release borrow
                    self.utf8_buffer = remaining_bytes;

                    // Feed valid text to parser (now safe since buffer borrow is released)
                    if !valid_string.is_empty() {
                        let events = self.parser.feed(&valid_string);
                        if !events.is_empty() {
                            self.pending_events = events.into_iter().collect();
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
}
