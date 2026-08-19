//! JSON-RPC 2.0 transport for language-server child processes.
//!
//! One `JsonRpcClient` owns one child process: a dedicated reader thread
//! parses `Content-Length` frames from stdout (blocking I/O on a dedicated
//! OS thread, the same isolation choice as the bash tool's pump threads —
//! see bd-xdcrh.4.3), a mutex-guarded stdin writer serializes frames, and a
//! pending map correlates responses to request ids. The async consumer polls
//! completion receivers with a tick loop so timeouts and ambient
//! cancellation stay responsive without blocking the runtime (bd-cv653.1.1).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver as StdReceiver, SyncSender as StdSyncSender, TrySendError};
use std::sync::{Mutex, MutexGuard};

use serde_json::Value;

use crate::error::{Error, Result};
use crate::tools::{ProcessCleanupMode, ProcessGuard};

/// Hard cap on a single JSON-RPC frame body (64 MiB); larger frames are
/// treated as transport corruption and kill the connection.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
/// Bound on the notification queue; overflow drops oldest-first by count and
/// is recorded in `dropped_notifications`.
const NOTIFICATION_QUEUE_CAP: usize = 1024;
/// Bound on retained server stderr (diagnostics surface), bytes.
const STDERR_TAIL_CAP: usize = 32 * 1024;

/// A JSON-RPC error object returned by the server.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Why a request could not complete.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransportError {
    /// The server returned a JSON-RPC error object.
    Server(RpcErrorObject),
    /// The transport closed (server exited or pipes broke).
    Closed(String),
    /// Local I/O failure writing to or reading from the server.
    Io(String),
}

impl TransportError {
    /// Machine-readable taxonomy code for logs and tool details.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Server(_) => "LSP_SERVER_ERROR",
            Self::Closed(_) => "LSP_TRANSPORT_CLOSED",
            Self::Io(_) => "LSP_TRANSPORT_IO",
        }
    }

    /// Taxonomy code for the MCP flavor of this transport (same classes,
    /// MCP_ prefix so failures name the right subsystem).
    #[must_use]
    pub fn mcp_code(&self) -> String {
        self.code().replace("LSP_", "MCP_")
    }

    /// Human-readable summary.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Server(err) => format!("server error {}: {}", err.code, err.message),
            Self::Closed(reason) => format!("transport closed: {reason}"),
            Self::Io(reason) => format!("transport I/O error: {reason}"),
        }
    }
}

/// A server notification (method + params), queued for the client layer.
#[derive(Debug, Clone)]
pub struct ServerNotification {
    pub method: String,
    pub params: Value,
}

/// How a spawned server's environment is composed.
///
/// Language servers inherit the ambient environment minus scrubbed vars
/// (toolchains need HOME/PATH). MCP servers are third-party code: an
/// explicit allowlist plus their config `env`, never ambient inheritance
/// (bd-cv653.6.1).
#[derive(Debug, Clone)]
pub enum EnvPolicy {
    /// Inherit the ambient environment, then remove the named vars.
    InheritAndScrub(&'static [&'static str]),
    /// Start empty, copy only the named ambient vars, then apply `env`.
    Allowlist(&'static [&'static str]),
}

/// Ambient vars copied to MCP server processes (no secrets: paths, locale,
/// terminal, and temp dirs only).
pub const MCP_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "TEMP",
    "TMP",
    "NO_COLOR",
    "TERM",
    "SystemRoot",
    "SYSTEMROOT",
    "APPDATA",
    "LOCALAPPDATA",
    "USERPROFILE",
    "COMSPEC",
];

/// Shared writer: frames are written atomically under one mutex.
type SharedWriter = Mutex<ChildStdin>;

/// Pending request completions: reader thread sends exactly one result.
type PendingMap = Mutex<HashMap<u64, StdSyncSender<std::result::Result<Value, TransportError>>>>;

/// Hook for server→client requests (e.g. `workspace/applyEdit`). Returning
/// `Some(result)` overrides the default null response.
pub type ServerRequestHandler = std::sync::Arc<dyn Fn(&str, &Value) -> Option<Value> + Send + Sync>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Encode one JSON-RPC message as a `Content-Length` framed payload.
#[must_use]
pub fn encode_frame(body: &Value) -> Vec<u8> {
    let json = serde_json::to_vec(body).unwrap_or_else(|_| b"null".to_vec());
    let mut out = Vec::with_capacity(json.len() + 32);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", json.len()).as_bytes());
    out.extend_from_slice(&json);
    out
}

/// Read one framed message from `reader`. Returns `Ok(None)` on clean EOF
/// before any header byte.
fn read_frame(reader: &mut BufReader<impl Read>) -> std::io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut header_line = Vec::new();
    loop {
        header_line.clear();
        let read = reader.read_until(b'\n', &mut header_line)?;
        if read == 0 {
            return Ok(None); // EOF
        }
        let line = String::from_utf8_lossy(&header_line);
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(value) = trimmed
            .split_once(':')
            .map(|(k, v)| (k.trim(), v.trim()))
            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v)
        {
            content_length = value.parse::<usize>().ok();
        }
    }
    let length = content_length.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing Content-Length header",
        )
    })?;
    if length > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame body {length} bytes exceeds cap {MAX_FRAME_BYTES}"),
        ));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let value = serde_json::from_slice(&body).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid JSON body: {err}"),
        )
    })?;
    Ok(Some(value))
}

/// Bounded tail buffer for server stderr.
#[derive(Debug, Default)]
struct TailBuffer {
    data: String,
    cap: usize,
}

impl TailBuffer {
    fn push(&mut self, chunk: &str) {
        self.data.push_str(chunk);
        if self.data.len() > self.cap {
            let keep_from = self.data.len() - self.cap;
            let boundary = self.data.ceil_char_boundary(keep_from);
            self.data.drain(..boundary);
        }
    }
}

/// Poll a request-completion receiver until it resolves, the deadline
/// passes, or the ambient context cancels.
///
/// On timeout/cancel the caller-provided `on_abandon` runs (used to send
/// `$/cancelRequest`). The receiver is taken by value: `Receiver<T>` is
/// `Send` but not `Sync`, so a by-reference wait would make the caller's
/// future non-`Send`. Shared by the LSP client and the MCP stdio transport
/// (bd-cv653.1.1 / bd-cv653.6.1).
pub async fn await_completion<T>(
    rx: StdReceiver<T>,
    timeout: std::time::Duration,
    on_abandon: impl FnOnce(),
) -> std::result::Result<T, CompletionWaitError> {
    let cx = crate::agent_cx::AgentCx::for_current_or_request();
    let start = cx
        .cx()
        .timer_driver()
        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
    loop {
        match rx.try_recv() {
            Ok(value) => return Ok(value),
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err(CompletionWaitError::Closed);
            }
        }
        let now = cx
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        if std::time::Duration::from_nanos(now.duration_since(start)) >= timeout {
            on_abandon();
            return Err(CompletionWaitError::Timeout);
        }
        if cx.checkpoint().is_err() {
            on_abandon();
            return Err(CompletionWaitError::Cancelled);
        }
        asupersync::time::sleep(now, std::time::Duration::from_millis(10)).await;
    }
}

/// Why a completion wait ended without a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionWaitError {
    /// Deadline exceeded.
    Timeout,
    /// Ambient cancellation fired.
    Cancelled,
    /// The sender dropped without sending.
    Closed,
}

/// Apply the environment policy and the caller's env entries to a command.
fn apply_env_policy(cmd: &mut Command, policy: &EnvPolicy, env: &[(String, String)]) {
    match policy {
        EnvPolicy::InheritAndScrub(scrub) => {
            // Ambient inheritance minus the scrubbed vars. The
            // CARGO_TARGET_DIR scrub keeps server-embedded build runs
            // (rust-analyzer flycheck, cargo metadata) out of foreign,
            // possibly lock-contended, shared target pools.
            for var in *scrub {
                cmd.env_remove(var);
            }
        }
        EnvPolicy::Allowlist(allowlist) => {
            // No ambient inheritance: only allowlisted vars plus the
            // caller's (secret-resolved) env entries.
            cmd.env_clear();
            for var in *allowlist {
                if let Some(value) = std::env::var_os(var) {
                    cmd.env(var, value);
                }
            }
        }
    }
    // The caller's env applies last in both policies (overrides).
    cmd.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
}

/// Build the reader-thread body: parse frames, complete pending requests,
/// queue notifications, answer server→client requests. On exit the
/// transport flips dead and every pending request fails fast.
#[allow(clippy::too_many_arguments)]
fn reader_loop(
    stdout: std::process::ChildStdout,
    pending: std::sync::Arc<PendingMap>,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    writer: std::sync::Arc<SharedWriter>,
    stderr_tail: std::sync::Arc<Mutex<TailBuffer>>,
    notification_tx: StdSyncSender<ServerNotification>,
    dropped: std::sync::Arc<AtomicU64>,
    handler: std::sync::Arc<Mutex<Option<ServerRequestHandler>>>,
) -> impl FnOnce() + Send + 'static {
    move || {
        let mut reader = BufReader::new(stdout);
        let close_reason = loop {
            match read_frame(&mut reader) {
                Ok(Some(message)) => {
                    handle_message(
                        &message,
                        &pending,
                        &writer,
                        &notification_tx,
                        &dropped,
                        &stderr_tail,
                        &handler,
                    );
                }
                Ok(None) => break "server closed stdout (EOF)".to_string(),
                Err(err) => break format!("frame read error: {err}"),
            }
        };
        alive.store(false, Ordering::SeqCst);
        // Fail every outstanding request so waiters wake immediately.
        let mut pending = lock(&pending);
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(TransportError::Closed(close_reason.clone())));
        }
    }
}

/// One JSON-RPC connection to a language-server child process.
pub struct JsonRpcClient {
    child: Mutex<ProcessGuard>,
    writer: std::sync::Arc<SharedWriter>,
    pending: std::sync::Arc<PendingMap>,
    next_id: AtomicU64,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stderr_tail: std::sync::Arc<Mutex<TailBuffer>>,
    notification_rx: Mutex<StdReceiver<ServerNotification>>,
    dropped_notifications: std::sync::Arc<AtomicU64>,
    server_request_handler: std::sync::Arc<Mutex<Option<ServerRequestHandler>>>,
}

impl JsonRpcClient {
    /// Spawn `command` with `args`/`env` rooted at `cwd` and start the
    /// reader/stderr pump threads.
    ///
    /// # Errors
    ///
    /// Returns a tool error when the command cannot be spawned (missing
    /// binary is reported with an install hint by the caller).
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<Self> {
        Self::spawn_with_policy(
            command,
            args,
            env,
            cwd,
            &EnvPolicy::InheritAndScrub(&["CARGO_TARGET_DIR"]),
            "lsp",
        )
    }

    /// Spawn with an explicit environment policy. `flavor` tags spawn errors
    /// (`lsp` vs `mcp`) so failures name the right subsystem.
    ///
    /// # Errors
    ///
    /// Same spawn failures as [`Self::spawn`].
    pub fn spawn_with_policy(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
        policy: &EnvPolicy,
        flavor: &'static str,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_env_policy(&mut cmd, policy, env);
        let mut child: Child = cmd.spawn().map_err(|err| {
            Error::tool(
                flavor,
                format!(
                    "[{}_SERVER_MISSING] failed to spawn {command:?}: {err}",
                    flavor.to_ascii_uppercase()
                ),
            )
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::tool("lsp", "missing child stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::tool("lsp", "missing child stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::tool("lsp", "missing child stderr".to_string()))?;

        let writer = std::sync::Arc::new(Mutex::new(stdin));
        let pending: std::sync::Arc<PendingMap> = std::sync::Arc::new(Mutex::new(HashMap::new()));
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let stderr_tail = std::sync::Arc::new(Mutex::new(TailBuffer {
            data: String::new(),
            cap: STDERR_TAIL_CAP,
        }));
        let (notification_tx, notification_rx) =
            std::sync::mpsc::sync_channel::<ServerNotification>(NOTIFICATION_QUEUE_CAP);
        let dropped_notifications = std::sync::Arc::new(AtomicU64::new(0));
        let server_request_handler: std::sync::Arc<Mutex<Option<ServerRequestHandler>>> =
            std::sync::Arc::new(Mutex::new(None));

        // Reader thread: parse frames, complete pending requests, queue
        // notifications, answer server->client requests with null.
        {
            let reader_body = reader_loop(
                stdout,
                std::sync::Arc::clone(&pending),
                std::sync::Arc::clone(&alive),
                std::sync::Arc::clone(&writer),
                std::sync::Arc::clone(&stderr_tail),
                notification_tx,
                std::sync::Arc::clone(&dropped_notifications),
                std::sync::Arc::clone(&server_request_handler),
            );
            // Intentional detach: the reader exits on pipe EOF when the child dies.
            std::thread::spawn(reader_body); // ubs:ignore intentional detach (EOF-exit)
        }

        // Stderr pump: retain a bounded tail for diagnostics surfacing.
        {
            let stderr_tail = std::sync::Arc::clone(&stderr_tail);
            let pump_body = move || {
                let mut reader = BufReader::new(stderr);
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = String::from_utf8_lossy(&buf[..n]);
                            lock(&stderr_tail).push(&chunk);
                        }
                    }
                }
            };
            // Intentional detach: the pump exits on pipe EOF when the child dies.
            std::thread::spawn(pump_body); // ubs:ignore intentional detach (EOF-exit)
        }

        Ok(Self {
            child: Mutex::new(ProcessGuard::new(child, ProcessCleanupMode::ChildOnly)),
            writer,
            pending,
            next_id: AtomicU64::new(1),
            alive,
            stderr_tail,
            notification_rx: Mutex::new(notification_rx),
            dropped_notifications,
            server_request_handler,
        })
    }

    /// Install a handler for server→client requests. The handler runs on the
    /// reader thread; it must stay fast and non-blocking (file I/O for
    /// `workspace/applyEdit` is acceptable and bounded).
    pub fn set_server_request_handler(&self, handler: ServerRequestHandler) {
        *lock(&self.server_request_handler) = Some(handler);
    }

    /// Whether the transport is still up.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Send a request and return the completion receiver plus the request id
    /// (needed to send `$/cancelRequest` on timeout/cancellation).
    ///
    /// # Errors
    ///
    /// Returns an error when the transport is dead or the write fails.
    pub fn request(
        &self,
        method: &str,
        params: Value,
    ) -> std::result::Result<
        (u64, StdReceiver<std::result::Result<Value, TransportError>>),
        TransportError,
    > {
        if !self.is_alive() {
            return Err(TransportError::Closed(
                "server transport is not alive".to_string(),
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        lock(&self.pending).insert(id, tx);
        let mut frame_value = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
        });
        frame_value["params"] = params;
        let frame = encode_frame(&frame_value);
        let write_result = {
            let mut guard = lock(&self.writer);
            guard.write_all(&frame).and_then(|()| guard.flush())
        };
        if let Err(err) = write_result {
            lock(&self.pending).remove(&id);
            return Err(TransportError::Io(format!("request write failed: {err}")));
        }
        Ok((id, rx))
    }

    /// Send a notification (no response expected).
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub fn notify(&self, method: &str, params: Value) -> std::result::Result<(), TransportError> {
        let mut frame_value = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        frame_value["params"] = params;
        let frame = encode_frame(&frame_value);
        let mut guard = lock(&self.writer);
        guard
            .write_all(&frame)
            .and_then(|()| guard.flush())
            .map_err(|err| TransportError::Io(format!("notification write failed: {err}")))
    }

    /// Cancel an in-flight request: drop the pending entry and notify the
    /// server via `$/cancelRequest`.
    pub fn cancel_request(&self, id: u64) {
        lock(&self.pending).remove(&id);
        let _ = self.notify("$/cancelRequest", serde_json::json!({ "id": id }));
    }

    /// Drain queued server notifications (non-blocking).
    pub fn drain_notifications(&self) -> Vec<ServerNotification> {
        let mut out = Vec::new();
        let rx = lock(&self.notification_rx);
        while let Ok(notification) = rx.try_recv() {
            out.push(notification);
        }
        out
    }

    /// Count of notifications dropped due to queue overflow.
    #[must_use]
    pub fn dropped_notifications(&self) -> u64 {
        self.dropped_notifications.load(Ordering::SeqCst)
    }

    /// Bounded tail of server stderr (newest content last).
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        lock(&self.stderr_tail).data.clone()
    }

    /// Whether the child process has exited (reaps the exit status if so).
    #[must_use]
    pub fn child_exited(&self) -> bool {
        let status = lock(&self.child).try_wait_child();
        matches!(status, Ok(Some(_)))
    }

    /// Graceful stop: best-effort `shutdown` + `exit`, then kill.
    pub fn shutdown(&self) {
        // Do not wait for the response here; the client layer performs the
        // timed shutdown handshake when it has an async context.
        let _ = self.notify("exit", Value::Null);
        self.kill();
    }

    /// Kill the child process immediately.
    pub fn kill(&self) {
        let _ = lock(&self.child).kill();
        self.alive.store(false, Ordering::SeqCst);
    }
}

impl Drop for JsonRpcClient {
    fn drop(&mut self) {
        // ProcessGuard's Drop kills the child if still running; nothing else
        // to do, but be explicit about the intent.
        self.kill();
    }
}

/// Route one decoded message to its destination.
#[allow(clippy::option_if_let_else)] // let-else is clearer than map_or_else with early returns
fn handle_message<W: Write>(
    message: &Value,
    pending: &PendingMap,
    writer: &Mutex<W>,
    notification_tx: &StdSyncSender<ServerNotification>,
    dropped: &AtomicU64,
    stderr_tail: &Mutex<TailBuffer>,
    handler: &Mutex<Option<ServerRequestHandler>>,
) {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str);
    let is_response = method.is_none() && id.is_some();

    if is_response {
        // Response to one of our requests (ids we mint are u64).
        let Some(numeric_id) = id.and_then(|v| v.as_u64()) else {
            return;
        };
        let sender = lock(pending).remove(&numeric_id);
        if let Some(sender) = sender {
            let outcome = if let Some(error) = message.get("error") {
                Err(TransportError::Server(RpcErrorObject {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(-1),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown server error")
                        .to_string(),
                    data: error.get("data").cloned(),
                }))
            } else {
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = sender.send(outcome);
        }
        // Unknown id: response arrived after local timeout/cancel; drop.
        return;
    }

    if let (Some(id), Some(method)) = (id, method) {
        // Server -> client request: consult the installed handler first
        // (workspace/applyEdit and friends), otherwise respond with a null
        // result so the server never blocks on us (workspace/configuration,
        // registerCapability, workDoneProgress/create, showMessageRequest).
        // The id is echoed verbatim (servers may use string ids).
        let custom = lock(handler).as_ref().and_then(|handler| {
            handler(
                method,
                &message.get("params").cloned().unwrap_or(Value::Null),
            )
        });
        let frame = encode_frame(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": custom.unwrap_or(Value::Null),
        }));
        {
            let mut guard = lock(writer);
            let _ = guard.write_all(&frame).and_then(|()| guard.flush());
        }
        return;
    }

    if let Some(method) = method {
        if method == "window/logMessage" {
            let rendered = message
                .get("params")
                .and_then(|p| p.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("");
            lock(stderr_tail).push(&format!("[logMessage] {rendered}\n"));
        }
        let notification = ServerNotification {
            method: method.to_string(),
            params: message.get("params").cloned().unwrap_or(Value::Null),
        };
        match notification_tx.try_send(notification) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {
                dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
    // Anything else is malformed traffic; keep the transport alive and
    // ignore it.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_frame_uses_content_length() {
        let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"ping"});
        let frame = encode_frame(&body);
        let text = String::from_utf8(frame).expect("utf8");
        let json = serde_json::to_string(&body).expect("json");
        assert!(text.starts_with(&format!("Content-Length: {}\r\n\r\n", json.len())));
        assert!(text.ends_with(&json));
    }

    #[test]
    fn read_frame_roundtrips() {
        let body = serde_json::json!({"jsonrpc":"2.0","id":7,"result":{"ok":true}});
        let frame = encode_frame(&body);
        let mut reader = BufReader::new(frame.as_slice());
        let got = read_frame(&mut reader).expect("read").expect("some");
        assert_eq!(got, body);
    }

    #[test]
    fn read_frame_handles_back_to_back_frames() {
        let a = serde_json::json!({"a": 1});
        let b = serde_json::json!({"b": 2});
        let mut bytes = encode_frame(&a);
        bytes.extend_from_slice(&encode_frame(&b));
        let mut reader = BufReader::new(bytes.as_slice());
        assert_eq!(read_frame(&mut reader).expect("read"), Some(a));
        assert_eq!(read_frame(&mut reader).expect("read"), Some(b));
        assert_eq!(read_frame(&mut reader).expect("read"), None);
    }

    #[test]
    fn read_frame_rejects_missing_length() {
        let bytes = b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}";
        let mut reader = BufReader::new(bytes.as_slice());
        assert!(read_frame(&mut reader).is_err());
    }

    #[test]
    fn read_frame_rejects_oversize_length() {
        let bytes = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1);
        let mut reader = BufReader::new(bytes.as_bytes());
        assert!(read_frame(&mut reader).is_err());
    }

    #[test]
    fn tail_buffer_truncates_to_cap() {
        let mut tail = TailBuffer {
            data: String::new(),
            cap: 16,
        };
        tail.push("0123456789");
        tail.push("abcdefghij");
        assert!(tail.data.len() <= 16);
        assert!(tail.data.ends_with("abcdefghij"));
    }

    #[test]
    fn handle_message_completes_pending() {
        let pending: PendingMap = Mutex::new(HashMap::new());
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        lock(&pending).insert(42, tx);
        let (notification_tx, _notification_rx) = std::sync::mpsc::sync_channel(8);
        let dropped = AtomicU64::new(0);
        let stderr_tail = Mutex::new(TailBuffer::default());

        // Build a real writer around a pipe so the helper signature holds.
        let mut cmd = Command::new("cat");
        cmd.stdin(Stdio::piped());
        let mut child = cmd.spawn().expect("cat");
        let writer = Mutex::new(child.stdin.take().expect("stdin"));

        handle_message(
            &serde_json::json!({"jsonrpc":"2.0","id":42,"result":{"value":1}}),
            &pending,
            &writer,
            &notification_tx,
            &dropped,
            &stderr_tail,
            &Mutex::new(None),
        );
        let got = rx.try_recv().expect("completed");
        assert_eq!(
            got.ok().map(|v| v["value"].clone()),
            Some(serde_json::json!(1))
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn handle_message_answers_server_requests_with_null() {
        let pending: PendingMap = Mutex::new(HashMap::new());
        let (notification_tx, _rx) = std::sync::mpsc::sync_channel(8);
        let dropped = AtomicU64::new(0);
        let stderr_tail = Mutex::new(TailBuffer::default());
        let (reader, writer_end) = std::io::pipe().expect("pipe");
        let writer = Mutex::new(writer_end);

        handle_message(
            &serde_json::json!({"jsonrpc":"2.0","id":9,"method":"workspace/configuration","params":{}}),
            &pending,
            &writer,
            &notification_tx,
            &dropped,
            &stderr_tail,
            &Mutex::new(None),
        );
        let frame = read_frame(&mut BufReader::new(reader))
            .expect("read")
            .expect("some");
        assert_eq!(frame["id"], 9);
        assert!(frame.get("result").is_some());
    }

    #[test]
    fn request_and_notify_writes_do_not_deadlock() {
        // Regression guard: the writer mutex must be acquired once per frame
        // write (a lock-then-lock self-deadlock shipped in the first draft
        // and hung every request).
        let temp = tempfile::tempdir().expect("tempdir");
        let client = JsonRpcClient::spawn("cat", &[], &[], temp.path()).expect("spawn cat");
        let (id, _rx) = client
            .request("initialize", serde_json::json!({}))
            .expect("request write must succeed");
        client
            .notify("initialized", serde_json::json!({}))
            .expect("notify write must succeed");
        client.cancel_request(id);
        client.kill();
        assert!(!client.is_alive());
    }

    #[test]
    fn handle_message_routes_notifications_and_bounds_queue() {
        let pending: PendingMap = Mutex::new(HashMap::new());
        let (notification_tx, notification_rx) = std::sync::mpsc::sync_channel(2);
        let dropped = AtomicU64::new(0);
        let stderr_tail = Mutex::new(TailBuffer::default());
        let (_reader, writer_end) = std::io::pipe().expect("pipe");
        let writer = Mutex::new(writer_end);

        for i in 0..4 {
            handle_message(
                &serde_json::json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"i":i}}),
                &pending,
                &writer,
                &notification_tx,
                &dropped,
                &stderr_tail,
                &Mutex::new(None),
            );
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        assert!(notification_rx.try_recv().is_ok());
        assert!(notification_rx.try_recv().is_ok());
        assert!(notification_rx.try_recv().is_err());
    }
}
