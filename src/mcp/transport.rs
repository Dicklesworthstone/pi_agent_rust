//! MCP transports (bd-cv653.6.1).
//!
//! Stdio uses MCP's newline-delimited JSON-RPC wire format with a strict env
//! allowlist; streamable HTTP does POST-per-message with JSON or SSE responses,
//! `Mcp-Session-Id` continuity, and custom headers.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver as StdReceiver, SyncSender as StdSyncSender, TrySendError};
use std::time::Duration;

use async_trait::async_trait;
use futures::{FutureExt as _, StreamExt as _};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::lsp::jsonrpc::{
    CompletionWaitError, MCP_ENV_ALLOWLIST, PublicTailBuffer, RpcErrorObject, await_completion,
};
use crate::tools::{ProcessCleanupMode, ProcessGuard};

/// Default per-request timeout for MCP calls.
pub const DEFAULT_MCP_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on an HTTP response body (10 MiB).
const MAX_HTTP_BODY: usize = 10 * 1024 * 1024;
/// Bound teardown calls so an unresponsive server cannot stall shutdown.
const HTTP_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Cancellation is best-effort, but must never become an unbounded drop task.
const HTTP_CANCEL_TIMEOUT: Duration = Duration::from_secs(2);
/// Per-logical-stream event-id budget. Exhaustion fails closed rather than
/// forgetting old ids and risking duplicate server-request side effects.
const MAX_HTTP_SSE_EVENT_IDS: usize = 4096;
/// MCP protocol revision this client speaks.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

fn tool_err(code: &str, message: impl Into<String>) -> Error {
    Error::tool("mcp", format!("[{code}] {}", message.into()))
}

/// One transport connection to an MCP server.
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Send a request and await its response.
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value>;
    /// Send a notification.
    async fn notify(&self, method: &str, params: Value) -> Result<()>;
    /// Whether the transport is still usable.
    fn is_alive(&self) -> bool;
    /// Synchronously abort in-flight work. This is the cancellation-safe
    /// teardown path used when a connection future is dropped at a deadline.
    fn abort(&self);
    /// Activate transport-owned background work after the MCP handshake.
    /// Stdio has no separate receive channel; Streamable HTTP uses this for
    /// its optional server-message GET stream.
    async fn activate(self: std::sync::Arc<Self>) -> Result<()> {
        Ok(())
    }
    /// Close the transport (best effort).
    async fn close(&self);
    /// Recent server stderr (stdio) or last HTTP error detail, for `/mcp`.
    fn diagnostics_tail(&self) -> String;
}

// ============================================================================
// stdio transport
// ============================================================================

/// Maximum encoded size of one MCP stdio JSON-RPC message (10 MiB).
const MAX_STDIO_MESSAGE_BYTES: usize = 10 * 1024 * 1024;
/// A small bounded queue keeps pipe writes off async workers without allowing
/// an unresponsive server to accumulate unbounded outbound messages.
const STDIO_WRITER_QUEUE_CAP: usize = 8;
/// Grace periods for orderly stdin close and TERM before the final tree kill.
const STDIO_CLOSE_GRACE: Duration = Duration::from_millis(100);
const STDIO_TERM_GRACE: Duration = Duration::from_millis(100);
/// Give a responsive server one scheduling turn to observe a cancellation
/// notification before the timed-out connection is torn down.
const STDIO_CANCEL_GRACE: Duration = Duration::from_millis(20);

type StdioOutcome = std::result::Result<Value, McpStdioError>;
type StdioPending = Mutex<HashMap<u64, StdSyncSender<StdioOutcome>>>;

#[derive(Debug, Clone)]
enum McpStdioError {
    Server(RpcErrorObject),
    Closed(String),
    Io(String),
    Backpressure(String),
    Request(String),
    Protocol(String),
}

impl McpStdioError {
    const fn code(&self) -> &'static str {
        match self {
            Self::Server(_) => "MCP_SERVER_ERROR",
            Self::Closed(_) => "MCP_TRANSPORT_CLOSED",
            Self::Io(_) => "MCP_TRANSPORT_IO",
            Self::Backpressure(_) => "MCP_BACKPRESSURE",
            Self::Request(_) => "MCP_REQUEST_INVALID",
            Self::Protocol(_) => "MCP_PROTOCOL",
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Server(error) => format!("server error {}: {}", error.code, error.message),
            Self::Closed(reason) => format!("transport closed: {reason}"),
            Self::Io(reason) => format!("transport I/O error: {reason}"),
            Self::Backpressure(reason) => format!("transport backpressure: {reason}"),
            Self::Request(reason) => format!("invalid request: {reason}"),
            Self::Protocol(reason) => format!("protocol error: {reason}"),
        }
    }

    const fn breaks_transport(&self) -> bool {
        !matches!(
            self,
            Self::Server(_) | Self::Backpressure(_) | Self::Request(_)
        )
    }
}

enum WriterCommand {
    Message(Vec<u8>),
    Cancellation(Vec<u8>),
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TreeCleanupState {
    Pending,
    #[cfg(not(windows))]
    TermSent,
    Killed,
}

struct CappedJsonWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl CappedJsonWriter {
    const fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            exceeded: false,
        }
    }
}

impl Write for CappedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "encoded JSON exceeds configured limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn encode_stdio_message(value: &Value) -> std::result::Result<Vec<u8>, McpStdioError> {
    encode_stdio_message_with_limit(value, MAX_STDIO_MESSAGE_BYTES)
}

fn encode_stdio_message_with_limit(
    value: &Value,
    limit: usize,
) -> std::result::Result<Vec<u8>, McpStdioError> {
    let mut writer = CappedJsonWriter::new(limit);
    if let Err(error) = serde_json::to_writer(&mut writer, value) {
        if writer.exceeded {
            return Err(McpStdioError::Request(format!(
                "outbound message exceeds {limit} bytes"
            )));
        }
        return Err(McpStdioError::Request(format!(
            "cannot encode JSON-RPC: {error}"
        )));
    }
    writer.bytes.push(b'\n');
    Ok(writer.bytes)
}

fn try_enqueue_client_command(
    writer_tx: &StdSyncSender<WriterCommand>,
    command: WriterCommand,
) -> std::result::Result<(), McpStdioError> {
    writer_tx.try_send(command).map_err(|error| match error {
        TrySendError::Full(_) => {
            McpStdioError::Backpressure("outbound stdio queue is full".to_string())
        }
        TrySendError::Disconnected(_) => McpStdioError::Closed("stdio writer stopped".to_string()),
    })
}

fn wake_writer_shutdown(writer_tx: &StdSyncSender<WriterCommand>) {
    let _ = writer_tx.try_send(WriterCommand::Close);
}

fn read_stdio_message(reader: &mut impl BufRead) -> std::io::Result<Option<Value>> {
    read_stdio_message_with_limit(reader, MAX_STDIO_MESSAGE_BYTES)
}

fn read_stdio_message_with_limit(
    reader: &mut impl BufRead,
    limit: usize,
) -> std::io::Result<Option<Value>> {
    let mut message = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if message.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF before newline terminator",
            ));
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if message.len().saturating_add(newline) > limit {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("stdio message exceeds {limit} bytes"),
                ));
            }
            message.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            if message.last() == Some(&b'\r') {
                message.pop();
            }
            let value = serde_json::from_slice(&message).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid newline-delimited JSON: {error}"),
                )
            })?;
            return Ok(Some(value));
        }
        if message.len().saturating_add(available.len()) > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("stdio message exceeds {limit} bytes"),
            ));
        }
        let consumed = available.len();
        message.extend_from_slice(available);
        reader.consume(consumed);
    }
}

fn valid_server_request_id(id: &Value) -> bool {
    id.is_null() || id.is_string() || id.is_number()
}

fn valid_params(params: Option<&Value>) -> bool {
    params.is_none_or(|value| value.is_object() || value.is_array())
}

// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::significant_drop_in_scrutinee)]
fn route_stdio_message(
    message: &Value,
    pending: &StdioPending,
    writer_tx: &StdSyncSender<WriterCommand>,
) -> std::result::Result<(), McpStdioError> {
    let object = message
        .as_object()
        .ok_or_else(|| McpStdioError::Protocol("JSON-RPC message must be an object".to_string()))?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpStdioError::Protocol(
            "JSON-RPC message must declare jsonrpc \"2.0\"".to_string(),
        ));
    }

    let id = object.get("id");
    let method = object.get("method");
    let result = object.get("result");
    let error = object.get("error");

    if let Some(method) = method {
        if !method.is_string() || result.is_some() || error.is_some() {
            return Err(McpStdioError::Protocol(
                "request/notification envelope is malformed".to_string(),
            ));
        }
        if !valid_params(object.get("params")) {
            return Err(McpStdioError::Protocol(
                "request/notification params must be an object or array".to_string(),
            ));
        }
        let Some(id) = id else {
            return Ok(()); // Valid server notification; no consumer in v1.
        };
        if !valid_server_request_id(id) {
            return Err(McpStdioError::Protocol(
                "server request id must be a string, number, or null".to_string(),
            ));
        }
        let response = if method.as_str() == Some("ping") {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {},
            })
        } else {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": "client does not support server-initiated requests",
                },
            })
        };
        let encoded = encode_stdio_message(&response)?;
        return writer_tx
            .try_send(WriterCommand::Message(encoded))
            .map_err(|error| match error {
                TrySendError::Full(_) => {
                    McpStdioError::Io("outbound stdio queue is full".to_string())
                }
                TrySendError::Disconnected(_) => {
                    McpStdioError::Closed("stdio writer stopped".to_string())
                }
            });
    }

    let Some(id) = id.and_then(Value::as_u64) else {
        return Err(McpStdioError::Protocol(
            "response id must be an unsigned integer".to_string(),
        ));
    };
    if result.is_some() == error.is_some() {
        return Err(McpStdioError::Protocol(
            "response must contain exactly one of result or error".to_string(),
        ));
    }
    let outcome = if let Some(error) = error {
        let object = error.as_object().ok_or_else(|| {
            McpStdioError::Protocol("response error must be an object".to_string())
        })?;
        let code = object.get("code").and_then(Value::as_i64).ok_or_else(|| {
            McpStdioError::Protocol("response error code must be an integer".to_string())
        })?;
        let message = object
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                McpStdioError::Protocol("response error message must be a string".to_string())
            })?;
        Err(McpStdioError::Server(RpcErrorObject {
            code,
            message: message.to_string(),
            data: object.get("data").cloned(),
        }))
    } else {
        Ok(result.cloned().expect("result presence checked above"))
    };
    if let Some(sender) = lock(pending).remove(&id) {
        let _ = sender.send(outcome);
    }
    // Unknown ids are expected for late responses to locally cancelled
    // requests. They must never complete a different request.
    Ok(())
}

// Ownership is part of the call contract here.
#[allow(clippy::needless_pass_by_value)]
fn fail_pending(pending: &StdioPending, error: McpStdioError) {
    let senders: Vec<_> = lock(pending).drain().map(|(_, sender)| sender).collect();
    for sender in senders {
        let _ = sender.send(Err(error.clone()));
    }
}

// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::significant_drop_tightening)]
fn kill_tree_once(pid: u32, cleanup_state: &Mutex<TreeCleanupState>) {
    let mut state = lock(cleanup_state);
    if *state != TreeCleanupState::Killed {
        *state = TreeCleanupState::Killed;
        crate::tools::kill_process_group_tree(Some(pid));
    }
}

// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::significant_drop_tightening)]
fn terminate_tree_once(pid: u32, cleanup_state: &Mutex<TreeCleanupState>) {
    let mut state = lock(cleanup_state);
    if *state != TreeCleanupState::Pending {
        return;
    }
    // Windows implements tree discipline with a kill-on-close Job. Sending
    // TERM consumes that Job and terminates the whole tree, so a later
    // PID-based fallback would target stale identities rather than escalate.
    #[cfg(windows)]
    {
        *state = TreeCleanupState::Killed;
    }
    #[cfg(not(windows))]
    {
        *state = TreeCleanupState::TermSent;
    }
    crate::tools::terminate_process_group_tree(Some(pid));
}

fn stop_connection(
    pending: &StdioPending,
    alive: &AtomicBool,
    closing: &AtomicBool,
    tree_cleanup_state: &Mutex<TreeCleanupState>,
    pid: u32,
    error: McpStdioError,
) {
    alive.store(false, Ordering::SeqCst);
    fail_pending(pending, error);
    if !closing.load(Ordering::SeqCst) {
        kill_tree_once(pid, tree_cleanup_state);
    }
}

struct ReaderConnectionStop<'a> {
    writer_tx: &'a StdSyncSender<WriterCommand>,
    pending: &'a StdioPending,
    alive: &'a AtomicBool,
    closing: &'a AtomicBool,
    tree_cleanup_state: &'a Mutex<TreeCleanupState>,
    pid: u32,
}

impl ReaderConnectionStop<'_> {
    fn finish(self, error: McpStdioError, wake_writer: impl FnOnce(&StdSyncSender<WriterCommand>)) {
        // Publish the stopped state before the best-effort queue wake. If the
        // bounded queue is full, every queued write variant observes `alive`
        // and exits; if the writer drains first, Close wakes its recv.
        self.alive.store(false, Ordering::SeqCst);
        wake_writer(self.writer_tx);
        stop_connection(
            self.pending,
            self.alive,
            self.closing,
            self.tree_cleanup_state,
            self.pid,
            error,
        );
    }
}

fn stop_reader_connection(
    writer_tx: &StdSyncSender<WriterCommand>,
    pending: &StdioPending,
    alive: &AtomicBool,
    closing: &AtomicBool,
    tree_cleanup_state: &Mutex<TreeCleanupState>,
    pid: u32,
    error: McpStdioError,
) {
    ReaderConnectionStop {
        writer_tx,
        pending,
        alive,
        closing,
        tree_cleanup_state,
        pid,
    }
    .finish(error, wake_writer_shutdown);
}

// Ownership is part of the call contract here.
#[allow(clippy::needless_pass_by_value)]
fn writer_loop(
    mut stdin: impl Write,
    writer_rx: StdReceiver<WriterCommand>,
    pending: std::sync::Arc<StdioPending>,
    alive: std::sync::Arc<AtomicBool>,
    closing: std::sync::Arc<AtomicBool>,
    tree_cleanup_state: std::sync::Arc<Mutex<TreeCleanupState>>,
    pid: u32,
) {
    while let Ok(command) = writer_rx.recv() {
        match command {
            WriterCommand::Message(message) => {
                if !alive.load(Ordering::SeqCst) {
                    return;
                }
                if let Err(error) = stdin.write_all(&message).and_then(|()| stdin.flush()) {
                    stop_connection(
                        &pending,
                        &alive,
                        &closing,
                        &tree_cleanup_state,
                        pid,
                        McpStdioError::Io(format!("stdio write failed: {error}")),
                    );
                    return;
                }
            }
            WriterCommand::Cancellation(message) => {
                if !alive.load(Ordering::SeqCst) {
                    return;
                }
                if let Err(error) = stdin.write_all(&message).and_then(|()| stdin.flush()) {
                    stop_connection(
                        &pending,
                        &alive,
                        &closing,
                        &tree_cleanup_state,
                        pid,
                        McpStdioError::Io(format!("stdio cancellation write failed: {error}")),
                    );
                    return;
                }
            }
            WriterCommand::Close => return,
        }
    }
}

fn classify_read_error(error: &std::io::Error) -> McpStdioError {
    match error.kind() {
        std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof => {
            McpStdioError::Protocol(error.to_string())
        }
        _ => McpStdioError::Io(format!("stdio read failed: {error}")),
    }
}

// Ownership is part of the call contract here.
#[allow(clippy::needless_pass_by_value)]
fn reader_loop(
    stdout: std::process::ChildStdout,
    writer_tx: StdSyncSender<WriterCommand>,
    pending: std::sync::Arc<StdioPending>,
    alive: std::sync::Arc<AtomicBool>,
    closing: std::sync::Arc<AtomicBool>,
    tree_cleanup_state: std::sync::Arc<Mutex<TreeCleanupState>>,
    pid: u32,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_stdio_message(&mut reader) {
            Ok(Some(message)) => {
                if let Err(error) = route_stdio_message(&message, &pending, &writer_tx) {
                    stop_reader_connection(
                        &writer_tx,
                        &pending,
                        &alive,
                        &closing,
                        &tree_cleanup_state,
                        pid,
                        error,
                    );
                    return;
                }
            }
            Ok(None) => {
                stop_reader_connection(
                    &writer_tx,
                    &pending,
                    &alive,
                    &closing,
                    &tree_cleanup_state,
                    pid,
                    McpStdioError::Closed("server closed stdout (EOF)".to_string()),
                );
                return;
            }
            Err(error) => {
                let error = classify_read_error(&error);
                stop_reader_connection(
                    &writer_tx,
                    &pending,
                    &alive,
                    &closing,
                    &tree_cleanup_state,
                    pid,
                    error,
                );
                return;
            }
        }
    }
}

const fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn sanitize_stderr(chunk: &str) -> String {
    use std::fmt::Write as _;

    let mut sanitized = String::with_capacity(chunk.len());
    for character in chunk.chars() {
        if character == '\n' || character == '\t' {
            sanitized.push(character);
        } else if character.is_control() || is_bidi_control(character) {
            let _ = write!(sanitized, "\\u{{{:x}}}", u32::from(character));
        } else {
            sanitized.push(character);
        }
    }
    sanitized
}

struct McpStdioClient {
    child: Mutex<ProcessGuard>,
    pid: u32,
    writer_tx: StdSyncSender<WriterCommand>,
    pending: std::sync::Arc<StdioPending>,
    next_id: AtomicU64,
    alive: std::sync::Arc<AtomicBool>,
    closing: std::sync::Arc<AtomicBool>,
    tree_cleanup_state: std::sync::Arc<Mutex<TreeCleanupState>>,
    stderr_tail: std::sync::Arc<Mutex<PublicTailBuffer>>,
}

impl McpStdioClient {
    #[allow(clippy::too_many_lines)]
    fn spawn(command: &str, args: &[String], env: &[(String, String)], cwd: &Path) -> Result<Self> {
        let mut command_builder = crate::tools::command_with_default_sigpipe_in_dir(command, cwd)
            .map_err(|error| {
            tool_err(
                "MCP_SERVER_MISSING",
                format!("failed to prepare MCP server {command:?}: {error}"),
            )
        })?;
        command_builder
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        for &variable in MCP_ENV_ALLOWLIST {
            if let Some(value) = std::env::var_os(variable) {
                command_builder.env(variable, value);
            }
        }
        command_builder.envs(
            env.iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        );
        crate::tools::isolate_command_process_group(&mut command_builder);
        let mut child = command_builder.spawn().map_err(|error| {
            tool_err(
                "MCP_SERVER_MISSING",
                format!("failed to spawn MCP server {command:?}: {error}"),
            )
        })?;
        if !crate::tools::attach_child_job_discipline(&child) {
            let mut guard = ProcessGuard::new(child, ProcessCleanupMode::ProcessGroupTree);
            let _ = guard.kill();
            return Err(tool_err(
                "MCP_PROCESS_ISOLATION",
                "failed to attach MCP server to process-tree cleanup discipline",
            ));
        }
        let pid = child.id();
        let Some(stdin) = child.stdin.take() else {
            let mut guard = ProcessGuard::new(child, ProcessCleanupMode::ProcessGroupTree);
            let _ = guard.kill();
            return Err(tool_err(
                "MCP_TRANSPORT_IO",
                "spawned MCP server has no stdin",
            ));
        };
        let Some(stdout) = child.stdout.take() else {
            let mut guard = ProcessGuard::new(child, ProcessCleanupMode::ProcessGroupTree);
            let _ = guard.kill();
            return Err(tool_err(
                "MCP_TRANSPORT_IO",
                "spawned MCP server has no stdout",
            ));
        };
        let Some(stderr) = child.stderr.take() else {
            let mut guard = ProcessGuard::new(child, ProcessCleanupMode::ProcessGroupTree);
            let _ = guard.kill();
            return Err(tool_err(
                "MCP_TRANSPORT_IO",
                "spawned MCP server has no stderr",
            ));
        };

        let pending = std::sync::Arc::new(Mutex::new(HashMap::new()));
        let alive = std::sync::Arc::new(AtomicBool::new(true));
        let closing = std::sync::Arc::new(AtomicBool::new(false));
        let tree_cleanup_state = std::sync::Arc::new(Mutex::new(TreeCleanupState::Pending));
        let stderr_tail = std::sync::Arc::new(Mutex::new(PublicTailBuffer::new()));
        let (writer_tx, writer_rx) = std::sync::mpsc::sync_channel(STDIO_WRITER_QUEUE_CAP);
        let mut child_guard = ProcessGuard::new(child, ProcessCleanupMode::ChildOnly);

        let writer_thread = {
            let pending = std::sync::Arc::clone(&pending);
            let alive = std::sync::Arc::clone(&alive);
            let closing = std::sync::Arc::clone(&closing);
            let tree_cleanup_state = std::sync::Arc::clone(&tree_cleanup_state);
            std::thread::Builder::new()
                .name("pi-mcp-stdio-writer".to_string())
                .spawn(move || {
                    writer_loop(
                        stdin,
                        writer_rx,
                        pending,
                        alive,
                        closing,
                        tree_cleanup_state,
                        pid,
                    );
                })
        };
        let _writer_thread = match writer_thread {
            Ok(thread) => thread, // ubs:ignore intentional process-lifetime thread
            Err(error) => {
                crate::tools::kill_process_group_tree(Some(pid));
                let _ = child_guard.kill();
                return Err(tool_err(
                    "MCP_TRANSPORT_IO",
                    format!("failed to start MCP stdio writer thread: {error}"),
                ));
            }
        };
        let reader_thread = {
            let writer_tx = writer_tx.clone();
            let pending = std::sync::Arc::clone(&pending);
            let alive = std::sync::Arc::clone(&alive);
            let closing = std::sync::Arc::clone(&closing);
            let tree_cleanup_state = std::sync::Arc::clone(&tree_cleanup_state);
            std::thread::Builder::new()
                .name("pi-mcp-stdio-reader".to_string())
                .spawn(move || {
                    reader_loop(
                        stdout,
                        writer_tx,
                        pending,
                        alive,
                        closing,
                        tree_cleanup_state,
                        pid,
                    );
                })
        };
        let _reader_thread = match reader_thread {
            Ok(thread) => thread, // ubs:ignore intentional process-lifetime thread
            Err(error) => {
                kill_tree_once(pid, &tree_cleanup_state);
                let _ = child_guard.kill();
                return Err(tool_err(
                    "MCP_TRANSPORT_IO",
                    format!("failed to start MCP stdio reader thread: {error}"),
                ));
            }
        };
        let stderr_thread = {
            let stderr_tail = std::sync::Arc::clone(&stderr_tail);
            std::thread::Builder::new()
                .name("pi-mcp-stderr".to_string())
                .spawn(move || {
                    let mut reader = BufReader::new(stderr);
                    let mut bytes = [0u8; 4096];
                    loop {
                        match reader.read(&mut bytes) {
                            Ok(0) | Err(_) => return,
                            Ok(count) => {
                                let chunk = String::from_utf8_lossy(&bytes[..count]);
                                lock(&stderr_tail).push(&sanitize_stderr(&chunk));
                            }
                        }
                    }
                })
        };
        let _stderr_thread = match stderr_thread {
            Ok(thread) => thread, // ubs:ignore intentional process-lifetime thread
            Err(error) => {
                kill_tree_once(pid, &tree_cleanup_state);
                let _ = child_guard.kill();
                return Err(tool_err(
                    "MCP_TRANSPORT_IO",
                    format!("failed to start MCP stderr reader thread: {error}"),
                ));
            }
        };

        Ok(Self {
            child: Mutex::new(child_guard),
            pid,
            writer_tx,
            pending,
            next_id: AtomicU64::new(1),
            alive,
            closing,
            tree_cleanup_state,
            stderr_tail,
        })
    }

    fn enqueue(&self, command: WriterCommand) -> std::result::Result<(), McpStdioError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(McpStdioError::Closed(
                "server transport is not alive".to_string(),
            ));
        }
        try_enqueue_client_command(&self.writer_tx, command)
    }

    fn request(
        &self,
        method: &str,
        params: Value,
    ) -> std::result::Result<(u64, StdReceiver<StdioOutcome>), McpStdioError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        if id == u64::MAX {
            self.abort();
            return Err(McpStdioError::Request(
                "request id space exhausted".to_string(),
            ));
        }
        let mut message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
        });
        append_client_params(&mut message, params)?;
        let encoded = encode_stdio_message(&message)?;
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        lock(&self.pending).insert(id, sender);
        if let Err(error) = self.enqueue(WriterCommand::Message(encoded)) {
            lock(&self.pending).remove(&id);
            return Err(error);
        }
        Ok((id, receiver))
    }

    fn notify(&self, method: &str, params: Value) -> std::result::Result<(), McpStdioError> {
        let mut message = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        append_client_params(&mut message, params)?;
        self.enqueue(WriterCommand::Message(encode_stdio_message(&message)?))
    }

    fn cancel_request(&self, id: u64, send_notification: bool) {
        lock(&self.pending).remove(&id);
        if send_notification && self.alive.load(Ordering::SeqCst) {
            let cancellation = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {
                    "requestId": id,
                    "reason": "request timed out or was cancelled",
                },
            });
            if let Ok(encoded) = encode_stdio_message(&cancellation) {
                let _ = self
                    .writer_tx
                    .try_send(WriterCommand::Cancellation(encoded));
            }
        }
    }

    fn is_alive(&self) -> bool {
        if !self.alive.load(Ordering::SeqCst) {
            return false;
        }
        let child_status = lock(&self.child).try_wait_child();
        match child_status {
            Ok(Some(status)) => {
                stop_reader_connection(
                    &self.writer_tx,
                    &self.pending,
                    &self.alive,
                    &self.closing,
                    &self.tree_cleanup_state,
                    self.pid,
                    McpStdioError::Closed(format!("server process exited with {status}")),
                );
                false
            }
            Err(error) => {
                stop_reader_connection(
                    &self.writer_tx,
                    &self.pending,
                    &self.alive,
                    &self.closing,
                    &self.tree_cleanup_state,
                    self.pid,
                    McpStdioError::Io(format!("failed to inspect server process: {error}")),
                );
                false
            }
            Ok(None) => true,
        }
    }

    fn child_exited(&self) -> bool {
        matches!(lock(&self.child).try_wait_child(), Ok(Some(_)))
    }

    fn begin_close(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.alive.store(false, Ordering::SeqCst);
        fail_pending(
            &self.pending,
            McpStdioError::Closed("client closed the transport".to_string()),
        );
        wake_writer_shutdown(&self.writer_tx);
    }

    fn terminate_tree(&self) {
        terminate_tree_once(self.pid, &self.tree_cleanup_state);
    }

    fn abort(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.alive.store(false, Ordering::SeqCst);
        fail_pending(
            &self.pending,
            McpStdioError::Closed("client aborted the transport".to_string()),
        );
        wake_writer_shutdown(&self.writer_tx);
        let mut child = lock(&self.child);
        kill_tree_once(self.pid, &self.tree_cleanup_state);
        let _ = child.kill();
    }

    fn stderr_tail(&self) -> String {
        lock(&self.stderr_tail).tail()
    }
}

fn append_client_params(
    message: &mut Value,
    params: Value,
) -> std::result::Result<(), McpStdioError> {
    if params.is_null() {
        return Ok(());
    }
    if !params.is_object() && !params.is_array() {
        return Err(McpStdioError::Request(
            "outbound params must be an object, array, or null".to_string(),
        ));
    }
    message["params"] = params;
    Ok(())
}

struct PendingStdioRequest<'a> {
    client: &'a McpStdioClient,
    id: u64,
    send_cancellation: bool,
    armed: bool,
}

impl PendingStdioRequest<'_> {
    const fn cancellation_was_sent(&mut self) {
        self.send_cancellation = false;
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingStdioRequest<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.client.cancel_request(self.id, self.send_cancellation);
            self.client.abort();
        }
    }
}

struct StdioAbortGuard<'a> {
    client: &'a McpStdioClient,
    armed: bool,
}

impl StdioAbortGuard<'_> {
    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StdioAbortGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.client.abort();
        }
    }
}

impl Drop for McpStdioClient {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Newline-delimited JSON-RPC over a spawned child process with an env
/// allowlist and process-tree cleanup.
pub struct StdioTransport {
    rpc: McpStdioClient,
}

impl StdioTransport {
    /// Spawn the server with the MCP env allowlist (no ambient secrets).
    ///
    /// # Errors
    ///
    /// Fails when the command cannot be spawned.
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<Self> {
        let rpc = McpStdioClient::spawn(command, args, env, cwd)?;
        Ok(Self { rpc })
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let (id, rx) = self
            .rpc
            .request(method, params)
            .map_err(|error| tool_err(error.code(), error.message()))?;
        let send_cancellation = method != "initialize";
        let mut pending_request = PendingStdioRequest {
            client: &self.rpc,
            id,
            send_cancellation,
            armed: true,
        };
        let outcome = await_completion(rx, timeout, || {
            self.rpc.cancel_request(id, send_cancellation);
        })
        .await;
        match outcome {
            Ok(Ok(value)) => {
                pending_request.disarm();
                Ok(value)
            }
            Ok(Err(error)) => {
                if error.breaks_transport() {
                    self.rpc.abort();
                }
                pending_request.disarm();
                Err(tool_err(error.code(), error.message()))
            }
            Err(CompletionWaitError::Timeout) => {
                pending_request.cancellation_was_sent();
                if send_cancellation {
                    let cx = crate::agent_cx::AgentCx::for_current_or_request();
                    let now = cx
                        .cx()
                        .timer_driver()
                        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
                    asupersync::time::sleep(now, STDIO_CANCEL_GRACE).await;
                }
                self.rpc.abort();
                pending_request.disarm();
                Err(tool_err(
                    "MCP_TIMEOUT",
                    format!("request timed out after {} ms", timeout.as_millis()),
                ))
            }
            Err(CompletionWaitError::Cancelled) => {
                pending_request.cancellation_was_sent();
                if send_cancellation {
                    let cx = crate::agent_cx::AgentCx::for_current_or_request();
                    let now = cx
                        .cx()
                        .timer_driver()
                        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
                    asupersync::time::sleep(now, STDIO_CANCEL_GRACE).await;
                }
                self.rpc.abort();
                pending_request.disarm();
                Err(tool_err("MCP_CANCELLED", "cancelled by ambient context"))
            }
            Err(CompletionWaitError::Closed) => {
                self.rpc.abort();
                pending_request.disarm();
                Err(tool_err(
                    "MCP_TRANSPORT_CLOSED",
                    "completion channel dropped (server died)",
                ))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.rpc
            .notify(method, params)
            .map_err(|error| tool_err(error.code(), error.message()))
    }

    fn is_alive(&self) -> bool {
        self.rpc.is_alive()
    }

    fn abort(&self) {
        self.rpc.abort();
    }

    async fn close(&self) {
        let mut abort_guard = StdioAbortGuard {
            client: &self.rpc,
            armed: true,
        };
        self.rpc.begin_close();
        if wait_for_child_exit(&self.rpc, STDIO_CLOSE_GRACE).await {
            self.rpc.abort();
            abort_guard.disarm();
            return;
        }
        self.rpc.terminate_tree();
        if wait_for_child_exit(&self.rpc, STDIO_TERM_GRACE).await {
            self.rpc.abort();
            abort_guard.disarm();
            return;
        }
        self.rpc.abort();
        abort_guard.disarm();
    }

    fn diagnostics_tail(&self) -> String {
        self.rpc.stderr_tail()
    }
}

async fn wait_for_child_exit(client: &McpStdioClient, budget: Duration) -> bool {
    let cx = crate::agent_cx::AgentCx::for_current_or_request();
    let start = cx
        .cx()
        .timer_driver()
        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
    loop {
        if client.child_exited() {
            return true;
        }
        let now = cx
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        if Duration::from_nanos(now.duration_since(start)) >= budget {
            return false;
        }
        asupersync::time::sleep(now, Duration::from_millis(10)).await;
    }
}

// ============================================================================
// Streamable HTTP transport
// ============================================================================

/// Streamable HTTP MCP transport.
///
/// POST per message; the response may be a single JSON document or an SSE
/// stream of JSON-RPC messages. The `Mcp-Session-Id` from the initialize
/// response is replayed on later calls.
pub struct HttpTransport {
    client: crate::http::client::Client,
    url: String,
    headers: Vec<(String, String)>,
    session: Mutex<HttpSessionState>,
    next_id: AtomicU64,
    alive: std::sync::atomic::AtomicBool,
    listener_started: AtomicBool,
    session_changed: asupersync::sync::Notify,
    abort_notify: asupersync::sync::Notify,
    lane: std::sync::Arc<asupersync::sync::Mutex<()>>,
}

#[derive(Default)]
struct HttpSessionState {
    generation: u64,
    session_id: Option<String>,
    protocol_version: Option<String>,
    initialize_params: Option<Value>,
}

#[derive(Clone)]
struct HttpWireState {
    generation: u64,
    session_id: Option<String>,
    protocol_version: Option<String>,
}

struct HttpCancellationDispatch {
    client: crate::http::client::Client,
    url: String,
    headers: Vec<(String, String)>,
    wire_state: HttpWireState,
    request_id: u64,
}

impl HttpCancellationDispatch {
    async fn send(self) -> Result<()> {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {
                "requestId": self.request_id,
                "reason": "request timed out or was cancelled",
            },
        });
        let mut request = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        for (name, value) in self.headers {
            request = request.header(name, value);
        }
        if let Some(session_id) = self.wire_state.session_id {
            request = request.header("Mcp-Session-Id", session_id);
        }
        if let Some(protocol_version) = self.wire_state.protocol_version {
            request = request.header("Mcp-Protocol-Version", protocol_version);
        }
        let response = request
            .json(&frame)
            .map_err(|err| tool_err("MCP_TRANSPORT_IO", format!("encode cancellation: {err}")))?
            .timeout(HTTP_CANCEL_TIMEOUT)
            .send()
            .await
            .map_err(|err| tool_err("MCP_TRANSPORT_IO", format!("send cancellation: {err}")))?;
        let status = response.status();
        if status != 202 {
            return Err(tool_err(
                "MCP_PROTOCOL",
                format!("cancellation notification expected HTTP 202, received {status}"),
            ));
        }
        let body = response.bytes_limited(1).await.map_err(|err| {
            tool_err(
                "MCP_PROTOCOL",
                format!("cancellation response body was not empty: {err}"),
            )
        })?;
        if body.is_empty() {
            Ok(())
        } else {
            Err(tool_err(
                "MCP_PROTOCOL",
                "HTTP 202 cancellation response must have no body",
            ))
        }
    }
}

struct HttpSessionDeleteDispatch {
    client: crate::http::client::Client,
    url: String,
    headers: Vec<(String, String)>,
    wire_state: HttpWireState,
}

impl HttpSessionDeleteDispatch {
    async fn send(self, timeout: Duration) -> Result<()> {
        let Some(session_id) = self.wire_state.session_id else {
            return Ok(());
        };
        let mut request = self.client.delete(&self.url);
        for (name, value) in self.headers {
            request = request.header(name, value);
        }
        request = request.header("Mcp-Session-Id", session_id);
        // A server may assign a session in initialize response headers before
        // its body can be validated. That provisional state deliberately has
        // no negotiated version (so the GET supervisor stays dormant), but a
        // cleanup DELETE still identifies the version the client proposed.
        let protocol_version = self
            .wire_state
            .protocol_version
            .unwrap_or_else(|| MCP_PROTOCOL_VERSION.to_string());
        request = request.header("Mcp-Protocol-Version", protocol_version);
        HttpTransport::run_with_deadline(
            async move {
                let response = request.no_timeout().send().await.map_err(|err| {
                    tool_err("MCP_TRANSPORT_IO", format!("terminate HTTP session: {err}"))
                })?;
                let status = response.status();
                if (200..300).contains(&status) || status == 405 {
                    // Session teardown is complete once the status arrives.
                    // The body is non-semantic and must not extend close time.
                    return Ok(());
                }
                let body = response.text_limited(4096).await.unwrap_or_default();
                Err(tool_err(
                    "MCP_HTTP_STATUS",
                    format!("HTTP {status} terminating session: {}", body.trim()),
                ))
            },
            timeout,
            "HTTP session termination",
        )
        .await
    }
}

struct PendingHttpRequest<'a> {
    transport: &'a HttpTransport,
    cancellation: Option<HttpCancellationDispatch>,
    runtime: Option<asupersync::runtime::RuntimeHandle>,
    armed: bool,
}

impl PendingHttpRequest<'_> {
    fn disarm(&mut self) {
        self.armed = false;
        self.cancellation = None;
    }

    async fn cancel_and_abort(&mut self) {
        let cancellation = self.cancellation.take();
        self.transport.abort();
        self.armed = false;
        if let Some(cancellation) = cancellation {
            let _ = cancellation.send().await;
        }
    }
}

impl Drop for PendingHttpRequest<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let cancellation = self.cancellation.take();
        self.armed = false;
        self.transport.abort();
        if let (Some(runtime), Some(cancellation)) = (self.runtime.take(), cancellation) {
            let _ = runtime.try_spawn(async move {
                let _ = cancellation.send().await;
            });
        }
    }
}

#[derive(Default)]
struct HttpSseCursor {
    seen_ids: HashSet<String>,
    last_event_id: Option<String>,
    resume_safe: bool,
}

impl HttpSseCursor {
    fn accept(&mut self, event: &crate::sse::SseEvent) -> Result<bool> {
        let Some(event_id) = event.id.as_deref().filter(|_| event.id_was_explicit) else {
            // Without an id there is no checkpoint beyond this event. If it
            // contains a server request, resuming from an earlier id could
            // repeat the response side effect.
            self.resume_safe = false;
            return Ok(true);
        };
        if event_id.is_empty() {
            return Err(tool_err(
                "MCP_PROTOCOL",
                "MCP SSE event id must not be empty",
            ));
        }
        super::config::validate_http_header_value(event_id)
            .map_err(|err| tool_err("MCP_PROTOCOL", format!("invalid MCP SSE event id: {err}")))?;
        if self.seen_ids.contains(event_id) {
            return Ok(false);
        }
        if self.seen_ids.len() >= MAX_HTTP_SSE_EVENT_IDS {
            return Err(tool_err(
                "MCP_PROTOCOL",
                format!(
                    "MCP SSE stream exceeded the {MAX_HTTP_SSE_EVENT_IDS}-event id safety bound"
                ),
            ));
        }
        self.seen_ids.insert(event_id.to_string());
        self.last_event_id = Some(event_id.to_string());
        self.resume_safe = true;
        Ok(true)
    }

    fn resume_id(&self) -> Option<&str> {
        self.resume_safe
            .then_some(self.last_event_id.as_deref())
            .flatten()
    }
}

struct BoundedHttpSseDecoder {
    parser: crate::sse::SseParser,
    pending_utf8: Vec<u8>,
    received_bytes: usize,
}

impl BoundedHttpSseDecoder {
    fn new() -> Self {
        Self {
            parser: crate::sse::SseParser::new(),
            pending_utf8: Vec::new(),
            received_bytes: 0,
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<crate::sse::SseEvent>> {
        self.received_bytes = self
            .received_bytes
            .checked_add(chunk.len())
            .ok_or_else(|| tool_err("MCP_PROTOCOL", "SSE response byte count overflowed"))?;
        if self.received_bytes > MAX_HTTP_BODY {
            return Err(tool_err(
                "MCP_PROTOCOL",
                format!("SSE response exceeded {MAX_HTTP_BODY} bytes"),
            ));
        }
        self.pending_utf8.extend_from_slice(chunk);
        let valid_bytes = match std::str::from_utf8(&self.pending_utf8) {
            Ok(_) => self.pending_utf8.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(error) => {
                return Err(tool_err(
                    "MCP_PROTOCOL",
                    format!("SSE response is not valid UTF-8: {error}"),
                ));
            }
        };
        if valid_bytes == 0 {
            return Ok(Vec::new());
        }
        let tail = self.pending_utf8.split_off(valid_bytes);
        let text = std::str::from_utf8(&self.pending_utf8)
            .map_err(|err| tool_err("MCP_PROTOCOL", format!("invalid SSE UTF-8: {err}")))?;
        let events = self.parser.feed(text);
        self.pending_utf8 = tail;
        Ok(events)
    }

    fn finish(mut self) -> Result<Vec<crate::sse::SseEvent>> {
        let mut events = Vec::new();
        if !self.pending_utf8.is_empty() {
            let text = std::str::from_utf8(&self.pending_utf8).map_err(|err| {
                tool_err(
                    "MCP_PROTOCOL",
                    format!("SSE response ended with invalid UTF-8: {err}"),
                )
            })?;
            events.extend(self.parser.feed(text));
        }
        if let Some(event) = self.parser.flush() {
            events.push(event);
        }
        Ok(events)
    }
}

#[derive(Clone, Copy)]
enum HttpRoundTripKind<'a> {
    Request { expected_id: u64, method: &'a str },
    AcceptedMessage { description: &'static str },
}

#[derive(Clone, Copy)]
enum HttpResponseMediaType {
    Json,
    EventStream,
}

enum HttpEventStreamOpen {
    Unsupported,
    SessionExpired,
    Stream(crate::http::client::Response),
}

impl HttpTransport {
    /// # Errors
    ///
    /// Fails when custom headers are malformed, unsafe, duplicated, or claim
    /// a protocol-owned header.
    pub fn new(url: &str, headers: Vec<(String, String)>) -> Result<Self> {
        let headers = super::config::normalize_http_headers(headers).map_err(|reason| {
            tool_err(
                "MCP_CONFIG_INVALID",
                format!("invalid custom HTTP headers: {reason}"),
            )
        })?;
        Ok(Self {
            client: crate::http::client::Client::new(),
            url: url.to_string(),
            headers,
            session: Mutex::new(HttpSessionState::default()),
            next_id: AtomicU64::new(1),
            alive: std::sync::atomic::AtomicBool::new(true),
            listener_started: AtomicBool::new(false),
            session_changed: asupersync::sync::Notify::new(),
            abort_notify: asupersync::sync::Notify::new(),
            lane: std::sync::Arc::new(asupersync::sync::Mutex::new(())),
        })
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn next_request_id(&self) -> Result<u64> {
        self.next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| tool_err("MCP_PROTOCOL", "HTTP request id space exhausted"))
    }

    fn wire_state(&self) -> HttpWireState {
        let session = Self::lock(&self.session);
        HttpWireState {
            generation: session.generation,
            session_id: session.session_id.clone(),
            protocol_version: session.protocol_version.clone(),
        }
    }

    fn session_generation(&self) -> u64 {
        Self::lock(&self.session).generation
    }

    fn session_delete_dispatch(&self, wire_state: HttpWireState) -> HttpSessionDeleteDispatch {
        HttpSessionDeleteDispatch {
            client: self.client.clone(),
            url: self.url.clone(),
            headers: self.headers.clone(),
            wire_state,
        }
    }

    /// Seal the transport and atomically take the latest negotiated session
    /// for orderly DELETE. Session publication uses the same lock, so close
    /// cannot snapshot an older state while a newer initialize commits.
    fn seal_and_take_session(&self) -> HttpWireState {
        let mut session = Self::lock(&self.session);
        self.alive.store(false, std::sync::atomic::Ordering::SeqCst);
        let wire_state = HttpWireState {
            generation: session.generation,
            session_id: session.session_id.clone(),
            protocol_version: session.protocol_version.clone(),
        };
        let generation = session.generation.wrapping_add(1);
        *session = HttpSessionState {
            generation,
            ..HttpSessionState::default()
        };
        drop(session);
        self.abort_notify.notify_waiters();
        self.session_changed.notify_waiters();
        wire_state
    }

    /// Retire the transport only if `generation` still names the active
    /// protocol session. Holding the session lock across the terminal state
    /// transition prevents an old GET listener from racing a successful
    /// renewal and aborting the replacement session.
    fn abort_if_session_generation(&self, generation: u64) -> bool {
        let session = Self::lock(&self.session);
        if session.generation != generation || !self.alive.load(std::sync::atomic::Ordering::SeqCst)
        {
            return false;
        }
        self.alive.store(false, std::sync::atomic::Ordering::SeqCst);
        drop(session);
        self.abort_notify.notify_waiters();
        true
    }

    fn cancellation_dispatch(&self, request_id: u64) -> HttpCancellationDispatch {
        HttpCancellationDispatch {
            client: self.client.clone(),
            url: self.url.clone(),
            headers: self.headers.clone(),
            wire_state: self.wire_state(),
            request_id,
        }
    }

    fn expire_session_if_current(&self, generation: u64) {
        let mut session = Self::lock(&self.session);
        if session.generation != generation {
            return;
        }
        session.generation = session.generation.wrapping_add(1);
        session.session_id = None;
        session.protocol_version = None;
        drop(session);
        self.session_changed.notify_waiters();
    }

    fn reset_session_state(&self) {
        let mut session = Self::lock(&self.session);
        let generation = session.generation.wrapping_add(1);
        *session = HttpSessionState {
            generation,
            ..HttpSessionState::default()
        };
        drop(session);
        self.session_changed.notify_waiters();
    }

    fn begin_provisional_session(&self, candidate_session_id: Option<&str>) -> Result<()> {
        let Some(session_id) = candidate_session_id else {
            return Ok(());
        };
        validate_http_session_id(session_id)?;
        let mut session = Self::lock(&self.session);
        if !self.alive.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(tool_err(
                "MCP_TRANSPORT_CLOSED",
                "HTTP transport closed before initialize session publication",
            ));
        }
        session.generation = session.generation.wrapping_add(1);
        session.session_id = Some(session_id.to_string());
        drop(session);
        self.session_changed.notify_waiters();
        Ok(())
    }

    /// Admit only the first public initialize call. A failed initialize may
    /// leave a server-assigned provisional session for `close()` to retire;
    /// rejecting retries without mutation preserves that cleanup ownership.
    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn ensure_explicit_initialize_admissible(&self) -> Result<()> {
        let session = Self::lock(&self.session);
        if !self.alive.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(tool_err(
                "MCP_TRANSPORT_CLOSED",
                "cannot initialize a closed HTTP transport",
            ));
        }
        if session.session_id.is_some()
            || session.protocol_version.is_some()
            || session.initialize_params.is_some()
        {
            return Err(tool_err(
                "MCP_PROTOCOL",
                "HTTP transport is already initialized or has an initialize in progress",
            ));
        }
        Ok(())
    }

    async fn run_until_abort<F, T>(&self, operation: F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        if !self.alive.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(tool_err(
                "MCP_TRANSPORT_UNAVAILABLE",
                "HTTP transport was aborted before request dispatch",
            ));
        }
        let operation = operation.fuse();
        let aborted = self
            .abort_notify
            .wait_until(|| !self.alive.load(std::sync::atomic::Ordering::SeqCst))
            .fuse();
        futures::pin_mut!(operation, aborted);
        match futures::future::select(operation, aborted).await {
            futures::future::Either::Left((result, _)) => {
                // The operation and abort wakeup can become ready together.
                // Treat the result as linearized before close only when the
                // terminal flag still says the transport is live.
                if self.alive.load(std::sync::atomic::Ordering::SeqCst) {
                    result
                } else {
                    Err(tool_err(
                        "MCP_TRANSPORT_CLOSED",
                        "HTTP transport was aborted during an in-flight request",
                    ))
                }
            }
            futures::future::Either::Right(((), _)) => Err(tool_err(
                "MCP_TRANSPORT_CLOSED",
                "HTTP transport was aborted during an in-flight request",
            )),
        }
    }

    async fn run_until_session_change<F, T>(
        &self,
        generation: u64,
        operation: F,
    ) -> Result<Option<T>>
    where
        F: Future<Output = Result<T>>,
    {
        if self.session_generation() != generation {
            return Ok(None);
        }
        let operation = operation.fuse();
        let changed = self
            .session_changed
            .wait_until(|| self.session_generation() != generation)
            .fuse();
        futures::pin_mut!(operation, changed);
        match futures::future::select(operation, changed).await {
            futures::future::Either::Left((result, _)) => {
                // Both futures can become ready in the same scheduling turn.
                // In that tie the left-biased select may return an error from
                // the old stream, so re-check before exposing its result.
                if self.session_generation() == generation {
                    result.map(Some)
                } else {
                    Ok(None)
                }
            }
            futures::future::Either::Right(((), _)) => Ok(None),
        }
    }

    async fn run_with_deadline<F, T>(
        operation: F,
        timeout: Duration,
        description: &'static str,
    ) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        let now = asupersync::Cx::current()
            .and_then(|cx| cx.timer_driver())
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let operation = operation.fuse();
        let deadline = asupersync::time::sleep(now, timeout).fuse();
        futures::pin_mut!(operation, deadline);
        match futures::future::select(operation, deadline).await {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right(((), _)) => Err(tool_err(
                "MCP_TRANSPORT_IO",
                format!("{description} exceeded the absolute {timeout:?} deadline"),
            )),
        }
    }

    /// One POST round-trip, returning a validated JSON-RPC response value.
    async fn round_trip(
        &self,
        frame: &Value,
        timeout: Duration,
        kind: HttpRoundTripKind<'_>,
    ) -> Result<Value> {
        self.round_trip_with_wire_state(frame, timeout, kind, None)
            .await
    }

    async fn round_trip_with_wire_state(
        &self,
        frame: &Value,
        timeout: Duration,
        kind: HttpRoundTripKind<'_>,
        wire_state: Option<&HttpWireState>,
    ) -> Result<Value> {
        // Boxed: clippy::large_futures.
        Box::pin(self.run_until_abort(Self::run_with_deadline(
            self.round_trip_inner(frame, timeout, kind, wire_state),
            timeout,
            "HTTP JSON-RPC round trip",
        )))
        .await
    }

    #[allow(clippy::too_many_lines)]
    async fn round_trip_inner(
        &self,
        frame: &Value,
        timeout: Duration,
        kind: HttpRoundTripKind<'_>,
        wire_state_override: Option<&HttpWireState>,
    ) -> Result<Value> {
        let mut request = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        for (name, value) in &self.headers {
            // ubs:ignore names and values validated at HttpTransport::new
            request = request.header(name.clone(), value.clone());
        }
        let wire_state = wire_state_override
            .cloned()
            .unwrap_or_else(|| self.wire_state());
        let had_assigned_session = wire_state.session_id.is_some();
        if let Some(session) = wire_state.session_id.as_deref() {
            // ubs:ignore CR/LF-filtered at capture (hostile-server guard above)
            request = request.header("Mcp-Session-Id", session.to_string());
        }
        if let Some(protocol_version) = wire_state.protocol_version.as_deref() {
            // ubs:ignore validated at initialize-state capture below
            request = request.header("Mcp-Protocol-Version", protocol_version.to_string());
        }
        let response = request
            .json(frame)
            .map_err(|err| tool_err("MCP_TRANSPORT_IO", format!("encode: {err}")))?
            .timeout(timeout)
            .send()
            .await
            .map_err(|err| tool_err("MCP_TRANSPORT_IO", format!("send: {err}")))?;

        let status = response.status();
        let candidate_session_id = response
            .headers()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("mcp-session-id"))
            .map(|(_, value)| value.clone());
        let content_type = response
            .headers()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        if status == 404 && had_assigned_session {
            self.expire_session_if_current(wire_state.generation);
            return Err(tool_err(
                "MCP_SESSION_EXPIRED",
                "server rejected the active Mcp-Session-Id with HTTP 404",
            ));
        }
        if !(200..300).contains(&status) {
            let body = response.text_limited(4096).await.unwrap_or_default();
            return Err(tool_err(
                "MCP_HTTP_STATUS",
                format!("HTTP {status} from {}: {}", self.url, body.trim()),
            ));
        }
        let initialize_session_id = match kind {
            HttpRoundTripKind::Request {
                method: "initialize",
                ..
            } => candidate_session_id.as_deref(),
            _ => None,
        };
        if let Some(session_id) = initialize_session_id {
            validate_http_session_id(session_id)?;
        }
        if matches!(
            kind,
            HttpRoundTripKind::Request {
                method: "initialize",
                ..
            }
        ) && let Err(error) = self.begin_provisional_session(initialize_session_id)
        {
            // A close that won before the response headers were processed
            // could not have observed this server-created session. If the
            // response future is still being polled, retire that late session
            // before returning the terminal error.
            if is_transport_closed(&error)
                && let Some(session_id) = initialize_session_id
            {
                let _ = self
                    .send_session_delete(HttpWireState {
                        generation: self.session_generation(),
                        session_id: Some(session_id.to_string()),
                        protocol_version: Some(MCP_PROTOCOL_VERSION.to_string()),
                    })
                    .await;
            }
            return Err(error);
        }
        match kind {
            HttpRoundTripKind::AcceptedMessage { .. } if status == 202 => {
                let body = response.bytes_limited(1).await.map_err(|err| {
                    tool_err(
                        "MCP_PROTOCOL",
                        format!("HTTP 202 response body was not empty: {err}"),
                    )
                })?;
                if body.is_empty() {
                    return Ok(Value::Null);
                }
                return Err(tool_err(
                    "MCP_PROTOCOL",
                    "HTTP 202 response to a JSON-RPC notification or response must have no body",
                ));
            }
            HttpRoundTripKind::AcceptedMessage { description } => {
                return Err(tool_err(
                    "MCP_PROTOCOL",
                    format!("HTTP {description} expected status 202, received {status}"),
                ));
            }
            HttpRoundTripKind::Request { method, .. } if status == 202 => {
                return Err(tool_err(
                    "MCP_PROTOCOL",
                    format!("HTTP 202 cannot acknowledge JSON-RPC request {method:?}"),
                ));
            }
            HttpRoundTripKind::Request { .. } => {}
        }

        let HttpRoundTripKind::Request {
            expected_id,
            method,
        } = kind
        else {
            unreachable!("notification status handling returned above");
        };
        let media_type = classify_http_response_media_type(&content_type)?;
        let response_wire_state = if method == "initialize" {
            self.wire_state()
        } else {
            wire_state
        };
        let result = match media_type {
            HttpResponseMediaType::EventStream => {
                self.receive_sse_response(response, expected_id, timeout, &response_wire_state)
                    .await
            }
            HttpResponseMediaType::Json => match response.text_limited(MAX_HTTP_BODY).await {
                Ok(body) => serde_json::from_str::<Value>(&body)
                    .map_err(|err| {
                        tool_err(
                            "MCP_PROTOCOL",
                            format!("response is not JSON: {err} (body: {:.200})", body.trim()),
                        )
                    })
                    .and_then(|value| validate_jsonrpc_response(&value, expected_id)),
                Err(error) => Err(tool_err("MCP_TRANSPORT_IO", format!("read body: {error}"))),
            },
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => return Err(error),
        };

        if method == "initialize" {
            self.capture_initialize_state(&result, candidate_session_id.clone(), frame)?;
        }
        Ok(result)
    }

    fn capture_initialize_state(
        &self,
        result: &Value,
        candidate_session_id: Option<String>,
        initialize_frame: &Value,
    ) -> Result<()> {
        let result = result
            .as_object()
            .ok_or_else(|| tool_err("MCP_PROTOCOL", "initialize result must be an object"))?;
        let protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                tool_err(
                    "MCP_PROTOCOL",
                    "initialize result is missing protocolVersion",
                )
            })?;
        super::config::validate_http_header_value(protocol_version)
            .map_err(|err| tool_err("MCP_PROTOCOL", format!("invalid protocolVersion: {err}")))?;
        if protocol_version != MCP_PROTOCOL_VERSION {
            return Err(tool_err(
                "MCP_PROTOCOL",
                format!(
                    "server selected unsupported protocolVersion {protocol_version:?}; supported version is {MCP_PROTOCOL_VERSION}"
                ),
            ));
        }
        if !result.get("capabilities").is_some_and(Value::is_object) {
            return Err(tool_err(
                "MCP_PROTOCOL",
                "initialize result requires an object-valued capabilities field",
            ));
        }
        let server_info = result
            .get("serverInfo")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                tool_err(
                    "MCP_PROTOCOL",
                    "initialize result requires an object-valued serverInfo field",
                )
            })?;
        if server_info.get("name").and_then(Value::as_str).is_none()
            || server_info.get("version").and_then(Value::as_str).is_none()
        {
            return Err(tool_err(
                "MCP_PROTOCOL",
                "initialize result serverInfo requires string name and version fields",
            ));
        }
        if let Some(session_id) = candidate_session_id.as_deref() {
            validate_http_session_id(session_id)?;
        }
        let mut session = Self::lock(&self.session);
        if !self.alive.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(tool_err(
                "MCP_TRANSPORT_CLOSED",
                "HTTP transport closed before initialize state publication",
            ));
        }
        let generation = session.generation.wrapping_add(1);
        *session = HttpSessionState {
            generation,
            session_id: candidate_session_id,
            protocol_version: Some(protocol_version.to_string()),
            initialize_params: Some(
                initialize_frame
                    .get("params")
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
        };
        drop(session);
        self.session_changed.notify_waiters();
        Ok(())
    }

    async fn request_once(&self, method: &str, params: &Value, timeout: Duration) -> Result<Value> {
        validate_outgoing_params(method, params)?;
        let id = self.next_request_id()?;
        let mut frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
        });
        if !params.is_null() {
            frame["params"] = params.clone();
        }
        let cancellation = (method != "initialize").then(|| self.cancellation_dispatch(id));
        let mut pending_request = PendingHttpRequest {
            transport: self,
            cancellation,
            runtime: asupersync::runtime::Runtime::current_handle(),
            armed: true,
        };
        let result = self
            .round_trip(
                &frame,
                timeout,
                HttpRoundTripKind::Request {
                    expected_id: id,
                    method,
                },
            )
            .await;
        match &result {
            Ok(_) => pending_request.disarm(),
            Err(error) if is_session_expired(error) || is_server_error(error) => {
                pending_request.disarm();
            }
            Err(error) if is_transport_io(error) || is_delivery_indeterminate(error) => {
                pending_request.cancel_and_abort().await;
            }
            Err(_) => {
                // A malformed or mismatched response proves the request was
                // accepted but leaves its outcome unknowable. Never reuse
                // that logical HTTP session for another side effect.
                self.abort();
                pending_request.disarm();
            }
        }
        result
    }

    async fn notify_once(&self, method: &str, params: &Value) -> Result<()> {
        validate_outgoing_params(method, params)?;
        let mut frame = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        if !params.is_null() {
            frame["params"] = params.clone();
        }
        let mut pending_notification = PendingHttpRequest {
            transport: self,
            cancellation: None,
            runtime: None,
            armed: true,
        };
        let result = self
            .round_trip(
                &frame,
                DEFAULT_MCP_TIMEOUT,
                HttpRoundTripKind::AcceptedMessage {
                    description: "JSON-RPC notification",
                },
            )
            .await
            .map(|_| ());
        pending_notification.disarm();
        if let Err(error) = &result
            && !is_session_expired(error)
            && !is_server_error(error)
        {
            // The notification's delivery or framing is ambiguous. Retiring
            // the connection is safer than sending later side effects on a
            // stream whose protocol state may already have advanced.
            self.abort();
        }
        result
    }

    async fn send_server_response(&self, frame: &Value, wire_state: &HttpWireState) -> Result<()> {
        // Box the nested POST edge: an SSE response can contain a server
        // request, whose client response is another HTTP round-trip.
        let result = Box::pin(self.round_trip_with_wire_state(
            frame,
            DEFAULT_MCP_TIMEOUT,
            HttpRoundTripKind::AcceptedMessage {
                description: "JSON-RPC response",
            },
            Some(wire_state),
        ))
        .await;
        match result {
            Err(error) if is_session_expired(&error) => Err(tool_err(
                "MCP_DELIVERY_INDETERMINATE",
                "server rejected the client response inside an already-accepted SSE request; refusing to replay the originating request",
            )),
            result => result.map(|_| ()),
        }
    }

    async fn open_event_stream(
        &self,
        wire_state: &HttpWireState,
        last_event_id: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<HttpEventStreamOpen> {
        let had_session = wire_state.session_id.is_some();
        let mut request = self
            .client
            .get(&self.url)
            .header("Accept", "text/event-stream");
        for (name, value) in &self.headers {
            request = request.header(name.clone(), value.clone());
        }
        if let Some(session_id) = wire_state.session_id.as_deref() {
            request = request.header("Mcp-Session-Id", session_id.to_string());
        }
        if let Some(protocol_version) = wire_state.protocol_version.as_deref() {
            request = request.header("Mcp-Protocol-Version", protocol_version.to_string());
        }
        if let Some(last_event_id) = last_event_id {
            super::config::validate_http_header_value(last_event_id).map_err(|err| {
                tool_err(
                    "MCP_PROTOCOL",
                    format!("invalid Last-Event-ID resume cursor: {err}"),
                )
            })?;
            request = request.header("Last-Event-ID", last_event_id.to_string());
        }
        let header_timeout = timeout.unwrap_or(DEFAULT_MCP_TIMEOUT);
        Self::run_with_deadline(
            async move {
                let response = request.no_timeout().send().await.map_err(|err| {
                    tool_err(
                        "MCP_TRANSPORT_IO",
                        format!("open server event stream: {err}"),
                    )
                })?;
                let status = response.status();
                if status == 405 {
                    // GET is optional. Do not let a hostile or broken server
                    // hold activation open by drip-feeding an ignored body.
                    return Ok(HttpEventStreamOpen::Unsupported);
                }
                if status == 404 && had_session {
                    // The status is sufficient to classify expiry; dropping
                    // the body makes fail-close independent of body progress.
                    return Ok(HttpEventStreamOpen::SessionExpired);
                }
                if status != 200 {
                    // The server-initiated stream is optional. A server that
                    // answers the GET with anything but a stream (202, 404
                    // without a session, 4xx/5xx) simply does not offer one at
                    // this endpoint; treating that as fatal retired the whole
                    // transport, including a working POST channel, so the next
                    // request or notification failed with
                    // "HTTP transport was aborted before request dispatch".
                    let body = response.text_limited(4096).await.unwrap_or_default();
                    tracing::warn!(
                        status,
                        body = %body.trim(),
                        "MCP HTTP server event stream not offered; continuing without it"
                    );
                    return Ok(HttpEventStreamOpen::Unsupported);
                }
                let content_type = response
                    .headers()
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                    .map(|(_, value)| value.as_str())
                    .unwrap_or_default();
                if !content_type
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .eq_ignore_ascii_case("text/event-stream")
                {
                    tracing::warn!(
                        content_type,
                        "MCP HTTP GET answered without text/event-stream; continuing without a server event stream"
                    );
                    return Ok(HttpEventStreamOpen::Unsupported);
                }
                Ok(HttpEventStreamOpen::Stream(response))
            },
            header_timeout,
            "HTTP server-event stream establishment",
        )
        .await
    }

    async fn send_session_delete(&self, wire_state: HttpWireState) -> Result<()> {
        self.send_session_delete_with_timeout(wire_state, HTTP_CLOSE_TIMEOUT)
            .await
    }

    async fn send_session_delete_with_timeout(
        &self,
        wire_state: HttpWireState,
        timeout: Duration,
    ) -> Result<()> {
        self.session_delete_dispatch(wire_state).send(timeout).await
    }

    async fn handle_sse_message(
        &self,
        value: Value,
        expected_id: u64,
        wire_state: &HttpWireState,
    ) -> Result<Option<Value>> {
        let object = value
            .as_object()
            .ok_or_else(|| tool_err("MCP_PROTOCOL", "SSE JSON-RPC message must be an object"))?;
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(tool_err(
                "MCP_PROTOCOL",
                "SSE JSON-RPC message must declare jsonrpc \"2.0\"",
            ));
        }
        if let Some(method) = object.get("method") {
            if object.contains_key("result") || object.contains_key("error") {
                return Err(tool_err(
                    "MCP_PROTOCOL",
                    "SSE JSON-RPC request or notification must not contain result or error",
                ));
            }
            let method = method
                .as_str()
                .ok_or_else(|| tool_err("MCP_PROTOCOL", "SSE JSON-RPC method must be a string"))?;
            if object
                .get("params")
                .is_some_and(|params| !params.is_object())
            {
                return Err(tool_err(
                    "MCP_PROTOCOL",
                    "SSE JSON-RPC request or notification params must be an object",
                ));
            }
            let Some(id) = object.get("id") else {
                // Notifications do not require a response. Optional client
                // features are not advertised by this transport today, so
                // no notification dispatch is required at this layer.
                return Ok(None);
            };
            if !matches!(id, Value::Number(_) | Value::String(_)) {
                return Err(tool_err(
                    "MCP_PROTOCOL",
                    "SSE JSON-RPC request id must be a string or number",
                ));
            }
            let response = if method == "ping" {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}})
            } else {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("client method {method:?} is not supported")
                    }
                })
            };
            self.send_server_response(&response, wire_state).await?;
            return Ok(None);
        }
        validate_jsonrpc_response(&value, expected_id).map(Some)
    }

    async fn handle_sse_event(
        &self,
        event: crate::sse::SseEvent,
        expected_id: u64,
        wire_state: &HttpWireState,
    ) -> Result<Option<Value>> {
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(data).map_err(|err| {
            tool_err(
                "MCP_PROTOCOL",
                format!("SSE event data is not a JSON-RPC message: {err}"),
            )
        })?;
        self.handle_sse_message(value, expected_id, wire_state)
            .await
    }

    async fn handle_server_stream_event(
        &self,
        event: crate::sse::SseEvent,
        wire_state: &HttpWireState,
    ) -> Result<()> {
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(());
        }
        let value: Value = serde_json::from_str(data).map_err(|err| {
            tool_err(
                "MCP_PROTOCOL",
                format!("server-stream SSE data is not a JSON-RPC message: {err}"),
            )
        })?;
        if value.get("method").is_none() {
            return Err(tool_err(
                "MCP_PROTOCOL",
                "unsolicited HTTP GET stream must not contain a JSON-RPC response",
            ));
        }
        let unexpected_response = self.handle_sse_message(value, 0, wire_state).await?;
        debug_assert!(unexpected_response.is_none());
        Ok(())
    }

    async fn receive_request_sse_stream(
        &self,
        response: crate::http::client::Response,
        expected_id: u64,
        cursor: &mut HttpSseCursor,
        wire_state: &HttpWireState,
    ) -> Result<Option<Value>> {
        let mut stream = response.bytes_stream();
        let mut decoder = BoundedHttpSseDecoder::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|err| tool_err("MCP_TRANSPORT_IO", format!("read SSE body: {err}")))?;
            for event in decoder.feed(&chunk)? {
                if !cursor.accept(&event)? {
                    continue;
                }
                if let Some(result) = self
                    .handle_sse_event(event, expected_id, wire_state)
                    .await?
                {
                    return Ok(Some(result));
                }
            }
        }
        for event in decoder.finish()? {
            if !cursor.accept(&event)? {
                continue;
            }
            if let Some(result) = self
                .handle_sse_event(event, expected_id, wire_state)
                .await?
            {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    async fn receive_sse_response(
        &self,
        response: crate::http::client::Response,
        expected_id: u64,
        timeout: Duration,
        wire_state: &HttpWireState,
    ) -> Result<Value> {
        let mut cursor = HttpSseCursor::default();
        match self
            .receive_request_sse_stream(response, expected_id, &mut cursor, wire_state)
            .await
        {
            Ok(Some(result)) => return Ok(result),
            Ok(None) => {}
            Err(error) if is_transport_io(&error) && cursor.resume_id().is_some() => {}
            Err(error) => return Err(error),
        }
        let Some(last_event_id) = cursor.resume_id().map(str::to_string) else {
            return Err(tool_err(
                "MCP_DELIVERY_INDETERMINATE",
                "SSE request stream ended before its JSON-RPC response and supplied no resumable event id",
            ));
        };
        let resumed = match self
            .open_event_stream(wire_state, Some(&last_event_id), Some(timeout))
            .await?
        {
            HttpEventStreamOpen::Stream(response) => response,
            HttpEventStreamOpen::Unsupported => {
                return Err(tool_err(
                    "MCP_DELIVERY_INDETERMINATE",
                    "SSE request stream ended before its response and the server rejected one-shot resumption with HTTP 405",
                ));
            }
            HttpEventStreamOpen::SessionExpired => {
                return Err(tool_err(
                    "MCP_DELIVERY_INDETERMINATE",
                    "HTTP session expired after the server accepted a request; refusing to replay it in a new session",
                ));
            }
        };
        self.receive_request_sse_stream(resumed, expected_id, &mut cursor, wire_state)
            .await?
            .map_or_else(
                || {
                    Err(tool_err(
                        "MCP_DELIVERY_INDETERMINATE",
                        "one-shot SSE resumption ended before the pending JSON-RPC response",
                    ))
                },
                Ok,
            )
    }

    async fn receive_server_event_stream(
        &self,
        response: crate::http::client::Response,
        cursor: &mut HttpSseCursor,
        wire_state: &HttpWireState,
    ) -> Result<()> {
        let mut stream = response.bytes_stream();
        let mut decoder = BoundedHttpSseDecoder::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|err| {
                tool_err(
                    "MCP_TRANSPORT_IO",
                    format!("read server event stream: {err}"),
                )
            })?;
            for event in decoder.feed(&chunk)? {
                if cursor.accept(&event)? {
                    self.handle_server_stream_event(event, wire_state).await?;
                }
            }
        }
        for event in decoder.finish()? {
            if cursor.accept(&event)? {
                self.handle_server_stream_event(event, wire_state).await?;
            }
        }
        Ok(())
    }

    async fn run_server_event_listener(
        &self,
        wire_state: &HttpWireState,
        first: crate::http::client::Response,
    ) -> Result<()> {
        let mut cursor = HttpSseCursor::default();
        let first_result = self
            .receive_server_event_stream(first, &mut cursor, wire_state)
            .await;
        if let Err(error) = first_result
            && (!is_transport_io(&error) || cursor.resume_id().is_none())
        {
            return Err(error);
        }
        let Some(last_event_id) = cursor.resume_id().map(str::to_string) else {
            return Ok(());
        };
        let resumed = match self
            .open_event_stream(wire_state, Some(&last_event_id), None)
            .await?
        {
            HttpEventStreamOpen::Unsupported => return Ok(()),
            HttpEventStreamOpen::SessionExpired => {
                return Err(tool_err(
                    "MCP_SESSION_EXPIRED",
                    "server rejected the active GET stream session during resumption",
                ));
            }
            HttpEventStreamOpen::Stream(response) => response,
        };
        // Exactly one resume attempt per logical GET stream. A second
        // disconnect ends this optional receive channel instead of looping.
        self.receive_server_event_stream(resumed, &mut cursor, wire_state)
            .await
    }

    async fn run_server_event_supervisor(
        &self,
        mut wire_state: HttpWireState,
        first: crate::http::client::Response,
    ) -> Result<()> {
        let mut next_stream = Some(first);
        loop {
            if let Some(stream) = next_stream.take() {
                let listener_result = self
                    .run_until_session_change(
                        wire_state.generation,
                        self.run_server_event_listener(&wire_state, stream),
                    )
                    .await;
                let listener_result = match listener_result {
                    Ok(result) => result,
                    Err(error) => {
                        if self.abort_if_session_generation(wire_state.generation) {
                            tracing::warn!(error = %error, "MCP HTTP server-event listener failed");
                            return Err(error);
                        }
                        // A newer session won the race with the old listener
                        // failure. Its terminal state belongs only to the old
                        // generation, so continue with the replacement.
                        continue;
                    }
                };
                if listener_result == Some(()) {
                    // The optional channel ended or exhausted its single
                    // resume. Stay dormant until a newly initialized
                    // session supplies a fresh, cursor-free stream.
                    let _ = self
                        .run_until_session_change(
                            wire_state.generation,
                            futures::future::pending::<Result<()>>(),
                        )
                        .await?;
                }
            }

            wire_state = self.wire_state();
            if wire_state.protocol_version.is_none() {
                let _ = self
                    .run_until_session_change(
                        wire_state.generation,
                        futures::future::pending::<Result<()>>(),
                    )
                    .await?;
                continue;
            }

            let opened = self
                .run_until_session_change(
                    wire_state.generation,
                    self.open_event_stream(&wire_state, None, None),
                )
                .await;
            let opened = match opened {
                Ok(opened) => opened,
                Err(error) => {
                    if self.abort_if_session_generation(wire_state.generation) {
                        tracing::warn!(error = %error, "MCP HTTP server-event listener failed");
                        return Err(error);
                    }
                    continue;
                }
            };
            let Some(opened) = opened else {
                continue;
            };
            match opened {
                HttpEventStreamOpen::Stream(stream) => next_stream = Some(stream),
                HttpEventStreamOpen::Unsupported => {
                    let _ = self
                        .run_until_session_change(
                            wire_state.generation,
                            futures::future::pending::<Result<()>>(),
                        )
                        .await?;
                }
                HttpEventStreamOpen::SessionExpired => {
                    let error = tool_err(
                        "MCP_SESSION_EXPIRED",
                        "server rejected the active GET stream session with HTTP 404",
                    );
                    if self.abort_if_session_generation(wire_state.generation) {
                        tracing::warn!(error = %error, "MCP HTTP server-event listener failed");
                        return Err(error);
                    }
                }
            }
        }
    }

    async fn renew_session(&self) -> Result<()> {
        let initialize_params = Self::lock(&self.session)
            .initialize_params
            .clone()
            .ok_or_else(|| {
                tool_err(
                    "MCP_SESSION_EXPIRED",
                    "cannot renew an HTTP session without a validated initialize request",
                )
            })?;
        self.reset_session_state();
        let result = match self
            .request_once("initialize", &initialize_params, DEFAULT_MCP_TIMEOUT)
            .await
        {
            Ok(_) => {
                self.notify_once("notifications/initialized", &serde_json::json!({}))
                    .await
            }
            Err(error) => Err(error),
        };
        if result.is_err() {
            self.abort();
        }
        result
    }
}

fn classify_http_response_media_type(content_type: &str) -> Result<HttpResponseMediaType> {
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    if media_type.eq_ignore_ascii_case("application/json") {
        Ok(HttpResponseMediaType::Json)
    } else if media_type.eq_ignore_ascii_case("text/event-stream") {
        Ok(HttpResponseMediaType::EventStream)
    } else {
        Err(tool_err(
            "MCP_PROTOCOL",
            format!(
                "HTTP JSON-RPC request response requires Content-Type application/json or text/event-stream, received {content_type:?}"
            ),
        ))
    }
}

fn is_session_expired(error: &Error) -> bool {
    matches!(
        error,
        Error::Tool { tool, message }
            if tool == "mcp" && message.starts_with("[MCP_SESSION_EXPIRED]")
    )
}

fn is_delivery_indeterminate(error: &Error) -> bool {
    matches!(
        error,
        Error::Tool { tool, message }
            if tool == "mcp" && message.starts_with("[MCP_DELIVERY_INDETERMINATE]")
    )
}

fn is_transport_io(error: &Error) -> bool {
    matches!(
        error,
        Error::Tool { tool, message }
            if tool == "mcp" && message.starts_with("[MCP_TRANSPORT_IO]")
    )
}

fn is_transport_closed(error: &Error) -> bool {
    matches!(
        error,
        Error::Tool { tool, message }
            if tool == "mcp" && message.starts_with("[MCP_TRANSPORT_CLOSED]")
    )
}

fn is_server_error(error: &Error) -> bool {
    matches!(
        error,
        Error::Tool { tool, message }
            if tool == "mcp" && message.starts_with("[MCP_SERVER_ERROR]")
    )
}

fn validate_outgoing_params(method: &str, params: &Value) -> Result<()> {
    if params.is_null() || params.is_object() {
        Ok(())
    } else {
        Err(tool_err(
            "MCP_PROTOCOL",
            format!("JSON-RPC method {method:?} requires object-valued params"),
        ))
    }
}

fn validate_http_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty() || !session_id.bytes().all(|byte| matches!(byte, 0x21..=0x7e)) {
        return Err(tool_err(
            "MCP_PROTOCOL",
            "initialize response Mcp-Session-Id must contain only visible ASCII bytes",
        ));
    }
    super::config::validate_http_header_value(session_id)
        .map_err(|err| tool_err("MCP_PROTOCOL", format!("invalid Mcp-Session-Id: {err}")))
}

fn validate_jsonrpc_response(value: &Value, expected_id: u64) -> Result<Value> {
    let object = value
        .as_object()
        .ok_or_else(|| tool_err("MCP_PROTOCOL", "JSON-RPC response must be an object"))?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(tool_err(
            "MCP_PROTOCOL",
            "JSON-RPC response must declare jsonrpc \"2.0\"",
        ));
    }
    let expected = Value::from(expected_id);
    if object.get("id") != Some(&expected) {
        return Err(tool_err(
            "MCP_PROTOCOL",
            format!(
                "JSON-RPC response id did not match request: expected {expected_id}, received {}",
                object.get("id").unwrap_or(&Value::Null)
            ),
        ));
    }
    if object.contains_key("method") {
        return Err(tool_err(
            "MCP_PROTOCOL",
            "JSON-RPC response must not contain method",
        ));
    }
    match (object.get("result"), object.get("error")) {
        (Some(result), None) => Ok(result.clone()),
        (None, Some(error)) => {
            let error = error
                .as_object()
                .ok_or_else(|| tool_err("MCP_PROTOCOL", "JSON-RPC error must be an object"))?;
            if !matches!(
                error.get("code"),
                Some(Value::Number(code)) if code.is_i64() || code.is_u64()
            ) || error.get("message").and_then(Value::as_str).is_none()
            {
                return Err(tool_err(
                    "MCP_PROTOCOL",
                    "JSON-RPC error requires integer code and string message",
                ));
            }
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown server error");
            Err(tool_err(
                "MCP_SERVER_ERROR",
                format!("server error: {message}"),
            ))
        }
        _ => Err(tool_err(
            "MCP_PROTOCOL",
            "JSON-RPC response requires exactly one of result or error",
        )),
    }
}

/// Synchronous SSE envelope helper for focused parser tests. Production reads
/// and handles the response stream incrementally in `receive_sse_response`.
#[cfg(test)]
fn parse_sse_responses(body: &str, expected_id: u64) -> Result<Value> {
    let mut parser = crate::sse::SseParser::new();
    let events = parser.feed(body);
    for event in events {
        let Ok(value) = serde_json::from_str::<Value>(event.data.trim()) else {
            continue;
        };
        if value.get("result").is_some() || value.get("error").is_some() {
            return validate_jsonrpc_response(&value, expected_id);
        }
    }
    Err(tool_err(
        "MCP_PROTOCOL",
        "SSE stream ended without a JSON-RPC response",
    ))
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let _lane =
            asupersync::sync::OwnedMutexGuard::lock(std::sync::Arc::clone(&self.lane), cx.cx())
                .await
                .map_err(|_| tool_err("MCP_CANCELLED", "cancelled by ambient context"))?;
        if method == "initialize" {
            self.ensure_explicit_initialize_admissible()?;
        }
        let result = match self.request_once(method, &params, timeout).await {
            Err(error) if method != "initialize" && is_session_expired(&error) => {
                // A session-specific 404 proves the server rejected delivery,
                // so exactly one reinitialize and replay is safe. There is no
                // loop: a second 404 is returned to the caller.
                self.renew_session().await?;
                let result = self.request_once(method, &params, timeout).await;
                if let Err(error) = &result
                    && is_session_expired(error)
                {
                    self.abort();
                }
                result
            }
            result => result,
        };
        if let Err(error) = &result
            && is_delivery_indeterminate(error)
        {
            self.abort();
        }
        result
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let _lane =
            asupersync::sync::OwnedMutexGuard::lock(std::sync::Arc::clone(&self.lane), cx.cx())
                .await
                .map_err(|_| tool_err("MCP_CANCELLED", "cancelled by ambient context"))?;
        match self.notify_once(method, &params).await {
            Err(error) if is_session_expired(&error) => {
                self.renew_session().await?;
                if method == "notifications/initialized" {
                    Ok(())
                } else {
                    let result = self.notify_once(method, &params).await;
                    if let Err(error) = &result
                        && is_session_expired(error)
                    {
                        self.abort();
                    }
                    result
                }
            }
            result => result,
        }
    }

    fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn abort(&self) {
        // Serialize terminal admission with initialize-state publication.
        let session = Self::lock(&self.session);
        self.alive.store(false, std::sync::atomic::Ordering::SeqCst);
        drop(session);
        self.abort_notify.notify_waiters();
    }

    async fn activate(self: std::sync::Arc<Self>) -> Result<()> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(tool_err(
                "MCP_TRANSPORT_UNAVAILABLE",
                "cannot activate an aborted HTTP transport",
            ));
        }
        if self
            .listener_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        let Some(runtime) = asupersync::runtime::Runtime::current_handle() else {
            self.listener_started.store(false, Ordering::Release);
            return Err(tool_err(
                "MCP_RUNTIME_UNAVAILABLE",
                "cannot start the HTTP server-event listener outside an active runtime",
            ));
        };
        let wire_state = self.wire_state();
        let first = match self
            .run_until_abort(self.open_event_stream(&wire_state, None, None))
            .await
        {
            Ok(HttpEventStreamOpen::Stream(response)) => response,
            Ok(HttpEventStreamOpen::Unsupported) => return Ok(()),
            Ok(HttpEventStreamOpen::SessionExpired) => {
                self.abort();
                return Err(tool_err(
                    "MCP_SESSION_EXPIRED",
                    "server rejected the active GET stream session during activation",
                ));
            }
            Err(error) => {
                self.abort();
                return Err(error);
            }
        };
        if self.session_generation() != wire_state.generation {
            self.abort();
            return Err(tool_err(
                "MCP_SESSION_CHANGED",
                "HTTP session changed while activating its GET stream",
            ));
        }
        let listener_transport = std::sync::Arc::clone(&self);
        if let Err(error) = runtime.try_spawn(async move {
            let supervisor = listener_transport
                .run_until_abort(listener_transport.run_server_event_supervisor(wire_state, first));
            // Boxed: clippy::large_futures.
            let result = Box::pin(supervisor).await;
            if let Err(error) = result
                && listener_transport.is_alive()
            {
                tracing::warn!(error = %error, "MCP HTTP server-event listener failed");
            }
        }) {
            self.listener_started.store(false, Ordering::Release);
            return Err(tool_err(
                "MCP_RUNTIME_UNAVAILABLE",
                format!("failed to start HTTP server-event listener: {error}"),
            ));
        }
        Ok(())
    }

    async fn close(&self) {
        // Atomically reject new publication and take the latest session, then
        // tell the server to discard exactly that captured state.
        let wire_state = self.seal_and_take_session();
        let _ = self.send_session_delete(wire_state).await;
    }

    fn diagnostics_tail(&self) -> String {
        format!("http transport to {}", self.url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured (headers, body) pairs shared with a fixture server thread.
    type CapturedRequests = std::sync::Arc<std::sync::Mutex<Vec<(Vec<(String, String)>, String)>>>;

    #[test]
    // Naive count is fine at this data size; not worth a dependency.
    #[allow(clippy::naive_bytecount)]
    fn stdio_encoding_is_one_compact_json_line() {
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "text": "first\nsecond" },
        });
        let encoded = encode_stdio_message(&value).expect("encode");
        assert_eq!(encoded.last(), Some(&b'\n'));
        assert_eq!(encoded.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert!(!encoded.starts_with(b"Content-Length:"));
        let decoded: Value =
            serde_json::from_slice(&encoded[..encoded.len() - 1]).expect("decode JSON line");
        assert_eq!(decoded, value);
    }

    #[test]
    fn stdio_encoding_stops_at_the_configured_cap() {
        let value = serde_json::json!({"text": "\0\0\0\0"});
        let mut writer = CappedJsonWriter::new(8);
        let error = serde_json::to_writer(&mut writer, &value)
            .expect_err("escaped JSON must exceed the small cap");
        assert!(
            writer.exceeded,
            "cap error must be distinguished from JSON errors"
        );
        assert!(
            writer.bytes.len() <= 8,
            "capped writer over-allocated: {error}"
        );

        let error = encode_stdio_message_with_limit(&value, 8)
            .expect_err("outbound encoder must surface its cap");
        assert!(matches!(error, McpStdioError::Request(_)));
        assert!(error.message().contains("exceeds 8 bytes"));
    }

    #[test]
    fn stdio_writer_queue_reports_predispatch_backpressure_without_dropping_queued_work() {
        let (writer_tx, writer_rx) = std::sync::mpsc::sync_channel(1);
        try_enqueue_client_command(&writer_tx, WriterCommand::Message(vec![1]))
            .expect("first command fills the queue");
        let error = try_enqueue_client_command(&writer_tx, WriterCommand::Message(vec![2]))
            .expect_err("second command must observe bounded backpressure");
        assert!(matches!(error, McpStdioError::Backpressure(_)));
        assert!(!error.breaks_transport());
        let WriterCommand::Message(message) = writer_rx.recv().expect("queued command remains")
        else {
            panic!("expected queued message");
        };
        assert_eq!(message, vec![1]);
        let pending = Mutex::new(HashMap::new());
        let alive = AtomicBool::new(true);
        let closing = AtomicBool::new(true);
        let tree_cleanup_state = Mutex::new(TreeCleanupState::Pending);
        stop_reader_connection(
            &writer_tx,
            &pending,
            &alive,
            &closing,
            &tree_cleanup_state,
            0,
            McpStdioError::Closed("test shutdown".to_string()),
        );
        assert!(matches!(
            writer_rx
                .recv()
                .expect("terminal connection stop wakes idle writer"),
            WriterCommand::Close
        ));
    }

    #[test]
    fn stdio_writer_shutdown_survives_a_full_cancellation_queue() {
        struct HeldFirstWrite {
            entered: Option<std::sync::mpsc::Sender<()>>,
            release: std::sync::Arc<(Mutex<bool>, std::sync::Condvar)>,
        }

        impl Write for HeldFirstWrite {
            // Guard scope is deliberate; tightening drops would change lock-hold semantics.
            #[allow(clippy::significant_drop_tightening)]
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if let Some(entered) = self.entered.take() {
                    let _ = entered.send(());
                    let (released, wake) = &*self.release;
                    let mut released = lock(released);
                    while !*released {
                        released = wake
                            .wait(released)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                }
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (writer_tx, writer_rx) = std::sync::mpsc::sync_channel(1);
        assert!(
            writer_tx
                .try_send(WriterCommand::Cancellation(vec![1]))
                .is_ok()
        );
        let pending = std::sync::Arc::new(Mutex::new(HashMap::new()));
        let alive = std::sync::Arc::new(AtomicBool::new(true));
        let closing = std::sync::Arc::new(AtomicBool::new(true));
        let tree_cleanup_state = std::sync::Arc::new(Mutex::new(TreeCleanupState::Pending));
        let release = std::sync::Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let writer_release = std::sync::Arc::clone(&release);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let writer_pending = std::sync::Arc::clone(&pending);
        let writer_alive = std::sync::Arc::clone(&alive);
        let writer_closing = std::sync::Arc::clone(&closing);
        let writer_cleanup = std::sync::Arc::clone(&tree_cleanup_state);
        let writer = std::thread::spawn(move || {
            writer_loop(
                HeldFirstWrite {
                    entered: Some(entered_tx),
                    release: writer_release,
                },
                writer_rx,
                writer_pending,
                writer_alive,
                writer_closing,
                writer_cleanup,
                0,
            );
            let _ = done_tx.send(());
        });
        let first_write_entered = entered_rx.recv_timeout(Duration::from_millis(500)).is_ok();
        let second_cancellation_queued = writer_tx
            .try_send(WriterCommand::Cancellation(vec![2]))
            .is_ok();

        // Release the held first write only after the full-queue wake attempt.
        // With the required state-before-wake order, the queued cancellation
        // observes `alive=false` and exits. Reversing that order lets it write
        // and block in recv, producing a bounded red result here.
        let mut completed_after_wake = false;
        ReaderConnectionStop {
            writer_tx: &writer_tx,
            pending: &pending,
            alive: &alive,
            closing: &closing,
            tree_cleanup_state: &tree_cleanup_state,
            pid: 0,
        }
        .finish(
            McpStdioError::Closed("test shutdown".to_string()),
            |writer_tx| {
                wake_writer_shutdown(writer_tx);
                let (released, wake) = &*release;
                *lock(released) = true;
                wake.notify_all();
                completed_after_wake = done_rx.recv_timeout(Duration::from_millis(500)).is_ok();
            },
        );
        // Always release and join a mutated writer before asserting so a red
        // test cannot strand its helper thread.
        let (released, wake) = &*release;
        *lock(released) = true;
        wake.notify_all();
        drop(writer_tx);
        writer.join().expect("writer helper");
        assert!(first_write_entered, "writer never entered its first write");
        assert!(
            second_cancellation_queued,
            "test did not establish a full writer queue"
        );
        assert!(
            completed_after_wake,
            "stopped writer must exit after the full-queue wake seam"
        );
    }

    #[test]
    fn stdio_reader_preserves_back_to_back_messages() {
        let first = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}});
        let second = serde_json::json!({"jsonrpc":"2.0","id":2,"result":42});
        let mut bytes = encode_stdio_message(&first).expect("first");
        bytes.extend_from_slice(&encode_stdio_message(&second).expect("second"));
        let mut reader = BufReader::new(bytes.as_slice());
        assert_eq!(
            read_stdio_message(&mut reader).expect("first read"),
            Some(first)
        );
        assert_eq!(
            read_stdio_message(&mut reader).expect("second read"),
            Some(second)
        );
        assert_eq!(read_stdio_message(&mut reader).expect("EOF"), None);
    }

    #[test]
    fn stdio_reader_rejects_lsp_malformed_oversize_and_partial_lines() {
        let mut lsp = BufReader::new(b"Content-Length: 2\r\n\r\n{}".as_slice());
        assert_eq!(
            read_stdio_message_with_limit(&mut lsp, 64)
                .expect_err("LSP framing must not parse")
                .kind(),
            std::io::ErrorKind::InvalidData
        );

        let mut malformed = BufReader::new(b"{not-json}\n".as_slice());
        assert_eq!(
            read_stdio_message_with_limit(&mut malformed, 64)
                .expect_err("malformed JSON must fail")
                .kind(),
            std::io::ErrorKind::InvalidData
        );

        let mut oversize = BufReader::new(b"123456789\n".as_slice());
        assert_eq!(
            read_stdio_message_with_limit(&mut oversize, 8)
                .expect_err("oversize line must fail")
                .kind(),
            std::io::ErrorKind::InvalidData
        );

        let mut partial = BufReader::new(b"{}".as_slice());
        assert_eq!(
            read_stdio_message_with_limit(&mut partial, 8)
                .expect_err("unterminated final line must fail")
                .kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn stdio_router_validates_envelopes_and_correlates_exact_ids() {
        let pending: StdioPending = Mutex::new(HashMap::new());
        let (completion_tx, completion_rx) = std::sync::mpsc::sync_channel(1);
        lock(&pending).insert(7, completion_tx);
        let (writer_tx, _writer_rx) = std::sync::mpsc::sync_channel(1);

        route_stdio_message(
            &serde_json::json!({"jsonrpc":"2.0","id":8,"result":"wrong"}),
            &pending,
            &writer_tx,
        )
        .expect("unknown late id is ignored");
        assert!(matches!(
            completion_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        route_stdio_message(
            &serde_json::json!({"jsonrpc":"2.0","id":7,"result":"right"}),
            &pending,
            &writer_tx,
        )
        .expect("matching response");
        assert_eq!(
            completion_rx.recv().expect("completion").expect("success"),
            serde_json::json!("right")
        );

        for malformed in [
            serde_json::json!({"id":1,"result":null}),
            serde_json::json!({"jsonrpc":"2.0","id":"1","result":null}),
            serde_json::json!({"jsonrpc":"2.0","id":1}),
            serde_json::json!({"jsonrpc":"2.0","id":1,"result":null,"error":{"code":-1,"message":"both"}}),
            serde_json::json!({"jsonrpc":"2.0","id":1,"error":{"code":"bad","message":"x"}}),
        ] {
            assert!(
                matches!(
                    route_stdio_message(&malformed, &pending, &writer_tx),
                    Err(McpStdioError::Protocol(_))
                ),
                "malformed response was accepted: {malformed}"
            );
        }
    }

    #[test]
    fn stdio_router_handles_ping_and_rejects_unsupported_server_requests() {
        let pending: StdioPending = Mutex::new(HashMap::new());
        let (writer_tx, writer_rx) = std::sync::mpsc::sync_channel(2);
        route_stdio_message(
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": "ping-1",
                "method": "ping",
            }),
            &pending,
            &writer_tx,
        )
        .expect("ping response");
        let WriterCommand::Message(message) = writer_rx.recv().expect("ping writer command") else {
            panic!("expected JSON-RPC ping response message");
        };
        let response: Value =
            serde_json::from_slice(&message[..message.len() - 1]).expect("ping response JSON");
        assert_eq!(
            response,
            serde_json::json!({"jsonrpc":"2.0","id":"ping-1","result":{}})
        );

        route_stdio_message(
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": "server-request-1",
                "method": "sampling/createMessage",
                "params": {},
            }),
            &pending,
            &writer_tx,
        )
        .expect("unsupported request response");
        let WriterCommand::Message(message) = writer_rx.recv().expect("writer command") else {
            panic!("expected JSON-RPC response message");
        };
        let response: Value =
            serde_json::from_slice(&message[..message.len() - 1]).expect("response JSON");
        assert_eq!(response["id"], "server-request-1");
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn stdio_stderr_escapes_terminal_and_bidi_controls() {
        assert_eq!(
            sanitize_stderr("ok\u{1b}[31m\u{202e}end\n"),
            "ok\\u{1b}[31m\\u{202e}end\n"
        );
    }

    #[test]
    fn sse_parse_extracts_result() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
        let value = parse_sse_responses(body, 1).expect("result");
        assert_eq!(value["tools"], serde_json::json!([]));
    }

    #[test]
    fn sse_parse_surfaces_error() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"no method\"}}\n\n";
        let err = parse_sse_responses(body, 1).expect_err("error");
        assert!(err.to_string().contains("no method"), "{err}");
    }

    #[test]
    fn sse_parse_skips_non_rpc_events() {
        let body = "event: ping\ndata: {}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":42}\n\n";
        let value = parse_sse_responses(body, 2).expect("result after skip");
        assert_eq!(value, serde_json::json!(42));
    }

    #[test]
    fn sse_parse_rejects_wrong_response_version_and_id() {
        for body in [
            "event: message\ndata: {\"jsonrpc\":\"1.0\",\"id\":7,\"result\":42}\n\n",
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":8,\"result\":42}\n\n",
        ] {
            let error = parse_sse_responses(body, 7)
                .expect_err("a mismatched SSE response envelope must fail");
            assert!(error.to_string().contains("MCP_PROTOCOL"), "{error}");
        }
    }

    #[test]
    fn sse_parse_requires_response() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n";
        assert!(parse_sse_responses(body, 1).is_err());
    }

    #[test]
    fn http_transport_rejects_protocol_owned_and_unsafe_custom_headers() {
        for headers in [
            vec![(
                "Mcp-Protocol-Version".to_string(),
                MCP_PROTOCOL_VERSION.to_string(),
            )],
            vec![("Mcp-Session-Id".to_string(), "caller-owned".to_string())],
            vec![("X-Test".to_string(), "line\nforge".to_string())],
        ] {
            let error = HttpTransport::new("http://127.0.0.1:1/mcp", headers)
                .err()
                .expect("unsafe or protocol-owned custom header must fail closed");
            assert!(error.to_string().contains("MCP_CONFIG_INVALID"), "{error}");
        }
    }

    #[test]
    fn http_abort_cancels_in_flight_work_and_rejects_later_dispatch() {
        let transport = std::sync::Arc::new(
            HttpTransport::new("http://127.0.0.1:1/mcp", Vec::new()).expect("HTTP transport"),
        );
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let aborting_transport = std::sync::Arc::clone(&transport);
        let aborter = std::thread::spawn(move || {
            let operation_started = started_rx.recv_timeout(Duration::from_secs(1)).is_ok();
            aborting_transport.abort();
            operation_started
        });
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let guarded_result = runtime.block_on(async {
            let cx = crate::agent_cx::AgentCx::for_current_or_request();
            let now = cx
                .cx()
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
            asupersync::time::timeout(
                now,
                Duration::from_secs(2),
                Box::pin(transport.run_until_abort(async move {
                    started_tx.send(()).expect("signal operation start");
                    futures::future::pending::<Result<Value>>().await
                })),
            )
            .await
        });
        let operation_started = aborter.join().expect("abort thread");
        let error = guarded_result
            .expect("abort cancellation test exceeded its outer watchdog")
            .expect_err("abort must cancel the in-flight operation");
        assert!(
            operation_started,
            "the controlled operation must be polled before abort"
        );
        assert!(error.to_string().contains("MCP_TRANSPORT_CLOSED"));
        assert!(!transport.is_alive());

        let error = runtime
            .block_on(transport.request("tools/list", serde_json::json!({}), Duration::MAX))
            .expect_err("an aborted HTTP transport must reject later dispatch");
        assert!(error.to_string().contains("MCP_TRANSPORT_UNAVAILABLE"));
    }

    // ===== bd-zz6yo: streamable HTTP protocol contract =====

    /// Render a scripted response after substituting the request id.
    ///
    /// The placeholder and the decimal id can have different byte lengths, so
    /// a response template with `Content-Length` must be reframed after the
    /// substitution. Chunked responses have no such header and pass through.
    fn render_scripted_response(response: &str, request_id: &str) -> String {
        let mut rendered = response.replace("__ECHO_ID__", request_id);
        let Some(headers_end) = rendered.find("\r\n\r\n") else {
            return rendered;
        };
        let body_len = rendered.len().saturating_sub(headers_end + 4);
        let lowercase_headers = rendered[..headers_end].to_ascii_lowercase();
        let marker = "\r\ncontent-length:";
        let Some(marker_start) = lowercase_headers.find(marker) else {
            return rendered;
        };
        let value_start = marker_start + marker.len();
        let value_end = rendered[value_start..headers_end]
            .find("\r\n")
            .map_or(headers_end, |offset| value_start + offset);
        rendered.replace_range(value_start..value_end, &format!(" {body_len}"));
        rendered
    }

    /// One-connection-per-request HTTP fixture: serves scripted responses in
    /// order, captures each request's headers/body, and replaces the
    /// __ECHO_ID__ token with the captured request's JSON-RPC id so scripted
    /// results always satisfy the exact-id rule regardless of transport
    /// id allocation.
    struct ScriptedHttpServer {
        addr: std::net::SocketAddr,
        captured: CapturedRequests,
        request_lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        stopping: std::sync::Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl ScriptedHttpServer {
        fn start(responses: Vec<String>) -> Self {
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("bind scripted http server");
            let addr = listener.local_addr().expect("listener addr");
            let captured: CapturedRequests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let captured_for_thread = std::sync::Arc::clone(&captured);
            let request_lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let request_lines_for_thread = std::sync::Arc::clone(&request_lines);
            let stopping = std::sync::Arc::new(AtomicBool::new(false));
            let stopping_for_thread = std::sync::Arc::clone(&stopping);
            let handle = std::thread::spawn(move || {
                for response in responses {
                    if stopping_for_thread.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok((mut stream, _)) = listener.accept() else {
                        break;
                    };
                    if stopping_for_thread.load(Ordering::SeqCst) {
                        break;
                    }
                    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let mut reader =
                        std::io::BufReader::new(stream.try_clone().expect("clone stream"));
                    let mut request_line = String::new();
                    if std::io::BufRead::read_line(&mut reader, &mut request_line).is_err() {
                        break;
                    }
                    request_lines_for_thread
                        .lock()
                        .expect("request-line capture lock")
                        .push(request_line.trim_end().to_string());
                    let mut headers: Vec<(String, String)> = Vec::new();
                    let mut content_length = 0usize;
                    let mut request_body_id: Option<u64> = None;
                    loop {
                        let mut line = String::new();
                        match std::io::BufRead::read_line(&mut reader, &mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let trimmed = line.trim_end();
                                if trimmed.is_empty() {
                                    break;
                                }
                                if let Some((name, value)) = trimmed.split_once(':') {
                                    let name = name.trim().to_string();
                                    let value = value.trim().to_string();
                                    if name.eq_ignore_ascii_case("content-length") {
                                        content_length = value.parse().unwrap_or(0);
                                    }
                                    headers.push((name, value));
                                }
                            }
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    if content_length > 0 {
                        std::io::Read::read_exact(&mut reader, &mut body).ok();
                    }
                    let body_text = String::from_utf8_lossy(&body).to_string();
                    if request_body_id.is_none() {
                        request_body_id = body_text.split("\"id\":").nth(1).and_then(|rest| {
                            rest.chars()
                                .take_while(char::is_ascii_digit)
                                .collect::<String>()
                                .parse::<u64>()
                                .ok()
                        });
                    }
                    captured_for_thread
                        .lock()
                        .expect("capture lock")
                        .push((headers, body_text));
                    let id_text =
                        request_body_id.map_or_else(|| "null".to_string(), |n| n.to_string());
                    let response = render_scripted_response(&response, &id_text);
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
            Self {
                addr,
                captured,
                request_lines,
                stopping,
                handle: Some(handle),
            }
        }

        fn captured(&self) -> Vec<(Vec<(String, String)>, String)> {
            self.captured.lock().expect("capture lock").clone()
        }

        fn request_lines(&self) -> Vec<String> {
            self.request_lines
                .lock()
                .expect("request-line capture lock")
                .clone()
        }

        fn url(&self) -> String {
            format!("http://{}/mcp", self.addr)
        }
    }

    impl Drop for ScriptedHttpServer {
        fn drop(&mut self) {
            self.stopping.store(true, Ordering::SeqCst);
            let _ = std::net::TcpStream::connect(self.addr);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// Returns headers immediately, then drip-feeds a chunked body. This
    /// catches accidental body-idle semantics where a lifecycle operation is
    /// required to have an absolute deadline or ignore a non-semantic body.
    struct DripBodyHttpServer {
        addr: std::net::SocketAddr,
        request_received: std::sync::Arc<AtomicBool>,
        accepted: std::sync::Arc<AtomicU64>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl DripBodyHttpServer {
        fn start(status: u16, reason: &'static str) -> Self {
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("bind drip http server");
            let addr = listener.local_addr().expect("listener addr");
            let request_received = std::sync::Arc::new(AtomicBool::new(false));
            let request_received_for_thread = std::sync::Arc::clone(&request_received);
            let accepted = std::sync::Arc::new(AtomicU64::new(0));
            let accepted_for_thread = std::sync::Arc::clone(&accepted);
            let handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept drip request");
                accepted_for_thread.fetch_add(1, Ordering::AcqRel);
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut reader =
                    std::io::BufReader::new(stream.try_clone().expect("clone drip request stream"));
                loop {
                    let mut line = String::new();
                    match std::io::BufRead::read_line(&mut reader, &mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if line.trim_end().is_empty() => break,
                        Ok(_) => {}
                    }
                }
                request_received_for_thread.store(true, Ordering::Release);
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain\r\nconnection: close\r\ntransfer-encoding: chunked\r\n\r\n"
                );
                if stream.write_all(head.as_bytes()).is_ok() && stream.flush().is_ok() {
                    for _ in 0..200 {
                        if stream.write_all(b"1\r\nx\r\n").is_err() || stream.flush().is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let _ = stream.write_all(b"0\r\n\r\n");
                    let _ = stream.flush();
                }
                drop(stream);

                // Keep the listening socket briefly available so a mistaken
                // initialize cancellation notification becomes observable as
                // a second HTTP connection.
                listener.set_nonblocking(true).ok();
                for _ in 0..50 {
                    match listener.accept() {
                        Ok((_stream, _)) => {
                            accepted_for_thread.fetch_add(1, Ordering::AcqRel);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                addr,
                request_received,
                accepted,
                handle: Some(handle),
            }
        }

        fn url(&self) -> String {
            format!("http://{}/mcp", self.addr)
        }

        fn finish(mut self) -> u64 {
            if let Some(handle) = self.handle.take() {
                handle.join().expect("join drip http server");
            }
            self.accepted.load(Ordering::Acquire)
        }
    }

    impl Drop for DripBodyHttpServer {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    struct ControlledInitializeHttpServer {
        addr: std::net::SocketAddr,
        request_received: std::sync::Arc<AtomicBool>,
        release_headers: std::sync::Arc<AtomicBool>,
        headers_sent: std::sync::Arc<AtomicBool>,
        release_body: std::sync::Arc<AtomicBool>,
        delete_count: std::sync::Arc<AtomicU64>,
        shutdown: std::sync::Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl ControlledInitializeHttpServer {
        #[allow(clippy::too_many_lines)]
        fn start() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("bind controlled initialize server");
            let addr = listener.local_addr().expect("listener addr");
            let request_received = std::sync::Arc::new(AtomicBool::new(false));
            let request_received_for_thread = std::sync::Arc::clone(&request_received);
            let release_headers = std::sync::Arc::new(AtomicBool::new(false));
            let release_headers_for_thread = std::sync::Arc::clone(&release_headers);
            let headers_sent = std::sync::Arc::new(AtomicBool::new(false));
            let headers_sent_for_thread = std::sync::Arc::clone(&headers_sent);
            let release_body = std::sync::Arc::new(AtomicBool::new(false));
            let release_body_for_thread = std::sync::Arc::clone(&release_body);
            let delete_count = std::sync::Arc::new(AtomicU64::new(0));
            let delete_count_for_thread = std::sync::Arc::clone(&delete_count);
            let shutdown = std::sync::Arc::new(AtomicBool::new(false));
            let shutdown_for_thread = std::sync::Arc::clone(&shutdown);
            let handle = std::thread::spawn(move || {
                let read_request = |stream: &mut std::net::TcpStream| {
                    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let mut reader =
                        std::io::BufReader::new(stream.try_clone().expect("clone request stream"));
                    let mut request_line = String::new();
                    std::io::BufRead::read_line(&mut reader, &mut request_line)
                        .expect("read request line");
                    let mut headers = Vec::new();
                    let mut content_length = 0usize;
                    loop {
                        let mut line = String::new();
                        std::io::BufRead::read_line(&mut reader, &mut line)
                            .expect("read request header");
                        let trimmed = line.trim_end();
                        if trimmed.is_empty() {
                            break;
                        }
                        if let Some((name, value)) = trimmed.split_once(':') {
                            let name = name.trim().to_string();
                            let value = value.trim().to_string();
                            if name.eq_ignore_ascii_case("content-length") {
                                content_length = value.parse().unwrap_or(0);
                            }
                            headers.push((name, value));
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    if content_length > 0 {
                        std::io::Read::read_exact(&mut reader, &mut body)
                            .expect("read request body");
                    }
                    (request_line.trim_end().to_string(), headers, body)
                };

                let (mut initialize, _) = listener.accept().expect("accept initialize");
                let (_, _, body) = read_request(&mut initialize);
                let request_id = serde_json::from_slice::<Value>(&body)
                    .expect("initialize JSON")
                    .get("id")
                    .and_then(Value::as_u64)
                    .expect("initialize request id");
                request_received_for_thread.store(true, Ordering::Release);
                for _ in 0..500 {
                    if release_headers_for_thread.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let response_body = format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{request_id},\"result\":{}}}",
                    initialize_success_body()
                );
                let response_headers = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: sid-close-race\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                    response_body.len()
                );
                initialize
                    .write_all(response_headers.as_bytes())
                    .expect("write initialize response headers");
                initialize
                    .flush()
                    .expect("flush initialize response headers");
                headers_sent_for_thread.store(true, Ordering::Release);

                listener
                    .set_nonblocking(true)
                    .expect("set cleanup accept nonblocking");
                let mut body_sent = false;
                let mut shutdown_ticks = 0u16;
                for _ in 0..2_000 {
                    if !body_sent && release_body_for_thread.load(Ordering::Acquire) {
                        let _ = initialize.write_all(response_body.as_bytes());
                        let _ = initialize.flush();
                        body_sent = true;
                    }
                    match listener.accept() {
                        Ok((mut delete, _)) => {
                            let (request_line, headers, _) = read_request(&mut delete);
                            if request_line == "DELETE /mcp HTTP/1.1"
                                && headers.iter().any(|(name, value)| {
                                    name.eq_ignore_ascii_case("Mcp-Session-Id")
                                        && value == "sid-close-race"
                                })
                                && headers.iter().any(|(name, value)| {
                                    name.eq_ignore_ascii_case("Mcp-Protocol-Version")
                                        && value == MCP_PROTOCOL_VERSION
                                })
                            {
                                delete_count_for_thread.fetch_add(1, Ordering::AcqRel);
                            }
                            let _ = delete.write_all(http_empty(204, "No Content").as_bytes());
                            let _ = delete.flush();
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                    if shutdown_for_thread.load(Ordering::Acquire) {
                        shutdown_ticks += 1;
                        if shutdown_ticks >= 40 {
                            break;
                        }
                    }
                }
            });
            Self {
                addr,
                request_received,
                release_headers,
                headers_sent,
                release_body,
                delete_count,
                shutdown,
                handle: Some(handle),
            }
        }

        fn url(&self) -> String {
            format!("http://{}/mcp", self.addr)
        }

        fn delete_count(&self) -> u64 {
            self.delete_count.load(Ordering::Acquire)
        }
    }

    impl Drop for ControlledInitializeHttpServer {
        fn drop(&mut self) {
            self.release_headers.store(true, Ordering::Release);
            self.release_body.store(true, Ordering::Release);
            self.shutdown.store(true, Ordering::Release);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    struct CancellationHttpServer {
        addr: std::net::SocketAddr,
        captured: CapturedRequests,
        slow_started: std::sync::Arc<AtomicBool>,
        cancellation_received: std::sync::Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl CancellationHttpServer {
        #[allow(clippy::too_many_lines)]
        fn start() -> Self {
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("bind cancellation server");
            let addr = listener.local_addr().expect("listener addr");
            let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let captured_for_thread = std::sync::Arc::clone(&captured);
            let slow_started = std::sync::Arc::new(AtomicBool::new(false));
            let slow_started_for_thread = std::sync::Arc::clone(&slow_started);
            let cancellation_received = std::sync::Arc::new(AtomicBool::new(false));
            let cancellation_for_thread = std::sync::Arc::clone(&cancellation_received);
            let handle = std::thread::spawn(move || {
                let capture = |stream: &mut std::net::TcpStream| {
                    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let mut reader =
                        std::io::BufReader::new(stream.try_clone().expect("clone stream"));
                    let mut request_line = String::new();
                    std::io::BufRead::read_line(&mut reader, &mut request_line)
                        .expect("read request line");
                    let mut headers = Vec::new();
                    let mut content_length = 0usize;
                    loop {
                        let mut line = String::new();
                        std::io::BufRead::read_line(&mut reader, &mut line)
                            .expect("read request header");
                        let trimmed = line.trim_end();
                        if trimmed.is_empty() {
                            break;
                        }
                        if let Some((name, value)) = trimmed.split_once(':') {
                            let name = name.trim().to_string();
                            let value = value.trim().to_string();
                            if name.eq_ignore_ascii_case("content-length") {
                                content_length = value.parse().unwrap_or(0);
                            }
                            headers.push((name, value));
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    if content_length > 0 {
                        std::io::Read::read_exact(&mut reader, &mut body).expect("read body");
                    }
                    (headers, String::from_utf8_lossy(&body).to_string())
                };

                let (mut initialize, _) = listener.accept().expect("accept initialize");
                let initialize_request = capture(&mut initialize);
                let initialize_id = serde_json::from_str::<Value>(&initialize_request.1)
                    .ok()
                    .and_then(|frame| frame.get("id").and_then(Value::as_u64))
                    .unwrap_or(1);
                captured_for_thread
                    .lock()
                    .expect("capture initialize")
                    .push(initialize_request);
                let response = http_ok_with_session(
                    &initialize_id.to_string(),
                    initialize_success_body(),
                    "sid-cancel",
                );
                initialize
                    .write_all(response.as_bytes())
                    .expect("write initialize response");
                initialize.flush().expect("flush initialize response");
                drop(initialize);

                let (mut slow, _) = listener.accept().expect("accept slow request");
                let slow_request = capture(&mut slow);
                captured_for_thread
                    .lock()
                    .expect("capture slow request")
                    .push(slow_request);
                slow.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ntransfer-encoding: chunked\r\n\r\n",
                )
                .expect("write slow response head");
                slow.flush().expect("flush slow response head");
                slow_started_for_thread.store(true, Ordering::Release);
                let cancellation_for_holder = std::sync::Arc::clone(&cancellation_for_thread);
                let holder = std::thread::spawn(move || {
                    for _ in 0..500 {
                        if cancellation_for_holder.load(Ordering::Acquire) {
                            break;
                        }
                        // Keep producing legal SSE comment bytes. An idle-only
                        // timeout would be postponed forever by this activity;
                        // the MCP request's absolute deadline must still fire.
                        if slow.write_all(b"2\r\n:\n\r\n").is_err() || slow.flush().is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    drop(slow);
                });

                listener
                    .set_nonblocking(true)
                    .expect("set cancellation accept nonblocking");
                let mut cancellation = None;
                for _ in 0..500 {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            cancellation = Some(stream);
                            break;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
                if let Some(mut cancellation) = cancellation {
                    let cancellation_request = capture(&mut cancellation);
                    captured_for_thread
                        .lock()
                        .expect("capture cancellation")
                        .push(cancellation_request);
                    cancellation
                        .write_all(http_empty(202, "Accepted").as_bytes())
                        .expect("write cancellation response");
                    cancellation.flush().expect("flush cancellation response");
                    cancellation_for_thread.store(true, Ordering::Release);
                }
                let _ = holder.join();
            });
            Self {
                addr,
                captured,
                slow_started,
                cancellation_received,
                handle: Some(handle),
            }
        }

        fn url(&self) -> String {
            format!("http://{}/mcp", self.addr)
        }

        fn captured(&self) -> Vec<(Vec<(String, String)>, String)> {
            self.captured.lock().expect("capture lock").clone()
        }
    }

    impl Drop for CancellationHttpServer {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    // Ownership is part of the call contract here.
    #[allow(clippy::needless_pass_by_value)]
    fn http_ok_with_session(
        id_placeholder: &str,
        body: serde_json::Value,
        session: &str,
    ) -> String {
        let body = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id_placeholder},\"result\":{body}}}");
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: {session}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    // Ownership is part of the call contract here.
    #[allow(clippy::needless_pass_by_value)]
    fn http_ok_plain(id_placeholder: &str, body: serde_json::Value) -> String {
        let body = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id_placeholder},\"result\":{body}}}");
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    fn http_empty(status: u16, reason: &str) -> String {
        format!("HTTP/1.1 {status} {reason}\r\nconnection: close\r\ncontent-length: 0\r\n\r\n")
    }

    fn http_event_stream(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    async fn wait_for_requests(server: &ScriptedHttpServer, expected: usize) {
        for _ in 0..200 {
            if server.captured().len() >= expected {
                return;
            }
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
        }
        panic!(
            "timed out waiting for {expected} requests; captured {:?}",
            server.request_lines()
        );
    }

    async fn wait_for_flag(flag: &AtomicBool, description: &str) {
        for _ in 0..300 {
            if flag.load(Ordering::Acquire) {
                return;
            }
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {description}");
    }

    fn initialize_success_body() -> serde_json::Value {
        serde_json::json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "serverInfo": {"name": "fixture", "version": "0.0.1"}
        })
    }

    fn runtime_for_tests() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
    }

    /// HTTP 202 is a notification-only status: a JSON-RPC request receiving
    /// it must fail closed (bd-zz6yo).
    #[test]
    fn streamable_http_rejects_202_for_requests() {
        let server = ScriptedHttpServer::start(vec![
            "HTTP/1.1 202 Accepted\r\nconnection: close\r\ncontent-length: 0\r\n\r\n".to_string(),
        ]);
        let transport = HttpTransport::new(&server.url(), Vec::new()).expect("transport");
        let runtime = runtime_for_tests();
        let error = runtime
            .block_on(transport.request(
                "tools/list",
                serde_json::json!({}),
                Duration::from_secs(5),
            ))
            .expect_err("202 must reject a JSON-RPC request");
        assert!(
            error.to_string().contains("202 cannot acknowledge"),
            "{error}"
        );
    }

    /// Notifications REQUIRE 202-with-empty-body; a 202 carrying a body
    /// violates the contract, while an empty 202 resolves (bd-zz6yo).
    #[test]
    fn streamable_http_notification_202_body_rules() {
        let with_body = ScriptedHttpServer::start(vec![
            "HTTP/1.1 202 Accepted\r\nconnection: close\r\ncontent-length: 1\r\n\r\nx".to_string(),
        ]);
        let transport = HttpTransport::new(&with_body.url(), Vec::new()).expect("transport");
        let runtime = runtime_for_tests();
        let error = runtime
            .block_on(transport.notify("notifications/initialized", serde_json::json!({})))
            .expect_err("202 with a body must violate the notification contract");
        assert!(error.to_string().contains("must have no body"), "{error}");
        drop(transport);

        let without_body = ScriptedHttpServer::start(vec![
            "HTTP/1.1 202 Accepted\r\nconnection: close\r\ncontent-length: 0\r\n\r\n".to_string(),
        ]);
        let transport = HttpTransport::new(&without_body.url(), Vec::new()).expect("transport");
        runtime
            .block_on(transport.notify("notifications/initialized", serde_json::json!({})))
            .expect("empty 202 accepts the notification");
    }

    /// Wrong response id and missing/incorrect jsonrpc version are rejected
    /// before any result is surfaced (bd-zz6yo).
    #[test]
    fn streamable_http_rejects_wrong_id_and_version() {
        let wrong_id_body = r#"{"jsonrpc":"2.0","id":999,"result":{}}"#;
        let wrong_id = ScriptedHttpServer::start(vec![format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{wrong_id_body}",
            wrong_id_body.len()
        )]);
        let transport = HttpTransport::new(&wrong_id.url(), Vec::new()).expect("transport");
        let runtime = runtime_for_tests();
        let error = runtime
            .block_on(transport.request(
                "tools/list",
                serde_json::json!({}),
                Duration::from_secs(5),
            ))
            .expect_err("wrong id must be rejected");
        assert!(
            error.to_string().contains("did not match request"),
            "{error}"
        );
        drop(transport);

        let wrong_version_body = r#"{"jsonrpc":"1.0","id":1,"result":{}}"#;
        let wrong_version = ScriptedHttpServer::start(vec![format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{wrong_version_body}",
            wrong_version_body.len()
        )]);
        let transport = HttpTransport::new(&wrong_version.url(), Vec::new()).expect("transport");
        let error = runtime
            .block_on(transport.request(
                "tools/list",
                serde_json::json!({}),
                Duration::from_secs(5),
            ))
            .expect_err("wrong jsonrpc version must be rejected");
        assert!(error.to_string().contains("jsonrpc"), "{error}");
    }

    /// After a validated initialize, subsequent requests propagate BOTH the
    /// negotiated MCP-Protocol-Version and the assigned Mcp-Session-Id
    /// (bd-zz6yo).
    #[test]
    fn streamable_http_propagates_version_and_session_headers() {
        let server = ScriptedHttpServer::start(vec![
            http_ok_with_session("__ECHO_ID__", initialize_success_body(), "sid-77"),
            http_ok_plain("__ECHO_ID__", serde_json::json!({"tools": []})),
        ]);
        let transport = HttpTransport::new(&server.url(), Vec::new()).expect("transport");
        let runtime = runtime_for_tests();
        runtime
            .block_on(transport.request(
                "initialize",
                serde_json::json!({}),
                Duration::from_secs(5),
            ))
            .expect("initialize round trip");
        runtime
            .block_on(transport.request(
                "tools/list",
                serde_json::json!({}),
                Duration::from_secs(5),
            ))
            .expect("tools/list round trip");

        let captured = server.captured();
        assert_eq!(captured.len(), 2);
        let header = |index: usize, name: &str| {
            captured[index]
                .0
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        };
        assert_eq!(
            header(1, "Mcp-Session-Id").as_deref(),
            Some("sid-77"),
            "session id replays after initialize"
        );
        assert_eq!(
            header(1, "MCP-Protocol-Version").as_deref(),
            Some(MCP_PROTOCOL_VERSION),
            "negotiated version propagates on subsequent requests"
        );
        assert!(
            header(0, "Mcp-Session-Id").is_none(),
            "the initialize request itself sends no session id"
        );
    }

    /// Session-expiry 404 clears stale state and performs EXACTLY ONE
    /// bounded reinitialize/retry; a second consecutive expiry surfaces to
    /// the caller instead of looping (bd-zz6yo).
    #[test]
    fn streamable_http_session_expiry_renews_exactly_once() {
        let server = ScriptedHttpServer::start(vec![
            http_ok_with_session("__ECHO_ID__", initialize_success_body(), "sid-old"),
            "HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 0\r\n\r\n".to_string(),
            http_ok_with_session("__ECHO_ID__", initialize_success_body(), "sid-new"),
            http_empty(202, "Accepted"),
            http_ok_plain("__ECHO_ID__", serde_json::json!({"tools": []})),
            "HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 0\r\n\r\n".to_string(),
            http_ok_with_session("__ECHO_ID__", initialize_success_body(), "sid-new2"),
            http_empty(202, "Accepted"),
            "HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 0\r\n\r\n".to_string(),
        ]);
        let transport = HttpTransport::new(&server.url(), Vec::new()).expect("transport");
        let runtime = runtime_for_tests();

        runtime
            .block_on(transport.request(
                "initialize",
                serde_json::json!({}),
                Duration::from_secs(5),
            ))
            .expect("initial initialize");
        runtime
            .block_on(transport.request(
                "tools/list",
                serde_json::json!({}),
                Duration::from_secs(5),
            ))
            .expect("expired session renews exactly once and retries successfully");

        // A second consecutive expiry surfaces to the caller (no loop).
        let error = runtime
            .block_on(transport.request(
                "tools/list",
                serde_json::json!({}),
                Duration::from_secs(5),
            ))
            .expect_err("second expiry must surface instead of looping forever");
        assert!(error.to_string().contains("404"), "{error}");

        let captured = server.captured();
        assert_eq!(
            captured.len(),
            9,
            "first expiry: init, list-404, renewal-init, initialized, list-ok; \
             second expiry: list-404, renewal-init, initialized, list-404; got {captured:#?}"
        );
        assert_eq!(
            captured
                .iter()
                .filter(|(_, body)| body.contains("\"method\":\"initialize\""))
                .count(),
            3,
            "initialization must happen once initially and once per expired call"
        );
    }

    /// The optional GET channel is activated after the handshake, answers
    /// server pings, resumes exactly once with its own cursor, and suppresses
    /// a replayed event id rather than duplicating the response side effect.
    #[test]
    fn streamable_http_get_resumes_once_without_duplicate_delivery() {
        let first_stream = concat!(
            "id: stream-1:1\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":\"server-1\",\"method\":\"ping\"}\n\n",
        );
        let resumed_stream = concat!(
            "id: stream-1:1\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":\"server-1\",\"method\":\"ping\"}\n\n",
            "id: stream-1:2\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":\"server-2\",\"method\":\"ping\"}\n\n",
        );
        let server = ScriptedHttpServer::start(vec![
            http_ok_with_session("__ECHO_ID__", initialize_success_body(), "sid-get"),
            http_empty(202, "Accepted"),
            http_event_stream(first_stream),
            http_empty(202, "Accepted"),
            http_event_stream(resumed_stream),
            http_empty(202, "Accepted"),
        ]);
        let transport =
            std::sync::Arc::new(HttpTransport::new(&server.url(), Vec::new()).expect("transport"));
        let runtime = runtime_for_tests();
        runtime.block_on(async {
            transport
                .request("initialize", serde_json::json!({}), Duration::from_secs(5))
                .await
                .expect("initialize");
            transport
                .notify("notifications/initialized", serde_json::json!({}))
                .await
                .expect("initialized notification");
            std::sync::Arc::clone(&transport)
                .activate()
                .await
                .expect("activate GET listener");
            wait_for_requests(&server, 6).await;
            transport.abort();
        });

        let lines = server.request_lines();
        assert_eq!(
            lines.iter().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "POST /mcp HTTP/1.1",
                "POST /mcp HTTP/1.1",
                "GET /mcp HTTP/1.1",
                "POST /mcp HTTP/1.1",
                "GET /mcp HTTP/1.1",
                "POST /mcp HTTP/1.1",
            ]
        );
        let captured = server.captured();
        let header = |index: usize, name: &str| {
            captured[index]
                .0
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(header(2, "Accept"), Some("text/event-stream"));
        assert_eq!(header(2, "Last-Event-ID"), None);
        assert_eq!(header(4, "Last-Event-ID"), Some("stream-1:1"));
        assert_eq!(header(4, "Mcp-Session-Id"), Some("sid-get"));
        assert_eq!(
            captured
                .iter()
                .filter(|(_, body)| body.contains("\"id\":\"server-1\""))
                .count(),
            1,
            "replayed event id must not send the server-1 response twice"
        );
        assert_eq!(
            captured
                .iter()
                .filter(|(_, body)| body.contains("\"id\":\"server-2\""))
                .count(),
            1
        );
    }

    /// HTTP 405 disables only the optional GET channel. Orderly close still
    /// sends a bounded DELETE, and both 2xx and 405 DELETE responses are
    /// accepted by the best-effort teardown path.
    #[test]
    fn streamable_http_get_405_and_session_delete_lifecycle() {
        let server = ScriptedHttpServer::start(vec![
            http_ok_with_session("__ECHO_ID__", initialize_success_body(), "sid-close"),
            http_empty(202, "Accepted"),
            http_empty(405, "Method Not Allowed"),
            http_empty(204, "No Content"),
        ]);
        let transport =
            std::sync::Arc::new(HttpTransport::new(&server.url(), Vec::new()).expect("transport"));
        let runtime = runtime_for_tests();
        runtime.block_on(async {
            transport
                .request("initialize", serde_json::json!({}), Duration::from_secs(5))
                .await
                .expect("initialize");
            transport
                .notify("notifications/initialized", serde_json::json!({}))
                .await
                .expect("initialized notification");
            std::sync::Arc::clone(&transport)
                .activate()
                .await
                .expect("activate GET listener");
            wait_for_requests(&server, 3).await;
            transport.close().await;
            wait_for_requests(&server, 4).await;
        });
        let lines = server.request_lines();
        assert_eq!(lines[2], "GET /mcp HTTP/1.1");
        assert_eq!(lines[3], "DELETE /mcp HTTP/1.1");
        let captured = server.captured();
        assert!(captured[3].0.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("Mcp-Session-Id") && value == "sid-close"
        }));
        assert!(!transport.is_alive());

        let rejects_delete = ScriptedHttpServer::start(vec![
            http_ok_with_session("__ECHO_ID__", initialize_success_body(), "sid-no-delete"),
            http_empty(405, "Method Not Allowed"),
        ]);
        let transport = HttpTransport::new(&rejects_delete.url(), Vec::new()).expect("transport");
        runtime.block_on(async {
            transport
                .request("initialize", serde_json::json!({}), Duration::from_secs(5))
                .await
                .expect("initialize");
            transport.close().await;
        });
        assert_eq!(
            rejects_delete.request_lines(),
            vec!["POST /mcp HTTP/1.1", "DELETE /mcp HTTP/1.1"]
        );
    }

    #[test]
    fn streamable_http_idless_event_blocks_unsafe_resume_until_checkpoint() {
        let mut cursor = HttpSseCursor::default();
        let identified = crate::sse::SseEvent {
            id: Some("checkpoint-1".to_string()),
            id_was_explicit: true,
            data: "{}".to_string(),
            ..crate::sse::SseEvent::default()
        };
        assert!(cursor.accept(&identified).expect("identified event"));
        assert_eq!(cursor.resume_id(), Some("checkpoint-1"));

        let idless_request = crate::sse::SseEvent {
            // The parser carries the last id value forward, but marks that it
            // was not explicit on this event. Treating `id.is_some()` alone as
            // resumable would skip or replay this side effect incorrectly.
            id: Some("checkpoint-1".to_string()),
            id_was_explicit: false,
            data: r#"{"jsonrpc":"2.0","id":"ping-1","method":"ping"}"#.to_string(),
            ..crate::sse::SseEvent::default()
        };
        assert!(cursor.accept(&idless_request).expect("idless event"));
        assert_eq!(
            cursor.resume_id(),
            None,
            "an earlier checkpoint cannot safely resume past an idless side effect"
        );

        let next_checkpoint = crate::sse::SseEvent {
            id: Some("checkpoint-2".to_string()),
            id_was_explicit: true,
            data: "{}".to_string(),
            ..crate::sse::SseEvent::default()
        };
        assert!(cursor.accept(&next_checkpoint).expect("new checkpoint"));
        assert_eq!(cursor.resume_id(), Some("checkpoint-2"));
    }

    #[test]
    fn streamable_http_stale_generation_cannot_expire_new_session() {
        let transport =
            HttpTransport::new("http://127.0.0.1:1/mcp", Vec::new()).expect("HTTP transport");
        transport
            .capture_initialize_state(
                &initialize_success_body(),
                Some("sid-old".to_string()),
                &serde_json::json!({"params": {}}),
            )
            .expect("capture old session");
        let old = transport.wire_state();
        transport
            .capture_initialize_state(
                &initialize_success_body(),
                Some("sid-new".to_string()),
                &serde_json::json!({"params": {}}),
            )
            .expect("capture new session");

        transport.expire_session_if_current(old.generation);
        let current = transport.wire_state();
        assert_eq!(current.session_id.as_deref(), Some("sid-new"));
        assert_ne!(current.generation, old.generation);
    }

    #[test]
    fn streamable_http_generation_change_wins_ready_listener_error() {
        let transport =
            HttpTransport::new("http://127.0.0.1:1/mcp", Vec::new()).expect("HTTP transport");
        transport
            .capture_initialize_state(
                &initialize_success_body(),
                Some("sid-old".to_string()),
                &serde_json::json!({"params": {}}),
            )
            .expect("capture old session");
        let old = transport.wire_state();
        let runtime = runtime_for_tests();
        let outcome = runtime.block_on(async {
            transport
                .run_until_session_change(old.generation, async {
                    // Make both the operation error and generation-change
                    // waiter ready in one poll. A left-biased select must not
                    // let the stale error escape.
                    transport.capture_initialize_state(
                        &initialize_success_body(),
                        Some("sid-new".to_string()),
                        &serde_json::json!({"params": {}}),
                    )?;
                    Err::<(), Error>(tool_err(
                        "MCP_TRANSPORT_IO",
                        "old listener failed during renewal",
                    ))
                })
                .await
        });
        assert!(
            outcome.expect("generation race result").is_none(),
            "the old listener result must be discarded after renewal"
        );
        assert!(
            !transport.abort_if_session_generation(old.generation),
            "a stale listener must not retire the replacement session"
        );
        assert!(transport.is_alive());
        assert_eq!(
            transport.wire_state().session_id.as_deref(),
            Some("sid-new")
        );
    }

    #[test]
    fn streamable_http_abort_wins_simultaneously_ready_operation() {
        let transport =
            HttpTransport::new("http://127.0.0.1:1/mcp", Vec::new()).expect("HTTP transport");
        let error = runtime_for_tests()
            .block_on(transport.run_until_abort(async {
                // The operation becomes ready in the same poll that makes the
                // abort waiter ready. The terminal post-check must win.
                transport.abort();
                Ok::<(), Error>(())
            }))
            .expect_err("success must not escape after transport abort");
        assert!(
            error.to_string().contains("MCP_TRANSPORT_CLOSED"),
            "{error}"
        );
    }

    #[test]
    // The block deliberately yields the still-pending initialize future so the
    // test can drop it outside the runtime.
    #[allow(clippy::async_yields_async)]
    fn streamable_http_close_during_initialize_rejects_and_deletes_session() {
        let server = ControlledInitializeHttpServer::start();
        let transport = std::sync::Arc::new(
            HttpTransport::new(&server.url(), Vec::new()).expect("HTTP transport"),
        );
        let runtime = runtime_for_tests();
        let pending_initialize = runtime.block_on(async {
            let initialize = Box::pin(transport.request(
                "initialize",
                serde_json::json!({}),
                Duration::from_secs(5),
            ));
            let request_started = Box::pin(async {
                wait_for_flag(&server.request_received, "controlled initialize dispatch").await;
            });
            let pending_initialize =
                match futures::future::select(initialize, request_started).await {
                    futures::future::Either::Left((result, _)) => {
                        panic!("controlled initialize unexpectedly completed: {result:?}");
                    }
                    futures::future::Either::Right(((), pending_initialize)) => pending_initialize,
                };

            server.release_headers.store(true, Ordering::Release);
            wait_for_flag(&server.headers_sent, "controlled initialize headers").await;
            let provisional_published = Box::pin(async {
                for _ in 0..300 {
                    if transport.wire_state().session_id.as_deref() == Some("sid-close-race") {
                        return;
                    }
                    asupersync::time::sleep(
                        asupersync::time::wall_now(),
                        Duration::from_millis(10),
                    )
                    .await;
                }
                panic!("timed out waiting for provisional initialize session publication");
            });

            match futures::future::select(pending_initialize, provisional_published).await {
                futures::future::Either::Left((result, _)) => {
                    panic!("initialize completed before its body was released: {result:?}");
                }
                futures::future::Either::Right(((), pending_initialize)) => pending_initialize,
            }
        });

        // Exercise cancellation outside an active runtime. Dropping the armed
        // request must abort dispatch without erasing the provisional session
        // that an orderly close still owns.
        drop(pending_initialize);
        assert!(!transport.is_alive());
        assert_eq!(
            transport.wire_state().session_id.as_deref(),
            Some("sid-close-race"),
            "future drop must preserve the provisional cleanup candidate"
        );

        runtime.block_on(transport.close());
        assert_eq!(
            server.delete_count(),
            1,
            "close must await exactly one DELETE for the provisional session"
        );
        assert!(transport.wire_state().session_id.is_none());
    }

    #[test]
    fn streamable_http_failed_initialize_retry_preserves_session_for_close() {
        let malformed_body = "{";
        let server = ScriptedHttpServer::start(vec![
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: sid-malformed\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{malformed_body}",
                malformed_body.len()
            ),
            http_empty(204, "No Content"),
        ]);
        let transport = HttpTransport::new(&server.url(), Vec::new()).expect("HTTP transport");
        let runtime = runtime_for_tests();

        let first_error = runtime
            .block_on(transport.request(
                "initialize",
                serde_json::json!({}),
                Duration::from_secs(5),
            ))
            .expect_err("malformed initialize body must fail");
        assert!(first_error.to_string().contains("MCP_PROTOCOL"));
        assert!(!transport.is_alive());
        assert_eq!(
            transport.wire_state().session_id.as_deref(),
            Some("sid-malformed"),
            "the provisional session remains owned until close"
        );

        let retry_error = runtime
            .block_on(transport.request(
                "initialize",
                serde_json::json!({}),
                Duration::from_secs(5),
            ))
            .expect_err("closed transport must reject initialize retry");
        assert!(
            retry_error.to_string().contains("MCP_TRANSPORT_CLOSED"),
            "{retry_error}"
        );
        assert_eq!(
            transport.wire_state().session_id.as_deref(),
            Some("sid-malformed"),
            "rejected retry must not erase the cleanup candidate"
        );

        runtime.block_on(transport.close());
        assert!(transport.wire_state().session_id.is_none());
        assert_eq!(
            server.request_lines(),
            vec!["POST /mcp HTTP/1.1", "DELETE /mcp HTTP/1.1"],
            "failed initialize, rejected local retry, then exactly one DELETE"
        );
        let captured = server.captured();
        assert!(captured[1].0.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("Mcp-Session-Id") && value == "sid-malformed"
        }));
        assert!(captured[1].0.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("Mcp-Protocol-Version") && value == MCP_PROTOCOL_VERSION
        }));
    }

    #[test]
    fn streamable_http_reinitialize_rejects_without_abandoning_live_session() {
        let server = ScriptedHttpServer::start(vec![
            http_ok_with_session("__ECHO_ID__", initialize_success_body(), "sid-live"),
            http_empty(204, "No Content"),
        ]);
        let transport = HttpTransport::new(&server.url(), Vec::new()).expect("HTTP transport");
        let runtime = runtime_for_tests();

        runtime
            .block_on(transport.request(
                "initialize",
                serde_json::json!({}),
                Duration::from_secs(5),
            ))
            .expect("first initialize");
        let error = runtime
            .block_on(transport.request(
                "initialize",
                serde_json::json!({}),
                Duration::from_secs(5),
            ))
            .expect_err("an initialized transport must reject reinitialize");
        assert!(error.to_string().contains("MCP_PROTOCOL"), "{error}");
        assert!(transport.is_alive());
        assert_eq!(
            transport.wire_state().session_id.as_deref(),
            Some("sid-live"),
            "rejected reinitialize must preserve the live session"
        );

        runtime.block_on(transport.close());
        assert_eq!(
            server.request_lines(),
            vec!["POST /mcp HTTP/1.1", "DELETE /mcp HTTP/1.1"],
            "only the first initialize and orderly DELETE reach the server"
        );
    }

    #[test]
    fn streamable_http_get_404_fails_activation_and_retires_transport() {
        let server = ScriptedHttpServer::start(vec![
            http_ok_with_session("__ECHO_ID__", initialize_success_body(), "sid-expired"),
            http_empty(202, "Accepted"),
            http_empty(404, "Not Found"),
        ]);
        let transport =
            std::sync::Arc::new(HttpTransport::new(&server.url(), Vec::new()).expect("transport"));
        let runtime = runtime_for_tests();
        let error = runtime.block_on(async {
            transport
                .request("initialize", serde_json::json!({}), Duration::from_secs(5))
                .await
                .expect("initialize");
            transport
                .notify("notifications/initialized", serde_json::json!({}))
                .await
                .expect("initialized notification");
            std::sync::Arc::clone(&transport)
                .activate()
                .await
                .expect_err("session-bearing GET 404 must fail activation")
        });
        assert!(error.to_string().contains("MCP_SESSION_EXPIRED"), "{error}");
        assert!(!transport.is_alive());
        assert_eq!(server.request_lines().len(), 3);
    }

    #[test]
    fn streamable_http_resumed_get_404_retires_current_session() {
        let first_stream = concat!(
            "id: stream-expired:1\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":\"server-ping\",\"method\":\"ping\"}\n\n",
        );
        let server = ScriptedHttpServer::start(vec![
            http_ok_with_session("__ECHO_ID__", initialize_success_body(), "sid-expired"),
            http_empty(202, "Accepted"),
            http_event_stream(first_stream),
            http_empty(202, "Accepted"),
            http_empty(404, "Not Found"),
        ]);
        let transport =
            std::sync::Arc::new(HttpTransport::new(&server.url(), Vec::new()).expect("transport"));
        let runtime = runtime_for_tests();
        runtime.block_on(async {
            transport
                .request("initialize", serde_json::json!({}), Duration::from_secs(5))
                .await
                .expect("initialize");
            transport
                .notify("notifications/initialized", serde_json::json!({}))
                .await
                .expect("initialized notification");
            std::sync::Arc::clone(&transport)
                .activate()
                .await
                .expect("activate initial GET stream");
            wait_for_requests(&server, 5).await;
            for _ in 0..200 {
                if !transport.is_alive() {
                    return;
                }
                asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10))
                    .await;
            }
            panic!("resumed GET 404 did not retire the current transport");
        });
        assert!(!transport.is_alive());
        assert_eq!(server.request_lines()[4], "GET /mcp HTTP/1.1");
    }

    #[test]
    fn streamable_http_get_404_ignores_drip_body() {
        let server = DripBodyHttpServer::start(404, "Not Found");
        let transport = HttpTransport::new(&server.url(), Vec::new()).expect("transport");
        transport
            .capture_initialize_state(
                &initialize_success_body(),
                Some("sid-drip-get".to_string()),
                &serde_json::json!({"params": {}}),
            )
            .expect("capture session");
        let wire_state = transport.wire_state();
        let runtime = runtime_for_tests();
        let started_at = std::time::Instant::now();
        let opened = runtime
            .block_on(transport.open_event_stream(
                &wire_state,
                None,
                Some(Duration::from_millis(50)),
            ))
            .expect("classify GET 404");
        assert!(matches!(opened, HttpEventStreamOpen::SessionExpired));
        assert!(
            started_at.elapsed() < Duration::from_secs(1),
            "GET expiry classification waited for a non-semantic drip body"
        );
        assert_eq!(server.finish(), 1);
    }

    #[test]
    fn streamable_http_delete_error_has_absolute_deadline() {
        let server = DripBodyHttpServer::start(500, "Internal Server Error");
        let transport = HttpTransport::new(&server.url(), Vec::new()).expect("transport");
        transport
            .capture_initialize_state(
                &initialize_success_body(),
                Some("sid-drip-delete".to_string()),
                &serde_json::json!({"params": {}}),
            )
            .expect("capture session");
        let started_at = std::time::Instant::now();
        let error = runtime_for_tests()
            .block_on(transport.send_session_delete_with_timeout(
                transport.wire_state(),
                Duration::from_millis(50),
            ))
            .expect_err("drip-fed DELETE error must hit its absolute deadline");
        assert!(error.to_string().contains("MCP_TRANSPORT_IO"), "{error}");
        assert!(
            started_at.elapsed() < Duration::from_secs(1),
            "DELETE exceeded its absolute close budget"
        );
        assert_eq!(server.finish(), 1);
    }

    #[test]
    fn streamable_http_unfinished_initialize_guard_aborts_without_cancellation() {
        let transport =
            HttpTransport::new("http://127.0.0.1:1/mcp", Vec::new()).expect("HTTP transport");
        {
            let _unfinished_initialize = PendingHttpRequest {
                transport: &transport,
                cancellation: None,
                runtime: None,
                armed: true,
            };
        }
        assert!(!transport.is_alive());
    }

    #[test]
    fn streamable_http_dropped_initialize_future_aborts_without_cancellation() {
        let server = DripBodyHttpServer::start(500, "Internal Server Error");
        let transport = std::sync::Arc::new(
            HttpTransport::new(&server.url(), Vec::new()).expect("HTTP transport"),
        );
        let runtime = runtime_for_tests();
        runtime.block_on(async {
            let initialize = Box::pin(transport.request(
                "initialize",
                serde_json::json!({}),
                Duration::from_secs(5),
            ));
            let request_started = Box::pin(async {
                wait_for_flag(&server.request_received, "initialize request dispatch").await;
            });
            match futures::future::select(initialize, request_started).await {
                futures::future::Either::Left((result, _)) => {
                    panic!("initialize unexpectedly completed: {result:?}");
                }
                futures::future::Either::Right(((), pending_initialize)) => {
                    drop(pending_initialize);
                }
            }
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;
        });
        assert!(!transport.is_alive());
        assert_eq!(
            server.finish(),
            1,
            "initialize has no request id, so dropping it must not emit notifications/cancelled"
        );
    }

    #[test]
    fn streamable_http_dropped_initialized_future_aborts_without_cancellation() {
        let server = DripBodyHttpServer::start(500, "Internal Server Error");
        let transport = std::sync::Arc::new(
            HttpTransport::new(&server.url(), Vec::new()).expect("HTTP transport"),
        );
        transport
            .capture_initialize_state(
                &initialize_success_body(),
                Some("sid-initialized-drop".to_string()),
                &serde_json::json!({"params": {}}),
            )
            .expect("capture initialized session");
        let runtime = runtime_for_tests();
        runtime.block_on(async {
            let initialized =
                Box::pin(transport.notify("notifications/initialized", serde_json::json!({})));
            let request_started = Box::pin(async {
                wait_for_flag(
                    &server.request_received,
                    "initialized notification dispatch",
                )
                .await;
            });
            match futures::future::select(initialized, request_started).await {
                futures::future::Either::Left((result, _)) => {
                    panic!("initialized notification unexpectedly completed: {result:?}");
                }
                futures::future::Either::Right(((), pending_initialized)) => {
                    drop(pending_initialized);
                }
            }
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;
        });
        assert!(!transport.is_alive());
        assert_eq!(
            server.finish(),
            1,
            "notifications have no request id, so dropping initialized must not emit cancellation"
        );
    }

    /// Once an HTTP request has reached a response stream, its absolute timeout
    /// sends notifications/cancelled with the exact request id and retires
    /// the indeterminate session instead of allowing another call.
    #[test]
    fn streamable_http_timeout_cancels_and_retires_session() {
        let server = CancellationHttpServer::start();
        let transport = HttpTransport::new(&server.url(), Vec::new()).expect("HTTP transport");
        let runtime = runtime_for_tests();
        runtime.block_on(async {
            transport
                .request("initialize", serde_json::json!({}), Duration::from_secs(5))
                .await
                .expect("initialize");
            let started_at = std::time::Instant::now();
            let error = transport
                .request(
                    "tools/call",
                    serde_json::json!({"name": "slow", "arguments": {}}),
                    Duration::from_millis(50),
                )
                .await
                .expect_err("absolute timeout must fail the request");
            assert!(error.to_string().contains("MCP_TRANSPORT_IO"), "{error}");
            assert!(
                started_at.elapsed() < Duration::from_secs(2),
                "active SSE drip traffic must not extend the logical request deadline"
            );
            wait_for_flag(&server.cancellation_received, "timeout cancellation").await;
        });
        assert!(!transport.is_alive());
        let captured = server.captured();
        assert_eq!(captured.len(), 3, "initialize, request, cancellation");
        let request_id = serde_json::from_str::<Value>(&captured[1].1)
            .expect("request JSON")
            .get("id")
            .cloned()
            .expect("request id");
        let cancellation: Value = serde_json::from_str(&captured[2].1).expect("cancellation JSON");
        assert_eq!(cancellation["method"], "notifications/cancelled");
        assert_eq!(cancellation["params"]["requestId"], request_id);
        assert!(captured[2].0.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("Mcp-Session-Id") && value == "sid-cancel"
        }));
    }

    /// Dropping the in-flight request future follows the same safety rule as
    /// an explicit timeout: cancellation is dispatched from owned state and
    /// the original transport becomes permanently unusable.
    #[test]
    fn streamable_http_future_drop_cancels_and_retires_session() {
        let server = CancellationHttpServer::start();
        let transport = std::sync::Arc::new(
            HttpTransport::new(&server.url(), Vec::new()).expect("HTTP transport"),
        );
        let runtime = runtime_for_tests();
        runtime.block_on(async {
            transport
                .request("initialize", serde_json::json!({}), Duration::from_secs(5))
                .await
                .expect("initialize");

            let request = Box::pin(transport.request(
                "tools/call",
                serde_json::json!({"name": "slow", "arguments": {}}),
                Duration::from_secs(5),
            ));
            let request_started = Box::pin(async {
                wait_for_flag(&server.slow_started, "slow request dispatch").await;
            });
            match futures::future::select(request, request_started).await {
                futures::future::Either::Left((result, _)) => {
                    panic!("slow request unexpectedly completed: {result:?}");
                }
                futures::future::Either::Right(((), pending_request)) => {
                    drop(pending_request);
                }
            }
            wait_for_flag(&server.cancellation_received, "drop cancellation").await;
        });
        assert!(!transport.is_alive());
        let captured = server.captured();
        assert_eq!(captured.len(), 3, "initialize, request, cancellation");
        let request_id = serde_json::from_str::<Value>(&captured[1].1)
            .expect("request JSON")
            .get("id")
            .cloned()
            .expect("request id");
        let cancellation: Value = serde_json::from_str(&captured[2].1).expect("cancellation JSON");
        assert_eq!(cancellation["params"]["requestId"], request_id);
    }
}
