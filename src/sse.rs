//! Server-Sent Events (SSE) parser for asupersync HTTP client.
//!
//! Implements the SSE protocol (text/event-stream) on top of asupersync's
//! HTTP client for streaming LLM responses.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

const MAX_EVENT_DATA_BYTES: usize = 100 * 1024 * 1024;

/// A parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// Event type (from "event:" field, defaults to "message").
    pub event: Cow<'static, str>,
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
            event: Cow::Borrowed("message"),
            data: String::new(),
            id: None,
            retry: None,
        }
    }
}

/// Parser state for SSE stream.
#[derive(Debug)]
pub struct SseParser {
    buffer: String,
    current: SseEvent,
    has_data: bool,
    /// Whether we've already stripped the BOM from the first feed.
    bom_checked: bool,
    /// Number of bytes in `buffer` that have already been scanned for newlines.
    scanned_len: usize,
    /// Per-event data accumulation cap in bytes.
    max_event_data_bytes: usize,
}

impl Default for SseParser {
    fn default() -> Self {
        Self {
            buffer: String::new(),
            current: SseEvent::default(),
            has_data: false,
            bom_checked: false,
            scanned_len: 0,
            max_event_data_bytes: MAX_EVENT_DATA_BYTES,
        }
    }
}

impl SseParser {
    /// Create a new SSE parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a parser with a custom per-event data cap (for testing).
    #[cfg(test)]
    fn with_max_event_data_bytes(limit: usize) -> Self {
        Self {
            max_event_data_bytes: limit,
            ..Self::default()
        }
    }

    /// Intern common SSE event type names to avoid per-event String allocation.
    /// LLM streaming APIs use a fixed set of event types; matching them to
    /// `Cow::Borrowed` static strings eliminates one allocation per event.
    #[inline]
    fn intern_event_type(value: &str) -> Cow<'static, str> {
        match value {
            // Anthropic streaming events
            "message" => Cow::Borrowed("message"),
            "message_start" => Cow::Borrowed("message_start"),
            "message_stop" => Cow::Borrowed("message_stop"),
            "message_delta" => Cow::Borrowed("message_delta"),
            "content_block_start" => Cow::Borrowed("content_block_start"),
            "content_block_delta" => Cow::Borrowed("content_block_delta"),
            "content_block_stop" => Cow::Borrowed("content_block_stop"),
            // OpenAI Responses API streaming events
            "response.completed" => Cow::Borrowed("response.completed"),
            "response.done" => Cow::Borrowed("response.done"),
            "response.failed" => Cow::Borrowed("response.failed"),
            "response.incomplete" => Cow::Borrowed("response.incomplete"),
            "response.output_text.delta" => Cow::Borrowed("response.output_text.delta"),
            "response.output_text.done" => Cow::Borrowed("response.output_text.done"),
            "response.output_item.added" => Cow::Borrowed("response.output_item.added"),
            "response.output_item.done" => Cow::Borrowed("response.output_item.done"),
            "response.content_part.done" => Cow::Borrowed("response.content_part.done"),
            "response.function_call_arguments.delta" => {
                Cow::Borrowed("response.function_call_arguments.delta")
            }
            "response.reasoning_text.delta" => Cow::Borrowed("response.reasoning_text.delta"),
            "response.reasoning_text.done" => Cow::Borrowed("response.reasoning_text.done"),
            "response.reasoning_summary_text.delta" => {
                Cow::Borrowed("response.reasoning_summary_text.delta")
            }
            "response.reasoning_summary_text.done" => {
                Cow::Borrowed("response.reasoning_summary_text.done")
            }
            "response.reasoning_summary_part.done" => {
                Cow::Borrowed("response.reasoning_summary_part.done")
            }
            "response.created" => Cow::Borrowed("response.created"),
            // Common shared events
            "ping" => Cow::Borrowed("ping"),
            "error" => Cow::Borrowed("error"),
            _ => Cow::Owned(value.to_string()),
        }
    }

    #[inline]
    fn append_data_line(
        current: &mut SseEvent,
        value: &str,
        has_data: &mut bool,
        max_event_data_bytes: usize,
    ) {
        let projected_len = current
            .data
            .len()
            .saturating_add(value.len())
            .saturating_add(1);
        if projected_len > max_event_data_bytes {
            // Preserve the event boundary even when we drop this data line.
            // This avoids silently skipping oversized events, which can cause
            // downstream state machines to hang waiting for a completion event.
            *has_data = true;
            return;
        }
        current.data.push_str(value);
        current.data.push('\n');
        *has_data = true;
    }

    #[inline]
    fn parse_retry(value: &str) -> Option<u64> {
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
            value.parse().ok()
        } else {
            None
        }
    }

    /// Process a single line of SSE data.
    fn process_line(
        line: &str,
        current: &mut SseEvent,
        has_data: &mut bool,
        max_event_data_bytes: usize,
    ) {
        if let Some(rest) = line.strip_prefix(':') {
            // Comment line - ignore (but could be used for keep-alive)
            let _ = rest;
        } else if let Some((field, value)) = line.split_once(':') {
            // Field: value
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => current.event = Self::intern_event_type(value),
                "data" => Self::append_data_line(current, value, has_data, max_event_data_bytes),
                "id" if !value.contains('\0') => {
                    current.id = Some(value.to_string());
                }
                "retry" => current.retry = Self::parse_retry(value),
                _ => {} // Unknown field - ignore
            }
        } else {
            // Field with no value
            match line {
                "event" => current.event = Cow::Borrowed(""),
                "data" => Self::append_data_line(current, "", has_data, max_event_data_bytes),
                "id" => current.id = Some(String::new()),
                _ => {}
            }
        }
    }

    #[inline]
    fn reset_current_for_next_event(current: &mut SseEvent) {
        current.event = Cow::Borrowed("message");
        current.data.clear();
    }

    #[inline]
    fn carry_forward_event_state(current: &SseEvent) -> SseEvent {
        SseEvent {
            id: current.id.clone(),
            retry: current.retry,
            ..Default::default()
        }
    }

    #[inline]
    fn reset_after_buffer_limit<F>(&mut self, emit: &mut F)
    where
        F: FnMut(SseEvent),
    {
        self.buffer = String::new();
        self.current = SseEvent::default();
        self.has_data = false;
        self.bom_checked = false;
        self.scanned_len = 0;
        emit(SseEvent {
            event: Cow::Borrowed("error"),
            data: "SSE buffer limit exceeded".to_string(),
            ..Default::default()
        });
    }

    /// Process complete lines from `source`, dispatching events via `emit`.
    /// Returns the byte offset of the first unconsumed byte.
    #[inline]
    fn process_source<F>(
        source: &str,
        scan_start: usize,
        bom_checked: &mut bool,
        current: &mut SseEvent,
        has_data: &mut bool,
        max_event_data_bytes: usize,
        emit: &mut F,
    ) -> usize
    where
        F: FnMut(SseEvent),
    {
        let bytes = source.as_bytes();
        let mut start = 0usize;
        let mut search_pos = scan_start;

        // Strip UTF-8 BOM from the beginning of the stream (SSE spec compliance).
        if !*bom_checked && !source.is_empty() {
            *bom_checked = true;
            if source.starts_with('\u{FEFF}') {
                start = 3;
                if search_pos < 3 {
                    search_pos = 3;
                }
            }
        }

        // Use memchr2 to find either \r or \n
        while let Some(rel_pos) = memchr::memchr2(b'\r', b'\n', &bytes[search_pos..]) {
            let pos = search_pos + rel_pos;
            let b = bytes[pos];

            let line_end;
            let next_start;

            if b == b'\n' {
                // Bare LF
                line_end = pos;
                next_start = pos + 1;
            } else {
                // Found \r
                if pos + 1 < source.len() {
                    line_end = pos;
                    next_start = if bytes[pos + 1] == b'\n' {
                        // CRLF
                        pos + 2
                    } else {
                        // Bare CR
                        pos + 1
                    };
                } else {
                    // CR at end of buffer - wait for more data to check for \n
                    break;
                }
            }

            let line = &source[start..line_end];
            start = next_start;
            search_pos = next_start;

            if line.is_empty() {
                // Blank line = event boundary
                if *has_data {
                    // Trim trailing newline from data
                    if current.data.ends_with('\n') {
                        current.data.pop();
                    }
                    // Per SSE spec, an empty event name dispatches as "message".
                    if current.event.is_empty() {
                        current.event = Cow::Borrowed("message");
                    }
                    let next_event = Self::carry_forward_event_state(current);
                    emit(std::mem::take(current));
                    *current = next_event;
                    *has_data = false;
                } else {
                    Self::reset_current_for_next_event(current);
                }
            } else {
                Self::process_line(line, current, has_data, max_event_data_bytes);
            }
        }

        start
    }

    /// Feed data to the parser and emit any complete events to `emit`.
    fn feed_into<F>(&mut self, data: &str, mut emit: F)
    where
        F: FnMut(SseEvent),
    {
        const MAX_BUFFER_SIZE: usize = 10 * 1024 * 1024;
        if self.buffer.is_empty() {
            // Fast path: process data directly without copying to buffer.
            let consumed = Self::process_source(
                data,
                0,
                &mut self.bom_checked,
                &mut self.current,
                &mut self.has_data,
                self.max_event_data_bytes,
                &mut emit,
            );
            if consumed < data.len() {
                self.buffer.push_str(&data[consumed..]);
                if self.buffer.len() > MAX_BUFFER_SIZE {
                    self.reset_after_buffer_limit(&mut emit);
                    return;
                }
            }
        } else {
            // Slow path: parse against a temporary combined source so we only
            // retain the truly unconsumed tail instead of a giant drained buffer.
            let mut combined = std::mem::take(&mut self.buffer);
            combined.push_str(data);
            // Re-scan from the last safe point (minus 1 to handle split CRLF).
            let scan_start = self.scanned_len.saturating_sub(1);
            let consumed = Self::process_source(
                &combined,
                scan_start,
                &mut self.bom_checked,
                &mut self.current,
                &mut self.has_data,
                self.max_event_data_bytes,
                &mut emit,
            );
            if consumed < combined.len() {
                // Build buffer fresh from truly unprocessed tail only (no duplication).
                self.buffer = combined[consumed..].to_string();
            }
            if self.buffer.len() > MAX_BUFFER_SIZE {
                self.reset_after_buffer_limit(&mut emit);
                return;
            }
        }
        // Whether we drained or not, the entire remaining buffer has been scanned.
        self.scanned_len = self.buffer.len();
    }

    /// Feed data to the parser and extract any complete events.
    ///
    /// Returns a vector of parsed events. Events are delimited by blank lines.
    pub fn feed(&mut self, data: &str) -> Vec<SseEvent> {
        let mut events = Vec::with_capacity(4);
        self.feed_into(data, |event| events.push(event));
        events
    }

    /// Check if the parser has any pending data.
    pub const fn has_pending(&self) -> bool {
        !self.buffer.is_empty() || self.has_data
    }

    /// Flush any pending event (called when stream ends).
    pub fn flush(&mut self) -> Option<SseEvent> {
        // First, process any remaining buffer content that doesn't end with newline
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            let line = line.trim_end_matches('\r');
            Self::process_line(
                line,
                &mut self.current,
                &mut self.has_data,
                self.max_event_data_bytes,
            );
        }

        if self.has_data {
            if self.current.data.ends_with('\n') {
                self.current.data.pop();
            }
            if self.current.event.is_empty() {
                self.current.event = Cow::Borrowed("message");
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
    pending_error: Option<std::io::Error>,
    pending_error_is_terminal: bool,
    terminated: bool,
    utf8_buffer: Vec<u8>,
}

impl<S> SseStream<S> {
    /// Create a new SSE stream from a byte stream.
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            parser: SseParser::new(),
            pending_events: VecDeque::new(),
            pending_error: None,
            pending_error_is_terminal: false,
            terminated: false,
            utf8_buffer: Vec::new(),
        }
    }
}

impl<S> SseStream<S>
where
    S: futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Unpin,
{
    #[inline]
    fn invalid_utf8_error() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Invalid UTF-8 in SSE stream",
        )
    }

    fn feed_parsed_chunk(parser: &mut SseParser, pending: &mut VecDeque<SseEvent>, s: &str) {
        parser.feed_into(s, |event| pending.push_back(event));
    }

    fn feed_to_pending(&mut self, s: &str) {
        Self::feed_parsed_chunk(&mut self.parser, &mut self.pending_events, s);
    }

    fn process_chunk_without_utf8_tail(&mut self, bytes: &[u8]) -> Result<(), std::io::Error> {
        let mut processed = 0;
        let mut first_error: Option<std::io::Error> = None;
        loop {
            match std::str::from_utf8(&bytes[processed..]) {
                Ok(s) => {
                    if !s.is_empty() {
                        self.feed_to_pending(s);
                    }
                    return first_error.map_or(Ok(()), Err);
                }
                Err(err) => {
                    let valid_len = err.valid_up_to();
                    if valid_len > 0 {
                        let s = std::str::from_utf8(&bytes[processed..processed + valid_len])
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                        self.feed_to_pending(s);
                        processed += valid_len;
                    }

                    if let Some(invalid_len) = err.error_len() {
                        processed += invalid_len;
                        if first_error.is_none() {
                            first_error = Some(Self::invalid_utf8_error());
                        }
                    } else {
                        self.utf8_buffer.extend_from_slice(&bytes[processed..]);
                        return first_error.map_or(Ok(()), Err);
                    }
                }
            }
        }
    }

    fn process_chunk_with_utf8_tail(&mut self, bytes: &[u8]) -> Result<(), std::io::Error> {
        self.utf8_buffer.extend_from_slice(bytes);
        let mut processed = 0;
        let mut first_error: Option<std::io::Error> = None;
        loop {
            match std::str::from_utf8(&self.utf8_buffer[processed..]) {
                Ok(s) => {
                    if !s.is_empty() {
                        Self::feed_parsed_chunk(&mut self.parser, &mut self.pending_events, s);
                    }
                    self.utf8_buffer.clear();
                    return first_error.map_or(Ok(()), Err);
                }
                Err(err) => {
                    let valid_len = err.valid_up_to();
                    if valid_len > 0 {
                        let s = std::str::from_utf8(
                            &self.utf8_buffer[processed..processed + valid_len],
                        )
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                        Self::feed_parsed_chunk(&mut self.parser, &mut self.pending_events, s);
                        processed += valid_len;
                    }

                    if let Some(invalid_len) = err.error_len() {
                        processed += invalid_len;
                        if first_error.is_none() {
                            first_error = Some(Self::invalid_utf8_error());
                        }
                    } else {
                        // Move remaining bytes to start of utf8_buffer
                        let remaining = self.utf8_buffer.len() - processed;
                        self.utf8_buffer.copy_within(processed.., 0);
                        self.utf8_buffer.truncate(remaining);
                        return first_error.map_or(Ok(()), Err);
                    }
                }
            }
        }
    }

    fn process_chunk(&mut self, bytes: &[u8]) -> Result<(), std::io::Error> {
        if self.utf8_buffer.is_empty() {
            self.process_chunk_without_utf8_tail(bytes)
        } else {
            self.process_chunk_with_utf8_tail(bytes)
        }
    }

    fn poll_stream_end(&mut self) -> Poll<Option<Result<SseEvent, std::io::Error>>> {
        if !self.utf8_buffer.is_empty() {
            // EOF with an incomplete UTF-8 tail is a terminal stream error.
            // Clear parser state so repeated polls don't emit the same error forever.
            self.utf8_buffer.clear();
            self.pending_events.clear();
            self.pending_error = None;
            self.parser = SseParser::new();
            self.terminated = true;
            return Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Stream ended with incomplete UTF-8 sequence",
            ))));
        }

        if let Some(event) = self.parser.flush() {
            self.terminated = true;
            return Poll::Ready(Some(Ok(event)));
        }
        self.terminated = true;
        Poll::Ready(None)
    }

    /// Poll for the next SSE event.
    pub fn poll_next_event(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<SseEvent, std::io::Error>>> {
        if let Some(event) = self.pending_events.pop_front() {
            return Poll::Ready(Some(Ok(event)));
        }
        if let Some(err) = self.pending_error.take() {
            if self.pending_error_is_terminal {
                self.pending_error_is_terminal = false;
                self.pending_events.clear();
                self.utf8_buffer.clear();
                self.parser = SseParser::new();
                self.terminated = true;
            }
            return Poll::Ready(Some(Err(err)));
        }
        if self.terminated {
            return Poll::Ready(None);
        }

        loop {
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    if let Err(err) = self.process_chunk(&bytes) {
                        if let Some(event) = self.pending_events.pop_front() {
                            self.pending_error = Some(err);
                            self.pending_error_is_terminal = true;
                            return Poll::Ready(Some(Ok(event)));
                        }
                        self.pending_events.clear();
                        self.utf8_buffer.clear();
                        self.parser = SseParser::new();
                        self.terminated = true;
                        return Poll::Ready(Some(Err(err)));
                    }

                    if let Some(event) = self.pending_events.pop_front() {
                        return Poll::Ready(Some(Ok(event)));
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => {
                    return self.poll_stream_end();
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
    use serde_json::json;
    use std::fmt::Write as _;
    use std::io::ErrorKind;
    use std::pin::Pin;

    #[derive(Debug, Clone)]
    struct GeneratedEvent {
        event: Option<String>,
        id: Option<String>,
        retry: Option<u32>,
        data: Vec<String>,
        comment: Option<String>,
    }

    #[derive(Debug, Clone, Copy)]
    enum LineEnding {
        Lf,
        Cr,
        CrLf,
    }

    impl LineEnding {
        fn as_str(self) -> &'static str {
            match self {
                Self::Lf => "\n",
                Self::Cr => "\r",
                Self::CrLf => "\r\n",
            }
        }
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

    fn line_ending_strategy() -> impl Strategy<Value = LineEnding> {
        prop_oneof![
            Just(LineEnding::Lf),
            Just(LineEnding::Cr),
            Just(LineEnding::CrLf),
        ]
    }

    fn unicode_line() -> impl Strategy<Value = String> {
        prop::collection::vec(
            any::<char>().prop_filter("no CR/LF", |c| *c != '\r' && *c != '\n'),
            0..24,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    fn id_field_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            4 => "[ -~]{0,24}".prop_map(|s| s),
            1 => ("[ -~]{0,12}", "[ -~]{0,12}").prop_map(|(head, tail)| format!("{head}\0{tail}")),
        ]
    }

    fn retry_field_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            6 => (0u64..=50_000u64).prop_map(|n| n.to_string()),
            2 => (u64::MAX - 10..=u64::MAX).prop_map(|n| n.to_string()),
            2 => "[a-zA-Z]{1,16}".prop_map(|s| s),
            1 => "-[0-9]{1,24}".prop_map(|s| s),
            1 => "\\+[0-9]{1,24}".prop_map(|s| s),
            1 => ((u128::from(u64::MAX) + 1)..=(u128::from(u64::MAX) + 50_000))
                .prop_map(|n| n.to_string()),
            1 => Just(String::new()),
        ]
    }

    fn oversized_data_len_strategy() -> impl Strategy<Value = usize> {
        // Keep average runtime reasonable while still exercising multi-megabyte inputs.
        prop_oneof![
            10 => 1024usize..=65_536usize,
            5 => 65_537usize..=262_144usize,
            2 => 262_145usize..=1_048_576usize,
            1 => 1_048_577usize..=3_145_728usize,
        ]
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

    fn render_stream_with_line_endings(
        events: &[GeneratedEvent],
        terminal_delimiter: bool,
        line_ending: LineEnding,
    ) -> String {
        let canonical = render_stream(events, terminal_delimiter);
        if matches!(line_ending, LineEnding::Lf) {
            canonical
        } else {
            canonical.replace('\n', line_ending.as_str())
        }
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

    fn split_bytes(input: &[u8], chunk_sizes: &[usize]) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        let mut start = 0usize;

        for &size in chunk_sizes {
            if start >= input.len() {
                break;
            }
            let end = (start + size).min(input.len());
            chunks.push(input[start..end].to_vec());
            start = end;
        }

        if start < input.len() {
            chunks.push(input[start..].to_vec());
        }

        chunks
    }

    fn parse_stream_chunks(chunks: Vec<Vec<u8>>) -> (Vec<SseEvent>, Vec<ErrorKind>) {
        let mut stream = SseStream::new(stream::iter(
            chunks.into_iter().map(Ok::<Vec<u8>, std::io::Error>),
        ));
        let mut events = Vec::new();
        let mut errors = Vec::new();

        futures::executor::block_on(async {
            while let Some(item) = stream.next().await {
                match item {
                    Ok(event) => events.push(event),
                    Err(err) => errors.push(err.kind()),
                }
            }
        });

        (events, errors)
    }

    fn parse_stream_chunks_limited(
        chunks: Vec<Vec<u8>>,
        max_items: usize,
    ) -> (Vec<SseEvent>, Vec<ErrorKind>) {
        let mut stream = SseStream::new(stream::iter(
            chunks.into_iter().map(Ok::<Vec<u8>, std::io::Error>),
        ));
        let mut events = Vec::new();
        let mut errors = Vec::new();

        futures::executor::block_on(async {
            for _ in 0..max_items {
                let Some(item) = stream.next().await else {
                    break;
                };
                match item {
                    Ok(event) => events.push(event),
                    Err(err) => errors.push(err.kind()),
                }
            }
        });

        (events, errors)
    }

    fn parse_stream_single_chunk(input: &[u8]) -> (Vec<SseEvent>, Vec<ErrorKind>) {
        parse_stream_chunks(vec![input.to_vec()])
    }

    fn parse_stream_chunked(
        input: &[u8],
        chunk_sizes: &[usize],
    ) -> (Vec<SseEvent>, Vec<ErrorKind>) {
        let chunks = split_bytes(input, chunk_sizes);
        parse_stream_chunks(chunks)
    }

    fn parse_stream_chunked_limited(
        input: &[u8],
        chunk_sizes: &[usize],
        max_items: usize,
    ) -> (Vec<SseEvent>, Vec<ErrorKind>) {
        let chunks = split_bytes(input, chunk_sizes);
        parse_stream_chunks_limited(chunks, max_items)
    }

    fn diag_json(
        fixture_id: &str,
        parser: &SseParser,
        input: &str,
        expected: &str,
        actual: &str,
    ) -> String {
        json!({
            "fixture_id": fixture_id,
            "seed": "deterministic-static",
            "env": {
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "cwd": std::env::current_dir().ok().map(|path| path.display().to_string()),
            },
            "input_preview": input,
            "parser_state": {
                "has_pending": parser.has_pending(),
            },
            "expected": expected,
            "actual": actual,
        })
        .to_string()
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
    fn test_last_event_id_persists_across_dispatched_events() {
        let mut parser = SseParser::new();
        let events = parser.feed("id: 123\ndata: first\n\ndata: second\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id.as_deref(), Some("123"));
        assert_eq!(events[1].id.as_deref(), Some("123"));
        assert_eq!(events[1].data, "second");
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
    fn test_buffer_no_duplication_on_straddle() {
        // Test that buffer content is not duplicated when data straddles event boundaries.
        // This exercises the slow-path scenario that previously had a buffer duplication bug.
        let mut parser = SseParser::new();

        // Feed partial event data that fills buffer but doesn't complete event
        let events = parser.feed("data: start");
        assert!(events.is_empty());
        assert!(parser.has_pending()); // Buffer should have partial data

        // Feed more data that triggers slow path processing and leaves partial data
        let events = parser.feed("_middle_incomplete");
        assert!(events.is_empty());
        assert!(parser.has_pending()); // Should still have buffered data

        // Complete the event - this should work without duplicated data
        let events = parser.feed("\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "start_middle_incomplete");

        // Verify buffer is clean for next event
        let events = parser.feed("data: second\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "second");
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
    fn test_retry_field_rejects_non_digit_prefix() {
        let mut parser = SseParser::new();
        let events = parser.feed("retry: +3000\ndata: test\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].retry, None);
    }

    #[test]
    fn test_retry_hint_persists_across_dispatched_events() {
        let mut parser = SseParser::new();
        let events = parser.feed("retry: 3000\ndata: first\n\ndata: second\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].retry, Some(3000));
        assert_eq!(events[1].retry, Some(3000));
    }

    #[test]
    fn test_append_data_line_enforces_projected_limit() {
        let mut current = SseEvent::default();
        let mut has_data = false;

        SseParser::append_data_line(&mut current, "ab", &mut has_data, 3);
        assert_eq!(current.data, "ab\n");
        assert!(has_data);

        SseParser::append_data_line(&mut current, "c", &mut has_data, 3);
        assert_eq!(current.data, "ab\n");
    }

    #[test]
    fn test_data_cap_single_oversized_line_via_feed() {
        // A single oversized data line should still emit an event boundary
        // (with empty data) rather than being silently dropped.
        let mut parser = SseParser::with_max_event_data_bytes(10);
        let events = parser.feed("data: this-is-longer-than-ten-bytes\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "");
    }

    #[test]
    fn test_data_cap_accumulation_via_feed() {
        // Multiple small data lines that collectively exceed the cap:
        // accepted lines are kept, the line that would push past the cap is rejected.
        let mut parser = SseParser::with_max_event_data_bytes(10);
        // "abc\n" = 4 bytes after first append
        let events = parser.feed("data: abc\ndata: def\ndata: ghi\n\n");
        assert_eq!(events.len(), 1);
        // "abc\n" (4) + "def\n" (4) = 8 bytes; "ghi\n" (4) would make 12 > 10, rejected.
        // Trailing newline stripped on emit → "abc\ndef"
        assert_eq!(events[0].data, "abc\ndef");
    }

    #[test]
    fn test_data_cap_exact_boundary_via_feed() {
        // Data that exactly reaches the cap should be accepted.
        let mut parser = SseParser::with_max_event_data_bytes(4);
        // "abc\n" = 4 bytes, exactly at cap
        let events = parser.feed("data: abc\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "abc");
    }

    #[test]
    fn test_data_cap_next_event_resets() {
        // After a capped event, the next event should start fresh.
        let mut parser = SseParser::with_max_event_data_bytes(6);
        let events = parser.feed("data: abcde\ndata: rejected\n\ndata: ok\n\n");
        assert_eq!(events.len(), 2);
        // First event: "abcde\n" = 6 bytes at cap; "rejected\n" would exceed → dropped.
        assert_eq!(events[0].data, "abcde");
        // Second event starts fresh with a clean data buffer.
        assert_eq!(events[1].data, "ok");
    }

    #[test]
    fn test_data_cap_chunked_delivery() {
        // Cap enforcement must work even when data arrives in small chunks.
        let mut parser = SseParser::with_max_event_data_bytes(10);
        parser.feed("data: abc\n");
        parser.feed("data: def\n");
        // "abc\n" (4) + "def\n" (4) = 8; "toolong\n" (8) would make 16 > 10
        let events = parser.feed("data: toolong\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "abc\ndef");
    }

    #[test]
    fn test_data_cap_flush_path() {
        // Cap must also be enforced when the stream ends without a trailing blank line.
        let mut parser = SseParser::with_max_event_data_bytes(6);
        parser.feed("data: abcde\n");
        parser.feed("data: no\n");
        // "abcde\n" = 6 bytes at cap; "no\n" (3) would make 9 > 6 → rejected.
        let event = parser.flush().expect("should flush pending event");
        assert_eq!(event.data, "abcde");
    }

    #[test]
    fn test_keep_alive_comment_does_not_emit_event() {
        let mut parser = SseParser::new();
        let events = parser.feed(": keepalive\n\n");
        assert!(events.is_empty());
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
    fn test_event_without_data_is_ignored() {
        let mut parser = SseParser::new();
        let events = parser.feed("event: ping\n\n");
        assert!(
            events.is_empty(),
            "event block without data should not emit an event"
        );
    }

    #[test]
    fn test_unknown_field_is_ignored() {
        let mut parser = SseParser::new();
        let events = parser.feed("foo: bar\ndata: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
        assert_eq!(events[0].event, "message");
    }

    #[test]
    fn test_error_event_parsing() {
        let mut parser = SseParser::new();
        let events = parser.feed("event: error\ndata: {\"message\":\"boom\"}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "error");
        assert_eq!(events[0].data, "{\"message\":\"boom\"}");
    }

    #[test]
    fn test_empty_event_field_defaults_to_message() {
        let mut parser = SseParser::new();
        let input = "event\ndata: hello\n\n";
        let events = parser.feed(input);
        let diag = diag_json(
            "sse-empty-event-field-default",
            &parser,
            input,
            r#"{"event":"message","data":"hello"}"#,
            &format!("{events:?}"),
        );

        assert_eq!(events.len(), 1, "{diag}");
        assert_eq!(events[0].event, "message", "{diag}");
        assert_eq!(events[0].data, "hello", "{diag}");
    }

    #[test]
    fn test_large_payload_event() {
        let mut parser = SseParser::new();
        let payload = "x".repeat(128 * 1024);
        let input = format!("data: {payload}\n\n");
        let events = parser.feed(&input);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data.len(), payload.len());
        assert_eq!(events[0].data, payload);
    }

    #[test]
    fn test_buffer_limit_overflow_resets_parser_state() {
        let mut parser = SseParser::new();
        assert!(parser.feed("data: stale\n").is_empty());

        let oversized = "x".repeat(10 * 1024 * 1024 + 1);
        let overflow_events = parser.feed(&oversized);
        assert_eq!(overflow_events.len(), 1);
        assert_eq!(overflow_events[0].event, "error");
        assert_eq!(overflow_events[0].data, "SSE buffer limit exceeded");

        assert!(!parser.has_pending());
        assert!(parser.buffer.capacity() < 1024);
        assert!(parser.flush().is_none());

        let fresh = parser.feed("data: fresh\n\n");
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].data, "fresh");
    }

    #[test]
    fn test_large_complete_chunk_does_not_trip_buffer_limit_fast_path() {
        let mut parser = SseParser::new();
        let payload = "x".repeat(10 * 1024 * 1024 + 1);
        let events = parser.feed(&format!("data: {payload}\n\n"));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "message");
        assert_eq!(events[0].data.len(), payload.len());
        assert_eq!(events[0].data, payload);
        assert!(!parser.has_pending());
        assert!(parser.buffer.capacity() < 1024);
        assert!(parser.flush().is_none());
    }

    #[test]
    fn test_large_complete_chunk_does_not_trip_buffer_limit_with_buffered_prefix() {
        let mut parser = SseParser::new();
        assert!(parser.feed("data: ").is_empty());

        let payload = "x".repeat(10 * 1024 * 1024 + 1);
        let events = parser.feed(&format!("{payload}\n\n"));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "message");
        assert_eq!(events[0].data.len(), payload.len());
        assert_eq!(events[0].data, payload);
        assert!(!parser.has_pending());
        assert!(parser.buffer.capacity() < 1024);
        assert!(parser.flush().is_none());
    }

    #[test]
    fn test_rapid_sequential_events() {
        let mut parser = SseParser::new();
        let mut input = String::new();
        for i in 0..200 {
            let _ = write!(&mut input, "event: e{i}\ndata: payload{i}\n\n");
        }
        let events = parser.feed(&input);
        assert_eq!(events.len(), 200);
        assert_eq!(events[0].event, "e0");
        assert_eq!(events[0].data, "payload0");
        assert_eq!(events[199].event, "e199");
        assert_eq!(events[199].data, "payload199");
    }

    #[test]
    fn test_stream_event_name_matrix() {
        let names = [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
            "message",
            "error",
            "ping",
            "response.created",
            "response.output_text.delta",
            "response.completed",
        ];

        let mut parser = SseParser::new();
        let mut input = String::new();
        for name in names {
            let _ = write!(&mut input, "event: {name}\ndata: {{}}\n\n");
        }

        let events = parser.feed(&input);
        assert_eq!(events.len(), names.len());
        for (idx, name) in names.iter().enumerate() {
            assert_eq!(events[idx].event, *name);
            assert_eq!(events[idx].data, "{}");
        }
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
    fn test_stream_handles_crlf_split_across_partial_frames() {
        let chunks = vec![
            Ok(b"data: first\r".to_vec()),
            Ok(b"\n".to_vec()),
            Ok(b"\r".to_vec()),
            Ok(b"\n".to_vec()),
        ];
        let mut stream = SseStream::new(stream::iter(chunks));

        futures::executor::block_on(async {
            let first = stream.next().await.expect("first event").expect("ok");
            let diag = json!({
                "fixture_id": "sse-crlf-split-across-chunks",
                "seed": "deterministic-static",
                "expected": {"event": "message", "data": "first"},
                "actual": {"event": first.event, "data": first.data},
            })
            .to_string();
            assert_eq!(first.data, "first", "{diag}");
            assert!(stream.next().await.is_none(), "{diag}");
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
    fn test_stream_flush_eof_is_terminal_for_repeated_polls() {
        let inner: Pin<Box<dyn futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Send>> =
            Box::pin(stream::unfold(
                Some(b"data: last".to_vec()),
                |state| async { state.map(|bytes| (Ok(bytes), None)) },
            ));
        let mut stream = SseStream::new(inner);

        futures::executor::block_on(async {
            let event = stream.next().await.expect("event").expect("ok");
            assert_eq!(event.data, "last");
            assert!(stream.next().await.is_none());
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
            assert!(
                stream.next().await.is_none(),
                "incomplete UTF-8 at EOF should produce a terminal error"
            );
        });
    }

    #[test]
    fn test_stream_surfaces_pending_event_before_utf8_error() {
        // Input: "data: ok\n\ndata: \xFF\n\n"
        // The parser feeds valid prefix "data: ok\n\ndata: " → emits event("ok"),
        // then recovers remainder "\n\n" after the 0xFF → completes partial "data: "
        // → emits event(""). All pending events drain before the error.
        let chunks = vec![Ok(b"data: ok\n\ndata: \xFF\n\n".to_vec())];
        let mut stream = SseStream::new(stream::iter(chunks));

        futures::executor::block_on(async {
            let first = stream.next().await.expect("first item").expect("first ok");
            let diag = json!({
                "fixture_id": "sse-valid-event-before-invalid-utf8",
                "seed": "deterministic-static",
                "expected_sequence": ["Ok(data=ok)", "Ok(data=)", "Err(invalid utf8)"],
                "actual_first": {"event": first.event, "data": first.data},
            })
            .to_string();
            assert_eq!(first.data, "ok", "{diag}");

            // The recovered remainder "\n\n" completes the partial "data: " line,
            // producing an empty-data event before the error surfaces.
            let second = stream
                .next()
                .await
                .expect("second item")
                .expect("second ok");
            assert_eq!(second.data, "", "{diag}");

            let err = stream
                .next()
                .await
                .expect("third item")
                .expect_err("third should be utf8 error");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{diag}");
        });
    }

    #[test]
    fn test_stream_resumes_parsing_remainder_after_utf8_error() {
        // "data: ok\n\n" (valid) + 0xFF (invalid) + "data: after\n\n" (valid)
        // Sent in one chunk.
        // The recovery code feeds remainder "data: after\n\n" to pending_events
        // before the error is stored, so events drain first:
        // Expect: Ok(ok), Ok(after), Err(invalid)

        let mut bytes = b"data: ok\n\n".to_vec();
        bytes.push(0xFF);
        bytes.extend_from_slice(b"data: after\n\n");

        let mut stream = SseStream::new(stream::iter(vec![Ok(bytes)]));

        futures::executor::block_on(async {
            // 1. "ok" — from valid prefix before 0xFF
            let first = stream.next().await.expect("1").expect("ok");
            assert_eq!(first.data, "ok");

            // 2. "after" — recovered from remainder after 0xFF (pending events drain first)
            let second = stream.next().await.expect("2").expect("after");
            assert_eq!(second.data, "after");

            // 3. Error — surfaces after all pending events are delivered
            let err = stream.next().await.expect("3").expect_err("error");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        });
    }

    #[test]
    fn test_stream_does_not_flush_partial_tail_after_utf8_error() {
        let mut bytes = b"data: ok\n\n".to_vec();
        bytes.push(0xFF);
        bytes.extend_from_slice(b"data: partial");

        let mut stream = SseStream::new(stream::iter(vec![Ok(bytes)]));

        futures::executor::block_on(async {
            let first = stream.next().await.expect("1").expect("ok");
            assert_eq!(first.data, "ok");

            let err = stream.next().await.expect("2").expect_err("error");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
            assert!(
                stream.next().await.is_none(),
                "utf-8 parse errors should terminate the stream without flushing a partial tail"
            );
        });
    }

    #[test]
    fn test_bom_stripping_with_preceding_empty_chunk() {
        let mut parser = SseParser::new();
        // Feed empty chunk first - should not mark BOM as checked
        let events = parser.feed("");
        assert!(events.is_empty());

        // Feed content with BOM
        let events = parser.feed("\u{FEFF}data: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
        // Ensure the BOM didn't end up in the field name (causing it to be ignored)
        assert_eq!(events[0].event, "message");
    }

    /// Regression test for fuzz crash artifact
    /// (crash-28de6b0685a7989f4c24ea8feea29aaa57d50053).
    ///
    /// Verifies the chunking invariant: feeding data whole, char-by-char, and
    /// split at midpoint must produce identical events and flush results.
    #[test]
    fn test_fuzz_regression_crash_28de6b() {
        let data: &[u8] = &[
            0x64, 0x3d, 0x74, 0x61, 0x3a, 0x20, 0x6c, 0x69, 0x6e, 0x65, 0x31, 0x0a, 0x5a, 0x61,
            0x74, 0x61, 0x3a, 0x20, 0x6c, 0x69, 0x6e, 0x65, 0x32, 0x0a, 0x64, 0x61, 0x74, 0x61,
            0x3a, 0x20, 0x6c, 0x9f, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd,
            0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0x28, 0xcd, 0xcd, 0xa1,
            0xa1, 0xa1, 0xa1, 0xa1, 0xa1, 0xa1, 0xa1, 0xa1, 0xa1, 0xa1, 0xa1, 0xa1, 0xa1, 0xa1,
            0xa1, 0xa1, 0xa1, 0xa1, 0xa1, 0xa1, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd,
            0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0x82, 0x82, 0x82, 0x82, 0x82, 0x82, 0x82,
            0x82, 0x82, 0x82, 0x82, 0x82, 0x82, 0x82, 0x82, 0x82, 0x82, 0x82, 0x40, 0x82, 0xcd,
            0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0x91,
            0x9a, 0x93, 0x69, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00,
        ];
        let input = String::from_utf8_lossy(data);

        // Whole feed
        let mut parser_whole = SseParser::new();
        let events_whole = parser_whole.feed(&input);
        let flush_whole = parser_whole.flush();

        // Char-by-char
        let mut parser_char = SseParser::new();
        let mut events_char = Vec::new();
        for ch in input.chars() {
            let mut buf = [0u8; 4];
            events_char.extend(parser_char.feed(ch.encode_utf8(&mut buf)));
        }
        let flush_char = parser_char.flush();

        // Split at midpoint
        let mid = input.len() / 2;
        let mut split_at = mid;
        while !input.is_char_boundary(split_at) && split_at < input.len() {
            split_at += 1;
        }
        let (part1, part2) = input.split_at(split_at);
        let mut parser_split = SseParser::new();
        let mut events_split: Vec<_> = parser_split.feed(part1);
        events_split.extend(parser_split.feed(part2));
        let flush_split = parser_split.flush();

        assert_eq!(events_whole, events_split, "whole vs split events");
        assert_eq!(flush_whole, flush_split, "whole vs split flush");
        assert_eq!(events_whole, events_char, "whole vs char events");
        assert_eq!(flush_whole, flush_char, "whole vs char flush");
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
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

        #[test]
        fn sse_line_ending_chunking_invariant(
            events in prop::collection::vec(event_strategy(), 1..10),
            chunk_sizes in prop::collection::vec(1usize..32, 0..20),
            terminal_delimiter in any::<bool>(),
            line_ending in line_ending_strategy(),
        ) {
            let input = render_stream_with_line_endings(&events, terminal_delimiter, line_ending);
            let expected = parse_all(&input);
            let actual = parse_chunked(&input, &chunk_sizes);
            prop_assert_eq!(actual, expected);
        }

        #[test]
        fn sse_id_with_null_bytes_is_rejected(
            id in id_field_strategy(),
            data in ascii_line(),
        ) {
            let input = format!("id: {id}\ndata: {data}\n\n");
            let events = parse_all(&input);

            prop_assert_eq!(events.len(), 1);
            let expected_id = if id.contains('\0') { None } else { Some(id) };
            prop_assert_eq!(events[0].id.as_ref(), expected_id.as_ref());
        }

        #[test]
        fn sse_retry_field_fuzz_last_assignment_wins(
            retry_values in prop::collection::vec(retry_field_strategy(), 1..6),
            data in ascii_line(),
        ) {
            let mut input = String::new();
            for value in &retry_values {
                let _ = writeln!(&mut input, "retry: {value}");
            }
            let _ = writeln!(&mut input, "data: {data}");
            input.push('\n');

            let events = parse_all(&input);
            prop_assert_eq!(events.len(), 1);

            let expected_retry = retry_values.last().and_then(|value| {
                if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
                    value.parse::<u64>().ok()
                } else {
                    None
                }
            });
            prop_assert_eq!(events[0].retry, expected_retry);
        }

        #[test]
        fn sse_duplicate_fields_apply_spec_semantics(
            event_names in prop::collection::vec("[a-z_]{1,12}", 1..6),
            ids in prop::collection::vec(id_field_strategy(), 0..6),
            data_lines in prop::collection::vec(ascii_line(), 1..6),
        ) {
            let mut input = String::new();

            for event_name in &event_names {
                let _ = writeln!(&mut input, "event: {event_name}");
            }
            for id in &ids {
                let _ = writeln!(&mut input, "id: {id}");
            }
            for line in &data_lines {
                let _ = writeln!(&mut input, "data: {line}");
            }
            input.push('\n');

            let events = parse_all(&input);
            prop_assert_eq!(events.len(), 1);
            prop_assert_eq!(events[0].event.as_ref(), event_names.last().expect("non-empty"));

            let expected_id = ids.iter().rfind(|id| !id.contains('\0')).cloned();
            prop_assert_eq!(events[0].id.as_ref(), expected_id.as_ref());

            let expected_data = data_lines.join("\n");
            prop_assert_eq!(events[0].data.as_str(), expected_data);
        }

        #[test]
        fn sse_oversized_data_fields_round_trip(len in oversized_data_len_strategy(),) {
            let payload = "x".repeat(len);
            let input = format!("data: {payload}\n\n");
            let events = parse_all(&input);

            prop_assert_eq!(events.len(), 1);
            prop_assert_eq!(events[0].data.len(), len);
            prop_assert_eq!(events[0].data.as_str(), payload);
        }

        #[test]
        fn sse_stream_utf8_valid_input_chunking_invariant(
            lines in prop::collection::vec(unicode_line(), 1..5),
            chunk_sizes in prop::collection::vec(1usize..16, 0..24),
        ) {
            let mut input = String::new();
            for line in &lines {
                let _ = writeln!(&mut input, "data: {line}");
            }
            input.push('\n');

            let (single_events, single_errors) = parse_stream_single_chunk(input.as_bytes());
            let (chunked_events, chunked_errors) =
                parse_stream_chunked(input.as_bytes(), &chunk_sizes);

            prop_assert!(single_errors.is_empty(), "single-chunk had UTF-8 errors");
            prop_assert!(chunked_errors.is_empty(), "chunked parse had UTF-8 errors");
            prop_assert_eq!(chunked_events, single_events);
        }

        #[test]
        fn sse_stream_bom_start_stripped_embedded_preserved(
            left in unicode_line(),
            right in unicode_line(),
            chunk_sizes in prop::collection::vec(1usize..8, 0..24),
        ) {
            let start_bom = format!("\u{FEFF}data: {left}{right}\n\n");
            let (start_events, start_errors) =
                parse_stream_chunked(start_bom.as_bytes(), &chunk_sizes);
            prop_assert!(start_errors.is_empty(), "start BOM should be valid UTF-8");
            prop_assert_eq!(start_events.len(), 1);
            let expected_start = format!("{left}{right}");
            prop_assert_eq!(start_events[0].data.as_str(), expected_start);

            let embedded_bom = format!("data: {left}\u{FEFF}{right}\n\n");
            let (embedded_events, embedded_errors) =
                parse_stream_chunked(embedded_bom.as_bytes(), &chunk_sizes);
            prop_assert!(embedded_errors.is_empty(), "embedded BOM should be preserved");
            prop_assert_eq!(embedded_events.len(), 1);
            let expected_embedded = format!("{left}\u{FEFF}{right}");
            prop_assert_eq!(embedded_events[0].data.as_str(), expected_embedded);
        }

        #[test]
        fn sse_stream_invalid_utf8_yields_invalid_data_errors(
            prefix in ascii_line(),
            suffix in ascii_line(),
            invalid_len in 1usize..4,
            chunk_sizes in prop::collection::vec(1usize..8, 0..20),
        ) {
            let mut bytes = format!("data: {prefix}\n\n").into_bytes();
            bytes.extend(std::iter::repeat_n(0xFFu8, invalid_len));
            bytes.extend(format!("data: {suffix}\n\n").as_bytes());

            let (events, errors) = parse_stream_chunked_limited(&bytes, &chunk_sizes, 32);
            prop_assert!(
                events.iter().any(|event| event.data == prefix),
                "event before invalid sequence should still be surfaced"
            );
            prop_assert!(!errors.is_empty(), "invalid UTF-8 should emit at least one error");
            prop_assert!(
                errors.iter().all(|kind| *kind == ErrorKind::InvalidData),
                "all stream decoding errors must be InvalidData"
            );
        }
    }
}
