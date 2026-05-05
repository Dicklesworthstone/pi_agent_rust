//! c137-blink broker client for lifecycle events and inbound steering hints.
//!
//! The client is intentionally best-effort: when `C137_BLINK_BROKER` is unset,
//! unreachable, or restarted, Pi continues normally and blink events may be
//! dropped. The broker wire protocol is newline-delimited JSON.

use crate::model::{AssistantMessage, ContentBlock, Message, UserContent, UserMessage};
use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use asupersync::net::unix::UnixStream;
use asupersync::runtime::RuntimeHandle;
use chrono::Utc;
use crossbeam_queue::ArrayQueue;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

pub const PLATFORM_SOURCE: &str = "c137-pi-rust";
const ENV_BROKER: &str = "C137_BLINK_BROKER";
const EVENT_QUEUE_CAPACITY: usize = 1024;
const HINT_QUEUE_CAPACITY: usize = 256;
const READ_BUF_SIZE: usize = 4096;
const BACKOFF_MS: [u64; 5] = [1_000, 2_000, 4_000, 5_000, 5_000];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloFields {
    pub session_id: String,
    pub project: String,
    pub cwd: String,
    pub platform_source: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BlinkEvent {
    SessionStart { cwd: String },
    AgentStart { prompt_size: usize },
    ToolExecutionStart {
        tool: String,
        tool_call_id: Option<String>,
        input_size: usize,
    },
    ToolResult {
        tool: String,
        tool_call_id: Option<String>,
        input_size: usize,
        response_size: usize,
    },
    AgentEnd {
        duration_ms: u64,
        last_assistant_size: usize,
    },
    SessionShutdown,
    Status { message: String },
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Hint {
    pub message: String,
    pub source: Option<String>,
    pub ts: u64,
}

#[derive(Debug, Deserialize)]
struct RawHint {
    event: String,
    message: Option<String>,
    source: Option<String>,
    ts: Option<u64>,
    session_id: Option<String>,
}

#[derive(Clone)]
pub struct BlinkClient {
    event_queue: Arc<ArrayQueue<BlinkEvent>>,
    hint_rx: Arc<std::sync::Mutex<mpsc::Receiver<Hint>>>,
    shutdown: Arc<AtomicBool>,
}

impl BlinkClient {
    /// Reads `C137_BLINK_BROKER`; returns `None` if unset or empty.
    ///
    /// The background task is best-effort and silently reconnects/drops events
    /// when the broker is unavailable.
    pub fn start(hello: HelloFields, runtime_handle: RuntimeHandle) -> Option<Self> {
        let broker = env::var_os(ENV_BROKER).and_then(|value| {
            let path = PathBuf::from(value);
            if path.as_os_str().is_empty() {
                None
            } else {
                Some(path)
            }
        })?;
        Some(Self::start_with_path(hello, broker, runtime_handle))
    }

    pub fn start_with_path(hello: HelloFields, broker: PathBuf, runtime_handle: RuntimeHandle) -> Self {
        let event_queue = Arc::new(ArrayQueue::new(EVENT_QUEUE_CAPACITY));
        let (hint_tx, hint_rx) = mpsc::sync_channel(HINT_QUEUE_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let task_queue = Arc::clone(&event_queue);
        let task_shutdown = Arc::clone(&shutdown);
        runtime_handle.spawn(async move {
            run_connection_task(broker, hello, task_queue, hint_tx, task_shutdown).await;
        });

        Self {
            event_queue,
            hint_rx: Arc::new(std::sync::Mutex::new(hint_rx)),
            shutdown,
        }
    }

    /// Best-effort. No-op if the bounded queue is full or shutting down.
    pub fn emit(&self, event: BlinkEvent) {
        if self.shutdown.load(Ordering::Relaxed) {
            return;
        }
        let _ = self.event_queue.push(event);
    }

    /// Drain any currently available inbound hints without blocking.
    pub fn drain_hints(&self) -> Vec<Hint> {
        let Ok(rx) = self.hint_rx.lock() else {
            return Vec::new();
        };
        let mut hints = Vec::new();
        while let Ok(hint) = rx.try_recv() {
            hints.push(hint);
        }
        hints
    }

    /// Compatibility hook for callers that want direct receiver ownership.
    ///
    /// This v0 client keeps the receiver internally so it can be registered as
    /// an agent message fetcher; direct subscription returns an empty receiver.
    pub fn subscribe_hints(&self) -> mpsc::Receiver<Hint> {
        let (_tx, rx) = mpsc::channel();
        rx
    }

    /// Clean shutdown: enqueue `SessionShutdown` and mark background task done.
    pub async fn shutdown(self) {
        self.emit(BlinkEvent::SessionShutdown);
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// One-shot wire-up helper: build hello fields, start the broker client,
/// emit `SessionStart`, and return the client ready to hand to
/// `Agent::set_blink_client`. Returns `None` when `C137_BLINK_BROKER` is
/// unset (the no-broker case is silent by design).
pub fn start_session(cwd: &Path, runtime_handle: RuntimeHandle) -> Option<Arc<BlinkClient>> {
    let hello = make_hello_fields(cwd);
    let cwd_str = hello.cwd.clone();
    let client = BlinkClient::start(hello, runtime_handle)?;
    client.emit(BlinkEvent::SessionStart { cwd: cwd_str });
    Some(Arc::new(client))
}

pub fn make_hello_fields(cwd: &Path) -> HelloFields {
    let project = derive_project_name(cwd);
    let ts = now_ms();
    let pid = std::process::id();
    HelloFields {
        session_id: format!("c137blink-pi-rust-{project}-{ts}-{pid}"),
        project,
        cwd: cwd.display().to_string(),
        platform_source: PLATFORM_SOURCE.to_string(),
    }
}

pub fn derive_project_name(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

/// Bytes of the most recent user message's text content. Returns 0 if no user
/// message is present. Per c137-blink spec: `prompt_size` on `agent_start`.
pub fn prompt_size_bytes(messages: &[Message]) -> usize {
    for msg in messages.iter().rev() {
        if let Message::User(UserMessage { content, .. }) = msg {
            return match content {
                UserContent::Text(text) => text.len(),
                UserContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text(t) => t.text.len(),
                        _ => 0,
                    })
                    .sum(),
            };
        }
    }
    0
}

/// Bytes of an assistant message's text content (sum of all text blocks).
/// Per c137-blink spec: `last_assistant_size` on `agent_end`.
pub fn assistant_text_size(message: &AssistantMessage) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(t) => t.text.len(),
            _ => 0,
        })
        .sum()
}

pub fn hint_to_steering_message(hint: &Hint) -> Message {
    let prefix = hint.source.as_ref().map_or_else(
        || "[orchestrator hint]".to_string(),
        |source| format!("[orchestrator hint from {source}]"),
    );
    Message::User(UserMessage {
        content: UserContent::Text(format!("{prefix} {}", hint.message)),
        timestamp: i64::try_from(hint.ts).unwrap_or(i64::MAX),
    })
}

pub fn encode_hello(fields: &HelloFields, ts: u64) -> String {
    json!({
        "event": "hello",
        "session_id": fields.session_id,
        "project": fields.project,
        "cwd": fields.cwd,
        "platform_source": fields.platform_source,
        "ts": ts,
    })
    .to_string()
}

pub fn encode_event(fields: &HelloFields, event: &BlinkEvent, ts: u64) -> String {
    let mut value = serde_json::to_value(event).unwrap_or_else(|_| json!({"event":"status","message":"encode failed"}));
    let obj = value.as_object_mut().expect("BlinkEvent serializes to object");
    insert_front(obj, "platform_source", Value::String(fields.platform_source.clone()));
    insert_front(obj, "project", Value::String(fields.project.clone()));
    insert_front(obj, "session_id", Value::String(fields.session_id.clone()));
    insert_front(obj, "ts", Value::Number(ts.into()));
    serde_json::to_string(&value).expect("blink event JSON serialization cannot fail")
}

pub fn decode_hint_line(line: &str, session_id: &str) -> Option<Hint> {
    let raw: RawHint = serde_json::from_str(line).ok()?;
    if raw.event != "hint" {
        return None;
    }
    if raw.session_id.as_deref().is_some_and(|id| id != session_id) {
        return None;
    }
    Some(Hint {
        message: raw.message?,
        source: raw.source,
        ts: raw.ts.unwrap_or_else(now_ms),
    })
}

pub fn decode_hint_chunks(chunks: &[&[u8]], session_id: &str) -> Vec<Hint> {
    let mut decoder = NdjsonHintDecoder::new(session_id.to_string());
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(decoder.push(chunk));
    }
    out
}

struct NdjsonHintDecoder {
    session_id: String,
    buffer: Vec<u8>,
}

impl NdjsonHintDecoder {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            buffer: Vec::new(),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Vec<Hint> {
        self.buffer.extend_from_slice(chunk);
        let mut hints = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=pos).collect::<Vec<_>>();
            if matches!(line.last(), Some(b'\n')) {
                line.pop();
            }
            if matches!(line.last(), Some(b'\r')) {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&line) else {
                continue;
            };
            if let Some(hint) = decode_hint_line(text, &self.session_id) {
                hints.push(hint);
            }
        }
        hints
    }
}

async fn run_connection_task(
    broker: PathBuf,
    hello: HelloFields,
    event_queue: Arc<ArrayQueue<BlinkEvent>>,
    hint_tx: mpsc::SyncSender<Hint>,
    shutdown: Arc<AtomicBool>,
) {
    let mut state = ConnectionState::Disconnected;
    let mut backoff_index = 0usize;
    while !shutdown.load(Ordering::Relaxed) {
        state = match state {
            ConnectionState::Disconnected | ConnectionState::Reconnecting => ConnectionState::Connecting,
            other => other,
        };
        let stream = UnixStream::connect(&broker).await;
        match stream {
            Ok(stream) => {
                state = ConnectionState::Connected;
                backoff_index = 0;
                if run_connected(stream, &hello, &event_queue, &hint_tx, &shutdown)
                    .await
                    .is_err()
                {
                    state = ConnectionState::Reconnecting;
                }
            }
            Err(_) => {
                state = ConnectionState::Reconnecting;
            }
        }
        if matches!(state, ConnectionState::Reconnecting | ConnectionState::Connecting)
            && !shutdown.load(Ordering::Relaxed)
        {
            let delay_ms = BACKOFF_MS[backoff_index.min(BACKOFF_MS.len() - 1)];
            backoff_index = backoff_index.saturating_add(1);
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(delay_ms)).await;
        }
    }
}

async fn run_connected(
    stream: UnixStream,
    hello: &HelloFields,
    event_queue: &Arc<ArrayQueue<BlinkEvent>>,
    hint_tx: &mpsc::SyncSender<Hint>,
    shutdown: &AtomicBool,
) -> std::io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    write_ndjson(&mut writer, &encode_hello(hello, now_ms())).await?;
    let mut pending = VecDeque::new();
    let mut decoder = NdjsonHintDecoder::new(hello.session_id.clone());
    let mut read_buf = [0u8; READ_BUF_SIZE];

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        while let Some(event) = event_queue.pop() {
            pending.push_back(event);
        }
        while let Some(event) = pending.pop_front() {
            write_ndjson(&mut writer, &encode_event(hello, &event, now_ms())).await?;
        }

        match reader.read(&mut read_buf).await {
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "blink broker closed")),
            Ok(n) => {
                for hint in decoder.push(&read_buf[..n]) {
                    let _ = hint_tx.try_send(hint);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) => return Err(err),
        }
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(25)).await;
    }
}

async fn write_ndjson<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

fn insert_front(obj: &mut Map<String, Value>, key: &str, value: Value) {
    let existing = std::mem::take(obj);
    obj.insert(key.to_string(), value);
    obj.extend(existing);
}

fn now_ms() -> u64 {
    Utc::now().timestamp_millis().try_into().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;

    fn hello() -> HelloFields {
        HelloFields {
            session_id: "c137blink-pi-rust-demo-123-7".to_string(),
            project: "demo".to_string(),
            cwd: "/tmp/demo".to_string(),
            platform_source: PLATFORM_SOURCE.to_string(),
        }
    }

    #[test]
    fn encoder_produces_canonical_ndjson_shapes_for_events() {
        let fields = hello();
        assert_eq!(
            encode_hello(&fields, 1),
            r#"{"cwd":"/tmp/demo","event":"hello","platform_source":"c137-pi-rust","project":"demo","session_id":"c137blink-pi-rust-demo-123-7","ts":1}"#
        );

        let cases = [
            (
                BlinkEvent::SessionStart { cwd: "/tmp/demo".to_string() },
                r#"{"cwd":"/tmp/demo","event":"session_start","platform_source":"c137-pi-rust","project":"demo","session_id":"c137blink-pi-rust-demo-123-7","ts":2}"#,
            ),
            (
                BlinkEvent::AgentStart { prompt_size: 5 },
                r#"{"event":"agent_start","platform_source":"c137-pi-rust","project":"demo","prompt_size":5,"session_id":"c137blink-pi-rust-demo-123-7","ts":2}"#,
            ),
            (
                BlinkEvent::ToolExecutionStart { tool: "read".to_string(), tool_call_id: Some("call-1".to_string()), input_size: 17 },
                r#"{"event":"tool_execution_start","input_size":17,"platform_source":"c137-pi-rust","project":"demo","session_id":"c137blink-pi-rust-demo-123-7","tool":"read","tool_call_id":"call-1","ts":2}"#,
            ),
            (
                BlinkEvent::ToolResult { tool: "read".to_string(), tool_call_id: None, input_size: 17, response_size: 23 },
                r#"{"event":"tool_result","input_size":17,"platform_source":"c137-pi-rust","project":"demo","response_size":23,"session_id":"c137blink-pi-rust-demo-123-7","tool":"read","tool_call_id":null,"ts":2}"#,
            ),
            (
                BlinkEvent::AgentEnd { duration_ms: 42, last_assistant_size: 9 },
                r#"{"duration_ms":42,"event":"agent_end","last_assistant_size":9,"platform_source":"c137-pi-rust","project":"demo","session_id":"c137blink-pi-rust-demo-123-7","ts":2}"#,
            ),
            (
                BlinkEvent::SessionShutdown,
                r#"{"event":"session_shutdown","platform_source":"c137-pi-rust","project":"demo","session_id":"c137blink-pi-rust-demo-123-7","ts":2}"#,
            ),
            (
                BlinkEvent::Status { message: "ok".to_string() },
                r#"{"event":"status","message":"ok","platform_source":"c137-pi-rust","project":"demo","session_id":"c137blink-pi-rust-demo-123-7","ts":2}"#,
            ),
        ];

        for (event, expected) in cases {
            assert_eq!(encode_event(&fields, &event, 2), expected);
        }
    }

    #[test]
    fn decoder_handles_partial_reads_and_malformed_lines() {
        let chunks = [
            br#"{"event":"hint","message":"he"#.as_slice(),
            br#"llo","source":"ctl","ts":7,"session_id":"c137blink-pi-rust-demo-123-7"}
not-json
{"event":"status"}
{"event":"hint","message":"ignored","ts":8,"session_id":"other"}
{"event":"hint","message":"later","ts":9}
"#.as_slice(),
        ];
        let hints = decode_hint_chunks(&chunks, "c137blink-pi-rust-demo-123-7");
        assert_eq!(
            hints,
            vec![
                Hint { message: "hello".to_string(), source: Some("ctl".to_string()), ts: 7 },
                Hint { message: "later".to_string(), source: None, ts: 9 },
            ]
        );
    }

    #[test]
    fn hello_is_first_frame_after_connect() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let (client, mut broker) = UnixStream::pair().expect("socket pair");
            let fields = hello();
            let queue = Arc::new(ArrayQueue::new(4));
            queue.push(BlinkEvent::SessionStart { cwd: "/tmp/demo".to_string() }).expect("queue event");
            let (hint_tx, _hint_rx) = mpsc::sync_channel(4);
            let shutdown = Arc::new(AtomicBool::new(false));
            let task_shutdown = Arc::clone(&shutdown);

            let join = runtime.handle().spawn(async move {
                run_connected(client, &fields, &queue, &hint_tx, &task_shutdown).await
            });

            let mut buf = [0u8; 512];
            let n = broker.read(&mut buf).await.expect("read frames");
            let text = std::str::from_utf8(&buf[..n]).expect("utf8");
            assert!(text.lines().next().expect("first line").contains(r#""event":"hello""#));
            assert!(text.find(r#""event":"hello""#) < text.find(r#""event":"session_start""#));
            shutdown.store(true, Ordering::Relaxed);
            let _ = join.await;
        });
    }
}
