//! MCP transports (bd-cv653.6.1).
//!
//! Stdio uses MCP's newline-delimited JSON-RPC wire format with a strict env
//! allowlist; streamable HTTP does POST-per-message with JSON or SSE responses,
//! `Mcp-Session-Id` continuity, and custom headers.

use std::collections::HashMap;
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver as StdReceiver, SyncSender as StdSyncSender, TrySendError};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt as _;
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
        TrySendError::Disconnected(_) => {
            McpStdioError::Closed("stdio writer stopped".to_string())
        }
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
        return writer_tx.try_send(WriterCommand::Message(encoded)).map_err(|error| {
            match error {
                TrySendError::Full(_) => {
                    McpStdioError::Io("outbound stdio queue is full".to_string())
                }
                TrySendError::Disconnected(_) => {
                    McpStdioError::Closed("stdio writer stopped".to_string())
                }
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

fn fail_pending(pending: &StdioPending, error: McpStdioError) {
    let senders: Vec<_> = lock(pending).drain().map(|(_, sender)| sender).collect();
    for sender in senders {
        let _ = sender.send(Err(error.clone()));
    }
}

fn kill_tree_once(pid: u32, cleanup_state: &Mutex<TreeCleanupState>) {
    let mut state = lock(cleanup_state);
    if *state != TreeCleanupState::Killed {
        *state = TreeCleanupState::Killed;
        crate::tools::kill_process_group_tree(Some(pid));
    }
}

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
    fn finish(
        self,
        error: McpStdioError,
        wake_writer: impl FnOnce(&StdSyncSender<WriterCommand>),
    ) {
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

fn classify_read_error(error: std::io::Error) -> McpStdioError {
    match error.kind() {
        std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof => {
            McpStdioError::Protocol(error.to_string())
        }
        _ => McpStdioError::Io(format!("stdio read failed: {error}")),
    }
}

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
                let error = classify_read_error(error);
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

fn is_bidi_control(character: char) -> bool {
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
    fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<Self> {
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
        let tree_cleanup_state =
            std::sync::Arc::new(Mutex::new(TreeCleanupState::Pending));
        let stderr_tail =
            std::sync::Arc::new(Mutex::new(PublicTailBuffer::new()));
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

    fn request(&self, method: &str, params: Value) -> std::result::Result<
        (u64, StdReceiver<StdioOutcome>),
        McpStdioError,
    > {
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
    fn cancellation_was_sent(&mut self) {
        self.send_cancellation = false;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingStdioRequest<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.client
                .cancel_request(self.id, self.send_cancellation);
            self.client.abort();
        }
    }
}

struct StdioAbortGuard<'a> {
    client: &'a McpStdioClient,
    armed: bool,
}

impl StdioAbortGuard<'_> {
    fn disarm(&mut self) {
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

/// Reject CR/LF in a header name or value (header-injection guard). Config
/// files and server-assigned session ids are both untrusted here.
fn require_header_safe(what: &str, value: &str) -> Result<()> {
    if value.contains(['\r', '\n']) {
        return Err(tool_err(
            "MCP_HEADER_UNSAFE",
            format!("{what} contains CR/LF; refusing to send it as a header"),
        ));
    }
    Ok(())
}

/// Streamable HTTP MCP transport.
///
/// POST per message; the response may be a single JSON document or an SSE
/// stream of JSON-RPC messages. The `Mcp-Session-Id` from the initialize
/// response is replayed on later calls.
pub struct HttpTransport {
    client: crate::http::client::Client,
    url: String,
    headers: Vec<(String, String)>,
    session_id: Mutex<Option<String>>,
    alive: std::sync::atomic::AtomicBool,
    abort_notify: asupersync::sync::Notify,
    lane: std::sync::Arc<asupersync::sync::Mutex<()>>,
}

impl HttpTransport {
    /// # Errors
    ///
    /// Fails when any custom header name/value contains CR/LF.
    pub fn new(url: &str, headers: Vec<(String, String)>) -> Result<Self> {
        for (name, value) in &headers {
            require_header_safe("header name", name)?;
            require_header_safe(&format!("header {name:?} value"), value)?;
        }
        Ok(Self {
            client: crate::http::client::Client::new(),
            url: url.to_string(),
            headers,
            session_id: Mutex::new(None),
            alive: std::sync::atomic::AtomicBool::new(true),
            abort_notify: asupersync::sync::Notify::new(),
            lane: std::sync::Arc::new(asupersync::sync::Mutex::new(())),
        })
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right(((), _)) => Err(tool_err(
                "MCP_TRANSPORT_CLOSED",
                "HTTP transport was aborted during an in-flight request",
            )),
        }
    }

    /// One POST round-trip, returning the JSON-RPC response value.
    async fn round_trip(&self, frame: &Value, timeout: Duration) -> Result<Value> {
        self.run_until_abort(self.round_trip_inner(frame, timeout))
            .await
    }

    async fn round_trip_inner(&self, frame: &Value, timeout: Duration) -> Result<Value> {
        let mut request = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        for (name, value) in &self.headers {
            // ubs:ignore CR/LF-validated at HttpTransport::new (require_header_safe)
            request = request.header(name.clone(), value.clone());
        }
        let assigned_session = { Self::lock(&self.session_id).clone() };
        if let Some(session) = assigned_session {
            // ubs:ignore CR/LF-filtered at capture (hostile-server guard above)
            request = request.header("Mcp-Session-Id", session);
        }
        let response = request
            .json(frame)
            .map_err(|err| tool_err("MCP_TRANSPORT_IO", format!("encode: {err}")))?
            .timeout(timeout)
            .send()
            .await
            .map_err(|err| tool_err("MCP_TRANSPORT_IO", format!("send: {err}")))?;

        let status = response.status();
        // Capture the session id the server assigned (initialize response).
        // A CR/LF-laden id is a hostile-server header-injection attempt:
        // fail closed by ignoring it.
        if let Some(assigned) = response
            .headers()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("mcp-session-id"))
            .map(|(_, value)| value.clone())
            .filter(|value| !value.contains(['\r', '\n']))
        {
            *Self::lock(&self.session_id) = Some(assigned);
        }
        let content_type = response
            .headers()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        if status == 202 {
            // Accepted: notification/response acknowledgment, no body.
            return Ok(Value::Null);
        }
        if !(200..300).contains(&status) {
            let body = response.text_limited(4096).await.unwrap_or_default();
            return Err(tool_err(
                "MCP_HTTP_STATUS",
                format!("HTTP {status} from {}: {}", self.url, body.trim()),
            ));
        }
        if content_type.contains("text/event-stream") {
            let body = response
                .text_limited(MAX_HTTP_BODY)
                .await
                .map_err(|err| tool_err("MCP_TRANSPORT_IO", format!("read SSE body: {err}")))?;
            return parse_sse_responses(&body);
        }
        let body = response
            .text_limited(MAX_HTTP_BODY)
            .await
            .map_err(|err| tool_err("MCP_TRANSPORT_IO", format!("read body: {err}")))?;
        let value: Value = serde_json::from_str(&body).map_err(|err| {
            tool_err(
                "MCP_PROTOCOL",
                format!("response is not JSON: {err} (body: {:.200})", body.trim()),
            )
        })?;
        // Unwrap the JSON-RPC envelope, same as the SSE path.
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown server error");
            return Err(tool_err(
                "MCP_SERVER_ERROR",
                format!("server error: {message}"),
            ));
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// Parse an SSE body into the first JSON-RPC message carrying a result or
/// error (legacy SSE accept: events may nest `data:` JSON-RPC frames).
fn parse_sse_responses(body: &str) -> Result<Value> {
    let mut parser = crate::sse::SseParser::new();
    let events = parser.feed(body);
    for event in events {
        for line in event.data.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value.get("result").is_some() || value.get("error").is_some() {
                if let Some(error) = value.get("error") {
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown server error");
                    return Err(tool_err(
                        "MCP_SERVER_ERROR",
                        format!("server error: {message}"),
                    ));
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }
    Err(tool_err(
        "MCP_PROTOCOL",
        "SSE stream ended without a JSON-RPC response",
    ))
}

/// Request id counter for the HTTP transport (process-global).
static NEXT_HTTP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[async_trait]
impl McpTransport for HttpTransport {
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let _lane =
            asupersync::sync::OwnedMutexGuard::lock(std::sync::Arc::clone(&self.lane), cx.cx())
                .await
                .map_err(|_| tool_err("MCP_CANCELLED", "cancelled by ambient context"))?;
        let id = NEXT_HTTP_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.round_trip(&frame, timeout).await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.round_trip(&frame, DEFAULT_MCP_TIMEOUT)
            .await
            .map(|_| ())
    }

    fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn abort(&self) {
        self.alive
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.abort_notify.notify_waiters();
    }

    async fn close(&self) {
        // Local teardown only: no session DELETE in v1 (server support for
        // it is spotty and the session expires server-side regardless).
        self.abort();
        Self::lock(&self.session_id).take();
    }

    fn diagnostics_tail(&self) -> String {
        format!("http transport to {}", self.url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
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
        assert!(writer.exceeded, "cap error must be distinguished from JSON errors");
        assert!(writer.bytes.len() <= 8, "capped writer over-allocated: {error}");

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
        let tree_cleanup_state =
            std::sync::Arc::new(Mutex::new(TreeCleanupState::Pending));
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
        let first_write_entered = entered_rx
            .recv_timeout(Duration::from_millis(500))
            .is_ok();
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
                completed_after_wake = done_rx
                    .recv_timeout(Duration::from_millis(500))
                    .is_ok();
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
        assert_eq!(read_stdio_message(&mut reader).expect("first read"), Some(first));
        assert_eq!(read_stdio_message(&mut reader).expect("second read"), Some(second));
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
        let value = parse_sse_responses(body).expect("result");
        assert_eq!(value["tools"], serde_json::json!([]));
    }

    #[test]
    fn sse_parse_surfaces_error() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"no method\"}}\n\n";
        let err = parse_sse_responses(body).expect_err("error");
        assert!(err.to_string().contains("no method"), "{err}");
    }

    #[test]
    fn sse_parse_skips_non_rpc_events() {
        let body = "event: ping\ndata: {}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":42}\n\n";
        let value = parse_sse_responses(body).expect("result after skip");
        assert_eq!(value, serde_json::json!(42));
    }

    #[test]
    fn sse_parse_requires_response() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n";
        assert!(parse_sse_responses(body).is_err());
    }

    #[test]
    fn http_abort_cancels_in_flight_work_and_rejects_later_dispatch() {
        let transport = std::sync::Arc::new(
            HttpTransport::new("http://127.0.0.1:1/mcp", Vec::new())
                .expect("HTTP transport"),
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
}
